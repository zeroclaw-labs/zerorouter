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
pub const STRIPE_PUBLISHABLE_KEY_ENV: &str = "STRIPE_PUBLISHABLE_KEY";
pub const STRIPE_API_BASE_ENV: &str = "STRIPE_API_BASE";
pub const DEFAULT_STRIPE_API_BASE: &str = "https://api.stripe.com";
pub const STRIPE_WEBHOOK_SECRET_ENV: &str = "STRIPE_WEBHOOK_SECRET";
/// Turns on the stablecoin deposit rail. See [`StripeSettings::crypto_rail`].
pub const CRYPTO_RAIL_ENV: &str = "ZEROROUTER_CRYPTO_RAIL";
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
    /// The `pk_...` key Stripe.js is initialized with in the browser.
    ///
    /// Publishable by design — it is meant to be shipped to the client, and
    /// [`crate::portal`] hands it to the SPA over `/api/me`. It is still
    /// configuration rather than a constant: it differs per environment
    /// (test/live) and per account, so hardcoding it would pin the portal to
    /// one Stripe account.
    ///
    /// It is required, not optional, because Embedded Checkout cannot mount
    /// without it. A deployment with a secret key but no publishable key would
    /// start happily and then fail at the point a customer tries to pay, which
    /// is exactly the "silently disabled" shape the feature-group check exists
    /// to prevent.
    pub publishable_key: String,
    pub webhook_secret: String,
    pub checkout_min_usd: Decimal,
    pub checkout_max_usd: Decimal,
    /// Stripe API origin. Production default; tests point it at a fixture
    /// server so the sweep and outbound calls are testable without Stripe.
    pub api_base: String,
    /// Whether this deployment offers the stablecoin deposit rail.
    ///
    /// **This is a declaration by the operator, not a discovery.** Stripe gates
    /// stablecoin acceptance behind a Dashboard access request that it reviews
    /// and approves out of band; there is no supported API that answers "is my
    /// own account approved for this payment method?" (`/v1/account`'s
    /// `capabilities` hash carries `crypto_payments` for CONNECTED accounts,
    /// not for the platform's own). Detecting it by creating a probe session
    /// and reading what Stripe offers would mean minting throwaway sessions on
    /// the live account, so the honest mechanism is an explicit flag the
    /// operator sets once the Dashboard says the method is active.
    ///
    /// The consequence of the flag being WRONG is bounded and unequal, which is
    /// why it defaults to off:
    ///
    /// - Set when it should not be: the portal offers the rail, Stripe refuses
    ///   the session (the requested payment method is not active), the customer
    ///   sees `checkout_failed`, and NO money moves.
    /// - Unset when it could be set: the rail simply is not offered. Card
    ///   purchases are untouched.
    ///
    /// Neither can mis-price a purchase, because the fee schedule travels with
    /// the session's payment-method restriction rather than with this flag.
    pub crypto_rail: bool,
}

impl std::fmt::Debug for StripeSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StripeSettings")
            .field("secret_key", &"<scrubbed>")
            // Not scrubbed: a publishable key is served to every browser that
            // loads the portal, so hiding it here would buy nothing and make
            // a misconfiguration harder to diagnose.
            .field("publishable_key", &self.publishable_key)
            .field("webhook_secret", &"<scrubbed>")
            .field("checkout_min_usd", &self.checkout_min_usd)
            .field("checkout_max_usd", &self.checkout_max_usd)
            .field("crypto_rail", &self.crypto_rail)
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
                STRIPE_PUBLISHABLE_KEY_ENV,
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
        // Read BEFORE the feature group so an unparseable value aborts startup
        // even on a deployment with no Stripe configuration at all — the same
        // discipline `redemption_tax::mode_from_env` follows, and for the same
        // reason: a typo must never read as "off".
        let crypto_rail = bool_env(CRYPTO_RAIL_ENV, false)?;
        let stripe = feature_group(
            "Stripe billing",
            [
                (STRIPE_SECRET_KEY_ENV, optional_env(STRIPE_SECRET_KEY_ENV)),
                (
                    STRIPE_PUBLISHABLE_KEY_ENV,
                    optional_env(STRIPE_PUBLISHABLE_KEY_ENV),
                ),
                (
                    STRIPE_WEBHOOK_SECRET_ENV,
                    optional_env(STRIPE_WEBHOOK_SECRET_ENV),
                ),
            ],
        )?
        .map(
            |[secret_key, publishable_key, webhook_secret]| StripeSettings {
                secret_key,
                publishable_key,
                webhook_secret,
                checkout_min_usd,
                checkout_max_usd,
                api_base: optional_env(STRIPE_API_BASE_ENV)
                    .map(|base| base.trim_end_matches('/').to_owned())
                    .unwrap_or_else(|| DEFAULT_STRIPE_API_BASE.to_owned()),
                crypto_rail,
            },
        );
        // Naming the mistake beats silently ignoring it: the rail is a Stripe
        // payment method, so asking for it without Stripe configured is a
        // deployment error the operator wants to hear about at startup rather
        // than discover from a portal that renders no crypto option.
        if crypto_rail && stripe.is_none() {
            bail!(
                "{CRYPTO_RAIL_ENV} is set but Stripe billing is not configured; the stablecoin rail is a Stripe payment method and cannot be offered without it"
            );
        }

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
/// deployments can still run cap-only without any billing configuration.
///
/// The default is `true`, and the absent case is the reason why: credits are
/// the only ceiling backed by money. With enforcement off, the per-key and
/// derived per-user spend/velocity caps on `api_keys` are the sole limit on
/// what a user can consume, and those caps are self-service — the portal lets
/// a user raise a key's own `spend_cap_usd`. Unconfigured must therefore land
/// on the safe side, and so must a value that is *present but empty*, which
/// [`optional_env`] normalizes to absent: a blank setting is not a deployment
/// decision, so it cannot be read as one.
///
/// Cap-only stays a supported deployment shape via an explicit `false`/`0`;
/// that opt-out is announced once at startup rather than left silent — see
/// [`warn_cap_only_opt_out`]. A value that is neither recognized nor blank
/// aborts startup instead of falling back in either direction.
pub fn credits_required_from_env() -> Result<bool> {
    let required = match optional_env(REQUIRE_CREDITS_ENV).as_deref() {
        None => true,
        Some("true" | "1") => true,
        Some("false" | "0") => false,
        Some(other) => bail!("{REQUIRE_CREDITS_ENV} must be true or false, got {other:?}"),
    };
    warn_cap_only_opt_out(required);
    Ok(required)
}

