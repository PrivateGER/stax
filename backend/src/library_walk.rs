use std::{
    collections::HashMap,
    fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Instant,
};

use time::OffsetDateTime;
use tokio::{
    sync::Semaphore,
    task::{self, JoinSet},
};
use tracing::warn;
use uuid::Uuid;

use crate::{
    clock::format_timestamp,
    persistence::{CachedMediaRecord, CachedProbeFields, Persistence, WalkRecord},
    protocol::SubtitleTrack,
    thumbnails::{thumbnail_is_up_to_date, thumbnail_path_for},
};

const SUPPORTED_MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "webm", "mov", "m4v", "avi", "mp3", "flac", "wav", "m4a", "aac", "ogg",
];
const SUPPORTED_SUBTITLE_EXTENSIONS: &[&str] = &["vtt", "srt"];

/// Outcome of the walk stage for one library root. Used so the scan
/// handler can enqueue probes/thumbnails for the freshly-inserted rows
/// without a second pass over the database.
#[derive(Debug, Default)]
pub struct WalkOutcome {
    pub total: usize,
    pub cached: usize,
    pub pending: usize,
    pub elapsed_ms: u64,
}

/// One file's worth of metadata produced by the per-directory blocking
/// scan. Subtitles are pre-attached per matching media stem so the
/// recursive walker doesn't need a second `read_dir` per media file.
#[derive(Debug)]
pub(crate) struct DirMediaCandidate {
    pub(crate) relative_path: String,
    pub(crate) file_name: String,
    pub(crate) extension: Option<String>,
    pub(crate) size_bytes: u64,
    pub(crate) modified_at: String,
    modified_system: Option<std::time::SystemTime>,
    pub(crate) subtitle_tracks: Vec<SubtitleTrack>,
}

#[derive(Debug, Default)]
pub(crate) struct DirContents {
    pub(crate) subdirs: Vec<PathBuf>,
    pub(crate) media: Vec<DirMediaCandidate>,
}

#[derive(Clone)]
struct WalkContext {
    root_path: Arc<PathBuf>,
    root_path_string: Arc<String>,
    persistence: Persistence,
    existing_records: Arc<HashMap<String, CachedMediaRecord>>,
    indexed_at: Arc<String>,
    thumbnail_cache_dir: Option<Arc<PathBuf>>,
    semaphore: Arc<Semaphore>,
}

