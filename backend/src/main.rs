use std::{net::SocketAddr, path::PathBuf};

use clap::{ArgAction, Parser, ValueEnum};
use stax_backend::{
    DEFAULT_API_ADDR, DEFAULT_LOG_FILTER, RuntimeConfig, ServerConfig, build_app, ffmpeg,
    init_tracing, library, load_state_from_runtime, shutdown_signal,
};
use tracing::info;

#[derive(Clone, Debug, ValueEnum)]
enum HwAccelFlag {
    None,
    Auto,
    Nvenc,
    Vaapi,
    Qsv,
    Videotoolbox,
}

impl From<HwAccelFlag> for ffmpeg::FfmpegHardwareAcceleration {
    fn from(value: HwAccelFlag) -> Self {
        match value {
            HwAccelFlag::None => Self::None,
            HwAccelFlag::Auto => Self::Auto,
            HwAccelFlag::Nvenc => Self::Nvenc,
            HwAccelFlag::Vaapi => Self::Vaapi {
                device: ffmpeg::default_vaapi_device(),
            },
            HwAccelFlag::Qsv => Self::Qsv,
            HwAccelFlag::Videotoolbox => Self::Videotoolbox,
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "stax-backend")]
#[command(about = "Backend service for synchronized media playback")]
struct Cli {
    #[arg(long, default_value = DEFAULT_API_ADDR)]
    api_addr: SocketAddr,

    #[arg(long, default_value = "stax.db")]
    database_path: PathBuf,

    #[arg(long = "library-root", value_name = "PATH", action = ArgAction::Append)]
    library_roots: Vec<PathBuf>,

    #[arg(long)]
    frontend_origin: Option<String>,

    #[arg(long, default_value = DEFAULT_LOG_FILTER)]
    log_filter: String,

    #[arg(long)]
    ffprobe_bin: Option<PathBuf>,

    #[arg(long, action = ArgAction::SetTrue)]
    no_ffprobe: bool,

    #[arg(long)]
    ffmpeg_bin: Option<PathBuf>,

    #[arg(long, action = ArgAction::SetTrue)]
    no_ffmpeg: bool,

    #[arg(long, value_enum, default_value_t = HwAccelFlag::None)]
    hw_accel: HwAccelFlag,

    #[arg(long)]
    vaapi_device: Option<PathBuf>,

    #[arg(long)]
    thumbnail_dir: Option<PathBuf>,

    #[arg(long, action = ArgAction::SetTrue)]
    no_thumbnail_cache: bool,

    #[arg(long)]
    stream_copy_dir: Option<PathBuf>,

    #[arg(long, action = ArgAction::SetTrue)]
    no_stream_copy_cache: bool,

    #[arg(long, default_value_t = library::default_probe_workers())]
    probe_workers: usize,

    #[arg(long, default_value_t = library::default_walk_workers())]
    walk_workers: usize,

    #[arg(long, default_value_t = 2)]
    thumbnail_workers: usize,

    #[arg(long, default_value_t = 1)]
    stream_copy_workers: usize,
}

impl Cli {
    fn into_server_config(self) -> ServerConfig {
        let mut library_config = if self.library_roots.is_empty() {
            library::LibraryConfig::default()
        } else {
            library::LibraryConfig::from_paths(self.library_roots)
        };

        library_config = library_config
            .with_hw_accel(match self.hw_accel {
                HwAccelFlag::Vaapi => ffmpeg::FfmpegHardwareAcceleration::Vaapi {
                    device: self
                        .vaapi_device
                        .unwrap_or_else(ffmpeg::default_vaapi_device),
                },
                other => other.into(),
            })
            .with_probe_workers(self.probe_workers)
            .with_walk_workers(self.walk_workers);

        library_config = if self.no_ffprobe {
            library_config.without_probe()
        } else if let Some(path) = self.ffprobe_bin {
            library_config.with_probe_command(path)
        } else {
            library_config
        };

        library_config = if self.no_ffmpeg {
            library_config.without_ffmpeg()
        } else if let Some(path) = self.ffmpeg_bin {
            library_config.with_ffmpeg_command(path)
        } else {
            library_config
        };

        library_config = if self.no_thumbnail_cache {
            library_config.without_thumbnail_cache_dir()
        } else if let Some(path) = self.thumbnail_dir {
            library_config.with_thumbnail_cache_dir(path)
        } else {
            library_config
        };

        library_config = if self.no_stream_copy_cache {
            library_config.without_stream_copy_cache_dir()
        } else if let Some(path) = self.stream_copy_dir {
            library_config.with_stream_copy_cache_dir(path)
        } else {
            library_config
        };

        ServerConfig {
            api_addr: self.api_addr,
            runtime: RuntimeConfig {
                database_path: self.database_path,
                library: library_config,
                stream_copy_workers: self.stream_copy_workers.max(1),
                thumbnail_workers: self.thumbnail_workers.max(1),
                frontend_origin: self.frontend_origin,
            },
            log_filter: self.log_filter,
        }
    }
}

#[tokio::main]
async fn main() {
    let config = Cli::parse().into_server_config();
    init_tracing(&config.log_filter);

    let app = build_app(
        load_state_from_runtime(config.runtime)
            .await
            .expect("backend state should initialize"),
    );

    info!("stax backend listening on http://{}", config.api_addr);

    let listener = tokio::net::TcpListener::bind(config.api_addr)
        .await
        .expect("failed to bind TCP listener");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}
