use std::path::{Path, PathBuf};

use crate::{
    persistence::StreamCopyRecord,
    protocol::{MediaItem, SubtitleSourceKind, SubtitleTrack, is_text_subtitle_codec},
};

pub(super) fn build_burn_subtitle_filter(
    media_item: &MediaItem,
    copy: &StreamCopyRecord,
    media_path: &Path,
) -> Result<String, String> {
    let Some((kind, index)) = copy.subtitle_kind.zip(copy.subtitle_index) else {
        return Err("A subtitle source is required for burned subtitles.".to_string());
    };

    match kind {
        SubtitleSourceKind::Sidecar => {
            let track = media_item
                .subtitle_tracks
                .get(index as usize)
                .ok_or_else(|| "Selected sidecar subtitle track does not exist.".to_string())?;
            Ok(format!(
                "subtitles={}",
                escape_subtitles_path(&resolve_sidecar_subtitle_path(media_item, track))
            ))
        }
        SubtitleSourceKind::Embedded => {
            let ordinal = media_item
                .subtitle_streams
                .iter()
                .position(|stream| stream.index == index)
                .ok_or_else(|| "Selected embedded subtitle stream does not exist.".to_string())?;
            let stream = &media_item.subtitle_streams[ordinal];
            if !is_text_subtitle_codec(stream.codec.as_deref()) {
                return Err("Selected embedded subtitle stream cannot be burned in.".to_string());
            }
            Ok(format!(
                "subtitles={}:si={ordinal}",
                escape_subtitles_path(media_path)
            ))
        }
    }
}

pub(super) fn resolve_sidecar_subtitle_path(
    media_item: &MediaItem,
    track: &SubtitleTrack,
) -> PathBuf {
    let mut path = PathBuf::from(&media_item.root_path);
    for component in Path::new(&track.relative_path).components() {
        path.push(component.as_os_str());
    }
    path
}

fn escape_subtitles_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace(':', r"\\:")
        .replace('\'', r"\\\'")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace(',', "\\,")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::escape_subtitles_path;

    #[test]
    fn subtitles_filter_path_escape_preserves_apostrophes_and_options() {
        let escaped = escape_subtitles_path(Path::new("/tmp/You're an Akiba Maid: v3, [test].mkv"));

        assert_eq!(
            escaped,
            r"/tmp/You\\\'re an Akiba Maid\\: v3\, \[test\].mkv"
        );
        assert_eq!(
            format!("subtitles={escaped}:si=1"),
            r"subtitles=/tmp/You\\\'re an Akiba Maid\\: v3\, \[test\].mkv:si=1"
        );
    }
}
