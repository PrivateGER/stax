use std::{env, path::PathBuf};

use tokio::process::Command;
use tracing::warn;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FfmpegHardwareAcceleration {
    #[default]
    None,
    Auto,
    Nvenc,
    Vaapi {
        device: PathBuf,
    },
    Qsv,
    Videotoolbox,
}

impl FfmpegHardwareAcceleration {
    pub fn from_env() -> Self {
        let Some(raw) = env::var("SYNCPLAY_HW_ACCEL").ok() else {
            return Self::None;
        };

        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "none" | "off" | "false" => Self::None,
            "auto" => Self::Auto,
            "nvenc" | "cuda" => Self::Nvenc,
            "vaapi" => Self::Vaapi {
                device: vaapi_device_from_env(),
            },
            "qsv" => Self::Qsv,
            "videotoolbox" => Self::Videotoolbox,
            unknown => {
                warn!(
                    value = unknown,
                    "unknown SYNCPLAY_HW_ACCEL value; falling back to software"
                );
                Self::None
            }
        }
    }

    pub fn h264_encoder(&self) -> &'static str {
        match self {
            Self::None | Self::Auto => "libx264",
            Self::Nvenc => "h264_nvenc",
            Self::Vaapi { .. } => "h264_vaapi",
            Self::Qsv => "h264_qsv",
            Self::Videotoolbox => "h264_videotoolbox",
        }
    }

    pub fn h264_filter(&self, base_filter: Option<&str>) -> Option<String> {
        match self {
            Self::Nvenc => base_filter
                .map(|filter| format!("hwdownload,format=nv12,{filter},format=nv12,hwupload_cuda")),
            Self::Vaapi { .. } => Some(match base_filter {
                Some(filter) => format!("{filter},format=nv12,hwupload"),
                None => "format=nv12,hwupload".to_string(),
            }),
            _ => base_filter.map(str::to_string),
        }
    }

    pub fn uses_software_h264_encoder(&self) -> bool {
        matches!(self, Self::None | Self::Auto)
    }
}

pub fn apply_input_acceleration(command: &mut Command, hw_accel: &FfmpegHardwareAcceleration) {
    match hw_accel {
        FfmpegHardwareAcceleration::None => {}
        FfmpegHardwareAcceleration::Auto => {
            command.arg("-hwaccel").arg("auto");
        }
        FfmpegHardwareAcceleration::Nvenc => {
            command.arg("-hwaccel").arg("cuda");
        }
        FfmpegHardwareAcceleration::Vaapi { device } => {
            command
                .arg("-vaapi_device")
                .arg(device)
                .arg("-hwaccel")
                .arg("vaapi");
        }
        FfmpegHardwareAcceleration::Qsv => {
            command.arg("-hwaccel").arg("qsv");
        }
        FfmpegHardwareAcceleration::Videotoolbox => {
            command.arg("-hwaccel").arg("videotoolbox");
        }
    }
}

pub fn apply_input_acceleration_with_hw_frames(
    command: &mut Command,
    hw_accel: &FfmpegHardwareAcceleration,
) {
    match hw_accel {
        FfmpegHardwareAcceleration::Nvenc => {
            command
                .arg("-hwaccel")
                .arg("cuda")
                .arg("-hwaccel_output_format")
                .arg("cuda");
        }
        _ => apply_input_acceleration(command, hw_accel),
    }
}

fn vaapi_device_from_env() -> PathBuf {
    env::var_os("SYNCPLAY_VAAPI_DEVICE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/dev/dri/renderD128"))
}

#[cfg(test)]
mod tests {
    use super::FfmpegHardwareAcceleration;

    #[test]
    fn hardware_h264_encoders_match_accel_backend() {
        assert_eq!(FfmpegHardwareAcceleration::None.h264_encoder(), "libx264");
        assert_eq!(FfmpegHardwareAcceleration::Auto.h264_encoder(), "libx264");
        assert_eq!(
            FfmpegHardwareAcceleration::Nvenc.h264_encoder(),
            "h264_nvenc"
        );
        assert_eq!(FfmpegHardwareAcceleration::Qsv.h264_encoder(), "h264_qsv");
        assert_eq!(
            FfmpegHardwareAcceleration::Videotoolbox.h264_encoder(),
            "h264_videotoolbox"
        );
    }

    #[test]
    fn vaapi_h264_filter_uploads_software_filter_output() {
        let accel = FfmpegHardwareAcceleration::Vaapi {
            device: "/dev/dri/renderD128".into(),
        };

        assert_eq!(
            accel.h264_filter(Some("subtitles=/tmp/movie.mkv:si=0")),
            Some("subtitles=/tmp/movie.mkv:si=0,format=nv12,hwupload".to_string())
        );
        assert_eq!(
            accel.h264_filter(None),
            Some("format=nv12,hwupload".to_string())
        );
    }

    #[test]
    fn nvenc_h264_filter_bridges_subtitles_through_system_memory() {
        let accel = FfmpegHardwareAcceleration::Nvenc;

        assert_eq!(
            accel.h264_filter(Some("subtitles=/tmp/movie.mkv:si=0")),
            Some(
                "hwdownload,format=nv12,subtitles=/tmp/movie.mkv:si=0,format=nv12,hwupload_cuda"
                    .to_string()
            )
        );
        assert_eq!(accel.h264_filter(None), None);
    }
}
