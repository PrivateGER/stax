//! Background thumbnail generation.
//!
//! Library scans used to call `ffmpeg` synchronously per video to extract a
//! poster, which made `POST /api/library/scan` block on a serial pipeline of
//! decoder startups. This module moves that work onto a bounded background
//! worker pool: scans enqueue jobs and return immediately, workers chew
//! through the queue, and each job persists its outcome
//! (`thumbnail_generated_at` / `thumbnail_error`) so the frontend can pick
//! up freshly-generated thumbnails on its next poll.
//!
//! The generation pipeline tries cheaper, higher-quality sources before
//! falling back to a video frame:
//!   1. **Sidecar art** (Plex/Jellyfin convention: `poster.jpg`,
//!      `cover.jpg`, `folder.jpg`, plus per-file `<basename>.jpg` and
//!      `<basename>-poster.jpg`). Limited to the file's own directory so a
//!      whole series doesn't inherit one ancestor poster for every episode.
//!   2. **Embedded `attached_pic`** (mkv/mp4/m4a cover art).
//!   3. **Decoded video frame** via ffmpeg's `thumbnail` filter, which
//!      scores 100 candidate frames and picks the most representative one
//!      from a later point in the file, avoiding the "every episode gets
//!      the same 30-second cold-open frame" problem.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use time::OffsetDateTime;
use tokio::{
    process::Command,
    sync::{Semaphore, mpsc},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    clock::format_timestamp,
    env_config::{
        default_path as default_data_path, var as configured_var, var_os as configured_var_os,
    },
    ffmpeg::{FfmpegHardwareAcceleration, apply_input_acceleration},
    persistence::{PendingThumbnail, Persistence},
    scan_gate::ScanGate,
};

const DEFAULT_THUMBNAIL_CACHE_DIR: &str = "stax-thumbnails";
const LEGACY_THUMBNAIL_CACHE_DIR: &str = "syncplay-thumbnails";
const DEFAULT_WORKERS: usize = 2;
const THUMBNAIL_WIDTH: u32 = 480;
/// Image extensions tried for sidecar art, in order of preference.
const SIDECAR_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];
/// Per-folder sidecar names tried at the file's directory and every
/// ancestor up to the library root. Order matters: more specific first.
const SIDECAR_BASENAMES: &[&str] = &["poster", "cover", "folder"];

#[derive(Clone, Debug)]
pub struct ThumbnailConfig {
    pub cache_dir: Option<PathBuf>,
    pub ffmpeg_command: Option<PathBuf>,
    pub hw_accel: FfmpegHardwareAcceleration,
    pub max_concurrent: usize,
}

impl Default for ThumbnailConfig {
    fn default() -> Self {
        Self {
            cache_dir: Some(default_thumbnail_cache_dir()),
            ffmpeg_command: default_ffmpeg_command(),
            hw_accel: FfmpegHardwareAcceleration::None,
            max_concurrent: DEFAULT_WORKERS,
        }
    }
}

impl ThumbnailConfig {
    /// Pick up env-driven overrides on top of the supplied baseline. Caller
    /// supplies the ffmpeg/cache-dir defaults from `LibraryConfig` so the
    /// two configs stay aligned (both already follow `STAX_FFMPEG_BIN`
    /// and `STAX_THUMBNAIL_DIR`); this only adds the worker-count
    /// override that's specific to thumbnail generation.
    pub fn with_env_overrides(mut self) -> Self {
        if let Some(value) = configured_var("STAX_THUMBNAIL_WORKERS", "SYNCPLAY_THUMBNAIL_WORKERS")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .filter(|value| *value > 0)
        {
            self.max_concurrent = value;
        }

        self
    }
}

#[derive(Clone, Debug)]
pub struct ThumbnailJob {
    pub media_id: Uuid,
    pub media_path: PathBuf,
    pub video_codec: Option<String>,
    pub duration_seconds: Option<f64>,
}

impl ThumbnailJob {
    pub fn from_pending(pending: PendingThumbnail) -> Self {
        let root_path = PathBuf::from(&pending.root_path);
        let media_path = root_path.join(&pending.relative_path);

        Self {
            media_id: pending.media_id,
            media_path,
            video_codec: pending.video_codec,
            duration_seconds: pending.duration_seconds,
        }
    }
}

/// Handle on the background worker pool. Cloning is cheap (an `Arc` over an
/// mpsc sender), so the pool can be embedded in `AppState` and shared
/// across the request handlers that need to enqueue work.
#[derive(Clone)]
pub struct ThumbnailWorkerPool {
    sender: mpsc::UnboundedSender<ThumbnailJob>,
}

