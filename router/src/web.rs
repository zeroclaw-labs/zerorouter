//! Shared context for the web plane: the portal, control-plane API, OIDC
//! login, device authorization, and Stripe billing.
//!
//! The web plane is optional. When `ZEROROUTER_PUBLIC_BASE_URL` is unset the
//! router serves only the inference surface. When it is set, each feature
//! group (OIDC login, Stripe billing) must be either fully configured or fully
//! absent — a partially configured group aborts startup rather than running
//! with a silently disabled security control.

use std::{env, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;

use crate::sqlx::PgPool;

pub const PUBLIC_BASE_URL_ENV: &str = "ZEROROUTER_PUBLIC_BASE_URL";
pub const OIDC_ISSUER_URL_ENV: &str = "OIDC_ISSUER_URL";
pub const OIDC_CLIENT_ID_ENV: &str = "OIDC_CLIENT_ID";
pub const OIDC_CLIENT_SECRET_ENV: &str = "OIDC_CLIENT_SECRET";
pub const STRIPE_SECRET_KEY_ENV: &str = "STRIPE_SECRET_KEY";
pub const STRIPE_WEBHOOK_SECRET_ENV: &str = "STRIPE_WEBHOOK_SECRET";
pub const SIGNUP_CREDIT_ENV: &str = "ZEROROUTER_SIGNUP_CREDIT_USD";
pub const REQUIRE_CREDITS_ENV: &str = "ZEROROUTER_REQUIRE_CREDITS";
pub const PORTAL_DIST_ENV: &str = "ZEROROUTER_PORTAL_DIST";
pub const SESSION_TTL_ENV: &str = "ZEROROUTER_SESSION_TTL_SECS";
pub const CHECKOUT_MIN_ENV: &str = "ZEROROUTER_CHECKOUT_MIN_USD";
pub const CHECKOUT_MAX_ENV: &str = "ZEROROUTER_CHECKOUT_MAX_USD";
pub const DEVICE_CLIENT_IDS_ENV: &str = "ZEROROUTER_DEVICE_CLIENT_IDS";

const DEFAULT_PORTAL_DIST: &str = "portal/dist";
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_SESSION_TTL: Duration = Duration::from_secs(90 * 24 * 60 * 60);
const DEFAULT_DEVICE_CLIENT_ID: &str = "zeroclaw";

#[derive(Clone, Debug)]
pub struct OidcSettings {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Clone)]
pub struct StripeSettings {
    pub secret_key: String,
    pub webhook_secret: String,
    pub checkout_min_usd: Decimal,
    pub checkout_max_usd: Decimal,
}

impl std::fmt::Debug for StripeSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StripeSettings")
            .field("secret_key", &"<scrubbed>")
            .field("webhook_secret", &"<scrubbed>")
            .field("checkout_min_usd", &self.checkout_min_usd)
            .field("checkout_max_usd", &self.checkout_max_usd)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct WebConfig {
    /// Externally reachable base URL, without a trailing slash.
    pub public_base_url: String,
    /// Whether session cookies should carry the `Secure` attribute.
    pub secure_cookies: bool,
    pub oidc: Option<OidcSettings>,
    pub stripe: Option<StripeSettings>,
    pub signup_credit_usd: Decimal,
    pub portal_dist_path: PathBuf,
    pub session_ttl: Duration,
    /// Client identifiers allowed to start a device authorization.
    pub device_client_ids: Vec<String>,
}

