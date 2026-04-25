use std::{net::SocketAddr, path::PathBuf};

use crate::library::LibraryConfig;

pub const DEFAULT_API_ADDR: &str = "127.0.0.1:3001";
pub const DEFAULT_DATABASE_PATH: &str = "stax.db";
pub const DEFAULT_LOG_FILTER: &str = "stax_backend=debug,tower_http=info";

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub database_path: PathBuf,
    pub library: LibraryConfig,
    pub stream_copy_workers: usize,
    pub thumbnail_workers: usize,
    pub frontend_origin: Option<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            database_path: PathBuf::from(DEFAULT_DATABASE_PATH),
            library: LibraryConfig::default(),
            stream_copy_workers: 1,
            thumbnail_workers: 2,
            frontend_origin: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub api_addr: SocketAddr,
    pub runtime: RuntimeConfig,
    pub log_filter: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            api_addr: DEFAULT_API_ADDR
                .parse()
                .expect("default API address should parse"),
            runtime: RuntimeConfig::default(),
            log_filter: DEFAULT_LOG_FILTER.to_string(),
        }
    }
}
