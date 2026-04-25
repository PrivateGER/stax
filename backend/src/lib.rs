pub(crate) mod api;
pub mod app;
pub mod clock;
pub mod config;
pub mod ffmpeg;
pub mod library;
pub mod lifecycle;
pub(crate) mod origin;
pub mod persistence;
pub mod probes;
pub mod protocol;
pub(crate) mod rooms;
pub mod scan_gate;
pub mod stream_copies;
pub mod streaming;
pub mod thumbnails;

#[cfg(not(debug_assertions))]
mod frontend_assets;

pub(crate) const ROOM_EVENT_BUFFER: usize = 64;

pub use app::{
    AppState, DEFAULT_EMPTY_ROOM_GRACE, build_app, load_state, load_state_from_runtime,
    load_state_with_library, load_state_with_library_and_grace, load_state_with_runtime,
    load_state_with_runtime_and_grace, seeded_state,
};
pub use config::{
    DEFAULT_API_ADDR, DEFAULT_DATABASE_PATH, DEFAULT_LOG_FILTER, RuntimeConfig, ServerConfig,
};
pub use lifecycle::{init_tracing, shutdown_signal};
