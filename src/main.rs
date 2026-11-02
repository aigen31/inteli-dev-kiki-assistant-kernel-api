//! Kiki assistant kernel: one assistant, many channels.
//!
//! The kernel owns conversation state, the model router, the tool loop and
//! safety policy. Site and CRM remain owners of their data and expose typed
//! tools through an internal contract.

mod api;
mod config;
mod error;
mod model;
mod prompt;
mod provider;
mod service;
mod store;

use std::sync::Arc;

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use crate::api::AppState;
use crate::config::Config;
use crate::service::AssistantService;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    if let Err(error) = run().await {
        tracing::error!(%error, "kernel stopped");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let config = Config::from_env()?;

    let pool = store::init(&config.database_url)
        .await
        .map_err(|error| format!("cannot initialize database: {error}"))?;

    let service = Arc::new(AssistantService::new(config.clone(), pool));
    let state = AppState { service, config: config.clone() };
    let app = api::router(state);

    let listener = TcpListener::bind(config.bind)
        .await
        .map_err(|error| format!("cannot bind {}: {error}", config.bind))?;

    tracing::info!(bind = %config.bind, "kiki assistant kernel listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| format!("server error: {error}"))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