pub(crate) async fn walk_root(
    root_path: PathBuf,
    thumbnail_cache_dir: Option<PathBuf>,
    existing_records: HashMap<String, CachedMediaRecord>,
    walk_workers: usize,
    persistence: Persistence,
) -> Result<WalkOutcome, String> {
    let metadata = fs::metadata(&root_path).map_err(|error| {
        format!(
            "could not read library root '{}': {error}",
            root_path.display()
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "library root '{}' is not a directory",
            root_path.display()
        ));
    }

    let started = Instant::now();
    let root_path_string = root_path.to_string_lossy().to_string();
    let context = WalkContext {
        root_path: Arc::new(root_path.clone()),
        root_path_string: Arc::new(root_path_string.clone()),
        persistence: persistence.clone(),
        existing_records: Arc::new(existing_records),
        indexed_at: Arc::new(format_timestamp(OffsetDateTime::now_utc())),
        thumbnail_cache_dir: thumbnail_cache_dir.map(Arc::new),
        semaphore: Arc::new(Semaphore::new(walk_workers.max(1))),
    };

    let stats = walk_dir(context.clone(), root_path).await?;

    persistence
        .prune_missing_paths(&root_path_string, &stats.kept_ids)
        .await
        .map_err(|error| format!("prune failed: {error}"))?;

    Ok(WalkOutcome {
        total: stats.kept_ids.len(),
        cached: stats.cached,
        pending: stats.pending,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

#[derive(Debug, Default)]
struct WalkStats {
    kept_ids: Vec<Uuid>,
    cached: usize,
    pending: usize,
}

impl WalkStats {
    fn merge(&mut self, other: WalkStats) {
        self.kept_ids.extend(other.kept_ids);
        self.cached += other.cached;
        self.pending += other.pending;
    }
}

fn walk_dir(
    context: WalkContext,
    dir: PathBuf,
) -> Pin<Box<dyn Future<Output = Result<WalkStats, String>> + Send>> {
    Box::pin(async move {
        let permit = context
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("walk semaphore should not close");
        let dir_for_blocking = dir.clone();
        let root_for_blocking = Arc::clone(&context.root_path);
        let indexed_at_for_blocking = Arc::clone(&context.indexed_at);
        let contents = task::spawn_blocking(move || {
            classify_directory(
                &root_for_blocking,
                &dir_for_blocking,
                &indexed_at_for_blocking,
            )
        })
        .await
        .map_err(|error| format!("walker task panicked: {error}"))?;
        drop(permit);

        let DirContents { subdirs, media } = contents;
        let mut joinset = JoinSet::new();
        for subdir in subdirs {
            let context = context.clone();
            joinset.spawn(walk_dir(context, subdir));
        }

        let mut stats = WalkStats::default();
        let mut records = Vec::with_capacity(media.len());
        let mut record_cache_flags = Vec::with_capacity(media.len());
        for candidate in media {
            match build_walk_record(&context, candidate) {
                Ok((record, is_cached)) => {
                    record_cache_flags.push(is_cached);
                    records.push(record);
                }
                Err(error) => warn!(%error, "skipping media file with malformed walk record"),
            }
        }

        if let Err(error) = context.persistence.upsert_walk_records(&records).await {
            warn!(
                dir = %dir.display(),
                count = records.len(),
                %error,
                "failed to upsert directory media records"
            );
        } else {
            for (record, is_cached) in records.iter().zip(record_cache_flags) {
                stats.kept_ids.push(record.id);
                if is_cached {
                    stats.cached += 1;
                } else {
                    stats.pending += 1;
                }
            }
        }

        while let Some(result) = joinset.join_next().await {
            match result {
                Ok(Ok(child)) => stats.merge(child),
                Ok(Err(error)) => warn!(%error, "subdirectory walk failed"),
                Err(error) => warn!(%error, "subdirectory walker panicked"),
            }
        }

        Ok(stats)
    })
}

fn build_walk_record(
    context: &WalkContext,
    candidate: DirMediaCandidate,
) -> Result<(WalkRecord, bool), String> {
    let prior = context.existing_records.get(&candidate.relative_path);
    let media_id = prior.map(|record| record.id).unwrap_or_else(Uuid::new_v4);

    let cached = prior.and_then(|record| {
        let unchanged = record.size_bytes == candidate.size_bytes
            && record.modified_at == candidate.modified_at
            && record.probe_error.is_none()
            && record.probed_at.is_some();
        if unchanged {
            Some(CachedProbeFields {
                probed_at: record.probed_at.clone(),
                probe_error: record.probe_error.clone(),
                duration_seconds: record.duration_seconds,
                container_name: record.container_name.clone(),
                video_codec: record.video_codec.clone(),
                audio_codec: record.audio_codec.clone(),
                width: record.width,
                height: record.height,
                playback_mode: record.playback_mode,
                video_profile: record.video_profile.clone(),
                video_level: record.video_level,
                video_pix_fmt: record.video_pix_fmt.clone(),
                video_bit_depth: record.video_bit_depth,
                audio_streams: record.audio_streams.clone(),
                subtitle_streams: record.subtitle_streams.clone(),
            })
        } else {
            None
        }
    });
    let is_cache_hit = cached.is_some();

    let cached_thumbnail = if is_cache_hit {
        let on_disk_fresh = context
            .thumbnail_cache_dir
            .as_deref()
            .map(|dir| thumbnail_path_for(dir, media_id))
            .map(|path| thumbnail_is_up_to_date(&path, candidate.modified_system))
            .unwrap_or(false);

        if on_disk_fresh {
            Some((Some(format_timestamp(OffsetDateTime::now_utc())), None))
        } else {
            Some((None, None))
        }
    } else {
        None
    };

    let content_type = candidate
        .extension
        .as_deref()
        .and_then(content_type_for_extension)
        .map(str::to_string);

    Ok((
        WalkRecord {
            id: media_id,
            root_path: context.root_path_string.as_ref().clone(),
            relative_path: candidate.relative_path,
            file_name: candidate.file_name,
            extension: candidate.extension,
            size_bytes: candidate.size_bytes,
            modified_at: candidate.modified_at,
            indexed_at: context.indexed_at.as_ref().clone(),
            content_type,
            subtitle_tracks: candidate.subtitle_tracks,
            cached_probe: cached,
            cached_thumbnail,
        },
        is_cache_hit,
    ))
}

pub(crate) fn classify_directory(root_path: &Path, dir: &Path, indexed_at: &str) -> DirContents {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            warn!(path = %dir.display(), %error, "skipping unreadable directory during library scan");
            return DirContents::default();
        }
    };

    let mut subdirs = Vec::new();
    let mut media_entries: Vec<(PathBuf, fs::Metadata, String, String)> = Vec::new();
    let mut subtitle_entries: Vec<(PathBuf, String, String)> = Vec::new();

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(value) => value,
            Err(error) => {
                warn!(path = %path.display(), %error, "skipping path with unreadable file type during library scan");
                continue;
            }
        };

        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            subdirs.push(path);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        let Some(extension) = normalized_extension(&path) else {
            continue;
        };
        let stem = path
            .file_stem()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default();

        if SUPPORTED_MEDIA_EXTENSIONS.contains(&extension.as_str()) {
            let metadata = match entry.metadata() {
                Ok(value) => value,
                Err(error) => {
                    warn!(path = %path.display(), %error, "skipping unreadable file during library scan");
                    continue;
                }
            };
            media_entries.push((path, metadata, stem, extension));
        } else if SUPPORTED_SUBTITLE_EXTENSIONS.contains(&extension.as_str()) {
            subtitle_entries.push((path, stem, extension));
        }
    }

    subdirs.sort_unstable();
    media_entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let mut media = Vec::with_capacity(media_entries.len());
    let mut subtitles_by_media_stem: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, (_, sub_stem, _)) in subtitle_entries.iter().enumerate() {
        subtitles_by_media_stem
            .entry(sub_stem.clone())
            .or_default()
            .push(index);

        for (dot_index, _) in sub_stem.match_indices('.') {
            subtitles_by_media_stem
                .entry(sub_stem[..dot_index].to_string())
                .or_default()
                .push(index);
        }
    }

    for (path, metadata, media_stem, extension) in media_entries {
        let modified_system = metadata.modified().ok();
        let modified_at = modified_system
            .map(OffsetDateTime::from)
            .map(format_timestamp)
            .unwrap_or_else(|| indexed_at.to_string());

        let Some(relative_path) = normalize_relative_path(path.strip_prefix(root_path).ok()) else {
            warn!(
                path = %path.display(),
                root = %root_path.display(),
                "skipping file outside library root"
            );
            continue;
        };

        let file_name = path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| relative_path.clone());

        let mut subtitle_tracks: Vec<SubtitleTrack> = subtitles_by_media_stem
            .get(&media_stem)
            .into_iter()
            .flatten()
            .filter_map(|index| {
                let (sub_path, sub_stem, sub_ext) = &subtitle_entries[*index];
                build_sidecar_track(root_path, &media_stem, sub_path, sub_stem, sub_ext)
            })
            .collect();
        subtitle_tracks
            .sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));

        media.push(DirMediaCandidate {
            relative_path,
            file_name,
            extension: Some(extension),
            size_bytes: metadata.len(),
            modified_at,
            modified_system,
            subtitle_tracks,
        });
    }

    DirContents { subdirs, media }
}

