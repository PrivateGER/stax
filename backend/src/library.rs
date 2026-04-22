use std::{
    collections::{BTreeSet, HashMap},
    env, fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Instant,
};

use serde::Deserialize;
use time::OffsetDateTime;
use tokio::{
    process::Command,
    sync::Semaphore,
    task::{self, JoinSet},
};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    clock::{format_timestamp, round_to},
    persistence::{
        CachedMediaRecord, CachedProbeFields, LibrarySnapshot, Persistence, PersistenceError,
        ProbeOutcome, WalkRecord,
    },
    protocol::{
        AudioStream, LibraryResponse, LibraryScanResponse, MediaItem, PlaybackMode, SubtitleStream,
        SubtitleTrack,
    },
    thumbnails::{
        default_ffmpeg_command, default_thumbnail_cache_dir, ffmpeg_command_from_env,
        thumbnail_cache_dir_from_env, thumbnail_is_up_to_date, thumbnail_path_for,
    },
};

const SUPPORTED_MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "webm", "mov", "m4v", "avi", "mp3", "flac", "wav", "m4a", "aac", "ogg",
];
const SUPPORTED_SUBTITLE_EXTENSIONS: &[&str] = &["vtt", "srt"];

const DEFAULT_PROBE_WORKERS: usize = 4;
const DEFAULT_WALK_WORKERS: usize = 8;

#[derive(Clone, Debug)]
pub struct LibraryConfig {
    root_paths: Vec<PathBuf>,
    probe_command: Option<PathBuf>,
    ffmpeg_command: Option<PathBuf>,
    thumbnail_cache_dir: Option<PathBuf>,
    stream_copy_cache_dir: Option<PathBuf>,
    probe_workers: usize,
    walk_workers: usize,
}

#[derive(Clone, Debug)]
pub struct LibraryService {
    config: LibraryConfig,
    persistence: Persistence,
}

/// Outcome of the walk stage for one library root. Used so the scan
/// handler in `lib.rs` can enqueue probes/thumbnails for the freshly-
/// inserted rows without a second pass over the database.
#[derive(Debug, Default)]
pub struct WalkOutcome {
    pub total: usize,
    pub cached: usize,
    pub pending: usize,
    pub elapsed_ms: u64,
}

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    format: Option<FfprobeFormat>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    #[serde(default)]
    index: Option<u32>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    profile: Option<String>,
    #[serde(default)]
    level: Option<i32>,
    pix_fmt: Option<String>,
    bits_per_raw_sample: Option<serde_json::Value>,
    width: Option<u32>,
    height: Option<u32>,
    channels: Option<u32>,
    channel_layout: Option<String>,
    #[serde(default)]
    tags: HashMap<String, String>,
    #[serde(default)]
    disposition: HashMap<String, u8>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    format_name: Option<String>,
    duration: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VideoPlaybackInfo<'a> {
    pub codec: Option<&'a str>,
    pub profile: Option<&'a str>,
    pub pix_fmt: Option<&'a str>,
    pub level: Option<u32>,
    pub bit_depth: Option<u8>,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            root_paths: Vec::new(),
            probe_command: default_probe_command(),
            ffmpeg_command: default_ffmpeg_command(),
            thumbnail_cache_dir: Some(default_thumbnail_cache_dir()),
            stream_copy_cache_dir: Some(default_stream_copy_cache_dir()),
            probe_workers: DEFAULT_PROBE_WORKERS,
            walk_workers: DEFAULT_WALK_WORKERS,
        }
    }
}

impl LibraryConfig {
    pub fn from_env() -> Self {
        let mut config = match env::var_os("SYNCPLAY_LIBRARY_ROOTS") {
            Some(raw_paths) => Self::from_paths(env::split_paths(&raw_paths)),
            None => Self::default(),
        };
        config.probe_command = probe_command_from_env();
        config.ffmpeg_command = ffmpeg_command_from_env();
        config.thumbnail_cache_dir = thumbnail_cache_dir_from_env();
        config.stream_copy_cache_dir = stream_copy_cache_dir_from_env();
        config.probe_workers = probe_workers_from_env();
        config.walk_workers = walk_workers_from_env();
        config
    }

    pub fn from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let current_dir = env::current_dir().ok();
        let normalized = paths
            .into_iter()
            .filter_map(|path| normalize_root_path(path, current_dir.as_deref()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        Self {
            root_paths: normalized,
            probe_command: default_probe_command(),
            ffmpeg_command: default_ffmpeg_command(),
            thumbnail_cache_dir: Some(default_thumbnail_cache_dir()),
            stream_copy_cache_dir: Some(default_stream_copy_cache_dir()),
            probe_workers: DEFAULT_PROBE_WORKERS,
            walk_workers: DEFAULT_WALK_WORKERS,
        }
    }

    pub fn root_paths(&self) -> &[PathBuf] {
        &self.root_paths
    }

    pub fn with_probe_command(mut self, probe_command: impl Into<PathBuf>) -> Self {
        self.probe_command = Some(probe_command.into());
        self
    }

    pub fn without_probe(mut self) -> Self {
        self.probe_command = None;
        self
    }

    pub fn probe_command(&self) -> Option<&Path> {
        self.probe_command.as_deref()
    }

    pub fn with_ffmpeg_command(mut self, ffmpeg_command: impl Into<PathBuf>) -> Self {
        self.ffmpeg_command = Some(ffmpeg_command.into());
        self
    }

    pub fn without_ffmpeg(mut self) -> Self {
        self.ffmpeg_command = None;
        self
    }

    pub fn ffmpeg_command(&self) -> Option<&Path> {
        self.ffmpeg_command.as_deref()
    }

    pub fn with_thumbnail_cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.thumbnail_cache_dir = Some(path.into());
        self
    }

