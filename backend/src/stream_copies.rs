use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Instant,
};

use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
    sync::{RwLock, Semaphore, mpsc},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    clock::format_timestamp,
    ffmpeg::{FfmpegHardwareAcceleration, apply_input_acceleration},
    library::{LibraryService, is_browser_safe_audio_codec_for_mp4, is_browser_safe_video_codec},
    persistence::{PendingStreamCopy, Persistence, StreamCopyRecord, stream_copy_summary_for},
    protocol::{
        MediaItem, StreamCopyStatus, StreamCopySummary, SubtitleMode, SubtitleSourceKind,
        SubtitleTrack, is_text_subtitle_codec,
    },
    streaming::convert_srt_to_vtt,
};

const DEFAULT_STREAM_COPY_CACHE_DIR: &str = "stax-stream-copies";
const DEFAULT_WORKERS: usize = 1;

type SharedStreamCopyProgress = Arc<RwLock<HashMap<Uuid, StreamCopyProgressSnapshot>>>;

#[derive(Clone, Debug, Default)]
struct StreamCopyProgressSnapshot {
    out_time_micros: Option<u64>,
    speed: Option<f32>,
}

#[derive(Debug, Default)]
struct FfmpegProgressBlock {
    out_time_micros: Option<u64>,
    speed: Option<f32>,
}

#[derive(Clone, Debug)]
pub struct StreamCopyConfig {
    pub cache_dir: Option<PathBuf>,
    pub ffmpeg_command: Option<PathBuf>,
    pub hw_accel: FfmpegHardwareAcceleration,
    pub max_concurrent: usize,
}

impl Default for StreamCopyConfig {
    fn default() -> Self {
        Self {
            cache_dir: Some(default_stream_copy_cache_dir()),
            ffmpeg_command: Some(PathBuf::from("ffmpeg")),
            hw_accel: FfmpegHardwareAcceleration::None,
            max_concurrent: DEFAULT_WORKERS,
        }
    }
}

#[derive(Clone, Debug)]
pub struct StreamCopyJob {
    pub media_id: Uuid,
}

impl StreamCopyJob {
    pub fn from_pending(pending: PendingStreamCopy) -> Self {
        Self {
            media_id: pending.media_id,
        }
    }
}

#[derive(Clone)]
pub struct StreamCopyWorkerPool {
    sender: mpsc::UnboundedSender<StreamCopyJob>,
    progress: SharedStreamCopyProgress,
}

impl StreamCopyWorkerPool {
    pub fn spawn(
        config: StreamCopyConfig,
        persistence: Persistence,
        library: LibraryService,
    ) -> Self {
        let (sender, mut receiver) = mpsc::unbounded_channel::<StreamCopyJob>();
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent.max(1)));
        let config = Arc::new(config);
        let progress = Arc::new(RwLock::new(HashMap::new()));
        let worker_progress = Arc::clone(&progress);
        info!(
            workers = config.max_concurrent,
            ffmpeg = ?config.ffmpeg_command,
            hw_accel = ?config.hw_accel,
            cache_dir = ?config.cache_dir,
            "stream copy worker pool starting"
        );

        tokio::spawn(async move {
            while let Some(job) = receiver.recv().await {
                let permit = match Arc::clone(&semaphore).acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => return,
                };
                let config = Arc::clone(&config);
                let persistence = persistence.clone();
                let library = library.clone();
                let progress = Arc::clone(&worker_progress);

                tokio::spawn(async move {
                    process_job(job, &config, &persistence, &library, &progress).await;
                    drop(permit);
                });
            }
        });

        Self { sender, progress }
    }

    pub fn enqueue(&self, job: StreamCopyJob) {
        let media_id = job.media_id;
        if let Err(error) = self.sender.send(job) {
            warn!(
                media_id = %error.0.media_id,
                "stream copy worker pool is closed; dropping job"
            );
        } else {
            debug!(%media_id, "stream copy job enqueued");
        }
    }

    pub fn enqueue_pending(&self, pending: Vec<PendingStreamCopy>) {
        for entry in pending {
            self.enqueue(StreamCopyJob::from_pending(entry));
        }
    }

    pub async fn summary_for(
        &self,
        media_id: Uuid,
        duration_seconds: Option<f64>,
        record: &StreamCopyRecord,
    ) -> StreamCopySummary {
        let mut summary = stream_copy_summary_for(media_id, record);
        if record.status == StreamCopyStatus::Running
            && let Some(progress) = self.progress_snapshot(media_id).await
        {
            summary.progress_ratio =
                progress_ratio_from_snapshot(duration_seconds, progress.out_time_micros);
            summary.progress_speed = progress.speed;
        }
        summary
    }

    async fn progress_snapshot(&self, media_id: Uuid) -> Option<StreamCopyProgressSnapshot> {
        let progress = self.progress.read().await;
        progress.get(&media_id).cloned()
    }
}

