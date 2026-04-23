use std::{env, net::SocketAddr};

use stax_backend::{build_app, init_tracing, shutdown_signal, state_from_env};
use tracing::info;

#[tokio::main]
async fn main() {
    init_tracing();

    let app = build_app(
        state_from_env()
            .await
            .expect("backend state should initialize"),
    );

    let bind_address = env::var("STAX_API_ADDR").unwrap_or_else(|_| "127.0.0.1:3001".into());
    let socket_addr: SocketAddr = bind_address
        .parse()
        .expect("STAX_API_ADDR must be a valid host:port pair");

    info!("stax backend listening on http://{socket_addr}");

    let listener = tokio::net::TcpListener::bind(socket_addr)
        .await
        .expect("failed to bind TCP listener");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}
