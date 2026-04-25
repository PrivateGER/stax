use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

use crate::{
    ffmpeg::FfmpegHardwareAcceleration,
    thumbnails::{default_ffmpeg_command, default_thumbnail_cache_dir},
};

const DEFAULT_PROBE_WORKERS: usize = 4;
const DEFAULT_WALK_WORKERS: usize = 8;

#[derive(Clone, Debug)]
pub struct LibraryConfig {
    root_paths: Vec<PathBuf>,
    probe_command: Option<PathBuf>,
    ffmpeg_command: Option<PathBuf>,
    hw_accel: FfmpegHardwareAcceleration,
    thumbnail_cache_dir: Option<PathBuf>,
    stream_copy_cache_dir: Option<PathBuf>,
    probe_workers: usize,
    walk_workers: usize,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            root_paths: Vec::new(),
            probe_command: default_probe_command(),
            ffmpeg_command: default_ffmpeg_command(),
            hw_accel: FfmpegHardwareAcceleration::None,
            thumbnail_cache_dir: Some(default_thumbnail_cache_dir()),
            stream_copy_cache_dir: Some(default_stream_copy_cache_dir()),
            probe_workers: DEFAULT_PROBE_WORKERS,
            walk_workers: DEFAULT_WALK_WORKERS,
        }
    }
}

impl LibraryConfig {
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
            hw_accel: FfmpegHardwareAcceleration::None,
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

    pub fn with_hw_accel(mut self, hw_accel: FfmpegHardwareAcceleration) -> Self {
        self.hw_accel = hw_accel;
        self
    }

    pub fn hw_accel(&self) -> &FfmpegHardwareAcceleration {
        &self.hw_accel
    }

    pub fn with_thumbnail_cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.thumbnail_cache_dir = Some(path.into());
        self
    }

    pub fn thumbnail_cache_dir(&self) -> Option<&Path> {
        self.thumbnail_cache_dir.as_deref()
    }

    pub fn without_thumbnail_cache_dir(mut self) -> Self {
        self.thumbnail_cache_dir = None;
        self
    }

    pub fn with_stream_copy_cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.stream_copy_cache_dir = Some(path.into());
        self
    }

    pub fn stream_copy_cache_dir(&self) -> Option<&Path> {
        self.stream_copy_cache_dir.as_deref()
    }

    pub fn without_stream_copy_cache_dir(mut self) -> Self {
        self.stream_copy_cache_dir = None;
        self
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

fn default_probe_command() -> Option<PathBuf> {
    Some(PathBuf::from("ffprobe"))
}

fn default_stream_copy_cache_dir() -> PathBuf {
    PathBuf::from("stax-stream-copies")
}

pub fn default_probe_workers() -> usize {
    DEFAULT_PROBE_WORKERS
}

pub fn default_walk_workers() -> usize {
    DEFAULT_WALK_WORKERS
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::normalize_root_path;

    #[test]
    fn library_config_normalizes_relative_paths_and_deduplicates() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("media");
        std::fs::create_dir_all(&root).unwrap();
        let normalized_relative =
            normalize_root_path(PathBuf::from("media"), Some(temp_dir.path())).unwrap();
        let normalized_absolute = normalize_root_path(root.clone(), Some(temp_dir.path())).unwrap();

        assert_eq!(normalized_relative, root);
        assert_eq!(normalized_absolute, root);
    }
}
