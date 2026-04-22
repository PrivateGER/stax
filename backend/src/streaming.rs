use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use axum::{
    body::Body,
    http::{
        HeaderValue, Response, StatusCode,
        header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE},
    },
};
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
};
use tokio_util::io::ReaderStream;

use crate::protocol::{MediaItem, SubtitleTrack};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ByteRange {
    start: u64,
    end: u64,
}

#[derive(Debug)]
pub(crate) enum StreamMediaError {
    NotFound,
    MalformedRange(&'static str),
    UnsatisfiableRange { file_len: u64 },
    Io(std::io::Error),
}

impl From<std::io::Error> for StreamMediaError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub(crate) async fn stream_media_response(
    media_item: &MediaItem,
    range_header: Option<&str>,
) -> Result<Response<Body>, StreamMediaError> {
    let media_path = resolve_media_path(media_item);
    let content_type = media_item
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");

    stream_file_response(&media_path, content_type, range_header).await
}

pub(crate) async fn stream_file_response(
    path: &Path,
    content_type: &str,
    range_header: Option<&str>,
) -> Result<Response<Body>, StreamMediaError> {
    let metadata = fs::metadata(path).await.map_err(map_file_error)?;

    if !metadata.is_file() {
        return Err(StreamMediaError::NotFound);
    }

    let file_len = metadata.len();
    let range = match range_header {
        Some(value) => Some(parse_range_header(value, file_len)?),
        None => None,
    };

    let mut file = File::open(path).await.map_err(map_file_error)?;
    let (status, start, end) = match range {
        Some(range) => (StatusCode::PARTIAL_CONTENT, range.start, range.end),
        None => (StatusCode::OK, 0, file_len.saturating_sub(1)),
    };
    let content_len = if file_len == 0 { 0 } else { end - start + 1 };

    if content_len > 0 {
        file.seek(SeekFrom::Start(start)).await?;
    }

    let body = if content_len == 0 {
        Body::empty()
    } else {
        Body::from_stream(ReaderStream::new(file.take(content_len)))
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;

    let headers = response.headers_mut();
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&content_len.to_string()).expect("content length should be valid"),
    );
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );

    if status == StatusCode::PARTIAL_CONTENT {
        headers.insert(
            CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{file_len}"))
                .expect("content range should be valid"),
        );
    }

    Ok(response)
}

pub(crate) async fn stream_thumbnail_response(
    thumbnail_path: &Path,
) -> Result<Response<Body>, StreamMediaError> {
    let metadata = fs::metadata(thumbnail_path).await.map_err(map_file_error)?;

    if !metadata.is_file() {
        return Err(StreamMediaError::NotFound);
    }

    let body = fs::read(thumbnail_path).await.map_err(map_file_error)?;
    let mut response = Response::new(Body::from(body));

    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));

    Ok(response)
}

pub(crate) async fn stream_subtitle_response(
    media_item: &MediaItem,
    subtitle_track: &SubtitleTrack,
) -> Result<Response<Body>, StreamMediaError> {
    let subtitle_path = resolve_relative_path(&media_item.root_path, &subtitle_track.relative_path);
    let metadata = fs::metadata(&subtitle_path).await.map_err(map_file_error)?;

    if !metadata.is_file() {
        return Err(StreamMediaError::NotFound);
    }

    let subtitle_body = fs::read(&subtitle_path).await.map_err(map_file_error)?;
    let subtitle_text = prepare_webvtt(&subtitle_body, &subtitle_track.extension);
    let mut response = Response::new(Body::from(subtitle_text.into_owned()));

    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/vtt; charset=utf-8"),
    );

    Ok(response)
}

pub(crate) async fn stream_webvtt_file_response(
    subtitle_path: &Path,
) -> Result<Response<Body>, StreamMediaError> {
    let metadata = fs::metadata(subtitle_path).await.map_err(map_file_error)?;

    if !metadata.is_file() {
        return Err(StreamMediaError::NotFound);
    }

    let body = fs::read(subtitle_path).await.map_err(map_file_error)?;
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/vtt; charset=utf-8"),
    );

    Ok(response)
}

pub(crate) fn unsatisfiable_range_response(file_len: u64) -> Response<Body> {
    let mut response = Response::new(Body::from("Requested range is not satisfiable."));
    *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
    response.headers_mut().insert(
        CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes */{file_len}"))
            .expect("content range should be valid"),
    );
    response
}

fn map_file_error(error: std::io::Error) -> StreamMediaError {
    if error.kind() == std::io::ErrorKind::NotFound {
        StreamMediaError::NotFound
    } else {
        StreamMediaError::Io(error)
    }
}

