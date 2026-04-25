use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    clock::format_timestamp,
    ffmpeg::FfmpegHardwareAcceleration,
    persistence::{LibrarySnapshot, LibraryStatusSnapshot, Persistence, PersistenceError},
    protocol::{LibraryResponse, LibraryScanResponse, LibraryStatusResponse, MediaItem},
    thumbnails::thumbnail_path_for,
};

use super::{LibraryConfig, walk::walk_root};

#[derive(Clone, Debug)]
pub struct LibraryService {
    config: LibraryConfig,
    persistence: Persistence,
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

    pub fn hw_accel(&self) -> &FfmpegHardwareAcceleration {
        self.config.hw_accel()
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
        let status = self.persistence.load_library_status().await?;
        let snapshot = self.persistence.load_library_snapshot().await?;

        Ok(snapshot.into_response(status))
    }

    pub async fn status(&self) -> Result<LibraryStatusResponse, PersistenceError> {
        let status = self.persistence.load_library_status().await?;

        Ok(status.into_response())
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
    /// `ProbeWorkerPool` - see `crate::probes`.
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

        let status = self.persistence.load_library_status().await?;
        let snapshot = self.persistence.load_library_snapshot().await?;
        let indexed_item_count = snapshot.items.len();
        let scanned_root_count = snapshot.roots.len();

        Ok(LibraryScanResponse {
            revision: status.revision,
            has_pending_background_work: status.has_pending_background_work,
            roots: snapshot.roots,
            items: snapshot.items,
            scanned_root_count,
            indexed_item_count,
            scanned_at,
        })
    }
}

impl LibrarySnapshot {
    fn into_response(self, status: LibraryStatusSnapshot) -> LibraryResponse {
        LibraryResponse {
            revision: status.revision,
            has_pending_background_work: status.has_pending_background_work,
            roots: self.roots,
            items: self.items,
        }
    }
}

impl LibraryStatusSnapshot {
    fn into_response(self) -> LibraryStatusResponse {
        LibraryStatusResponse {
            revision: self.revision,
            has_pending_background_work: self.has_pending_background_work,
        }
    }
}
