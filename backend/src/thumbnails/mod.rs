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
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    ffmpeg::FfmpegHardwareAcceleration,
    persistence::{PendingThumbnail, Persistence},
    scan_gate::ScanGate,
};

pub(crate) mod render;

use render::{ThumbnailOutcome, generate};

const DEFAULT_THUMBNAIL_CACHE_DIR: &str = "stax-thumbnails";
const DEFAULT_WORKERS: usize = 2;
const QUEUE_CAPACITY: usize = 4096;
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
    sender: mpsc::Sender<ThumbnailJob>,
    queued: Arc<Mutex<HashSet<Uuid>>>,
}

impl ThumbnailWorkerPool {
    /// Spawn the dispatcher and return a handle. The dispatcher lives as
    /// long as the process — there is no graceful shutdown today (workers
    /// are short-lived ffmpeg invocations and the process exits when all
    /// other tasks do).
    pub fn spawn(config: ThumbnailConfig, persistence: Persistence, scan_gate: ScanGate) -> Self {
        let (sender, mut receiver) = mpsc::channel::<ThumbnailJob>(QUEUE_CAPACITY);
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent.max(1)));
        info!(
            workers = config.max_concurrent,
            cache_dir = ?config.cache_dir,
            ffmpeg = ?config.ffmpeg_command,
            hw_accel = ?config.hw_accel,
            "thumbnail worker pool starting"
        );
        let config = Arc::new(config);
        let queued = Arc::new(Mutex::new(HashSet::new()));
        let worker_queued = Arc::clone(&queued);

        tokio::spawn(async move {
            while let Some(job) = receiver.recv().await {
                unmark_queued(&worker_queued, job.media_id);
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

        Self { sender, queued }
    }

    /// Enqueue a single job. Failures here mean the dispatcher task has
    /// died, which is unrecoverable for this process — log and drop.
    pub fn enqueue(&self, job: ThumbnailJob) {
        let media_id = job.media_id;
        let media_path = job.media_path.display().to_string();
        if !mark_queued(&self.queued, media_id) {
            debug!(%media_id, %media_path, "thumbnail job already queued; skipping duplicate");
            return;
        }

        match self.sender.try_send(job) {
            Ok(()) => debug!(%media_id, %media_path, "thumbnail job enqueued"),
            Err(error) => {
                unmark_queued(&self.queued, media_id);
                match error {
                    mpsc::error::TrySendError::Full(job) => {
                        warn!(media_id = %job.media_id, "thumbnail worker queue is full; dropping job");
                    }
                    mpsc::error::TrySendError::Closed(job) => {
                        warn!(media_id = %job.media_id, "thumbnail worker pool is closed; dropping job");
                    }
                }
            }
        }
    }

    pub fn enqueue_pending(&self, pending: Vec<PendingThumbnail>) {
        for entry in pending {
            self.enqueue(ThumbnailJob::from_pending(entry));
        }
    }
}

fn mark_queued(queued: &Arc<Mutex<HashSet<Uuid>>>, media_id: Uuid) -> bool {
    queued
        .lock()
        .map(|mut queued| queued.insert(media_id))
        .unwrap_or(false)
}

fn unmark_queued(queued: &Arc<Mutex<HashSet<Uuid>>>, media_id: Uuid) {
    if let Ok(mut queued) = queued.lock() {
        queued.remove(&media_id);
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

pub fn default_thumbnail_cache_dir() -> PathBuf {
    PathBuf::from(DEFAULT_THUMBNAIL_CACHE_DIR)
}

pub fn default_ffmpeg_command() -> Option<PathBuf> {
    Some(PathBuf::from("ffmpeg"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thumbnails::render::{
        find_sidecar_art, is_missing_attached_pic_error, thumbnail_seek_seconds,
    };
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
