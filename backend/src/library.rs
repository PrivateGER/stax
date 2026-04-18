use std::{
    collections::{BTreeSet, HashMap},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;
use time::OffsetDateTime;
use tokio::task;
use tracing::warn;
use uuid::Uuid;

use crate::{
    clock::{format_timestamp, round_to},
    persistence::{LibrarySnapshot, Persistence, PersistenceError},
    protocol::{LibraryResponse, LibraryScanResponse, MediaItem, SubtitleTrack},
};

const SUPPORTED_MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "webm", "mov", "m4v", "avi", "mp3", "flac", "wav", "m4a", "aac", "ogg",
];
const SUPPORTED_SUBTITLE_EXTENSIONS: &[&str] = &["vtt", "srt"];
const DEFAULT_THUMBNAIL_CACHE_DIR: &str = "syncplay-thumbnails";
const THUMBNAIL_WIDTH: u32 = 480;

#[derive(Clone, Debug)]
pub struct LibraryConfig {
    root_paths: Vec<PathBuf>,
    probe_command: Option<PathBuf>,
    ffmpeg_command: Option<PathBuf>,
    thumbnail_cache_dir: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct LibraryService {
    config: LibraryConfig,
    persistence: Persistence,
}

#[derive(Clone, Debug)]
pub(crate) struct ScannedMediaItem {
    pub id: Uuid,
    pub root_path: String,
    pub relative_path: String,
    pub file_name: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub modified_at: String,
    pub indexed_at: String,
    pub content_type: Option<String>,
    pub duration_seconds: Option<f64>,
    pub container_name: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub probed_at: Option<String>,
    pub probe_error: Option<String>,
    pub subtitle_tracks: Vec<SubtitleTrack>,
    pub thumbnail_generated_at: Option<String>,
    pub thumbnail_error: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct ThumbnailOutcome {
    generated_at: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct ProbeMetadata {
    duration_seconds: Option<f64>,
    container_name: Option<String>,
    video_codec: Option<String>,
    audio_codec: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    probed_at: Option<String>,
    probe_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    format: Option<FfprobeFormat>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    format_name: Option<String>,
    duration: Option<String>,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            root_paths: Vec::new(),
            probe_command: default_probe_command(),
            ffmpeg_command: default_ffmpeg_command(),
            thumbnail_cache_dir: Some(default_thumbnail_cache_dir()),
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
}

impl LibraryService {
    pub fn new(persistence: Persistence, config: LibraryConfig) -> Self {
        Self {
            config,
            persistence,
        }
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

    pub async fn scan(&self) -> Result<LibraryScanResponse, PersistenceError> {
        self.sync_config().await?;

        let scanned_at = format_timestamp(OffsetDateTime::now_utc());
        let probe_command = self.config.probe_command().map(Path::to_path_buf);
        let ffmpeg_command = self.config.ffmpeg_command().map(Path::to_path_buf);
        let thumbnail_cache_dir = self.config.thumbnail_cache_dir().map(Path::to_path_buf);

        for root_path in self.config.root_paths() {
            let root_path_string = root_path.to_string_lossy().to_string();
            let existing_ids = self
                .persistence
                .existing_media_ids(&root_path_string)
                .await?;

            match scan_root(
                root_path.clone(),
                probe_command.clone(),
                ffmpeg_command.clone(),
                thumbnail_cache_dir.clone(),
                existing_ids,
            )
            .await
            {
                Ok(items) => {
                    self.persistence
                        .replace_library_scan(&root_path_string, &items)
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

async fn scan_root(
    root_path: PathBuf,
    probe_command: Option<PathBuf>,
    ffmpeg_command: Option<PathBuf>,
    thumbnail_cache_dir: Option<PathBuf>,
    existing_ids: HashMap<String, Uuid>,
) -> Result<Vec<ScannedMediaItem>, String> {
    task::spawn_blocking(move || {
        scan_root_blocking(
            &root_path,
            probe_command.as_deref(),
            ffmpeg_command.as_deref(),
            thumbnail_cache_dir.as_deref(),
            &existing_ids,
        )
    })
    .await
    .map_err(|error| format!("scan worker failed: {error}"))?
}

fn scan_root_blocking(
    root_path: &Path,
    probe_command: Option<&Path>,
    ffmpeg_command: Option<&Path>,
    thumbnail_cache_dir: Option<&Path>,
    existing_ids: &HashMap<String, Uuid>,
) -> Result<Vec<ScannedMediaItem>, String> {
    let root_metadata = fs::metadata(root_path).map_err(|error| {
        format!(
            "could not read library root '{}': {error}",
            root_path.display()
        )
    })?;

    if !root_metadata.is_dir() {
        return Err(format!(
            "library root '{}' is not a directory",
            root_path.display()
        ));
    }

    let indexed_at = format_timestamp(OffsetDateTime::now_utc());
    let root_path_string = root_path.to_string_lossy().to_string();
    let mut items = Vec::new();

    collect_media_items(
        root_path,
        root_path,
        probe_command,
        ffmpeg_command,
        thumbnail_cache_dir,
        existing_ids,
        &root_path_string,
        &indexed_at,
        &mut items,
    );
    items.sort_unstable_by(|left, right| {
        left.root_path
            .cmp(&right.root_path)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });

    Ok(items)
}

fn collect_media_items(
    root_path: &Path,
    current_path: &Path,
    probe_command: Option<&Path>,
    ffmpeg_command: Option<&Path>,
    thumbnail_cache_dir: Option<&Path>,
    existing_ids: &HashMap<String, Uuid>,
    root_path_string: &str,
    indexed_at: &str,
    items: &mut Vec<ScannedMediaItem>,
) {
    let Ok(entries) = fs::read_dir(current_path) else {
        warn!(path = %current_path.display(), "skipping unreadable directory during library scan");
        return;
    };

    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_unstable_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            warn!(path = %path.display(), "skipping path with unreadable file type during library scan");
            continue;
        };

        if file_type.is_symlink() {
            continue;
        }

        if file_type.is_dir() {
            collect_media_items(
                root_path,
                &path,
                probe_command,
                ffmpeg_command,
                thumbnail_cache_dir,
                existing_ids,
                root_path_string,
                indexed_at,
                items,
            );
            continue;
        }

        if !file_type.is_file() || !is_supported_media_path(&path) {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            warn!(path = %path.display(), "skipping unreadable file during library scan");
            continue;
        };

        let modified_at = metadata
            .modified()
            .ok()
            .map(OffsetDateTime::from)
            .map(format_timestamp)
            .unwrap_or_else(|| indexed_at.to_string());

        let Some(relative_path) = normalize_relative_path(path.strip_prefix(root_path).ok()) else {
            warn!(path = %path.display(), root = %root_path.display(), "skipping file outside library root");
            continue;
        };

        let file_name = path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| relative_path.clone());
        let extension = normalized_extension(&path);
        let probe_metadata = probe_media_metadata(&path, probe_command);
        let subtitle_tracks = discover_sidecar_subtitles(root_path, &path);
        let media_id = existing_ids
            .get(&relative_path)
            .copied()
            .unwrap_or_else(Uuid::new_v4);
        let thumbnail = generate_thumbnail(
            &path,
            media_id,
            metadata.modified().ok(),
            probe_metadata.video_codec.as_deref(),
            probe_metadata.duration_seconds,
            ffmpeg_command,
            thumbnail_cache_dir,
        );

        items.push(ScannedMediaItem {
            id: media_id,
            root_path: root_path_string.to_string(),
            relative_path,
            file_name,
            extension: extension.clone(),
            size_bytes: metadata.len(),
            modified_at,
            indexed_at: indexed_at.to_string(),
            content_type: extension
                .as_deref()
                .and_then(content_type_for_extension)
                .map(str::to_string),
            duration_seconds: probe_metadata.duration_seconds,
            container_name: probe_metadata.container_name,
            video_codec: probe_metadata.video_codec,
            audio_codec: probe_metadata.audio_codec,
            width: probe_metadata.width,
            height: probe_metadata.height,
            probed_at: probe_metadata.probed_at,
            probe_error: probe_metadata.probe_error,
            subtitle_tracks,
            thumbnail_generated_at: thumbnail.generated_at,
            thumbnail_error: thumbnail.error,
        });
    }
}

fn discover_sidecar_subtitles(root_path: &Path, media_path: &Path) -> Vec<SubtitleTrack> {
    let Some(parent) = media_path.parent() else {
        return Vec::new();
    };
    let Some(media_stem) = media_path
        .file_stem()
        .map(|value| value.to_string_lossy().to_string())
    else {
        return Vec::new();
    };

    let Ok(entries) = fs::read_dir(parent) else {
        warn!(
            path = %parent.display(),
            "skipping subtitle discovery in unreadable directory during library scan"
        );
        return Vec::new();
    };

    let mut tracks = entries
        .filter_map(Result::ok)
        .filter_map(|entry| subtitle_track_from_entry(root_path, &media_stem, entry))
        .collect::<Vec<_>>();

    tracks.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
    tracks
}

fn subtitle_track_from_entry(
    root_path: &Path,
    media_stem: &str,
    entry: fs::DirEntry,
) -> Option<SubtitleTrack> {
    let path = entry.path();
    let Ok(file_type) = entry.file_type() else {
        warn!(
            path = %path.display(),
            "skipping subtitle path with unreadable file type during library scan"
        );
        return None;
    };

    if file_type.is_symlink() || !file_type.is_file() {
        return None;
    }

    let extension = normalized_extension(&path)?;

    if !SUPPORTED_SUBTITLE_EXTENSIONS.contains(&extension.as_str()) {
        return None;
    }

    let suffix = subtitle_suffix_for_media(&path, media_stem)?;
    let relative_path = normalize_relative_path(path.strip_prefix(root_path).ok())?;
    let file_name = path.file_name()?.to_string_lossy().to_string();
    let (label, language) = subtitle_presentation(&suffix);

    Some(SubtitleTrack {
        file_name,
        relative_path,
        extension,
        label,
        language,
    })
}

fn subtitle_suffix_for_media(path: &Path, media_stem: &str) -> Option<String> {
    let subtitle_stem = path.file_stem()?.to_string_lossy();

    if subtitle_stem == media_stem {
        return Some(String::new());
    }

    subtitle_stem
        .strip_prefix(media_stem)
        .and_then(|suffix| suffix.strip_prefix('.'))
        .map(str::to_string)
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

fn probe_media_metadata(path: &Path, probe_command: Option<&Path>) -> ProbeMetadata {
    let Some(probe_command) = probe_command else {
        return ProbeMetadata::default();
    };

    let probed_at = format_timestamp(OffsetDateTime::now_utc());
    let output = Command::new(probe_command)
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("format=format_name,duration:stream=codec_type,codec_name,width,height")
        .arg("-of")
        .arg("json")
        .arg(path)
        .output();

    match output {
        Ok(output) if output.status.success() => {
            match parse_probe_output(&output.stdout, probed_at.clone()) {
                Ok(metadata) => metadata,
                Err(error) => ProbeMetadata {
                    probed_at: Some(probed_at),
                    probe_error: Some(error),
                    ..ProbeMetadata::default()
                },
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let error = if stderr.is_empty() {
                format!("ffprobe exited with status {}", output.status)
            } else {
                format!("ffprobe failed: {stderr}")
            };

            ProbeMetadata {
                probed_at: Some(probed_at),
                probe_error: Some(error),
                ..ProbeMetadata::default()
            }
        }
        Err(error) => ProbeMetadata {
            probed_at: Some(probed_at),
            probe_error: Some(format!("ffprobe could not start: {error}")),
            ..ProbeMetadata::default()
        },
    }
}

fn parse_probe_output(output: &[u8], probed_at: String) -> Result<ProbeMetadata, String> {
    let parsed: FfprobeOutput = serde_json::from_slice(output)
        .map_err(|error| format!("ffprobe returned invalid JSON: {error}"))?;
    let video_stream = parsed
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"));
    let audio_stream = parsed
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("audio"));

    Ok(ProbeMetadata {
        duration_seconds: parsed
            .format
            .as_ref()
            .and_then(|format| format.duration.as_deref())
            .and_then(|duration| duration.parse::<f64>().ok())
            .map(|duration| round_to(duration, 3)),
        container_name: parsed
            .format
            .as_ref()
            .and_then(|format| format.format_name.clone()),
        video_codec: video_stream.and_then(|stream| stream.codec_name.clone()),
        audio_codec: audio_stream.and_then(|stream| stream.codec_name.clone()),
        width: video_stream.and_then(|stream| stream.width),
        height: video_stream.and_then(|stream| stream.height),
        probed_at: Some(probed_at),
        probe_error: None,
    })
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

fn default_ffmpeg_command() -> Option<PathBuf> {
    Some(PathBuf::from("ffmpeg"))
}

fn ffmpeg_command_from_env() -> Option<PathBuf> {
    match env::var_os("SYNCPLAY_FFMPEG_BIN") {
        Some(value) if value.is_empty() => None,
        Some(value) => Some(PathBuf::from(value)),
        None => default_ffmpeg_command(),
    }
}

fn default_thumbnail_cache_dir() -> PathBuf {
    PathBuf::from(DEFAULT_THUMBNAIL_CACHE_DIR)
}

fn thumbnail_cache_dir_from_env() -> Option<PathBuf> {
    match env::var_os("SYNCPLAY_THUMBNAIL_DIR") {
        Some(value) if value.is_empty() => None,
        Some(value) => Some(PathBuf::from(value)),
        None => Some(default_thumbnail_cache_dir()),
    }
}

pub(crate) fn thumbnail_path_for(cache_dir: &Path, media_id: Uuid) -> PathBuf {
    cache_dir.join(format!("{media_id}.jpg"))
}

fn generate_thumbnail(
    media_path: &Path,
    media_id: Uuid,
    media_modified_at: Option<std::time::SystemTime>,
    video_codec: Option<&str>,
    duration_seconds: Option<f64>,
    ffmpeg_command: Option<&Path>,
    thumbnail_cache_dir: Option<&Path>,
) -> ThumbnailOutcome {
    if video_codec.is_none() {
        return ThumbnailOutcome::default();
    }

    let Some(ffmpeg_command) = ffmpeg_command else {
        return ThumbnailOutcome::default();
    };
    let Some(cache_dir) = thumbnail_cache_dir else {
        return ThumbnailOutcome::default();
    };

    let output_path = thumbnail_path_for(cache_dir, media_id);

    if thumbnail_is_up_to_date(&output_path, media_modified_at) {
        return ThumbnailOutcome {
            generated_at: Some(format_timestamp(OffsetDateTime::now_utc())),
            error: None,
        };
    }

    if let Err(error) = fs::create_dir_all(cache_dir) {
        return ThumbnailOutcome {
            generated_at: None,
            error: Some(format!(
                "could not create thumbnail directory '{}': {error}",
                cache_dir.display()
            )),
        };
    }

    let seek_seconds = thumbnail_seek_seconds(duration_seconds);
    let generated_at = format_timestamp(OffsetDateTime::now_utc());
    let output = Command::new(ffmpeg_command)
        .arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-ss")
        .arg(format!("{seek_seconds:.3}"))
        .arg("-i")
        .arg(media_path)
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg(format!("scale={THUMBNAIL_WIDTH}:-2"))
        .arg("-q:v")
        .arg("4")
        .arg(&output_path)
        .output();

    match output {
        Ok(output) if output.status.success() => {
            if output_path.exists() {
                ThumbnailOutcome {
                    generated_at: Some(generated_at),
                    error: None,
                }
            } else {
                ThumbnailOutcome {
                    generated_at: None,
                    error: Some("ffmpeg reported success but no thumbnail file was produced".into()),
                }
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let error = if stderr.is_empty() {
                format!("ffmpeg exited with status {}", output.status)
            } else {
                format!("ffmpeg failed: {stderr}")
            };
            let _ = fs::remove_file(&output_path);

            ThumbnailOutcome {
                generated_at: None,
                error: Some(error),
            }
        }
        Err(error) => ThumbnailOutcome {
            generated_at: None,
            error: Some(format!("ffmpeg could not start: {error}")),
        },
    }
}

fn thumbnail_is_up_to_date(
    thumbnail_path: &Path,
    media_modified_at: Option<std::time::SystemTime>,
) -> bool {
    let Ok(metadata) = fs::metadata(thumbnail_path) else {
        return false;
    };

    if !metadata.is_file() || metadata.len() == 0 {
        return false;
    }

    let (Some(media_modified), Ok(thumbnail_modified)) = (media_modified_at, metadata.modified())
    else {
        return false;
    };

    thumbnail_modified >= media_modified
}

fn thumbnail_seek_seconds(duration_seconds: Option<f64>) -> f64 {
    match duration_seconds {
        Some(duration) if duration > 0.0 => {
            let fraction = duration * 0.10;
            fraction.clamp(0.0, 30.0)
        }
        _ => 0.0,
    }
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

fn is_supported_media_path(path: &Path) -> bool {
    let Some(extension) = normalized_extension(path) else {
        return false;
    };

    SUPPORTED_MEDIA_EXTENSIONS.contains(&extension.as_str())
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
    use std::{fs, path::PathBuf};
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

    #[test]
    fn scan_root_filters_unsupported_files_and_nested_symlinks() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("library");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("movie.mp4"), b"movie").unwrap();
        fs::write(nested.join("song.flac"), b"audio").unwrap();
        fs::write(root.join("notes.txt"), b"ignore").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("movie.mp4"), root.join("movie-link.mp4")).unwrap();

        let items = scan_root_blocking(&root, None, None, None, &HashMap::new()).unwrap();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].relative_path, "movie.mp4");
        assert_eq!(items[1].relative_path, "nested/song.flac");
    }

    #[test]
    fn scan_root_matches_supported_extensions_case_insensitively() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("library");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("FEATURE.MP4"), b"movie").unwrap();
        fs::write(root.join("concert.FlAc"), b"audio").unwrap();

        let items = scan_root_blocking(&root, None, None, None, &HashMap::new()).unwrap();
        let relative_paths = items
            .iter()
            .map(|item| item.relative_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(relative_paths, vec!["FEATURE.MP4", "concert.FlAc"]);
        assert_eq!(items[0].content_type.as_deref(), Some("video/mp4"));
        assert_eq!(items[1].content_type.as_deref(), Some("audio/flac"));
    }

    #[test]
    fn scan_root_discovers_sidecar_subtitles_for_matching_media() {
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

        let items = scan_root_blocking(&root, None, None, None, &HashMap::new()).unwrap();
        let subtitle_tracks = &items[0].subtitle_tracks;

        assert_eq!(items.len(), 1);
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
        let error =
            parse_probe_output(b"{ definitely-not-json", "2026-01-01T00:00:00Z".to_string())
                .unwrap_err();

        assert!(error.starts_with("ffprobe returned invalid JSON:"));
    }
}
