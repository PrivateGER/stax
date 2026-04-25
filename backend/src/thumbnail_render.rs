use std::{
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use time::OffsetDateTime;
use tokio::process::Command;
use tracing::{debug, warn};

use crate::{
    clock::format_timestamp,
    ffmpeg::{FfmpegHardwareAcceleration, apply_input_acceleration},
    thumbnails::{ThumbnailConfig, ThumbnailJob, thumbnail_is_up_to_date, thumbnail_path_for},
};

const THUMBNAIL_WIDTH: u32 = 480;
const SIDECAR_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];
const SIDECAR_BASENAMES: &[&str] = &["poster", "cover", "folder"];

pub(crate) enum ThumbnailOutcome {
    Generated {
        timestamp: String,
        source: ThumbnailSource,
    },
    Skipped,
    Failed(String),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ThumbnailSource {
    Cached,
    Sidecar,
    AttachedPic,
    Frame,
}

impl ThumbnailSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ThumbnailSource::Cached => "cached",
            ThumbnailSource::Sidecar => "sidecar",
            ThumbnailSource::AttachedPic => "attached_pic",
            ThumbnailSource::Frame => "frame",
        }
    }
}

fn now_timestamp() -> String {
    format_timestamp(OffsetDateTime::now_utc())
}

pub(crate) async fn generate(job: &ThumbnailJob, config: &ThumbnailConfig) -> ThumbnailOutcome {
    let Some(cache_dir) = config.cache_dir.as_deref() else {
        debug!(media_id = %job.media_id, "no cache_dir configured; skipping");
        return ThumbnailOutcome::Skipped;
    };

    let output_path = thumbnail_path_for(cache_dir, job.media_id);

    if let Ok(metadata) = fs::metadata(&job.media_path) {
        let modified = metadata.modified().ok();
        if thumbnail_is_up_to_date(&output_path, modified) {
            debug!(media_id = %job.media_id, "cached thumbnail is fresh; skipping ffmpeg");
            return ThumbnailOutcome::Generated {
                timestamp: now_timestamp(),
                source: ThumbnailSource::Cached,
            };
        }
    }

    if let Err(error) = fs::create_dir_all(cache_dir) {
        return ThumbnailOutcome::Failed(format!(
            "could not create thumbnail directory '{}': {error}",
            cache_dir.display()
        ));
    }

    if let Some(sidecar) = find_sidecar_art(&job.media_path)
        && let Some(ffmpeg) = config.ffmpeg_command.as_deref()
    {
        debug!(
            media_id = %job.media_id,
            sidecar = %sidecar.display(),
            "trying sidecar art source"
        );
        let started = Instant::now();
        match render_from_image(ffmpeg, &sidecar, &output_path).await {
            Ok(()) => {
                debug!(
                    media_id = %job.media_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "sidecar art rendered"
                );
                return ThumbnailOutcome::Generated {
                    timestamp: now_timestamp(),
                    source: ThumbnailSource::Sidecar,
                };
            }
            Err(error) => {
                warn!(
                    media_id = %job.media_id,
                    sidecar = %sidecar.display(),
                    %error,
                    "sidecar art conversion failed; falling through to other sources"
                );
            }
        }
    }

    let Some(ffmpeg) = config.ffmpeg_command.as_deref() else {
        debug!(media_id = %job.media_id, "no ffmpeg configured; skipping");
        return ThumbnailOutcome::Skipped;
    };

    debug!(media_id = %job.media_id, "trying embedded attached_pic source");
    let started = Instant::now();
    match render_from_attached_pic(ffmpeg, &job.media_path, &output_path).await {
        Ok(()) => {
            debug!(
                media_id = %job.media_id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "embedded attached_pic rendered"
            );
            return ThumbnailOutcome::Generated {
                timestamp: now_timestamp(),
                source: ThumbnailSource::AttachedPic,
            };
        }
        Err(error) => {
            if is_missing_attached_pic_error(&error) {
                debug!(
                    media_id = %job.media_id,
                    "no embedded attached_pic; falling through to frame extraction"
                );
            } else {
                debug!(
                    media_id = %job.media_id,
                    %error,
                    "embedded attached_pic extraction failed; falling through to frame extraction"
                );
            }
        }
    }

    if job.video_codec.is_none() {
        debug!(media_id = %job.media_id, "no video stream; skipping frame extraction");
        return ThumbnailOutcome::Skipped;
    }

    debug!(
        media_id = %job.media_id,
        duration_seconds = ?job.duration_seconds,
        "trying frame extraction source"
    );
    let started = Instant::now();
    match render_from_frame(
        ffmpeg,
        &job.media_path,
        &output_path,
        job.duration_seconds,
        &config.hw_accel,
    )
    .await
    {
        Ok(()) => {
            debug!(
                media_id = %job.media_id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "frame extraction rendered"
            );
            ThumbnailOutcome::Generated {
                timestamp: now_timestamp(),
                source: ThumbnailSource::Frame,
            }
        }
        Err(error) => ThumbnailOutcome::Failed(error),
    }
}