async fn process_job(
    job: StreamCopyJob,
    config: &StreamCopyConfig,
    persistence: &Persistence,
    library: &LibraryService,
    progress: &SharedStreamCopyProgress,
) {
    let media_id = job.media_id;
    clear_progress(progress, media_id).await;

    let Some(ffmpeg_command) = config.ffmpeg_command.as_deref() else {
        mark_stream_copy_failed(persistence, media_id, "ffmpeg is not configured.").await;
        return;
    };
    let Some(cache_dir) = config.cache_dir.as_deref() else {
        mark_stream_copy_failed(
            persistence,
            media_id,
            "Stream copy cache directory is not configured.",
        )
        .await;
        return;
    };

    let now = format_timestamp(time::OffsetDateTime::now_utc());
    if let Err(error) = persistence.mark_stream_copy_running(media_id, &now).await {
        warn!(%error, %media_id, "failed to mark stream copy running");
        return;
    }

    let media_item = match library.media_item(media_id).await {
        Ok(Some(item)) => item,
        Ok(None) => {
            mark_stream_copy_failed(persistence, media_id, "Media item no longer exists.").await;
            return;
        }
        Err(error) => {
            warn!(%error, %media_id, "failed to load media item for stream copy");
            mark_stream_copy_failed(persistence, media_id, "Failed to load media item.").await;
            return;
        }
    };
    let copy = match persistence.find_stream_copy(media_id).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            warn!(%media_id, "stream copy request disappeared before the worker started");
            return;
        }
        Err(error) => {
            warn!(%error, %media_id, "failed to load stream copy row");
            mark_stream_copy_failed(persistence, media_id, "Failed to load stream copy request.")
                .await;
            return;
        }
    };

    let started = Instant::now();
    match build_stream_copy(
        &media_item,
        &copy,
        cache_dir,
        ffmpeg_command,
        &config.hw_accel,
        progress,
    )
    .await
    {
        Ok(outcome) => {
            let now = format_timestamp(time::OffsetDateTime::now_utc());
            clear_progress(progress, media_id).await;
            if let Err(error) = persistence
                .mark_stream_copy_ready(
                    media_id,
                    &outcome.output_path,
                    outcome.content_type,
                    outcome.subtitle_path.as_deref(),
                    &now,
                )
                .await
            {
                warn!(%error, %media_id, "failed to persist ready stream copy");
                return;
            }
            info!(
                %media_id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "stream copy ready"
            );
        }
        Err(error_message) => {
            warn!(%media_id, error = %error_message, "stream copy failed");
            clear_progress(progress, media_id).await;
            mark_stream_copy_failed(persistence, media_id, &error_message).await;
        }
    }
}

async fn mark_stream_copy_failed(persistence: &Persistence, media_id: Uuid, message: &str) {
    let now = format_timestamp(time::OffsetDateTime::now_utc());
    if let Err(error) = persistence
        .mark_stream_copy_failed(media_id, message, &now)
        .await
    {
        warn!(%error, %media_id, "failed to persist stream copy failure");
    }
}

