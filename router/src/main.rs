use std::{env, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use zerorouter::{
    DEFAULT_TIER_CONFIG_PATH, PROVIDER_INVENTORY_PATH_ENV, RouterState, TIER_CONFIG_PATH_ENV,
    admin::{self, AdminArgs},
    app, byok,
    db::{database_pool_from_env, migrate},
    device, logging, oidc, portal, providers, redemption_tax, stripe,
    user::{self, UserArgs},
    web::{WebConfig, WebCtx, credits_required_from_env},
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
    /// Log in and inspect this machine's ZeroRouter account credential.
    User(UserArgs),
    /// Start the HTTP inference router (the default command).
    Serve,
}

#[tokio::main]
async fn main() -> Result<()> {
    let requested_filter = env::var("RUST_LOG").unwrap_or_else(|_| "info".to_owned());
    // The operator's filter selects among what may be logged; it does not decide
    // what may be logged. The pinned provider dependency includes sanitized
    // upstream bodies in its own events, and ZeroRouter's retention contract
    // permits metadata only — so those targets are denied by a separate layer
    // that no `RUST_LOG` value can reach. See `zerorouter::logging` for why the
    // appended `zeroclaw_log_event=off` this replaced was not a guarantee.
    logging::init(&requested_filter);

    match Cli::parse().command {
        Some(Command::Admin(args)) => admin::run(args).await,
        // The user CLI owns its own exit statuses — an agent driving it branches
        // on whether the failure was "not logged in", an HTTP error, or a
        // refused device grant, which a single anyhow bail cannot express. It
        // has already written its own diagnostics, so exit directly.
        Some(Command::User(args)) => std::process::exit(user::run(args).await),
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
    // Edge mode, stage 2: the operator's own upstreams, layered over the
    // shipped inventory. Before anything is served and before the tier catalog
    // is first read, because a candidate naming a provider this file declares
    // is a load error until it is installed — and fatal, so a deployment whose
    // local rung is misconfigured fails to start rather than quietly serving
    // every request to the cloud it was meant to keep traffic off.
    if let Some(path) = env::var_os(PROVIDER_INVENTORY_PATH_ENV).map(PathBuf::from) {
        let count = providers::load_operator_inventory(&path).with_context(|| {
            format!(
                "loading the operator provider inventory from {}",
                path.display()
            )
        })?;
        tracing::info!(path = %path.display(), providers = count, "operator providers registered");
    }
    let require_credits = credits_required_from_env()?;
    // Read before anything is served and independent of the web plane, on the
    // same contract as `credits_required_from_env` above: a malformed key is a
    // startup abort, and an absent one disables the feature. Reading it here
    // rather than inside `WebConfig` is deliberate — BYOK is a DISPATCH
    // feature whose management surface happens to be the portal, so both
    // planes take it from one read and cannot end up disagreeing about whether
    // it is on.
    let byok = byok::Keyring::from_env()?;
    let web_config = WebConfig::from_env()?;
    let pool = database_pool_from_env().await?;
    migrate(&pool).await?;

    let listener = tokio::net::TcpListener::bind(bind_address)
        .await
        .with_context(|| format!("failed to bind ZeroRouter to {bind_address}"))?;
    tracing::info!(
        %bind_address,
        require_credits,
        web_plane = web_config.is_some(),
        byok = byok.is_some(),
        "ZeroRouter listening"
    );
    let state = RouterState::with_database(tier_config_path, pool.clone(), require_credits)
        .with_byok(byok.clone());
    // The durable backstop for settlements that failed in-request: replays the
    // intent persisted on the reservation row (migration 0006) so delivered
    // inference cannot go unbilled because one transaction lost a connection.
    state.spawn_settlement_recovery();
    state.spawn_estimator_refresher();
    // Unconditional, unlike the autopay sweep below: the abandoned checkout
    // rows it removes are ZeroRouter's own and outlive whether Stripe is
    // configured right now.
    state.spawn_checkout_intent_cleanup();
    if let Some(stripe) = web_config.as_ref().and_then(|config| config.stripe.clone()) {
        // Parsed unconditionally so a typo in the mode refuses startup even
        // on a deployment that meant to leave it off; spawned only when the
        // operator has explicitly turned the mechanism on (see
        // `redemption_tax` — the flip is a deliberate, documented procedure,
        // never a side effect of configuring Stripe).
        let redemption_tax_mode =
            redemption_tax::mode_from_env().map_err(|message| anyhow::anyhow!(message))?;
        if redemption_tax_mode != redemption_tax::RedemptionTaxMode::Off {
            tracing::warn!(
                ?redemption_tax_mode,
                "redemption-time tax is ON; the Tax Settings preset must be the stored-value code or the same dollar is taxed twice (see DEPLOY.md)"
            );
            state.spawn_redemption_tax_sweep(stripe.clone(), redemption_tax_mode);
        }
        state.spawn_autopay_sweep(stripe);
    }
    let mut router = app(state.clone());
    if let Some(config) = web_config {
        let portal_dist = config.portal_dist_path.clone();
        let oidc_enabled = config.oidc.is_some();
        let stripe_enabled = config.stripe.is_some();
        let ctx = WebCtx::new(pool, config).with_byok(byok);
        router = router.merge(
            axum::Router::new()
                .merge(oidc::router())
                .merge(device::router())
                .merge(stripe::router())
                .merge(portal::router())
                // Bound the memory an unauthenticated request can force the
                // web plane to buffer (Stripe events and device/OIDC bodies are
                // small); the inference plane keeps its own 8 MiB chat limit.
                .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
                .with_state(ctx),
        );
        if portal_dist.is_dir() {
            router = router.fallback_service(portal::spa_router(&portal_dist));
        } else {
            tracing::warn!(path = %portal_dist.display(), "portal dist directory not found; SPA disabled");
        }
        tracing::info!(oidc_enabled, stripe_enabled, "web plane enabled");
    }
    let shutdown_state = state.clone();
    let server_result = axum::serve(listener, router)
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