    pub fn thumbnail_cache_dir(&self) -> Option<&Path> {
        self.thumbnail_cache_dir.as_deref()
    }

    pub fn with_stream_copy_cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.stream_copy_cache_dir = Some(path.into());
        self
    }

    pub fn stream_copy_cache_dir(&self) -> Option<&Path> {
        self.stream_copy_cache_dir.as_deref()
    }

    pub fn with_probe_workers(mut self, probe_workers: usize) -> Self {
        self.probe_workers = probe_workers.max(1);
        self
    }

    pub fn probe_workers(&self) -> usize {
        self.probe_workers.max(1)
    }

    pub fn with_walk_workers(mut self, walk_workers: usize) -> Self {
        self.walk_workers = walk_workers.max(1);
        self
    }

    pub fn walk_workers(&self) -> usize {
        self.walk_workers.max(1)
    }
}

impl LibraryService {
    pub fn new(persistence: Persistence, config: LibraryConfig) -> Self {
        Self {
            config,
            persistence,
        }
    }

    pub fn ffmpeg_command(&self) -> Option<&Path> {
        self.config.ffmpeg_command()
    }

    pub async fn sync_config(&self) -> Result<(), PersistenceError> {
        let root_paths = self
            .config
            .root_paths()
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect::<Vec<_>>();

        self.persistence.sync_library_roots(&root_paths).await
    }

    pub async fn snapshot(&self) -> Result<LibraryResponse, PersistenceError> {
        let snapshot = self.persistence.load_library_snapshot().await?;

        Ok(snapshot.into_response())
    }

    pub async fn media_item(&self, media_id: Uuid) -> Result<Option<MediaItem>, PersistenceError> {
        self.persistence.find_media_item(media_id).await
    }

    pub fn thumbnail_path(&self, media_id: Uuid) -> Option<PathBuf> {
        self.config
            .thumbnail_cache_dir()
            .map(|dir| thumbnail_path_for(dir, media_id))
    }

    /// Stage 1 of the staged scan pipeline: walk every configured library
    /// root, upsert one row per discovered file, and prune rows for files
    /// that are no longer on disk. Returns once the database is consistent
    /// with the filesystem; callers should then enqueue probes/thumbnails
    /// for the rows still missing metadata (the `WalkOutcome` per root
    /// reports the count). The actual probe work happens on the background
    /// `ProbeWorkerPool` — see `crate::probes`.
    pub async fn scan(&self) -> Result<LibraryScanResponse, PersistenceError> {
        self.sync_config().await?;

        let scanned_at = format_timestamp(OffsetDateTime::now_utc());
        let thumbnail_cache_dir = self.config.thumbnail_cache_dir().map(Path::to_path_buf);
        let walk_workers = self.config.walk_workers();

        for root_path in self.config.root_paths() {
            let root_path_string = root_path.to_string_lossy().to_string();
            let existing_records = self
                .persistence
                .existing_media_records(&root_path_string)
                .await?;

            match walk_root(
                root_path.clone(),
                thumbnail_cache_dir.clone(),
                existing_records,
                walk_workers,
                self.persistence.clone(),
            )
            .await
            {
                Ok(outcome) => {
                    info!(
                        root = %root_path_string,
                        total = outcome.total,
                        cached = outcome.cached,
                        pending = outcome.pending,
                        elapsed_ms = outcome.elapsed_ms,
                        "library walk complete"
                    );
                    self.persistence
                        .mark_root_scanned(&root_path_string)
                        .await?;
                }
                Err(error) => {
                    warn!(path = %root_path_string, %error, "library root scan failed");

                    self.persistence
                        .record_library_scan_failure(&root_path_string, &error)
                        .await?;
                }
            }
        }

        let snapshot = self.persistence.load_library_snapshot().await?;
        let indexed_item_count = snapshot.items.len();
        let scanned_root_count = snapshot.roots.len();

        Ok(LibraryScanResponse {
            roots: snapshot.roots,
            items: snapshot.items,
            scanned_root_count,
            indexed_item_count,
            scanned_at,
        })
    }
}

impl LibrarySnapshot {
    fn into_response(self) -> LibraryResponse {
        LibraryResponse {
            roots: self.roots,
            items: self.items,
        }
    }
}

/// One file's worth of metadata produced by the per-directory blocking
/// scan. The `metadata` is `fs::Metadata` so we can compute the modified
/// timestamp and size without a second stat. Subtitles are pre-attached
/// per matching media stem so the recursive walker doesn't need a second
/// `read_dir` per media file (the dominant cost on SMB).
#[derive(Debug)]
struct DirMediaCandidate {
    relative_path: String,
    file_name: String,
    extension: Option<String>,
    size_bytes: u64,
    modified_at: String,
    modified_system: Option<std::time::SystemTime>,
    subtitle_tracks: Vec<SubtitleTrack>,
}

#[derive(Debug, Default)]
struct DirContents {
    subdirs: Vec<PathBuf>,
    media: Vec<DirMediaCandidate>,
}

/// Shared state passed down the recursive walk. Cloning is cheap (only
/// `Arc`s and small fields).
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