struct StreamCopyOutcome {
    output_path: String,
    subtitle_path: Option<String>,
    content_type: &'static str,
}

struct FfmpegStreamCopyRequest<'a> {
    ffmpeg_command: &'a Path,
    media_item: &'a MediaItem,
    media_path: &'a Path,
    output_path: &'a Path,
    default_audio_ordinal: Option<usize>,
    burned_filter: Option<&'a str>,
    hw_accel: &'a FfmpegHardwareAcceleration,
}

async fn build_stream_copy(
    media_item: &MediaItem,
    copy: &StreamCopyRecord,
    cache_dir: &Path,
    ffmpeg_command: &Path,
    hw_accel: &FfmpegHardwareAcceleration,
    progress: &SharedStreamCopyProgress,
) -> Result<StreamCopyOutcome, String> {
    let media_path = crate::streaming::resolve_media_path(media_item);
    let output_dir = stream_copy_output_dir(cache_dir, media_item.id);
    fs::create_dir_all(&output_dir)
        .await
        .map_err(|error| format!("failed to create stream copy directory: {error}"))?;

    let has_video = media_item.video_codec.is_some();
    let (output_name, content_type) = if has_video {
        ("stream.mp4", "video/mp4")
    } else {
        ("stream.m4a", "audio/mp4")
    };
    let final_output_path = output_dir.join(output_name);
    let temp_output_path = temp_path_for_final(&final_output_path);
    let final_subtitle_path = output_dir.join("subtitle.vtt");
    let temp_subtitle_path = temp_path_for_final(&final_subtitle_path);

    let result = async {
        cleanup_path(&temp_output_path).await;
        cleanup_path(&temp_subtitle_path).await;

        let default_audio_ordinal = default_audio_stream_ordinal(media_item, copy)?;
        let burned_filter = if copy.subtitle_mode == SubtitleMode::Burned {
            Some(build_burn_subtitle_filter(media_item, copy, &media_path)?)
        } else {
            None
        };

        let request = FfmpegStreamCopyRequest {
            ffmpeg_command,
            media_item,
            media_path: &media_path,
            output_path: &temp_output_path,
            default_audio_ordinal,
            burned_filter: burned_filter.as_deref(),
            hw_accel,
        };
        run_ffmpeg_stream_copy(request, progress).await?;

        cleanup_path(&final_output_path).await;
        fs::rename(&temp_output_path, &final_output_path)
            .await
            .map_err(|error| format!("failed to finalize stream copy: {error}"))?;

        let subtitle_path = if copy.subtitle_mode == SubtitleMode::Sidecar {
            prepare_sidecar_subtitle(
                ffmpeg_command,
                media_item,
                copy,
                &media_path,
                &temp_subtitle_path,
                &final_subtitle_path,
            )
            .await?
        } else {
            cleanup_path(&final_subtitle_path).await;
            None
        };

        Ok(StreamCopyOutcome {
            output_path: final_output_path.to_string_lossy().to_string(),
            subtitle_path: subtitle_path.map(|path| path.to_string_lossy().to_string()),
            content_type,
        })
    }
    .await;

    if result.is_err() {
        cleanup_path(&temp_output_path).await;
        cleanup_path(&temp_subtitle_path).await;
        cleanup_path(&final_output_path).await;
        cleanup_path(&final_subtitle_path).await;
    }

    result
}

