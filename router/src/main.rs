use std::{env, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use zerorouter::{
    DEFAULT_TIER_CONFIG_PATH, RouterState, TIER_CONFIG_PATH_ENV,
    admin::{self, AdminArgs},
    app,
    db::{database_pool_from_env, migrate},
};

const DEFAULT_BIND_ADDRESS: &str = "0.0.0.0:8080";
const BIND_ADDRESS_ENV: &str = "ZEROROUTER_BIND";

#[derive(Debug, Parser)]
#[command(name = "zerorouter", version, about = "ZeroRouter inference router")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run database-backed API-key administration commands.
    Admin(AdminArgs),
    /// Start the HTTP inference router (the default command).
    Serve,
}

#[tokio::main]
async fn main() -> Result<()> {
    let requested_filter = env::var("RUST_LOG").unwrap_or_else(|_| "info".to_owned());
    // The pinned provider dependency includes sanitized upstream error bodies
    // in its own events. Disable those targets because ZeroRouter's retention
    // contract permits metadata only, never provider bodies.
    let log_filter = EnvFilter::new(format!(
        "{requested_filter},zeroclaw_log_event=off,zeroclaw_providers=off"
    ));
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(log_filter)
        .init();

    match Cli::parse().command {
        Some(Command::Admin(args)) => admin::run(args).await,
        Some(Command::Serve) | None => serve().await,
    }
}

async fn serve() -> Result<()> {
    let bind_address = env::var(BIND_ADDRESS_ENV)
        .unwrap_or_else(|_| DEFAULT_BIND_ADDRESS.to_owned())
        .parse::<SocketAddr>()
        .with_context(|| format!("invalid {BIND_ADDRESS_ENV}"))?;
    let tier_config_path = env::var_os(TIER_CONFIG_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_TIER_CONFIG_PATH));
    let pool = database_pool_from_env().await?;
    migrate(&pool).await?;

    let listener = tokio::net::TcpListener::bind(bind_address)
        .await
        .with_context(|| format!("failed to bind ZeroRouter to {bind_address}"))?;
    tracing::info!(%bind_address, "ZeroRouter listening");
    let state = RouterState::with_database(tier_config_path, pool);
    let shutdown_state = state.clone();
    let server_result = axum::serve(listener, app(state.clone()))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown_state.begin_shutdown();
        })
        .await;

    // Also cancel background work when the server exits due to an error rather
    // than a signal, then wait until every reservation has been settled.
    state.begin_shutdown();
    state.wait_for_background_tasks().await;
    server_result.context("ZeroRouter server stopped unexpectedly")
}

async fn shutdown_signal() {
    let control_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %error, "failed to install Ctrl-C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::error!(error = %error, "failed to install SIGTERM handler"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = control_c => {},
        () = terminate => {},
    }
}