pub(crate) fn resolve_media_path(media_item: &MediaItem) -> PathBuf {
    resolve_relative_path(&media_item.root_path, &media_item.relative_path)
}

fn resolve_relative_path(root_path: &str, relative_path: &str) -> PathBuf {
    let mut path = PathBuf::from(root_path);

    for component in Path::new(relative_path).components() {
        let value = component.as_os_str().to_string_lossy();

        if value.is_empty() || value == "." || value == ".." {
            continue;
        }

        path.push(component.as_os_str());
    }

    path
}

pub(crate) fn prepare_webvtt<'a>(bytes: &'a [u8], extension: &str) -> Cow<'a, str> {
    let text = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(&bytes[3..])
    } else {
        String::from_utf8_lossy(bytes)
    };

    if extension.eq_ignore_ascii_case("srt") {
        Cow::Owned(convert_srt_to_vtt(&text))
    } else {
        text
    }
}

pub(crate) fn convert_srt_to_vtt(input: &str) -> String {
    let mut converted = String::from("WEBVTT\n\n");

    for line in input.lines() {
        if line.contains("-->") {
            converted.push_str(&line.replace(',', "."));
        } else {
            converted.push_str(line);
        }
        converted.push('\n');
    }

    converted
}

pub(crate) fn parse_range_header(
    header_value: &str,
    file_len: u64,
) -> Result<ByteRange, StreamMediaError> {
    let value = header_value.trim();

    if !value.starts_with("bytes=") {
        return Err(StreamMediaError::MalformedRange(
            "Range header must start with 'bytes='.",
        ));
    }

    let range_value = &value["bytes=".len()..];

    if range_value.contains(',') {
        return Err(StreamMediaError::MalformedRange(
            "Only a single byte range is supported.",
        ));
    }

    if file_len == 0 {
        return Err(StreamMediaError::UnsatisfiableRange { file_len });
    }

    let (start, end) = range_value
        .split_once('-')
        .ok_or(StreamMediaError::MalformedRange(
            "Range header must use the form 'bytes=start-end'.",
        ))?;

    if start.is_empty() {
        let suffix_len = end.parse::<u64>().map_err(|_| {
            StreamMediaError::MalformedRange("Range suffix must be a valid non-negative integer.")
        })?;

        if suffix_len == 0 {
            return Err(StreamMediaError::UnsatisfiableRange { file_len });
        }

        let effective_len = suffix_len.min(file_len);
        return Ok(ByteRange {
            start: file_len - effective_len,
            end: file_len - 1,
        });
    }

    let start = start.parse::<u64>().map_err(|_| {
        StreamMediaError::MalformedRange("Range start must be a valid non-negative integer.")
    })?;

    if start >= file_len {
        return Err(StreamMediaError::UnsatisfiableRange { file_len });
    }

    let end = if end.is_empty() {
        file_len - 1
    } else {
        end.parse::<u64>()
            .map_err(|_| {
                StreamMediaError::MalformedRange("Range end must be a valid non-negative integer.")
            })?
            .min(file_len - 1)
    };

    if start > end {
        return Err(StreamMediaError::UnsatisfiableRange { file_len });
    }

    Ok(ByteRange { start, end })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_range_header_supports_open_ended_ranges() {
        let range = parse_range_header("bytes=5-", 10).unwrap();

        assert_eq!(range, ByteRange { start: 5, end: 9 });
    }

    #[test]
    fn parse_range_header_supports_suffix_ranges() {
        let range = parse_range_header("bytes=-4", 10).unwrap();

        assert_eq!(range, ByteRange { start: 6, end: 9 });
    }

    #[test]
    fn parse_range_header_rejects_multiple_ranges() {
        let error = parse_range_header("bytes=0-1,3-4", 10).unwrap_err();

        assert!(matches!(
            error,
            StreamMediaError::MalformedRange("Only a single byte range is supported.")
        ));
    }

    #[test]
    fn parse_range_header_rejects_unsatisfiable_range() {
        let error = parse_range_header("bytes=99-100", 10).unwrap_err();

        assert!(matches!(
            error,
            StreamMediaError::UnsatisfiableRange { file_len: 10 }
        ));
    }

    #[test]
    fn convert_srt_to_vtt_adds_header_and_normalizes_timecodes() {
        let converted = convert_srt_to_vtt("1\n00:00:01,250 --> 00:00:04,500\nHello\n");

        assert_eq!(
            converted,
            "WEBVTT\n\n1\n00:00:01.250 --> 00:00:04.500\nHello\n"
        );
    }
}