async fn prepare_sidecar_subtitle(
    ffmpeg_command: &Path,
    media_item: &MediaItem,
    copy: &StreamCopyRecord,
    media_path: &Path,
    temp_subtitle_path: &Path,
    final_subtitle_path: &Path,
) -> Result<Option<PathBuf>, String> {
    let Some((kind, index)) = copy.subtitle_kind.zip(copy.subtitle_index) else {
        return Err("A subtitle source is required for sidecar subtitles.".to_string());
    };

    cleanup_path(temp_subtitle_path).await;
    cleanup_path(final_subtitle_path).await;

    match kind {
        SubtitleSourceKind::Sidecar => {
            let track = media_item
                .subtitle_tracks
                .get(index as usize)
                .ok_or_else(|| "Selected sidecar subtitle track does not exist.".to_string())?;
            let subtitle_path = resolve_sidecar_subtitle_path(media_item, track);
            let bytes = fs::read(&subtitle_path)
                .await
                .map_err(|error| format!("failed to read subtitle track: {error}"))?;
            let prepared = match track.extension.to_ascii_lowercase().as_str() {
                "vtt" => String::from_utf8_lossy(&bytes).into_owned(),
                "srt" => convert_srt_to_vtt(&String::from_utf8_lossy(&bytes)),
                _ => {
                    return Err(format!(
                        "Subtitle sidecar format '{}' cannot be converted to WebVTT.",
                        track.extension
                    ));
                }
            };
            fs::write(temp_subtitle_path, prepared)
                .await
                .map_err(|error| format!("failed to write prepared subtitle: {error}"))?;
        }
        SubtitleSourceKind::Embedded => {
            let stream = media_item
                .subtitle_streams
                .iter()
                .find(|stream| stream.index == index)
                .ok_or_else(|| "Selected embedded subtitle stream does not exist.".to_string())?;
            if !is_text_subtitle_codec(stream.codec.as_deref()) {
                return Err(
                    "Selected embedded subtitle stream cannot be converted to WebVTT.".to_string(),
                );
            }
            let output = Command::new(ffmpeg_command)
                .arg("-y")
                .arg("-loglevel")
                .arg("error")
                .arg("-nostdin")
                .arg("-i")
                .arg(media_path)
                .arg("-map")
                .arg(format!("0:{index}"))
                .arg("-f")
                .arg("webvtt")
                .arg(temp_subtitle_path)
                .output()
                .await
                .map_err(|error| format!("failed to start ffmpeg subtitle extract: {error}"))?;
            if !output.status.success() {
                return Err(ffmpeg_error("subtitle extraction failed", &output.stderr));
            }
        }
    }

    fs::rename(temp_subtitle_path, final_subtitle_path)
        .await
        .map_err(|error| format!("failed to finalize prepared subtitle: {error}"))?;

    Ok(Some(final_subtitle_path.to_path_buf()))
}