async fn render_from_image(ffmpeg: &Path, source: &Path, output: &Path) -> Result<(), String> {
    let result = Command::new(ffmpeg)
        .arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(source)
        .arg("-vf")
        .arg(format!("scale={THUMBNAIL_WIDTH}:-2"))
        .arg("-frames:v")
        .arg("1")
        .arg("-q:v")
        .arg("4")
        .arg(output)
        .output()
        .await;

    classify_ffmpeg_result(result, output)
}

async fn render_from_attached_pic(
    ffmpeg: &Path,
    media_path: &Path,
    output: &Path,
) -> Result<(), String> {
    let result = Command::new(ffmpeg)
        .arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(media_path)
        .arg("-map")
        .arg("0:v:disp:attached_pic")
        .arg("-vf")
        .arg(format!("scale={THUMBNAIL_WIDTH}:-2"))
        .arg("-frames:v")
        .arg("1")
        .arg("-q:v")
        .arg("4")
        .arg(output)
        .output()
        .await;

    classify_ffmpeg_result(result, output)
}

async fn render_from_frame(
    ffmpeg: &Path,
    media_path: &Path,
    output: &Path,
    duration_seconds: Option<f64>,
    hw_accel: &FfmpegHardwareAcceleration,
) -> Result<(), String> {
    let seek = thumbnail_seek_seconds(duration_seconds);
    let mut command = Command::new(ffmpeg);
    command.arg("-y").arg("-loglevel").arg("error");
    apply_input_acceleration(&mut command, hw_accel);
    let result = command
        .arg("-ss")
        .arg(format!("{seek:.3}"))
        .arg("-i")
        .arg(media_path)
        .arg("-vf")
        .arg(format!("thumbnail=100,scale={THUMBNAIL_WIDTH}:-2"))
        .arg("-frames:v")
        .arg("1")
        .arg("-q:v")
        .arg("4")
        .arg(output)
        .output()
        .await;

    classify_ffmpeg_result(result, output)
}

fn classify_ffmpeg_result(
    result: std::io::Result<std::process::Output>,
    output: &Path,
) -> Result<(), String> {
    match result {
        Ok(output_data) if output_data.status.success() => {
            if output.exists() && fs::metadata(output).map(|m| m.len() > 0).unwrap_or(false) {
                Ok(())
            } else {
                let _ = fs::remove_file(output);
                Err("ffmpeg reported success but no thumbnail file was produced".into())
            }
        }
        Ok(output_data) => {
            let stderr = String::from_utf8_lossy(&output_data.stderr)
                .trim()
                .to_string();
            let _ = fs::remove_file(output);
            if stderr.is_empty() {
                Err(format!("ffmpeg exited with status {}", output_data.status))
            } else {
                Err(format!("ffmpeg failed: {stderr}"))
            }
        }
        Err(error) => Err(format!("ffmpeg could not start: {error}")),
    }
}

pub(crate) fn is_missing_attached_pic_error(error: &str) -> bool {
    error.contains("matches no streams")
}

pub(crate) fn find_sidecar_art(media_path: &Path) -> Option<PathBuf> {
    let parent = media_path.parent()?;
    let stem = media_path.file_stem()?.to_string_lossy().to_string();

    for extension in SIDECAR_EXTENSIONS {
        for suffix in ["-poster", ""] {
            let candidate = parent.join(format!("{stem}{suffix}.{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    for basename in SIDECAR_BASENAMES {
        for extension in SIDECAR_EXTENSIONS {
            let candidate = parent.join(format!("{basename}.{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

pub(crate) fn thumbnail_seek_seconds(duration_seconds: Option<f64>) -> f64 {
    duration_seconds
        .filter(|value| *value > 0.0)
        .map(|value| value * 0.2)
        .unwrap_or(0.0)
}