fn build_sidecar_track(
    root_path: &Path,
    media_stem: &str,
    sub_path: &Path,
    sub_stem: &str,
    sub_extension: &str,
) -> Option<SubtitleTrack> {
    let suffix = if sub_stem == media_stem {
        String::new()
    } else {
        sub_stem
            .strip_prefix(media_stem)
            .and_then(|rest| rest.strip_prefix('.'))
            .map(str::to_string)?
    };

    let relative_path = normalize_relative_path(sub_path.strip_prefix(root_path).ok())?;
    let file_name = sub_path.file_name()?.to_string_lossy().to_string();
    let (label, language) = subtitle_presentation(&suffix);

    Some(SubtitleTrack {
        file_name,
        relative_path,
        extension: sub_extension.to_string(),
        label,
        language,
    })
}

pub(crate) fn subtitle_presentation(suffix: &str) -> (String, Option<String>) {
    if suffix.is_empty() {
        return ("Default".to_string(), None);
    }

    let tokens = suffix
        .split('.')
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();

    if tokens.is_empty() {
        return ("Default".to_string(), None);
    }

    let language = tokens
        .iter()
        .find_map(|token| infer_language_code(token))
        .map(|code| code.to_string());
    let label = tokens
        .iter()
        .map(|token| humanize_subtitle_token(token))
        .collect::<Vec<_>>()
        .join(" ");

    (label, language)
}

fn humanize_subtitle_token(token: &str) -> String {
    if token.eq_ignore_ascii_case("forced") {
        return "Forced".to_string();
    }

    if token.eq_ignore_ascii_case("sdh") {
        return "SDH".to_string();
    }

    if infer_language_code(token).is_some() {
        return token.to_ascii_uppercase();
    }

    let mut chars = token.chars();
    match chars.next() {
        Some(first) => {
            let remainder = chars.as_str().replace(['_', '-'], " ");
            format!(
                "{}{}",
                first.to_ascii_uppercase(),
                remainder.to_ascii_lowercase()
            )
        }
        None => "Subtitle".to_string(),
    }
}

fn infer_language_code(token: &str) -> Option<&str> {
    if token.len() == 2
        && token
            .chars()
            .all(|character| character.is_ascii_alphabetic())
    {
        return Some(token);
    }

    if token.len() == 5
        && token.as_bytes().get(2) == Some(&b'-')
        && token
            .chars()
            .enumerate()
            .all(|(index, character)| index == 2 || character.is_ascii_alphabetic())
    {
        return Some(token);
    }

    None
}

fn normalize_relative_path(path: Option<&Path>) -> Option<String> {
    let path = path?;
    let mut parts = Vec::new();

    for component in path.components() {
        let value = component.as_os_str().to_string_lossy();

        if value.is_empty() {
            continue;
        }

        parts.push(value.into_owned());
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn normalized_extension(path: &Path) -> Option<String> {
    path.extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

pub(crate) fn content_type_for_extension(extension: &str) -> Option<&'static str> {
    match extension {
        "mp4" | "m4v" => Some("video/mp4"),
        "mkv" => Some("video/x-matroska"),
        "webm" => Some("video/webm"),
        "mov" => Some("video/quicktime"),
        "avi" => Some("video/x-msvideo"),
        "mp3" => Some("audio/mpeg"),
        "flac" => Some("audio/flac"),
        "wav" => Some("audio/wav"),
        "m4a" | "aac" => Some("audio/aac"),
        "ogg" => Some("audio/ogg"),
        _ => None,
    }
}