async fn run_ffmpeg_stream_copy(
    request: FfmpegStreamCopyRequest<'_>,
    progress: &SharedStreamCopyProgress,
) -> Result<(), String> {
    let FfmpegStreamCopyRequest {
        ffmpeg_command,
        media_item,
        media_path,
        output_path,
        default_audio_ordinal,
        burned_filter,
        hw_accel,
    } = request;
    let has_video = media_item.video_codec.is_some();
    let browser_safe_video = media_item
        .video_codec
        .as_deref()
        .map(|codec| {
            is_browser_safe_video_codec(
                codec,
                media_item.video_profile.as_deref(),
                media_item.video_pix_fmt.as_deref(),
                media_item.video_level,
                media_item.video_bit_depth,
            )
        })
        .unwrap_or(false);
    let transcodes_video = has_video && !(browser_safe_video && burned_filter.is_none());
    let can_use_input_hwaccel = should_use_input_hwaccel(transcodes_video, burned_filter);
    let mut command = Command::new(ffmpeg_command);
    command
        .arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostats")
        .arg("-progress")
        .arg("pipe:1")
        .arg("-nostdin")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if can_use_input_hwaccel {
        apply_input_acceleration(&mut command, hw_accel);
    }
    command.arg("-i").arg(media_path);

    if has_video {
        command.arg("-map").arg("0:v:0?");
    } else {
        command.arg("-vn");
    }

    if media_item.audio_streams.is_empty() {
        command.arg("-an");
    } else {
        // Place the default track first so MediaBunny's getPrimaryAudioTrack
        // lands on the user's selection, then append the rest in source order.
        let mut output_order: Vec<usize> = Vec::with_capacity(media_item.audio_streams.len());
        if let Some(ordinal) = default_audio_ordinal {
            output_order.push(ordinal);
        }
        for ordinal in 0..media_item.audio_streams.len() {
            if Some(ordinal) != default_audio_ordinal {
                output_order.push(ordinal);
            }
        }

        for (output_audio_index, source_ordinal) in output_order.into_iter().enumerate() {
            let stream = &media_item.audio_streams[source_ordinal];
            command.arg("-map").arg(format!("0:{}", stream.index));
            let can_copy = stream
                .codec
                .as_deref()
                .map(is_browser_safe_audio_codec_for_mp4)
                .unwrap_or(false);
            if can_copy {
                command
                    .arg(format!("-c:a:{output_audio_index}"))
                    .arg("copy");
            } else {
                command.arg(format!("-c:a:{output_audio_index}")).arg("aac");
                command
                    .arg(format!("-b:a:{output_audio_index}"))
                    .arg("192k");
            }
        }
    }

    if has_video {
        if !transcodes_video {
            command.arg("-c:v").arg("copy");
        } else {
            command.arg("-c:v").arg(hw_accel.h264_encoder());
            if hw_accel.uses_software_h264_encoder() {
                command
                    .arg("-preset")
                    .arg("veryfast")
                    .arg("-pix_fmt")
                    .arg("yuv420p");
            }
            if let Some(filter) = hw_accel.h264_filter(burned_filter) {
                command.arg("-vf").arg(filter);
            }
        }
    }

    command
        .arg("-f")
        .arg("mp4")
        .arg("-movflags")
        .arg("+faststart")
        .arg(output_path);

    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start ffmpeg stream copy: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture ffmpeg progress output.".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture ffmpeg error output.".to_string())?;
    let media_id = media_item.id;
    let progress_reader =
        tokio::spawn(read_ffmpeg_progress(media_id, Arc::clone(progress), stdout));
    let stderr_reader = tokio::spawn(read_ffmpeg_stderr(stderr));

    let status = child
        .wait()
        .await
        .map_err(|error| format!("failed to wait for ffmpeg stream copy: {error}"))?;

    if let Err(error) = progress_reader.await {
        warn!(%media_id, %error, "ffmpeg progress reader task failed");
    }

    let stderr = match stderr_reader.await {
        Ok(Ok(stderr)) => stderr,
        Ok(Err(error)) => {
            warn!(%media_id, %error, "failed to read ffmpeg stderr");
            Vec::new()
        }
        Err(error) => {
            warn!(%media_id, %error, "ffmpeg stderr reader task failed");
            Vec::new()
        }
    };

    if !status.success() {
        return Err(ffmpeg_error("stream copy failed", &stderr));
    }

    Ok(())
}

fn should_use_input_hwaccel(transcodes_video: bool, burned_filter: Option<&str>) -> bool {
    transcodes_video && burned_filter.is_none()
}

async fn read_ffmpeg_progress(
    media_id: Uuid,
    progress: SharedStreamCopyProgress,
    stdout: tokio::process::ChildStdout,
) {
    let mut lines = BufReader::new(stdout).lines();
    let mut block = FfmpegProgressBlock::default();

    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                update_progress_block(&mut block, &line);
                if let Some(marker) = line.strip_prefix("progress=") {
                    write_progress_snapshot(&progress, media_id, &block).await;
                    if marker == "end" {
                        break;
                    }
                    block = FfmpegProgressBlock::default();
                }
            }
            Ok(None) => break,
            Err(error) => {
                warn!(%media_id, %error, "failed to read ffmpeg progress output");
                break;
            }
        }
    }
}