/// Name the exposure the cap-only opt-out accepts, once, at startup.
///
/// This fires only on the explicit opt-out now that requiring credits is the
/// default — reaching it means an operator deliberately turned off the one
/// ceiling that verifies spend is backed by money.
fn warn_cap_only_opt_out(required: bool) {
    if required {
        return;
    }
    tracing::warn!(
        setting = REQUIRE_CREDITS_ENV,
        "credit enforcement has been explicitly DISABLED: no prepaid balance is required or \
         debited, so inference is bounded only by the per-key and derived per-user \
         spend/velocity caps on api_keys — caps users can raise themselves from the portal. \
         Unset ZEROROUTER_REQUIRE_CREDITS to restore the default of requiring a funded balance"
    );
}

/// Shared state for every web-plane router.
#[derive(Clone)]
pub struct WebCtx {
    pub pool: PgPool,
    pub config: Arc<WebConfig>,
    /// The key BYOK credentials are sealed under, or `None` when this
    /// deployment has not provisioned one (see
    /// [`crate::byok::Keyring::from_env`]).
    ///
    /// Carried on the context rather than in [`WebConfig`] because BYOK is not
    /// a web-plane feature that happens to need a secret — it is a DISPATCH
    /// feature whose management surface is the portal, so the inference plane
    /// ([`crate::api::RouterState`]) holds the same keyring from the same
    /// read. Putting it in `WebConfig` would make the inference side reach
    /// through the web plane's configuration for something the web plane does
    /// not own.
    ///
    /// `None` is the whole ship-dark contract on this surface: the attach
    /// endpoint refuses with a clear reason, the listing is empty, and
    /// `/api/me` reports the capability as off so the SPA renders no BYOK
    /// section at all.
    pub byok: Option<Arc<crate::byok::Keyring>>,
}

impl WebCtx {
    #[must_use]
    pub fn new(pool: PgPool, config: WebConfig) -> Self {
        Self {
            pool,
            config: Arc::new(config),
            byok: None,
        }
    }

    /// Attach the BYOK keyring this deployment read from the environment.
    ///
    /// A builder rather than a fourth argument to [`Self::new`] so that every
    /// existing construction site — including the test harnesses — keeps
    /// describing a deployment WITHOUT BYOK, which is what they are. A test
    /// that means to describe a BYOK deployment has to say so, the same
    /// discipline [`crate::api::RouterState::fully_credentialed`] enforces for
    /// provider credentials.
    #[must_use]
    pub fn with_byok(mut self, byok: Option<Arc<crate::byok::Keyring>>) -> Self {
        self.byok = byok;
        self
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

/// Read a boolean setting, refusing anything unrecognized.
///
/// Blank normalizes to absent via [`optional_env`], so a variable that is
/// present but empty takes the default rather than reading as an opt-in.
fn bool_env(name: &str, default: bool) -> Result<bool> {
    match optional_env(name).as_deref() {
        None => Ok(default),
        Some("true" | "1") => Ok(true),
        Some("false" | "0") => Ok(false),
        Some(other) => bail!("{name} must be true or false, got {other:?}"),
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

    #[test]
    fn credit_enforcement_defaults_to_required_and_opts_out_only_explicitly() {
        // One test rather than four: the variable is process-global, so the
        // cases have to run in sequence to be meaningful.
        let restore = env::var(REQUIRE_CREDITS_ENV).ok();
        let set = |value: Option<&str>| match value {
            // SAFETY: no other test in this binary reads or writes
            // REQUIRE_CREDITS_ENV, and the cases below run in sequence.
            Some(value) => unsafe { env::set_var(REQUIRE_CREDITS_ENV, value) },
            None => unsafe { env::remove_var(REQUIRE_CREDITS_ENV) },
        };

        // Unconfigured must fail safe. Off, the only ceiling is a cap the user
        // can raise from their own portal.
        set(None);
        assert!(
            credits_required_from_env().expect("an unset variable must not error"),
            "credits must be required by default"
        );

        // Present but blank is not a deployment decision — it must land on the
        // safe side too, not on the old default.
        set(Some("   "));
        assert!(
            credits_required_from_env().expect("a blank value must not error"),
            "a blank value must not read as an opt-out"
        );

        for value in ["true", "1"] {
            set(Some(value));
            assert!(credits_required_from_env().expect("an explicit true must parse"));
        }

        // The cap-only opt-out stays supported for self-hosted deployments.
        for value in ["false", "0"] {
            set(Some(value));
            assert!(
                !credits_required_from_env().expect("an explicit false must parse"),
                "{value} must still disable credit enforcement"
            );
        }

        // ...but a value we cannot read is a hard startup error, never a
        // silent fallback in either direction.
        set(Some("yes"));
        let error =
            credits_required_from_env().expect_err("an unparseable value must abort startup");
        assert!(
            error.to_string().contains(REQUIRE_CREDITS_ENV),
            "the error must name the variable: {error}"
        );

        set(restore.as_deref());
    }
}
