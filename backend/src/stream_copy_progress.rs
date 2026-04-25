use std::{collections::HashMap, sync::Arc};

use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    sync::RwLock,
};
use tracing::warn;
use uuid::Uuid;

pub(crate) type SharedStreamCopyProgress = Arc<RwLock<HashMap<Uuid, StreamCopyProgressSnapshot>>>;

pub(crate) fn new_shared_stream_copy_progress() -> SharedStreamCopyProgress {
    Arc::new(RwLock::new(HashMap::new()))
}

#[derive(Clone, Debug, Default)]
pub(crate) struct StreamCopyProgressSnapshot {
    pub(crate) out_time_micros: Option<u64>,
    pub(crate) speed: Option<f32>,
}

#[derive(Debug, Default)]
struct FfmpegProgressBlock {
    out_time_micros: Option<u64>,
    speed: Option<f32>,
}

pub(crate) async fn read_ffmpeg_progress(
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

pub(crate) async fn read_ffmpeg_stderr(
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

pub(crate) async fn clear_progress(progress: &SharedStreamCopyProgress, media_id: Uuid) {
    let mut progress_map = progress.write().await;
    progress_map.remove(&media_id);
}

pub(crate) fn progress_ratio_from_snapshot(
    duration_seconds: Option<f64>,
    out_time_micros: Option<u64>,
) -> Option<f32> {
    let duration_seconds = duration_seconds.filter(|value| *value > 0.0)?;
    let out_time_seconds = out_time_micros? as f64 / 1_000_000.0;
    Some((out_time_seconds / duration_seconds).clamp(0.0, 1.0) as f32)
}