async fn read_ffmpeg_stderr(
    mut stderr: tokio::process::ChildStderr,
) -> Result<Vec<u8>, std::io::Error> {
    let mut bytes = Vec::new();
    stderr.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

fn update_progress_block(block: &mut FfmpegProgressBlock, line: &str) {
    let Some((key, value)) = line.split_once('=') else {
        return;
    };

    match key {
        // Despite the key name, ffmpeg emits this value in microseconds.
        "out_time_ms" => {
            block.out_time_micros = value.parse::<u64>().ok();
        }
        "speed" => {
            block.speed = parse_ffmpeg_speed(value);
        }
        _ => {}
    }
}

fn parse_ffmpeg_speed(value: &str) -> Option<f32> {
    value
        .trim()
        .strip_suffix('x')
        .and_then(|value| value.parse::<f32>().ok())
}

async fn write_progress_snapshot(
    progress: &SharedStreamCopyProgress,
    media_id: Uuid,
    block: &FfmpegProgressBlock,
) {
    if block.out_time_micros.is_none() && block.speed.is_none() {
        return;
    }

    let mut progress_map = progress.write().await;
    progress_map.insert(
        media_id,
        StreamCopyProgressSnapshot {
            out_time_micros: block.out_time_micros,
            speed: block.speed,
        },
    );
}

async fn clear_progress(progress: &SharedStreamCopyProgress, media_id: Uuid) {
    let mut progress_map = progress.write().await;
    progress_map.remove(&media_id);
}

fn progress_ratio_from_snapshot(
    duration_seconds: Option<f64>,
    out_time_micros: Option<u64>,
) -> Option<f32> {
    let duration_seconds = duration_seconds.filter(|value| *value > 0.0)?;
    let out_time_seconds = out_time_micros? as f64 / 1_000_000.0;
    Some((out_time_seconds / duration_seconds).clamp(0.0, 1.0) as f32)
}

/// Returns the position within `media_item.audio_streams` of the track that
/// should be surfaced as the default in the prepared file. All tracks are
/// preserved; this ordinal just decides which one lands at output audio:0.
fn default_audio_stream_ordinal(
    media_item: &MediaItem,
    copy: &StreamCopyRecord,
) -> Result<Option<usize>, String> {
    if media_item.audio_streams.is_empty() {
        return Ok(None);
    }

    match copy.audio_stream_index {
        Some(index) => media_item
            .audio_streams
            .iter()
            .position(|stream| stream.index == index)
            .map(Some)
            .ok_or_else(|| "Selected audio stream does not exist.".to_string()),
        None => Ok(Some(
            media_item
                .audio_streams
                .iter()
                .position(|stream| stream.default)
                .unwrap_or(0),
        )),
    }
}

fn build_burn_subtitle_filter(
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

fn resolve_sidecar_subtitle_path(media_item: &MediaItem, track: &SubtitleTrack) -> PathBuf {
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

fn ffmpeg_error(prefix: &str, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let message = stderr.trim();
    if message.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}: {message}")
    }
}

async fn cleanup_path(path: &Path) {
    match fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => warn!(path = %path.display(), %error, "failed to remove stale file"),
    }
}

pub fn default_stream_copy_cache_dir() -> PathBuf {
    PathBuf::from(DEFAULT_STREAM_COPY_CACHE_DIR)
}

pub fn stream_copy_output_dir(cache_dir: &Path, media_id: Uuid) -> PathBuf {
    cache_dir.join(media_id.to_string())
}

fn temp_path_for_final(final_path: &Path) -> PathBuf {
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
    use super::{escape_subtitles_path, should_use_input_hwaccel, temp_path_for_final};
    use std::path::Path;

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

    #[test]
    fn burned_subtitles_do_not_use_input_hwaccel() {
        let burned_filter = Some("subtitles=/tmp/movie.mkv:si=1");

        assert!(!should_use_input_hwaccel(true, burned_filter));
    }

    #[test]
    fn stream_copies_without_burn_in_can_use_input_hwaccel() {
        let burned_filter = Some("subtitles=/tmp/movie.mkv:si=1");

        assert!(!should_use_input_hwaccel(true, burned_filter));
        assert!(should_use_input_hwaccel(true, None));
    }
}
