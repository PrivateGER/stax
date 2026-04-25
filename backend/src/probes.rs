//! Background ffprobe worker pool.
//!
//! Stage 1 of the library scan (the directory walk) inserts every
//! discovered file into `media_items` with NULL probe columns; this pool
//! drains those rows in the background, runs ffprobe, and updates the row
//! once metadata is in hand. After a successful probe it also enqueues a
//! thumbnail job, since the thumbnail pipeline depends on `video_codec`
//! and `duration_seconds` for its seek heuristic.
//!
//! Pattern lifted directly from `crate::thumbnails::ThumbnailWorkerPool`:
//! mpsc channel for incoming jobs, `Semaphore` to bound concurrency,
//! per-job tokio task that takes the permit and persists the outcome.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    library::probe_media_metadata,
    persistence::{PendingProbe, Persistence},
    scan_gate::ScanGate,
    thumbnails::{ThumbnailJob, ThumbnailWorkerPool},
};

const DEFAULT_WORKERS: usize = 4;
const QUEUE_CAPACITY: usize = 4096;

#[derive(Clone, Debug)]
pub struct ProbeConfig {
    /// Path to the ffprobe binary. `None` disables probing entirely —
    /// the pool then drops every job on the floor (rows stay pending).
    pub probe_command: Option<PathBuf>,
    pub max_concurrent: usize,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            probe_command: None,
            max_concurrent: DEFAULT_WORKERS,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProbeJob {
    pub media_id: Uuid,
    pub media_path: PathBuf,
    pub root_path: PathBuf,
    pub extension: Option<String>,
}

impl ProbeJob {
    pub fn from_pending(pending: PendingProbe) -> Self {
        let root_path = PathBuf::from(&pending.root_path);
        let media_path = root_path.join(&pending.relative_path);

        Self {
            media_id: pending.media_id,
            media_path,
            root_path,
            extension: pending.extension,
        }
    }
}

/// Cheap-to-clone handle on the probe dispatcher, suitable for embedding
/// in `AppState`.
#[derive(Clone)]
pub struct ProbeWorkerPool {
    sender: mpsc::Sender<ProbeJob>,
    queued: Arc<Mutex<HashSet<Uuid>>>,
}

impl ProbeWorkerPool {
    /// Spawn the dispatcher and return a handle. Like the thumbnail pool,
    /// this lives for the duration of the process.
    pub fn spawn(
        config: ProbeConfig,
        persistence: Persistence,
        thumbnails: ThumbnailWorkerPool,
        scan_gate: ScanGate,
    ) -> Self {
        let (sender, mut receiver) = mpsc::channel::<ProbeJob>(QUEUE_CAPACITY);
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent.max(1)));
        info!(
            workers = config.max_concurrent,
            probe = ?config.probe_command,
            "probe worker pool starting"
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
                    "probe job dequeued; awaiting worker permit"
                );
                // Background probe work uses the shared scan gate so future
                // foreground work can pause it before taking a worker slot.
                scan_gate.wait_idle().await;
                let permit = match Arc::clone(&semaphore).acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => return, // semaphore closed → pool is shutting down
                };
                let config = Arc::clone(&config);
                let persistence = persistence.clone();
                let thumbnails = thumbnails.clone();

                tokio::spawn(async move {
                    process_job(job, &config, &persistence, &thumbnails).await;
                    drop(permit);
                });
            }
        });

        Self { sender, queued }
    }

    pub fn enqueue(&self, job: ProbeJob) {
        let media_id = job.media_id;
        let media_path = job.media_path.display().to_string();
        if !mark_queued(&self.queued, media_id) {
            debug!(%media_id, %media_path, "probe job already queued; skipping duplicate");
            return;
        }

        match self.sender.try_send(job) {
            Ok(()) => debug!(%media_id, %media_path, "probe job enqueued"),
            Err(error) => {
                unmark_queued(&self.queued, media_id);
                match error {
                    mpsc::error::TrySendError::Full(job) => {
                        warn!(media_id = %job.media_id, "probe worker queue is full; dropping job");
                    }
                    mpsc::error::TrySendError::Closed(job) => {
                        warn!(media_id = %job.media_id, "probe worker pool is closed; dropping job");
                    }
                }
            }
        }
    }

    pub fn enqueue_pending(&self, pending: Vec<PendingProbe>) {
        for entry in pending {
            self.enqueue(ProbeJob::from_pending(entry));
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

async fn process_job(
    job: ProbeJob,
    config: &ProbeConfig,
    persistence: &Persistence,
    thumbnails: &ThumbnailWorkerPool,
) {
    let media_id = job.media_id;
    let media_path = job.media_path.display().to_string();

    let Some(probe_command) = config.probe_command.as_deref() else {
        // Probing is disabled (no ffprobe configured). Leave the row
        // pending forever — the user explicitly opted out, and we don't
        // want to lie about a probe that didn't happen.
        debug!(%media_id, %media_path, "no ffprobe configured; dropping probe job");
        return;
    };

    debug!(%media_id, %media_path, "probe job started");
    let started = Instant::now();
    let outcome = probe_media_metadata(
        &job.media_path,
        job.extension.as_deref(),
        Some(probe_command),
    )
    .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let probe_succeeded = outcome.probe_error.is_none();
    let video_codec = outcome.video_codec.clone();
    let duration_seconds = outcome.duration_seconds;

    info!(
        %media_id,
        elapsed_ms,
        succeeded = probe_succeeded,
        "probe complete"
    );

    if let Err(error) = persistence.update_probe_metadata(media_id, &outcome).await {
        warn!(%error, %media_id, "failed to persist probe outcome");
        return;
    }

    // After a successful probe the thumbnail row is reset to pending; chain
    // the thumbnail job inline so we don't wait for the next poll/restart
    // to pick it up. Failed probes leave the row alone — generating a
    // thumbnail without a known codec/duration would be guessing.
    if probe_succeeded {
        thumbnails.enqueue(ThumbnailJob {
            media_id,
            media_path: job.media_path,
            video_codec,
            duration_seconds,
        });
    }
}
