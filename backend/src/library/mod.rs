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

pub(crate) mod config;
pub(crate) mod playback;
pub(crate) mod probe;
pub(crate) mod walk;

pub use config::{LibraryConfig, default_probe_workers, default_walk_workers};
pub(crate) use playback::{is_browser_safe_audio_codec_for_mp4, is_browser_safe_video_codec};
use walk::walk_root;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        library::playback::{VideoPlaybackInfo, classify_playback_mode},
        library::probe::{parse_probe_output, probe_media_metadata},
        library::walk::{
            DirMediaCandidate, classify_directory, content_type_for_extension,
            subtitle_presentation,
        },
        protocol::{AudioStream, PlaybackMode},
    };
    use std::{fs, process::Command as StdCommand};
    use tempfile::TempDir;

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