impl ThumbnailWorkerPool {
    /// Spawn the dispatcher and return a handle. The dispatcher lives as
    /// long as the process — there is no graceful shutdown today (workers
    /// are short-lived ffmpeg invocations and the process exits when all
    /// other tasks do).
    pub fn spawn(config: ThumbnailConfig, persistence: Persistence, scan_gate: ScanGate) -> Self {
        let (sender, mut receiver) = mpsc::unbounded_channel::<ThumbnailJob>();
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent.max(1)));
        info!(
            workers = config.max_concurrent,
            cache_dir = ?config.cache_dir,
            ffmpeg = ?config.ffmpeg_command,
            hw_accel = ?config.hw_accel,
            "thumbnail worker pool starting"
        );
        let config = Arc::new(config);

        tokio::spawn(async move {
            while let Some(job) = receiver.recv().await {
                debug!(
                    media_id = %job.media_id,
                    media_path = %job.media_path.display(),
                    "thumbnail job dequeued; awaiting worker permit"
                );
                // Pause new thumbnail work while foreground playback or
                // preparation owns the library mount. Thumbnail generation
                // also invokes ffmpeg, so it can otherwise starve the
                // user-facing transcoder. See `scan_gate` for details.
                scan_gate.wait_idle().await;
                let permit = match Arc::clone(&semaphore).acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => return, // semaphore closed → pool is shutting down
                };
                let config = Arc::clone(&config);
                let persistence = persistence.clone();

                tokio::spawn(async move {
                    process_job(job, &config, &persistence).await;
                    drop(permit);
                });
            }
        });

        Self { sender }
    }

    /// Enqueue a single job. Failures here mean the dispatcher task has
    /// died, which is unrecoverable for this process — log and drop.
    pub fn enqueue(&self, job: ThumbnailJob) {
        let media_id = job.media_id;
        let media_path = job.media_path.display().to_string();
        if let Err(error) = self.sender.send(job) {
            warn!(media_id = %error.0.media_id, "thumbnail worker pool is closed; dropping job");
        } else {
            debug!(%media_id, %media_path, "thumbnail job enqueued");
        }
    }

    pub fn enqueue_pending(&self, pending: Vec<PendingThumbnail>) {
        for entry in pending {
            self.enqueue(ThumbnailJob::from_pending(entry));
        }
    }
}

async fn process_job(job: ThumbnailJob, config: &ThumbnailConfig, persistence: &Persistence) {
    let media_id = job.media_id;
    let media_path = job.media_path.display().to_string();
    debug!(%media_id, %media_path, "thumbnail job started");
    let started = Instant::now();
    let outcome = generate(&job, config).await;
    let elapsed_ms = started.elapsed().as_millis();

    match &outcome {
        ThumbnailOutcome::Generated { timestamp, source } => {
            info!(%media_id, source = source.as_str(), elapsed_ms = elapsed_ms as u64, "thumbnail generated");
            if let Err(error) = persistence
                .update_thumbnail_state(media_id, Some(timestamp.as_str()), None)
                .await
            {
                warn!(%error, %media_id, "failed to persist generated thumbnail state");
            }
        }
        ThumbnailOutcome::Skipped => {
            // Nothing to do — leave the row in its pending state so a future
            // change (e.g. user adds a video stream) can re-evaluate. This
            // is reached for audio-only files when no sidecar art exists.
            // We still need to mark it so we don't re-queue it on every
            // restart; record an empty error to break the loop.
            let message = "no thumbnail source available (no sidecar art, no video stream)";
            debug!(%media_id, %media_path, elapsed_ms = elapsed_ms as u64, "thumbnail job skipped: {message}");
            if let Err(error) = persistence
                .update_thumbnail_state(media_id, None, Some(message))
                .await
            {
                warn!(%error, %media_id, "failed to persist skipped thumbnail state");
            }
        }
        ThumbnailOutcome::Failed(message) => {
            warn!(%media_id, %media_path, %message, elapsed_ms = elapsed_ms as u64, "thumbnail generation failed");
            if let Err(error) = persistence
                .update_thumbnail_state(media_id, None, Some(message.as_str()))
                .await
            {
                warn!(%error, %media_id, "failed to persist thumbnail error state");
            }
        }
    }
}

enum ThumbnailOutcome {
    Generated {
        timestamp: String,
        source: ThumbnailSource,
    },
    Skipped,
    Failed(String),
}