impl WebConfig {
    /// Read the web-plane configuration from the environment.
    ///
    /// Returns `Ok(None)` when the web plane is disabled entirely, and an
    /// error when a feature group is only partially configured.
    pub fn from_env() -> Result<Option<Self>> {
        let Some(public_base_url) = optional_env(PUBLIC_BASE_URL_ENV) else {
            for name in [
                OIDC_ISSUER_URL_ENV,
                OIDC_CLIENT_ID_ENV,
                OIDC_CLIENT_SECRET_ENV,
                STRIPE_SECRET_KEY_ENV,
                STRIPE_WEBHOOK_SECRET_ENV,
            ] {
                if optional_env(name).is_some() {
                    bail!(
                        "{name} is set but {PUBLIC_BASE_URL_ENV} is not; refusing to run with a partially configured web plane"
                    );
                }
            }
            return Ok(None);
        };
        if !public_base_url.starts_with("http://") && !public_base_url.starts_with("https://") {
            bail!("{PUBLIC_BASE_URL_ENV} must start with http:// or https://");
        }
        let public_base_url = public_base_url.trim_end_matches('/').to_owned();
        let secure_cookies = public_base_url.starts_with("https://");

        let oidc = feature_group(
            "OIDC login",
            [
                (OIDC_ISSUER_URL_ENV, optional_env(OIDC_ISSUER_URL_ENV)),
                (OIDC_CLIENT_ID_ENV, optional_env(OIDC_CLIENT_ID_ENV)),
                (OIDC_CLIENT_SECRET_ENV, optional_env(OIDC_CLIENT_SECRET_ENV)),
            ],
        )?
        .map(|[issuer_url, client_id, client_secret]| OidcSettings {
            issuer_url,
            client_id,
            client_secret,
        });

        let checkout_min_usd = decimal_env(CHECKOUT_MIN_ENV, Decimal::from(5))?;
        let checkout_max_usd = decimal_env(CHECKOUT_MAX_ENV, Decimal::from(1000))?;
        if checkout_min_usd <= Decimal::ZERO {
            bail!("{CHECKOUT_MIN_ENV} must be positive");
        }
        if checkout_max_usd < checkout_min_usd {
            bail!("{CHECKOUT_MAX_ENV} must be at least {CHECKOUT_MIN_ENV}");
        }
        let stripe = feature_group(
            "Stripe billing",
            [
                (STRIPE_SECRET_KEY_ENV, optional_env(STRIPE_SECRET_KEY_ENV)),
                (
                    STRIPE_WEBHOOK_SECRET_ENV,
                    optional_env(STRIPE_WEBHOOK_SECRET_ENV),
                ),
            ],
        )?
        .map(|[secret_key, webhook_secret]| StripeSettings {
            secret_key,
            webhook_secret,
            checkout_min_usd,
            checkout_max_usd,
        });

        let signup_credit_usd = decimal_env(SIGNUP_CREDIT_ENV, Decimal::ZERO)?;
        if signup_credit_usd < Decimal::ZERO {
            bail!("{SIGNUP_CREDIT_ENV} cannot be negative");
        }

        let session_ttl = match optional_env(SESSION_TTL_ENV) {
            None => DEFAULT_SESSION_TTL,
            Some(raw) => {
                let seconds = raw
                    .parse::<u64>()
                    .with_context(|| format!("{SESSION_TTL_ENV} must be a positive integer"))?;
                if seconds == 0 {
                    bail!("{SESSION_TTL_ENV} must be positive");
                }
                Duration::from_secs(seconds).min(MAX_SESSION_TTL)
            }
        };

        let portal_dist_path = optional_env(PORTAL_DIST_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_PORTAL_DIST));

        let device_client_ids = optional_env(DEVICE_CLIENT_IDS_ENV)
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![DEFAULT_DEVICE_CLIENT_ID.to_owned()]);
        if device_client_ids.is_empty() {
            bail!("{DEVICE_CLIENT_IDS_ENV} must list at least one client id");
        }

        Ok(Some(Self {
            public_base_url,
            secure_cookies,
            oidc,
            stripe,
            signup_credit_usd,
            portal_dist_path,
            session_ttl,
            device_client_ids,
        }))
    }

    #[must_use]
    pub fn absolute_url(&self, path: &str) -> String {
        debug_assert!(path.starts_with('/'));
        format!("{}{path}", self.public_base_url)
    }
}

/// Whether inference admission additionally requires a positive prepaid
/// credit balance. Independent of the web plane so that self-hosted
/// deployments can run cap-only without any billing configuration.
pub fn credits_required_from_env() -> Result<bool> {
    match optional_env(REQUIRE_CREDITS_ENV).as_deref() {
        None => Ok(false),
        Some("true" | "1") => Ok(true),
        Some("false" | "0") => Ok(false),
        Some(other) => bail!("{REQUIRE_CREDITS_ENV} must be true or false, got {other:?}"),
    }
}

/// Shared state for every web-plane router.
#[derive(Clone)]
pub struct WebCtx {
    pub pool: PgPool,
    pub config: Arc<WebConfig>,
}

impl WebCtx {
    #[must_use]
    pub fn new(pool: PgPool, config: WebConfig) -> Self {
        Self {
            pool,
            config: Arc::new(config),
        }
    }
}

fn optional_env(name: &str) -> Option<String> {
    let value = env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn decimal_env(name: &str, default: Decimal) -> Result<Decimal> {
    match optional_env(name) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<Decimal>()
            .with_context(|| format!("{name} must be a decimal number")),
    }
}

/// Enforce that a named feature group is fully present or fully absent.
fn feature_group<const N: usize>(
    label: &str,
    entries: [(&str, Option<String>); N],
) -> Result<Option<[String; N]>> {
    let set = entries.iter().filter(|(_, value)| value.is_some()).count();
    if set == 0 {
        return Ok(None);
    }
    if set < N {
        let missing = entries
            .iter()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(", ");
        bail!("{label} is partially configured; missing: {missing}");
    }
    let mut values = entries.into_iter().map(|(_, value)| {
        value.unwrap_or_default() // set == N guarantees Some
    });
    Ok(Some(std::array::from_fn(|_| {
        values.next().unwrap_or_default()
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_groups_are_all_or_nothing() {
        assert!(
            feature_group("t", [("A", None::<String>), ("B", None)])
                .expect("absent group is fine")
                .is_none()
        );
        let full = feature_group(
            "t",
            [("A", Some("1".to_owned())), ("B", Some("2".to_owned()))],
        )
        .expect("full group is fine")
        .expect("group should be present");
        assert_eq!(full, ["1".to_owned(), "2".to_owned()]);
        assert!(feature_group("t", [("A", Some("1".to_owned())), ("B", None)]).is_err());
    }
}
