use std::path::{Path, PathBuf};

use uuid::Uuid;

const DEFAULT_STREAM_COPY_CACHE_DIR: &str = "stax-stream-copies";

pub fn default_stream_copy_cache_dir() -> PathBuf {
    PathBuf::from(DEFAULT_STREAM_COPY_CACHE_DIR)
}

pub fn stream_copy_output_dir(cache_dir: &Path, media_id: Uuid) -> PathBuf {
    cache_dir.join(media_id.to_string())
}

pub(super) fn temp_path_for_final(final_path: &Path) -> PathBuf {
    let parent = final_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let file_name = final_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("output");

    let temp_name = match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            format!("{stem}.tmp.{extension}")
        }
        _ => format!("{file_name}.tmp"),
    };

    parent.join(temp_name)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::temp_path_for_final;

    #[test]
    fn temp_path_preserves_media_extension_for_ffmpeg_muxer_detection() {
        let temp = temp_path_for_final(Path::new("/tmp/stream.mp4"));
        assert_eq!(temp, Path::new("/tmp/stream.tmp.mp4"));
    }

    #[test]
    fn temp_path_preserves_audio_extension_for_ffmpeg_muxer_detection() {
        let temp = temp_path_for_final(Path::new("/tmp/stream.m4a"));
        assert_eq!(temp, Path::new("/tmp/stream.tmp.m4a"));
    }

    #[test]
    fn temp_path_preserves_vtt_extension() {
        let temp = temp_path_for_final(Path::new("/tmp/subtitle.vtt"));
        assert_eq!(temp, Path::new("/tmp/subtitle.tmp.vtt"));
    }

    #[test]
    fn temp_path_falls_back_when_no_extension_exists() {
        let temp = temp_path_for_final(Path::new("/tmp/output"));
        assert_eq!(temp, Path::new("/tmp/output.tmp"));
    }
}