/// Which generation source produced the cached jpeg. Surfaced via tracing
/// so operators can see at a glance whether their library is well-curated
/// (sidecar/embedded dominate) or relying on frame extraction.
#[derive(Clone, Copy, Debug)]
enum ThumbnailSource {
    Cached,
    Sidecar,
    AttachedPic,
    Frame,
}

impl ThumbnailSource {
    fn as_str(self) -> &'static str {
        match self {
            ThumbnailSource::Cached => "cached",
            ThumbnailSource::Sidecar => "sidecar",
            ThumbnailSource::AttachedPic => "attached_pic",
            ThumbnailSource::Frame => "frame",
        }
    }
}

fn now_timestamp() -> String {
    format_timestamp(OffsetDateTime::now_utc())
}

async fn generate(job: &ThumbnailJob, config: &ThumbnailConfig) -> ThumbnailOutcome {
    let Some(cache_dir) = config.cache_dir.as_deref() else {
        debug!(media_id = %job.media_id, "no cache_dir configured; skipping");
        return ThumbnailOutcome::Skipped;
    };

    let output_path = thumbnail_path_for(cache_dir, job.media_id);

    // Cheap freshness check: if the cached jpeg is newer than the source,
    // accept it as up-to-date without spawning ffmpeg.
    if let Ok(metadata) = fs::metadata(&job.media_path) {
        let modified = metadata.modified().ok();
        if thumbnail_is_up_to_date(&output_path, modified) {
            debug!(media_id = %job.media_id, "cached thumbnail is fresh; skipping ffmpeg");
            return ThumbnailOutcome::Generated {
                timestamp: now_timestamp(),
                source: ThumbnailSource::Cached,
            };
        }
    }

    if let Err(error) = fs::create_dir_all(cache_dir) {
        return ThumbnailOutcome::Failed(format!(
            "could not create thumbnail directory '{}': {error}",
            cache_dir.display()
        ));
    }

    // Source 1: sidecar art (cheapest — a single file copy/scale).
    if let Some(sidecar) = find_sidecar_art(&job.media_path)
        && let Some(ffmpeg) = config.ffmpeg_command.as_deref()
    {
        debug!(
            media_id = %job.media_id,
            sidecar = %sidecar.display(),
            "trying sidecar art source"
        );
        let started = Instant::now();
        match render_from_image(ffmpeg, &sidecar, &output_path).await {
            Ok(()) => {
                debug!(
                    media_id = %job.media_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "sidecar art rendered"
                );
                return ThumbnailOutcome::Generated {
                    timestamp: now_timestamp(),
                    source: ThumbnailSource::Sidecar,
                };
            }
            Err(error) => {
                warn!(
                    media_id = %job.media_id,
                    sidecar = %sidecar.display(),
                    %error,
                    "sidecar art conversion failed; falling through to other sources"
                );
            }
        }
    }

    let Some(ffmpeg) = config.ffmpeg_command.as_deref() else {
        // No ffmpeg means we have no way to produce a thumbnail; treat as
        // pending forever (the user disabled generation explicitly).
        debug!(media_id = %job.media_id, "no ffmpeg configured; skipping");
        return ThumbnailOutcome::Skipped;
    };

    // Source 2: embedded cover art via `attached_pic`. Only meaningful for
    // containers that can carry it; if the container doesn't have one,
    // ffmpeg exits non-zero and we fall through.
    debug!(media_id = %job.media_id, "trying embedded attached_pic source");
    let started = Instant::now();
    match render_from_attached_pic(ffmpeg, &job.media_path, &output_path).await {
        Ok(()) => {
            debug!(
                media_id = %job.media_id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "embedded attached_pic rendered"
            );
            return ThumbnailOutcome::Generated {
                timestamp: now_timestamp(),
                source: ThumbnailSource::AttachedPic,
            };
        }
        Err(error) => {
            if is_missing_attached_pic_error(&error) {
                debug!(
                    media_id = %job.media_id,
                    "no embedded attached_pic; falling through to frame extraction"
                );
            } else {
                debug!(
                    media_id = %job.media_id,
                    %error,
                    "embedded attached_pic extraction failed; falling through to frame extraction"
                );
            }
        }
    }

    // Source 3: decoded video frame. Only attempt if there's actually a
    // video stream — saves an ffmpeg startup for audio-only files.
    if job.video_codec.is_none() {
        debug!(media_id = %job.media_id, "no video stream; skipping frame extraction");
        return ThumbnailOutcome::Skipped;
    }

    debug!(
        media_id = %job.media_id,
        duration_seconds = ?job.duration_seconds,
        "trying frame extraction source"
    );
    let started = Instant::now();
    match render_from_frame(
        ffmpeg,
        &job.media_path,
        &output_path,
        job.duration_seconds,
        &config.hw_accel,
    )
    .await
    {
        Ok(()) => {
            debug!(
                media_id = %job.media_id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "frame extraction rendered"
            );
            ThumbnailOutcome::Generated {
                timestamp: now_timestamp(),
                source: ThumbnailSource::Frame,
            }
        }
        Err(error) => ThumbnailOutcome::Failed(error),
    }
}