async fn walk_root(
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

/// Recursive parallel directory walk. Each `read_dir` is dispatched to
/// `spawn_blocking` (so blocking SMB I/O doesn't starve the runtime) and
/// gated by the shared semaphore (so we don't fork-bomb the network with
/// thousands of in-flight directory listings). Subdirectories are
/// recursed into in parallel via `JoinSet`.
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
        // Release the permit before recursing so child tasks can grab it.
        drop(permit);

        let DirContents { subdirs, media } = contents;

        // Fan out subdirectory scans in parallel.
        let mut joinset = JoinSet::new();
        for subdir in subdirs {
            let context = context.clone();
            joinset.spawn(walk_dir(context, subdir));
        }

        let mut stats = WalkStats::default();

        // Persist this directory's media files. Writes go to a single
        // SQLite connection so parallelism here would just queue at the
        // pool — process serially to keep the code simple.
        for candidate in media {
            match build_walk_record(&context, candidate) {
                Ok((record, is_cached)) => {
                    let media_id = record.id;
                    if let Err(error) = context.persistence.upsert_walk_record(&record).await {
                        warn!(%media_id, %error, "failed to upsert walk record");
                        continue;
                    }
                    stats.kept_ids.push(media_id);
                    if is_cached {
                        stats.cached += 1;
                    } else {
                        stats.pending += 1;
                    }
                }
                Err(error) => warn!(%error, "skipping media file with malformed walk record"),
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

/// Build a `WalkRecord` from a discovered candidate. Decides cache hit
/// vs. miss based on `(size_bytes, modified_at)` against the prior row.
/// Returns `(record, is_cache_hit)` so callers can tally cached vs.
/// pending counts.
fn build_walk_record(
    context: &WalkContext,
    candidate: DirMediaCandidate,
) -> Result<(WalkRecord, bool), String> {
    let prior = context.existing_records.get(&candidate.relative_path);
    let media_id = prior.map(|record| record.id).unwrap_or_else(Uuid::new_v4);

    // Cache reuse: same size + same mtime + last probe didn't error.
    // Failed probes always retry (the file may have been fixed); a
    // successful probe sticks until the file changes.
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

    // Only carry the cached thumbnail outcome through if both the file is
    // unchanged AND the cached jpeg on disk is still up to date with the
    // source. Otherwise leave it blank so the thumbnail pool re-runs.
    let cached_thumbnail = if is_cache_hit {
        let on_disk_fresh = context
            .thumbnail_cache_dir
            .as_deref()
            .map(|dir| thumbnail_path_for(dir, media_id))
            .map(|path| thumbnail_is_up_to_date(&path, candidate.modified_system))
            .unwrap_or(false);

        if on_disk_fresh {
            // Mark fresh; future loads can stream it directly.
            Some((Some(format_timestamp(OffsetDateTime::now_utc())), None))
        } else {
            // Re-queue thumbnail generation by leaving the columns NULL.
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

/// Single-`read_dir` partition of a directory's children into
/// subdirectories, media candidates, and sidecar subtitles. Sidecars are
/// matched to media files by stem in-place so the caller doesn't need to
/// re-list the directory per media file. This is the optimization that
/// kills the N×directory-listing cost the previous implementation had on
/// high-latency filesystems.
fn classify_directory(root_path: &Path, dir: &Path, indexed_at: &str) -> DirContents {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            warn!(path = %dir.display(), %error, "skipping unreadable directory during library scan");
            return DirContents::default();
        }
    };

    let mut subdirs = Vec::new();
    let mut media_entries: Vec<(PathBuf, fs::Metadata, String, String)> = Vec::new();
    // (path, stem, extension)
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

        let mut subtitle_tracks: Vec<SubtitleTrack> = subtitle_entries
            .iter()
            .filter_map(|(sub_path, sub_stem, sub_ext)| {
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

/// Build a `SubtitleTrack` for a sidecar file if its stem matches the
/// media stem. Returns None if the subtitle doesn't pair with this
/// media file.
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

fn subtitle_presentation(suffix: &str) -> (String, Option<String>) {
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

/// Run ffprobe against `path` and turn its JSON output into a
/// `ProbeOutcome` ready to be persisted by the probe pool. `probe_command`
/// is required — pool callers check for it upfront and skip enqueueing if
/// disabled, so this function never silently no-ops.
pub(crate) async fn probe_media_metadata(
    path: &Path,
    file_extension: Option<&str>,
    probe_command: Option<&Path>,
) -> ProbeOutcome {
    let probed_at = format_timestamp(OffsetDateTime::now_utc());
    let Some(probe_command) = probe_command else {
        return probe_outcome_with_error(probed_at, "no ffprobe configured");
    };

    let output = Command::new(probe_command)
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg(
            "format=format_name,duration:stream=index,codec_type,codec_name,profile,level,\
             pix_fmt,bits_per_raw_sample,width,height,channels,channel_layout:\
             stream_tags=language,title:stream_disposition=default,forced",
        )
        .arg("-of")
        .arg("json")
        .arg(path)
        .output()
        .await;

    match output {
        Ok(output) if output.status.success() => {
            match parse_probe_output(&output.stdout, file_extension, probed_at.clone()) {
                Ok(metadata) => metadata,
                Err(error) => probe_outcome_with_error(probed_at, &error),
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let error = if stderr.is_empty() {
                format!("ffprobe exited with status {}", output.status)
            } else {
                format!("ffprobe failed: {stderr}")
            };
            probe_outcome_with_error(probed_at, &error)
        }
        Err(error) => {
            probe_outcome_with_error(probed_at, &format!("ffprobe could not start: {error}"))
        }
    }
}

/// Build an error-only `ProbeOutcome`. Centralized so every fallback path
/// produces a consistent NULL set with playback_mode=Direct as the
/// non-null sentinel.
fn probe_outcome_with_error(probed_at: String, error: &str) -> ProbeOutcome {
    ProbeOutcome {
        probed_at,
        probe_error: Some(error.to_string()),
        duration_seconds: None,
        container_name: None,
        video_codec: None,
        audio_codec: None,
        width: None,
        height: None,
        playback_mode: PlaybackMode::Direct,
        video_profile: None,
        video_level: None,
        video_pix_fmt: None,
        video_bit_depth: None,
        audio_streams: Vec::new(),
        subtitle_streams: Vec::new(),
    }
}

fn parse_probe_output(
    output: &[u8],
    file_extension: Option<&str>,
    probed_at: String,
) -> Result<ProbeOutcome, String> {
    let parsed: FfprobeOutput = serde_json::from_slice(output)
        .map_err(|error| format!("ffprobe returned invalid JSON: {error}"))?;

    let video_stream = parsed
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"));

    let mut audio_streams: Vec<AudioStream> = parsed
        .streams
        .iter()
        .filter(|stream| stream.codec_type.as_deref() == Some("audio"))
        .enumerate()
        .map(|(fallback_index, stream)| AudioStream {
            index: stream.index.unwrap_or(fallback_index as u32),
            codec: stream.codec_name.clone(),
            channels: stream.channels,
            channel_layout: stream.channel_layout.clone(),
            language: lookup_tag(&stream.tags, "language"),
            title: lookup_tag(&stream.tags, "title"),
            default: stream.disposition.get("default").copied().unwrap_or(0) != 0,
        })
        .collect();

    if !audio_streams.iter().any(|stream| stream.default)
        && let Some(first) = audio_streams.first_mut()
    {
        first.default = true;
    }

    let subtitle_streams: Vec<SubtitleStream> = parsed
        .streams
        .iter()
        .filter(|stream| stream.codec_type.as_deref() == Some("subtitle"))
        .enumerate()
        .map(|(fallback_index, stream)| SubtitleStream {
            index: stream.index.unwrap_or(fallback_index as u32),
            codec: stream.codec_name.clone(),
            language: lookup_tag(&stream.tags, "language"),
            title: lookup_tag(&stream.tags, "title"),
            default: stream.disposition.get("default").copied().unwrap_or(0) != 0,
            forced: stream.disposition.get("forced").copied().unwrap_or(0) != 0,
        })
        .collect();

    let container_name = parsed
        .format
        .as_ref()
        .and_then(|format| format.format_name.clone());
    let video_profile = video_stream.and_then(|stream| stream.profile.clone());
    let video_level = video_stream
        .and_then(|stream| stream.level)
        .filter(|value| *value > 0)
        .map(|value| value as u32);
    let video_pix_fmt = video_stream.and_then(|stream| stream.pix_fmt.clone());
    let video_bit_depth = video_stream.and_then(|stream| {
        stream
            .bits_per_raw_sample
            .as_ref()
            .and_then(parse_json_u8_like)
    });

    let playback_mode = classify_playback_mode(
        container_name.as_deref(),
        file_extension,
        VideoPlaybackInfo {
            codec: video_stream.and_then(|stream| stream.codec_name.as_deref()),
            profile: video_profile.as_deref(),
            pix_fmt: video_pix_fmt.as_deref(),
            level: video_level,
            bit_depth: video_bit_depth,
        },
        &audio_streams,
    );

    Ok(ProbeOutcome {
        duration_seconds: parsed
            .format
            .as_ref()
            .and_then(|format| format.duration.as_deref())
            .and_then(|duration| duration.parse::<f64>().ok())
            .map(|duration| round_to(duration, 3)),
        container_name,
        video_codec: video_stream.and_then(|stream| stream.codec_name.clone()),
        audio_codec: audio_streams
            .first()
            .and_then(|stream| stream.codec.clone()),
        width: video_stream.and_then(|stream| stream.width),
        height: video_stream.and_then(|stream| stream.height),
        probed_at,
        probe_error: None,
        playback_mode,
        video_profile,
        video_level,
        video_pix_fmt,
        video_bit_depth,
        audio_streams,
        subtitle_streams,
    })
}

fn lookup_tag(tags: &HashMap<String, String>, key: &str) -> Option<String> {
    for (tag_key, tag_value) in tags {
        if tag_key.eq_ignore_ascii_case(key) && !tag_value.trim().is_empty() {
            return Some(tag_value.clone());
        }
    }
    None
}

fn parse_json_u8_like(value: &serde_json::Value) -> Option<u8> {
    match value {
        serde_json::Value::Number(number) => number.as_u64().and_then(|v| u8::try_from(v).ok()),
        serde_json::Value::String(text) => text.parse::<u8>().ok(),
        _ => None,
    }
}

pub(crate) fn classify_playback_mode(
    container: Option<&str>,
    file_extension: Option<&str>,
    video: VideoPlaybackInfo<'_>,
    audio_streams: &[AudioStream],
) -> PlaybackMode {
    let container_ok = container
        .map(|value| is_client_demuxable_container(value, file_extension))
        .unwrap_or(false);
    let has_any_stream = video.codec.is_some() || !audio_streams.is_empty();

    if !has_any_stream {
        return PlaybackMode::Unsupported;
    }

    let audio_ok = audio_streams.is_empty()
        || audio_streams.iter().all(|stream| {
            stream
                .codec
                .as_deref()
                .map(is_client_decodable_audio_codec)
                .unwrap_or(false)
        });

    let audio_transcodable = audio_streams.iter().all(|stream| {
        stream
            .codec
            .as_deref()
            .map(|codec| {
                is_client_decodable_audio_codec(codec) || is_transcodable_audio_codec(codec)
            })
            .unwrap_or(false)
    });

    let video_client_decodable = match video.codec {
        None => true,
        Some(codec) => is_browser_safe_video_codec(
            codec,
            video.profile,
            video.pix_fmt,
            video.level,
            video.bit_depth,
        ),
    };

    if video_client_decodable && audio_ok && container_ok {
        return PlaybackMode::Direct;
    }

    if !audio_transcodable {
        return PlaybackMode::Unsupported;
    }

    PlaybackMode::NeedsPreparation
}

pub(crate) fn is_client_demuxable_container(container: &str, file_extension: Option<&str>) -> bool {
    // ffprobe's format_name is a comma-separated list of equivalent demuxer tags.
    // Critically, "matroska,webm" applies to BOTH .mkv and .webm files. MediaBunny
    // can demux both, but keep trusting the extension when the format tag is ambiguous.
    let normalized_extension = file_extension.map(|value| value.to_ascii_lowercase());
    let tokens = container
        .split(',')
        .map(|token| token.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();

    // Matroska/WebM is not native-HTML-safe, but the MediaBunny player can demux it directly.
    if tokens.iter().any(|token| token == "matroska") {
        return matches!(normalized_extension.as_deref(), Some("mkv" | "webm"));
    }

    tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "mov"
                | "mp4"
                | "m4a"
                | "m4v"
                | "3gp"
                | "3g2"
                | "mj2"
                | "webm"
                | "mp3"
                | "ogg"
                | "flac"
                | "wav"
        )
    })
}

pub(crate) fn is_browser_safe_video_codec(
    codec: &str,
    profile: Option<&str>,
    pix_fmt: Option<&str>,
    level: Option<u32>,
    bit_depth: Option<u8>,
) -> bool {
    let codec = codec.to_ascii_lowercase();
    match codec.as_str() {
        "h264" | "avc" | "avc1" => {
            let profile_ok = profile
                .map(|value| {
                    let value = value.to_ascii_lowercase();
                    value.contains("baseline") || value.contains("main") || value.contains("high")
                })
                .unwrap_or(true);
            let pix_fmt_ok = pix_fmt
                .map(|value| matches!(value.to_ascii_lowercase().as_str(), "yuv420p" | "yuvj420p"))
                .unwrap_or(true);
            let level_ok = level.map(|value| value <= 51).unwrap_or(true);
            let bit_depth_ok = bit_depth.map(|value| value <= 8).unwrap_or(true);
            profile_ok && pix_fmt_ok && level_ok && bit_depth_ok
        }
        "vp9" | "av1" => pix_fmt
            .map(|value| {
                matches!(
                    value.to_ascii_lowercase().as_str(),
                    "yuv420p" | "yuvj420p" | "yuv420p10le"
                )
            })
            .unwrap_or(true),
        "vp8" => true,
        _ => false,
    }
}

pub(crate) fn is_client_decodable_audio_codec(codec: &str) -> bool {
    let codec = codec.to_ascii_lowercase();
    let normalized = codec.replace('_', "-");

    matches!(
        normalized.as_str(),
        // Native WebCodecs / browser decode targets in the MediaBunny player.
        "aac" | "opus" | "flac" | "vorbis"
            // MP3 is additionally backed by the mpg123 WASM decoder when native
            // WebCodecs support is missing.
            | "mp3"
            // MediaBunny has an internal PCM decoder wrapper for these variants.
            | "pcm-s8"
            | "pcm-u8"
            | "pcm-s16"
            | "pcm-s16le"
            | "pcm-s16be"
            | "pcm-s24"
            | "pcm-s24le"
            | "pcm-s24be"
            | "pcm-s32"
            | "pcm-s32le"
            | "pcm-s32be"
            | "pcm-f32"
            | "pcm-f32le"
            | "pcm-f32be"
            | "pcm-f64"
            | "pcm-f64le"
            | "pcm-f64be"
            | "pcm-mulaw"
            | "pcm-alaw"
            | "ulaw"
            | "alaw"
    )
}

pub(crate) fn is_transcodable_audio_codec(codec: &str) -> bool {
    matches!(
        codec.to_ascii_lowercase().as_str(),
        "ac3" | "eac3" | "dts" | "truehd" | "pcm_s16le" | "pcm_s24le" | "pcm_s32le" | "pcm_f32le"
    )
}

fn default_probe_command() -> Option<PathBuf> {
    Some(PathBuf::from("ffprobe"))
}

fn probe_command_from_env() -> Option<PathBuf> {
    match env::var_os("SYNCPLAY_FFPROBE_BIN") {
        Some(value) if value.is_empty() => None,
        Some(value) => Some(PathBuf::from(value)),
        None => default_probe_command(),
    }
}

fn default_stream_copy_cache_dir() -> PathBuf {
    PathBuf::from("syncplay-stream-copies")
}

fn stream_copy_cache_dir_from_env() -> Option<PathBuf> {
    match env::var_os("SYNCPLAY_STREAM_COPY_DIR") {
        Some(value) if value.is_empty() => None,
        Some(value) => Some(PathBuf::from(value)),
        None => Some(default_stream_copy_cache_dir()),
    }
}

fn probe_workers_from_env() -> usize {
    env::var("SYNCPLAY_PROBE_WORKERS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_PROBE_WORKERS)
}

fn walk_workers_from_env() -> usize {
    env::var("SYNCPLAY_WALK_WORKERS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_WALK_WORKERS)
}

fn normalize_root_path(path: PathBuf, current_dir: Option<&Path>) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        return None;
    }

    let joined_path = if path.is_absolute() {
        path
    } else if let Some(current_dir) = current_dir {
        current_dir.join(path)
    } else {
        path
    };

    Some(
        fs::canonicalize(&joined_path)
            .unwrap_or(joined_path)
            .components()
            .collect(),
    )
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

fn content_type_for_extension(extension: &str) -> Option<&'static str> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, process::Command as StdCommand};
    use tempfile::TempDir;

    #[test]
    fn library_config_normalizes_relative_paths_and_deduplicates() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("media");
        fs::create_dir_all(&root).unwrap();
        let normalized_relative =
            normalize_root_path(PathBuf::from("media"), Some(temp_dir.path())).unwrap();
        let normalized_absolute = normalize_root_path(root.clone(), Some(temp_dir.path())).unwrap();

        assert_eq!(normalized_relative, root);
        assert_eq!(normalized_absolute, root);
    }

    /// Synchronous test helper: recursively classify every directory
    /// under `root` and flatten the resulting media candidates. The
    /// production walker does this in parallel via tokio; these tests
    /// just need a deterministic flattened list to assert against.
    fn flat_walk(root: &Path) -> Vec<DirMediaCandidate> {
        fn recurse(root: &Path, dir: &Path, out: &mut Vec<DirMediaCandidate>) {
            let contents = classify_directory(root, dir, "1970-01-01T00:00:00Z");
            for media in contents.media {
                out.push(media);
            }
            for subdir in contents.subdirs {
                recurse(root, &subdir, out);
            }
        }

        let mut out = Vec::new();
        recurse(root, root, &mut out);
        out.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        out
    }

    #[test]
    fn classify_directory_filters_unsupported_files_and_nested_symlinks() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("library");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("movie.mp4"), b"movie").unwrap();
        fs::write(nested.join("song.flac"), b"audio").unwrap();
        fs::write(root.join("notes.txt"), b"ignore").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("movie.mp4"), root.join("movie-link.mp4")).unwrap();

        let candidates = flat_walk(&root);

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].relative_path, "movie.mp4");
        assert_eq!(candidates[1].relative_path, "nested/song.flac");
    }

    #[test]
    fn classify_directory_matches_supported_extensions_case_insensitively() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("library");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("FEATURE.MP4"), b"movie").unwrap();
        fs::write(root.join("concert.FlAc"), b"audio").unwrap();

        let candidates = flat_walk(&root);
        let relative_paths = candidates
            .iter()
            .map(|item| item.relative_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(relative_paths, vec!["FEATURE.MP4", "concert.FlAc"]);
        assert_eq!(
            candidates[0]
                .extension
                .as_deref()
                .and_then(content_type_for_extension),
            Some("video/mp4"),
        );
        assert_eq!(
            candidates[1]
                .extension
                .as_deref()
                .and_then(content_type_for_extension),
            Some("audio/flac"),
        );
    }

    #[test]
    fn classify_directory_discovers_sidecar_subtitles_for_matching_media() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("library");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("movie.mp4"), b"movie").unwrap();
        fs::write(
            root.join("movie.en.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nHi\n",
        )
        .unwrap();
        fs::write(
            root.join("movie.forced.vtt"),
            b"WEBVTT\n\n00:00.000 --> 00:01.000\nHi\n",
        )
        .unwrap();
        fs::write(root.join("movie-night.en.srt"), b"ignore").unwrap();

        let candidates = flat_walk(&root);
        let subtitle_tracks = &candidates[0].subtitle_tracks;

        assert_eq!(candidates.len(), 1);
        assert_eq!(subtitle_tracks.len(), 2);
        assert_eq!(subtitle_tracks[0].relative_path, "movie.en.srt");
        assert_eq!(subtitle_tracks[0].label, "EN");
        assert_eq!(subtitle_tracks[0].language.as_deref(), Some("en"));
        assert_eq!(subtitle_tracks[1].relative_path, "movie.forced.vtt");
        assert_eq!(subtitle_tracks[1].label, "Forced");
    }

    #[test]
    fn subtitle_presentation_marks_default_tracks() {
        let (label, language) = subtitle_presentation("");

        assert_eq!(label, "Default");
        assert_eq!(language, None);
    }

    #[test]
    fn parse_probe_output_extracts_video_and_audio_metadata() {
        let metadata = parse_probe_output(
            br#"{
                "format": {
                    "format_name": "matroska,webm",
                    "duration": "62.5123"
                },
                "streams": [
                    {"codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080},
                    {"codec_type": "audio", "codec_name": "aac"}
                ]
            }"#,
            Some("mkv"),
            "2026-01-01T00:00:00Z".to_string(),
        )
        .unwrap();

        assert_eq!(metadata.container_name.as_deref(), Some("matroska,webm"));
        assert_eq!(metadata.duration_seconds, Some(62.512));
        assert_eq!(metadata.video_codec.as_deref(), Some("h264"));
        assert_eq!(metadata.audio_codec.as_deref(), Some("aac"));
        assert_eq!(metadata.width, Some(1920));
        assert_eq!(metadata.height, Some(1080));
        assert_eq!(metadata.probe_error, None);
    }

    #[test]
    fn parse_probe_output_returns_error_for_invalid_json() {
        let error = parse_probe_output(
            b"{ definitely-not-json",
            Some("mp4"),
            "2026-01-01T00:00:00Z".to_string(),
        )
        .unwrap_err();

        assert!(error.starts_with("ffprobe returned invalid JSON:"));
    }

    fn audio_stream(codec: &str) -> AudioStream {
        AudioStream {
            index: 0,
            codec: Some(codec.to_string()),
            channels: Some(2),
            channel_layout: Some("stereo".to_string()),
            language: None,
            title: None,
            default: true,
        }
    }

    #[test]
    fn classifier_marks_h264_aac_mp4_as_direct() {
        let mode = classify_playback_mode(
            Some("mov,mp4,m4a,3gp,3g2,mj2"),
            Some("mp4"),
            VideoPlaybackInfo {
                codec: Some("h264"),
                profile: Some("High"),
                pix_fmt: Some("yuv420p"),
                level: Some(40),
                bit_depth: Some(8),
            },
            &[audio_stream("aac")],
        );

        assert_eq!(mode, PlaybackMode::Direct);
    }

    #[test]
    fn classifier_marks_mkv_with_client_decodable_codecs_as_direct() {
        let mode = classify_playback_mode(
            Some("matroska,webm"),
            Some("mkv"),
            VideoPlaybackInfo {
                codec: Some("h264"),
                profile: Some("High"),
                pix_fmt: Some("yuv420p"),
                level: Some(40),
                bit_depth: Some(8),
            },
            &[audio_stream("aac")],
        );

        assert_eq!(mode, PlaybackMode::Direct);
    }

    #[test]
    fn classifier_marks_mkv_with_mp3_audio_as_direct() {
        let mode = classify_playback_mode(
            Some("matroska,webm"),
            Some("mkv"),
            VideoPlaybackInfo {
                codec: Some("h264"),
                profile: Some("High"),
                pix_fmt: Some("yuv420p"),
                level: Some(40),
                bit_depth: Some(8),
            },
            &[audio_stream("mp3")],
        );

        assert_eq!(mode, PlaybackMode::Direct);
    }

    #[test]
    fn classifier_marks_mkv_with_flac_audio_as_direct() {
        let mode = classify_playback_mode(
            Some("matroska,webm"),
            Some("mkv"),
            VideoPlaybackInfo {
                codec: Some("h264"),
                profile: Some("High"),
                pix_fmt: Some("yuv420p"),
                level: Some(40),
                bit_depth: Some(8),
            },
            &[audio_stream("flac")],
        );

        assert_eq!(mode, PlaybackMode::Direct);
    }

    #[test]
    fn classifier_marks_webm_as_direct() {
        let mode = classify_playback_mode(
            Some("matroska,webm"),
            Some("webm"),
            VideoPlaybackInfo {
                codec: Some("vp9"),
                pix_fmt: Some("yuv420p"),
                ..VideoPlaybackInfo::default()
            },
            &[audio_stream("opus")],
        );

        assert_eq!(mode, PlaybackMode::Direct);
    }

    #[test]
    fn classifier_marks_mkv_with_ac3_as_needs_preparation() {
        let mode = classify_playback_mode(
            Some("matroska,webm"),
            Some("mkv"),
            VideoPlaybackInfo {
                codec: Some("h264"),
                profile: Some("High"),
                pix_fmt: Some("yuv420p"),
                level: Some(40),
                bit_depth: Some(8),
            },
            &[audio_stream("ac3")],
        );

        assert_eq!(mode, PlaybackMode::NeedsPreparation);
    }

    #[test]
    fn classifier_marks_dts_as_needs_preparation() {
        let mode = classify_playback_mode(
            Some("matroska,webm"),
            Some("mkv"),
            VideoPlaybackInfo {
                codec: Some("h264"),
                profile: Some("High"),
                pix_fmt: Some("yuv420p"),
                level: Some(40),
                bit_depth: Some(8),
            },
            &[audio_stream("dts")],
        );

        assert_eq!(mode, PlaybackMode::NeedsPreparation);
    }

    #[test]
    fn classifier_marks_hevc_as_needs_preparation() {
        let mode = classify_playback_mode(
            Some("matroska,webm"),
            Some("mkv"),
            VideoPlaybackInfo {
                codec: Some("hevc"),
                profile: Some("Main"),
                pix_fmt: Some("yuv420p"),
                level: Some(120),
                bit_depth: Some(8),
            },
            &[audio_stream("aac")],
        );

        assert_eq!(mode, PlaybackMode::NeedsPreparation);
    }

    #[test]
    fn classifier_marks_avi_mpeg4_as_needs_preparation() {
        let mode = classify_playback_mode(
            Some("avi"),
            Some("avi"),
            VideoPlaybackInfo {
                codec: Some("mpeg4"),
                profile: Some("Simple Profile"),
                pix_fmt: Some("yuv420p"),
                level: Some(5),
                bit_depth: Some(8),
            },
            &[audio_stream("mp3")],
        );

        assert_eq!(mode, PlaybackMode::NeedsPreparation);
    }

    #[test]
    fn classifier_marks_10_bit_h264_as_needs_preparation() {
        let mode = classify_playback_mode(
            Some("matroska,webm"),
            Some("mkv"),
            VideoPlaybackInfo {
                codec: Some("h264"),
                profile: Some("High 10"),
                pix_fmt: Some("yuv420p10le"),
                level: Some(40),
                bit_depth: Some(10),
            },
            &[audio_stream("aac")],
        );

        assert_eq!(mode, PlaybackMode::NeedsPreparation);
    }

    #[test]
    fn classifier_marks_multi_audio_mixed_as_needs_preparation() {
        let english = audio_stream("aac");
        let japanese = AudioStream {
            index: 1,
            codec: Some("ac3".into()),
            channels: Some(6),
            channel_layout: Some("5.1".into()),
            language: Some("jpn".into()),
            title: Some("Japanese".into()),
            default: false,
        };

        let mode = classify_playback_mode(
            Some("matroska,webm"),
            Some("mkv"),
            VideoPlaybackInfo {
                codec: Some("h264"),
                profile: Some("High"),
                pix_fmt: Some("yuv420p"),
                level: Some(40),
                bit_depth: Some(8),
            },
            &[english, japanese],
        );

        assert_eq!(mode, PlaybackMode::NeedsPreparation);
    }

    #[test]
    fn classifier_marks_no_streams_as_unsupported() {
        let mode = classify_playback_mode(
            Some("matroska,webm"),
            Some("mkv"),
            VideoPlaybackInfo::default(),
            &[],
        );

        assert_eq!(mode, PlaybackMode::Unsupported);
    }

    #[test]
    fn classifier_marks_audio_only_mp3_as_direct() {
        let mode = classify_playback_mode(
            Some("mp3"),
            Some("mp3"),
            VideoPlaybackInfo::default(),
            &[audio_stream("mp3")],
        );

        assert_eq!(mode, PlaybackMode::Direct);
    }

    #[test]
    fn classifier_marks_audio_only_wav_pcm_as_direct() {
        let mode = classify_playback_mode(
            Some("wav"),
            Some("wav"),
            VideoPlaybackInfo::default(),
            &[audio_stream("pcm_s16le")],
        );

        assert_eq!(mode, PlaybackMode::Direct);
    }

    #[test]
    fn parse_probe_output_populates_audio_and_subtitle_streams() {
        let metadata = parse_probe_output(
            br#"{
                "format": {
                    "format_name": "matroska,webm",
                    "duration": "120.0"
                },
                "streams": [
                    {
                        "index": 0,
                        "codec_type": "video",
                        "codec_name": "h264",
                        "profile": "High",
                        "level": 40,
                        "pix_fmt": "yuv420p",
                        "bits_per_raw_sample": "8",
                        "width": 1920,
                        "height": 1080
                    },
                    {
                        "index": 1,
                        "codec_type": "audio",
                        "codec_name": "ac3",
                        "channels": 6,
                        "channel_layout": "5.1",
                        "tags": {"language": "eng", "title": "English"},
                        "disposition": {"default": 1}
                    },
                    {
                        "index": 2,
                        "codec_type": "audio",
                        "codec_name": "aac",
                        "channels": 2,
                        "tags": {"language": "jpn"}
                    },
                    {
                        "index": 3,
                        "codec_type": "subtitle",
                        "codec_name": "subrip",
                        "tags": {"language": "eng"},
                        "disposition": {"forced": 1}
                    }
                ]
            }"#,
            Some("mkv"),
            "2026-01-01T00:00:00Z".to_string(),
        )
        .unwrap();

        assert_eq!(metadata.video_profile.as_deref(), Some("High"));
        assert_eq!(metadata.video_level, Some(40));
        assert_eq!(metadata.video_pix_fmt.as_deref(), Some("yuv420p"));
        assert_eq!(metadata.video_bit_depth, Some(8));
        assert_eq!(metadata.audio_streams.len(), 2);
        assert_eq!(metadata.audio_streams[0].codec.as_deref(), Some("ac3"));
        assert_eq!(metadata.audio_streams[0].language.as_deref(), Some("eng"));
        assert_eq!(metadata.audio_streams[0].title.as_deref(), Some("English"));
        assert!(metadata.audio_streams[0].default);
        assert_eq!(metadata.audio_streams[1].codec.as_deref(), Some("aac"));
        assert!(!metadata.audio_streams[1].default);
        assert_eq!(metadata.subtitle_streams.len(), 1);
        assert!(metadata.subtitle_streams[0].forced);
        assert_eq!(metadata.playback_mode, PlaybackMode::NeedsPreparation);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_media_metadata_extracts_real_stream_titles_and_languages() {
        if !external_av_tools_available() {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let subtitle_path = temp_dir.path().join("sample.srt");
        let media_path = temp_dir.path().join("sample-with-tags.mkv");
        fs::write(
            &subtitle_path,
            "1\n00:00:00,000 --> 00:00:00,800\nHello from subtitle\n",
        )
        .unwrap();

        let status = StdCommand::new("ffmpeg")
            .arg("-y")
            .arg("-f")
            .arg("lavfi")
            .arg("-i")
            .arg("testsrc=size=320x240:rate=1")
            .arg("-f")
            .arg("lavfi")
            .arg("-i")
            .arg("sine=frequency=1000:sample_rate=48000")
            .arg("-f")
            .arg("srt")
            .arg("-i")
            .arg(&subtitle_path)
            .arg("-t")
            .arg("1")
            .arg("-metadata:s:a:0")
            .arg("language=eng")
            .arg("-metadata:s:a:0")
            .arg("title=ENG")
            .arg("-metadata:s:s:0")
            .arg("language=eng")
            .arg("-metadata:s:s:0")
            .arg("title=Signs")
            .arg("-c:v")
            .arg("libx264")
            .arg("-pix_fmt")
            .arg("yuv420p")
            .arg("-c:a")
            .arg("aac")
            .arg("-c:s")
            .arg("srt")
            .arg("-shortest")
            .arg(&media_path)
            .status()
            .expect("ffmpeg invocation failed to start");
        assert!(status.success(), "ffmpeg exited with status {status}");

        let metadata =
            probe_media_metadata(&media_path, Some("mkv"), Some(Path::new("ffprobe"))).await;

        assert_eq!(metadata.probe_error, None);
        assert_eq!(metadata.audio_streams.len(), 1);
        assert_eq!(metadata.audio_streams[0].language.as_deref(), Some("eng"));
        assert_eq!(metadata.audio_streams[0].title.as_deref(), Some("ENG"));
        assert_eq!(metadata.subtitle_streams.len(), 1);
        assert_eq!(
            metadata.subtitle_streams[0].language.as_deref(),
            Some("eng")
        );
        assert_eq!(metadata.subtitle_streams[0].title.as_deref(), Some("Signs"));
    }

    #[cfg(unix)]
    fn external_av_tools_available() -> bool {
        fn probe(binary: &str) -> bool {
            StdCommand::new(binary)
                .arg("-version")
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false)
        }

        probe("ffmpeg") && probe("ffprobe")
    }
}