async fn render_from_image(ffmpeg: &Path, source: &Path, output: &Path) -> Result<(), String> {
    let result = Command::new(ffmpeg)
        .arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(source)
        .arg("-vf")
        .arg(format!("scale={THUMBNAIL_WIDTH}:-2"))
        .arg("-frames:v")
        .arg("1")
        .arg("-q:v")
        .arg("4")
        .arg(output)
        .output()
        .await;

    classify_ffmpeg_result(result, output)
}

async fn render_from_attached_pic(
    ffmpeg: &Path,
    media_path: &Path,
    output: &Path,
) -> Result<(), String> {
    let result = Command::new(ffmpeg)
        .arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(media_path)
        .arg("-map")
        .arg("0:v:disp:attached_pic")
        .arg("-vf")
        .arg(format!("scale={THUMBNAIL_WIDTH}:-2"))
        .arg("-frames:v")
        .arg("1")
        .arg("-q:v")
        .arg("4")
        .arg(output)
        .output()
        .await;

    classify_ffmpeg_result(result, output)
}

async fn render_from_frame(
    ffmpeg: &Path,
    media_path: &Path,
    output: &Path,
    duration_seconds: Option<f64>,
    hw_accel: &FfmpegHardwareAcceleration,
) -> Result<(), String> {
    let seek = thumbnail_seek_seconds(duration_seconds);
    // `thumbnail=100` scores the next 100 frames after the seek and picks
    // the most representative one. We intentionally sample later in the
    // file so episodic content doesn't collapse onto the same opening
    // credits frame across a whole season.
    let mut command = Command::new(ffmpeg);
    command.arg("-y").arg("-loglevel").arg("error");
    apply_input_acceleration(&mut command, hw_accel);
    let result = command
        .arg("-ss")
        .arg(format!("{seek:.3}"))
        .arg("-i")
        .arg(media_path)
        .arg("-vf")
        .arg(format!("thumbnail=100,scale={THUMBNAIL_WIDTH}:-2"))
        .arg("-frames:v")
        .arg("1")
        .arg("-q:v")
        .arg("4")
        .arg(output)
        .output()
        .await;

    classify_ffmpeg_result(result, output)
}

fn classify_ffmpeg_result(
    result: std::io::Result<std::process::Output>,
    output: &Path,
) -> Result<(), String> {
    match result {
        Ok(output_data) if output_data.status.success() => {
            if output.exists() && fs::metadata(output).map(|m| m.len() > 0).unwrap_or(false) {
                Ok(())
            } else {
                let _ = fs::remove_file(output);
                Err("ffmpeg reported success but no thumbnail file was produced".into())
            }
        }
        Ok(output_data) => {
            let stderr = String::from_utf8_lossy(&output_data.stderr)
                .trim()
                .to_string();
            let _ = fs::remove_file(output);
            if stderr.is_empty() {
                Err(format!("ffmpeg exited with status {}", output_data.status))
            } else {
                Err(format!("ffmpeg failed: {stderr}"))
            }
        }
        Err(error) => Err(format!("ffmpeg could not start: {error}")),
    }
}

fn is_missing_attached_pic_error(error: &str) -> bool {
    error.contains("matches no streams")
}

/// Look for sidecar art only in the media file's own directory. This keeps
/// curated per-movie posters working while avoiding a single ancestor
/// `poster.jpg` from flattening an entire show to one repeated image.
fn find_sidecar_art(media_path: &Path) -> Option<PathBuf> {
    let parent = media_path.parent()?;
    let stem = media_path.file_stem()?.to_string_lossy().to_string();

    // Per-file art, only at the immediate parent directory.
    for extension in SIDECAR_EXTENSIONS {
        for suffix in ["-poster", ""] {
            let candidate = parent.join(format!("{stem}{suffix}.{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // Per-folder art, but only for the immediate directory.
    for basename in SIDECAR_BASENAMES {
        for extension in SIDECAR_EXTENSIONS {
            let candidate = parent.join(format!("{basename}.{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

pub fn thumbnail_path_for(cache_dir: &Path, media_id: Uuid) -> PathBuf {
    cache_dir.join(format!("{media_id}.jpg"))
}

/// True iff a cached JPEG exists, is non-empty, and is at least as new as
/// the source media. Returning false simply triggers regeneration; never
/// false-positive here.
pub fn thumbnail_is_up_to_date(
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

/// Sample a later point in the file before letting the `thumbnail` filter
/// score frames. Using 20% avoids the repeated "30 second mark" thumbnails
/// that long episodic content used to get from the old hard clamp.
pub fn thumbnail_seek_seconds(duration_seconds: Option<f64>) -> f64 {
    match duration_seconds {
        Some(duration) if duration > 0.0 => duration * 0.20,
        _ => 0.0,
    }
}

pub fn default_thumbnail_cache_dir() -> PathBuf {
    default_data_path(DEFAULT_THUMBNAIL_CACHE_DIR, LEGACY_THUMBNAIL_CACHE_DIR)
}

pub fn thumbnail_cache_dir_from_env() -> Option<PathBuf> {
    match configured_var_os("STAX_THUMBNAIL_DIR", "SYNCPLAY_THUMBNAIL_DIR") {
        Some(value) if value.is_empty() => None,
        Some(value) => Some(PathBuf::from(value)),
        None => Some(default_thumbnail_cache_dir()),
    }
}

pub fn default_ffmpeg_command() -> Option<PathBuf> {
    Some(PathBuf::from("ffmpeg"))
}

pub fn ffmpeg_command_from_env() -> Option<PathBuf> {
    match configured_var_os("STAX_FFMPEG_BIN", "SYNCPLAY_FFMPEG_BIN") {
        Some(value) if value.is_empty() => None,
        Some(value) => Some(PathBuf::from(value)),
        None => default_ffmpeg_command(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn finds_per_file_poster_sidecar() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let media = root.join("movie.mkv");
        fs::write(&media, b"x").unwrap();
        let poster = root.join("movie-poster.jpg");
        fs::write(&poster, b"img").unwrap();

        assert_eq!(find_sidecar_art(&media), Some(poster));
    }

    #[test]
    fn finds_basename_sidecar_when_no_poster() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let media = root.join("movie.mkv");
        fs::write(&media, b"x").unwrap();
        let cover = root.join("movie.png");
        fs::write(&cover, b"img").unwrap();

        assert_eq!(find_sidecar_art(&media), Some(cover));
    }

    #[test]
    fn falls_back_to_folder_poster() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let nested = root.join("season-1");
        fs::create_dir_all(&nested).unwrap();
        let media = nested.join("episode-01.mkv");
        fs::write(&media, b"x").unwrap();
        let folder_art = nested.join("folder.jpg");
        fs::write(&folder_art, b"img").unwrap();

        assert_eq!(find_sidecar_art(&media), Some(folder_art));
    }

    #[test]
    fn does_not_use_ancestor_poster() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let nested = root.join("series").join("season-1");
        fs::create_dir_all(&nested).unwrap();
        let media = nested.join("episode-01.mkv");
        fs::write(&media, b"x").unwrap();
        let ancestor_art = root.join("series").join("poster.jpg");
        fs::write(&ancestor_art, b"img").unwrap();

        assert_eq!(find_sidecar_art(&media), None);
    }

    #[test]
    fn does_not_walk_above_immediate_directory() {
        let temp = TempDir::new().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let above_art = temp.path().join("poster.jpg");
        fs::write(&above_art, b"img").unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let media = root.join("movie.mkv");
        fs::write(&media, b"x").unwrap();

        assert_eq!(find_sidecar_art(&media), None);
    }

    #[test]
    fn seek_seconds_uses_later_point_without_thirty_second_cap() {
        assert_eq!(thumbnail_seek_seconds(Some(7200.0)), 1440.0);
        assert_eq!(thumbnail_seek_seconds(Some(60.0)), 12.0);
        assert_eq!(thumbnail_seek_seconds(Some(1500.0)), 300.0);
        assert_eq!(thumbnail_seek_seconds(Some(0.0)), 0.0);
        assert_eq!(thumbnail_seek_seconds(None), 0.0);
    }

    #[test]
    fn recognizes_missing_attached_pic_ffmpeg_error() {
        assert!(is_missing_attached_pic_error(
            "ffmpeg failed: Stream map '' matches no streams.\n\
             To ignore this, add a trailing '?' to the map."
        ));
        assert!(!is_missing_attached_pic_error(
            "ffmpeg failed: Invalid data found when processing input"
        ));
    }
}
