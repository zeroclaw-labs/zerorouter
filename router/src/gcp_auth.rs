//! Google service-account access tokens for the `vertex` lane.
//!
//! Every other upstream in this repo is reached with a credential that is a
//! STRING AN OPERATOR PASTES: an API key that lives in a secret, is read from
//! the environment once per request, and is sent verbatim. Vertex is the one
//! exception, and it is not a stylistic one — Google does not issue a
//! long-lived key for the surface this lane dispatches on:
//!
//! > "Only Google Cloud Auth is supported using the OpenAI library in
//! > Gemini Enterprise Agent Platform."
//! > — cloud.google.com/vertex-ai/generative-ai/docs/start/openai
//!
//! Vertex *express mode* does issue an API key, and it is deliberately not used
//! here. Its endpoints take a key INSTEAD of a project and a location
//! (`.../v1/publishers/google/models/{model}:generateContent?key=…`), and a
//! project id is precisely what the zero-retention configuration is applied to
//! — `projects/{id}/cacheConfig` is the switch that turns off the 24-hour cache.
//! An express-mode key therefore cannot reach the posture this lane is sold
//! under, quite apart from being Preview, gmail-account-only, and rate-limited
//! as a trial. See `[retention.vertex]` in `config/tiers.toml`.
//!
//! So the credential the operator stores is a service-account JSON blob, and
//! this module exchanges it for the short-lived bearer token the wire actually
//! sends. The exchange is the standard two-legged OAuth flow
//! (developers.google.com/identity/protocols/oauth2/service-account): sign a
//! JWT with the account's RSA private key, POST it as a `jwt-bearer` assertion,
//! receive an access token that lives one hour.
//!
//! ## Why a cache, and why the cache is the risky part
//!
//! Tokens last an hour and minting one is a network round trip against a
//! Google endpoint that can be slow or down. Minting per request would put a
//! second upstream dependency in front of every Vertex call and turn a token
//! endpoint hiccup into a lane outage. So tokens are cached.
//!
//! A token cache has exactly one way to be dangerous, and it is worth naming
//! before reading the code: **serving a token past its expiry**. That failure
//! is not loud. The token does not stop working locally; it works right up
//! until Google rejects it, at which point every request on the lane 401s at
//! once, and the router's retry machinery reads that as an upstream failure
//! rather than a credential that needed refreshing. [`REFRESH_SKEW`] and the
//! comparison in [`AccessToken::usable_at`] — which [`TokenCache::token`]
//! consults before serving anything — are the whole defence, and both are
//! covered by tests that fail if either is weakened.
//!
//! ## Why refresh happens under a lock
//!
//! `ProviderRoute::new` runs per request, so under load many requests reach an
//! expired cache at the same instant. Without a lock each would mint its own
//! token — a thundering herd against Google's token endpoint, which rate-limits
//! it. The cache is therefore a `tokio::sync::Mutex` held ACROSS the mint: the
//! first request through does the work and the rest wait and then find a fresh
//! token already in the cell. Holding a lock across an await is normally worth
//! a second look; here it is the point, and the alternative (mint outside the
//! lock, then store) is the thundering herd this exists to prevent.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{SignatureEncoding, Signer};
use serde::Deserialize;
use sha2::Sha256;

/// The OAuth scope a Vertex call needs. Google documents no narrower scope for
/// `aiplatform.googleapis.com`; `cloud-platform` is what its own client
/// libraries request for this API.
pub const VERTEX_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// How long before real expiry a cached token stops being served.
///
/// **This is a safety margin, not a tuning knob, and shrinking it toward zero
/// reintroduces the bug it exists to prevent.** A token that is valid when this
/// process checks it must still be valid when Google checks it, and between
/// those two moments sit: the rest of route construction, the request body
/// being serialized, the TCP connect and TLS handshake to `aiplatform`, and —
/// the one that actually bites — up to fifteen minutes of upstream budget for a
/// long generation that Google validates the token at the START of. Five
/// minutes is the conventional margin for this flow and comfortably covers
/// everything but the pathological case.
///
/// It also absorbs clock skew between this host and Google's, which is the
/// failure that would otherwise be maddening to diagnose: a host running a few
/// minutes fast serves tokens Google considers expired, and the symptom is an
/// intermittent 401 on a credential that is demonstrably correct.
pub const REFRESH_SKEW: Duration = Duration::from_secs(300);

/// The lifetime requested for the signed assertion.
///
/// One hour is the maximum Google accepts (`exp` may not be more than an hour
/// past `iat`); a longer value is rejected outright rather than clamped.
const ASSERTION_LIFETIME: Duration = Duration::from_secs(3600);

const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);

/// Google's default token endpoint, used when the service-account JSON omits
/// `token_uri`. Real keys minted by Google always carry it; the fallback keeps
/// a hand-written fixture from being a special case.
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// What went wrong getting a token.
///
/// Deliberately NOT collapsed into one opaque string: the three cases have
/// different operators and different fixes. A malformed key is a deployment
/// mistake that will never fix itself, a rejected assertion is usually a
/// permissions or clock problem on a well-formed key, and a transport failure
/// is Google being unreachable and is worth retrying.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// The credential is not a usable service-account key. Permanent until the
    /// secret is replaced.
    #[error("vertex service-account credential is unusable: {0}")]
    Credential(String),
    /// The token endpoint could not be reached, or did not answer in time.
    #[error("could not reach Google's token endpoint: {0}")]
    Transport(String),
    /// The token endpoint answered, and the answer was a refusal.
    #[error("Google refused the service-account assertion (HTTP {status}): {body}")]
    Refused { status: u16, body: String },
}

/// A service-account key, parsed once so a malformed one fails at load rather
/// than on the first request that needs it.
///
/// The private key is stored already-parsed for the same reason: PEM decoding
/// on every mint would be wasted work, and — more importantly — a key that
/// cannot be decoded should not present as a lane that is up.
#[derive(Clone)]
pub struct ServiceAccountKey {
    client_email: String,
    signing_key: Arc<SigningKey<Sha256>>,
    key_id: Option<String>,
    token_uri: String,
}

impl std::fmt::Debug for ServiceAccountKey {
    /// Hand-written so a `{:?}` in a log line or an error can never print the
    /// private key. `derive(Debug)` on this struct would put RSA key material
    /// into any tracing call that formatted a provider, which is the kind of
    /// leak that is discovered in a log aggregator months later.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServiceAccountKey")
            .field("client_email", &self.client_email)
            .field("key_id", &self.key_id)
            .field("token_uri", &self.token_uri)
            .field("signing_key", &"<redacted>")
            .finish()
    }
}

/// The subset of Google's service-account JSON this flow needs.
#[derive(Deserialize)]
struct ServiceAccountJson {
    client_email: Option<String>,
    private_key: Option<String>,
    private_key_id: Option<String>,
    token_uri: Option<String>,
    #[serde(rename = "type")]
    account_type: Option<String>,
}

impl ServiceAccountKey {
    /// Parse a service-account key from the JSON Google hands out.
    ///
    /// Every failure here is a permanent deployment fault, so each one names
    /// the field at fault rather than saying "invalid credential": the operator
    /// reading this message is holding a JSON file and needs to know which line
    /// of it is wrong.
    pub fn from_json(raw: &str) -> Result<Self, TokenError> {
        let parsed: ServiceAccountJson = serde_json::from_str(raw.trim())
            .map_err(|error| TokenError::Credential(format!("not valid JSON: {error}")))?;

        // Checked before the fields are read because it is the error an
        // operator is most likely to hit and the least obvious to diagnose:
        // Google's console offers several JSON downloads and only one of them
        // is a service-account key. An OAuth *client* secret has the same
        // shape, the same file extension, and no `client_email` — without this
        // check the message would be "missing client_email", which does not
        // tell them they downloaded the wrong artifact.
        if let Some(account_type) = parsed.account_type.as_deref()
            && account_type != "service_account"
        {
            return Err(TokenError::Credential(format!(
                "expected a service-account key (\"type\": \"service_account\"), got \
                 \"{account_type}\" — an OAuth client secret or an external-account file will \
                 not work on this lane"
            )));
        }

        let client_email = parsed
            .client_email
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| TokenError::Credential("missing `client_email`".to_owned()))?;
        let private_key = parsed
            .private_key
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| TokenError::Credential("missing `private_key`".to_owned()))?;

        // `\n` inside a JSON string is already a newline by the time serde is
        // done, so nothing is unescaped here. What IS handled is the operator
        // who pasted the PEM through a shell that flattened it: a key whose
        // newlines arrived as the two characters `\` and `n` is rejected with
        // that named as the cause, because the PEM decoder's own error for it
        // is unreadable.
        if private_key.contains("\\n") {
            return Err(TokenError::Credential(
                "`private_key` contains the literal characters `\\n` rather than newlines — the \
                 JSON was probably escaped a second time on its way into the secret"
                    .to_owned(),
            ));
        }

        let key = RsaPrivateKey::from_pkcs8_pem(private_key.trim()).map_err(|error| {
            TokenError::Credential(format!(
                "`private_key` is not a PKCS#8 PEM RSA key: {error}"
            ))
        })?;

        Ok(Self {
            client_email,
            signing_key: Arc::new(SigningKey::<Sha256>::new(key)),
            key_id: parsed
                .private_key_id
                .filter(|value| !value.trim().is_empty()),
            token_uri: parsed
                .token_uri
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_TOKEN_URI.to_owned()),
        })
    }

    /// The account this key authenticates as. Safe to log — it is an email
    /// address, not a secret — and worth logging, because "which service
    /// account is this lane using" is the first question of any Vertex
    /// permission problem.
    #[must_use]
    pub fn client_email(&self) -> &str {
        &self.client_email
    }

    /// Build and sign the assertion for one token request.
    ///
    /// Split out from [`Self::mint`] so the JWT can be asserted on directly by
    /// tests without a live token endpoint: the signature is the part that is
    /// impossible to eyeball and easy to get subtly wrong.
    fn assertion(&self, issued_at: SystemTime) -> Result<String, TokenError> {
        let issued = issued_at
            .duration_since(UNIX_EPOCH)
            .map_err(|_| TokenError::Credential("system clock is before 1970".to_owned()))?
            .as_secs();
        let expires = issued + ASSERTION_LIFETIME.as_secs();

        let header = match &self.key_id {
            Some(kid) => serde_json::json!({ "alg": "RS256", "typ": "JWT", "kid": kid }),
            None => serde_json::json!({ "alg": "RS256", "typ": "JWT" }),
        };
        let claims = serde_json::json!({
            "iss": self.client_email,
            "scope": VERTEX_SCOPE,
            // `aud` is the token endpoint itself, NOT the API being called.
            // Getting this wrong yields a signed assertion Google rejects with
            // a message that does not mention the audience.
            "aud": self.token_uri,
            "iat": issued,
            "exp": expires,
        });

        let encode = |value: &serde_json::Value| -> Result<String, TokenError> {
            serde_json::to_vec(value)
                .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
                .map_err(|error| TokenError::Credential(format!("could not encode JWT: {error}")))
        };

        let signing_input = format!("{}.{}", encode(&header)?, encode(&claims)?);
        let signature = self
            .signing_key
            .try_sign(signing_input.as_bytes())
            .map_err(|error| TokenError::Credential(format!("could not sign JWT: {error}")))?;

        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        ))
    }

    /// Exchange the key for an access token. One network round trip, no
    /// caching — [`TokenCache`] owns that decision.
    pub async fn mint(
        &self,
        http: &reqwest::Client,
        now: SystemTime,
    ) -> Result<AccessToken, TokenError> {
        let assertion = self.assertion(now)?;
        let response = http
            .post(&self.token_uri)
            .timeout(TOKEN_TIMEOUT)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
            ])
            .send()
            .await
            .map_err(|error| TokenError::Transport(error.to_string()))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| TokenError::Transport(error.to_string()))?;

        if !status.is_success() {
            // Truncated because Google's error bodies are occasionally an HTML
            // error page, and a full one in a log line buries everything around
            // it. The status plus the first 500 characters carries the
            // `error`/`error_description` pair that actually names the fault.
            let body: String = body.chars().take(500).collect();
            return Err(TokenError::Refused {
                status: status.as_u16(),
                body,
            });
        }

        let parsed: TokenResponse = serde_json::from_str(&body).map_err(|error| {
            TokenError::Transport(format!("token endpoint returned unreadable JSON: {error}"))
        })?;

        if parsed.access_token.trim().is_empty() {
            return Err(TokenError::Transport(
                "token endpoint returned an empty access_token".to_owned(),
            ));
        }

        Ok(AccessToken {
            token: parsed.access_token,
            // A response without `expires_in` is treated as ALREADY at the
            // margin rather than as long-lived: `valid_token` will decline to
            // serve it and the next request mints again. The opposite default
            // — assume an hour — would invent a lifetime the endpoint never
            // promised, which is the "cache past expiry" failure wearing a
            // different hat.
            expires_at: now + Duration::from_secs(parsed.expires_in.unwrap_or(0)),
        })
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    expires_in: Option<u64>,
}

/// A minted token and the instant it stops being usable.
#[derive(Clone)]
pub struct AccessToken {
    token: String,
    expires_at: SystemTime,
}

impl std::fmt::Debug for AccessToken {
    /// Same reasoning as [`ServiceAccountKey`]'s: a bearer token is a bearer
    /// credential, and one printed into a log is one an attacker can use for
    /// the rest of the hour.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccessToken")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl AccessToken {
    #[must_use]
    pub fn secret(&self) -> &str {
        &self.token
    }

    /// Whether this token may still be handed to a caller at `now`.
    ///
    /// **The single guard this module exists to get right.** The comparison is
    /// against expiry MINUS [`REFRESH_SKEW`], never against expiry itself:
    /// a token that is valid for another ten seconds is useless to a request
    /// that has not been sent yet. Weakening this to `now < expires_at` — or
    /// dropping the check so a cached token is always served — is the
    /// "serve past expiry" failure, and `a_token_inside_the_refresh_skew_is_not_served`
    /// is the test that catches it.
    #[must_use]
    pub fn usable_at(&self, now: SystemTime) -> bool {
        match self.expires_at.checked_sub(REFRESH_SKEW) {
            Some(deadline) => now < deadline,
            // Expiry within five minutes of the epoch is not a real token; the
            // subtraction underflowing means the same thing as being expired.
            None => false,
        }
    }
}

/// A service-account key plus the token most recently minted from it.
///
/// One of these per provider, built once and shared; see the module docs for
/// why the mutex is held across the mint rather than around the store.
pub struct TokenCache {
    key: ServiceAccountKey,
    http: reqwest::Client,
    cached: tokio::sync::Mutex<Option<AccessToken>>,
}

impl TokenCache {
    #[must_use]
    pub fn new(key: ServiceAccountKey, http: reqwest::Client) -> Self {
        Self {
            key,
            http,
            cached: tokio::sync::Mutex::new(None),
        }
    }

    /// Return a token good for at least [`REFRESH_SKEW`] from `now`, minting
    /// one if the cache cannot supply it.
    ///
    /// Note the SECOND check after the lock is acquired: a request that waited
    /// on the mutex must re-read the cell, because the holder it waited for has
    /// very likely just filled it. Skipping that re-read would make every
    /// queued request mint its own token immediately after the one it waited
    /// for — the herd this lock exists to stop, merely delayed by one round
    /// trip.
    pub async fn token(&self, now: SystemTime) -> Result<AccessToken, TokenError> {
        let mut cached = self.cached.lock().await;
        if let Some(token) = cached.as_ref().filter(|token| token.usable_at(now)) {
            return Ok(token.clone());
        }

        let minted = self.key.mint(&self.http, now).await?;

        // A freshly minted token that is ALREADY inside the skew is not stored.
        // It would otherwise poison the cache for its whole (short) life:
        // `token` would keep finding it unusable, mint a replacement, and store
        // that one — correct, but it makes the cache a liar for as long as the
        // endpoint keeps returning short lifetimes. Returning it unstored
        // serves the request that paid for it and leaves the cell empty for the
        // next one.
        if minted.usable_at(now) {
            *cached = Some(minted.clone());
        }
        Ok(minted)
    }

    /// The account this cache authenticates as.
    #[must_use]
    pub fn client_email(&self) -> &str {
        self.key.client_email()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway 2048-bit key, generated for this test file and used nowhere
    /// else. Committed deliberately: the alternative is generating one per test
    /// run, and RSA keygen is slow enough to be felt in a suite this size.
    fn test_key_json(token_uri: &str) -> String {
        // Generated once with:
        //   openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048
        let pem = include_str!("../tests/fixtures/vertex_test_key.pem");
        serde_json::json!({
            "type": "service_account",
            "project_id": "zr-vertex-test",
            "private_key_id": "test-key-id",
            "private_key": pem,
            "client_email": "zr-test@zr-vertex-test.iam.gserviceaccount.com",
            "token_uri": token_uri,
        })
        .to_string()
    }

    fn key() -> ServiceAccountKey {
        ServiceAccountKey::from_json(&test_key_json(DEFAULT_TOKEN_URI))
            .expect("the fixture key must parse")
    }

    #[test]
    fn a_service_account_key_parses_and_keeps_its_identity() {
        let key = key();
        assert_eq!(
            key.client_email(),
            "zr-test@zr-vertex-test.iam.gserviceaccount.com"
        );
        assert_eq!(key.token_uri, DEFAULT_TOKEN_URI);
    }

    /// The private key must never reach a log line. This is asserted rather
    /// than left to review because `derive(Debug)` is the default a future
    /// edit would reach for, and the leak it causes is invisible until
    /// someone reads production logs.
    #[test]
    fn debug_output_never_carries_key_material() {
        let rendered = format!("{:?}", key());
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(!rendered.contains("PRIVATE KEY"), "{rendered}");
        assert!(!rendered.contains("BEGIN"), "{rendered}");

        let token = AccessToken {
            token: "ya29.super-secret".to_owned(),
            expires_at: SystemTime::now(),
        };
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }

    #[test]
    fn an_oauth_client_secret_is_refused_by_name() {
        let raw = serde_json::json!({ "type": "authorized_user", "client_id": "x" }).to_string();
        let error = ServiceAccountKey::from_json(&raw).expect_err("wrong key type must be refused");
        assert!(
            error.to_string().contains("service_account"),
            "the message must name what was expected: {error}"
        );
    }

    #[test]
    fn a_double_escaped_private_key_says_so() {
        let raw = serde_json::json!({
            "type": "service_account",
            "client_email": "a@b.iam.gserviceaccount.com",
            "private_key": "-----BEGIN PRIVATE KEY-----\\nMIIE\\n-----END PRIVATE KEY-----",
        })
        .to_string();
        let error = ServiceAccountKey::from_json(&raw).expect_err("must be refused");
        assert!(
            error.to_string().contains("escaped a second time"),
            "{error}"
        );
    }

    /// The assertion is what Google verifies, so its three load-bearing claims
    /// are checked directly: it is RS256, its audience is the TOKEN endpoint
    /// (not the API), and it asks for the scope Vertex needs.
    #[test]
    fn the_assertion_carries_what_google_verifies() {
        let key = key();
        let jwt = key
            .assertion(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
            .expect("signing must succeed");

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "a JWT is three dot-separated segments");

        let decode = |segment: &str| -> serde_json::Value {
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segment).expect("base64url"))
                .expect("JSON")
        };
        let header = decode(parts[0]);
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["kid"], "test-key-id");

        let claims = decode(parts[1]);
        assert_eq!(claims["aud"], DEFAULT_TOKEN_URI);
        assert_eq!(claims["scope"], VERTEX_SCOPE);
        assert_eq!(
            claims["iss"],
            "zr-test@zr-vertex-test.iam.gserviceaccount.com"
        );
        assert_eq!(claims["iat"], 1_700_000_000_u64);
        assert_eq!(
            claims["exp"], 1_700_003_600_u64,
            "Google rejects an assertion living more than an hour"
        );
    }

    /// The signature must actually verify under the account's public key. A
    /// test that only checked the JWT's SHAPE would pass against a signature
    /// of the wrong bytes, which is the realistic way this breaks.
    #[test]
    fn the_assertion_signature_verifies_under_the_public_key() {
        use rsa::pkcs1v15::VerifyingKey;
        use rsa::signature::Verifier;

        let pem = include_str!("../tests/fixtures/vertex_test_key.pem");
        let private = RsaPrivateKey::from_pkcs8_pem(pem.trim()).expect("fixture parses");
        let verifying = VerifyingKey::<Sha256>::new(private.to_public_key());

        let jwt = key().assertion(SystemTime::now()).expect("signing");
        let (signing_input, signature) = jwt.rsplit_once('.').expect("three segments");
        let signature = rsa::pkcs1v15::Signature::try_from(
            URL_SAFE_NO_PAD
                .decode(signature)
                .expect("base64url")
                .as_ref(),
        )
        .expect("a signature");

        verifying
            .verify(signing_input.as_bytes(), &signature)
            .expect("the assertion must verify under the account's own public key");
    }

    // ---------------------------------------------------------------------
    // The expiry guard. These are the tests that make `REFRESH_SKEW` real.
    // ---------------------------------------------------------------------

    fn token_expiring_in(seconds: u64, now: SystemTime) -> AccessToken {
        AccessToken {
            token: "ya29.test".to_owned(),
            expires_at: now + Duration::from_secs(seconds),
        }
    }

    #[test]
    fn a_token_with_plenty_of_life_is_usable() {
        let now = SystemTime::now();
        assert!(token_expiring_in(3600, now).usable_at(now));
    }

    /// The guard, stated as a test: a token INSIDE the refresh margin is not
    /// served even though it has not technically expired.
    ///
    /// Disabling the skew (comparing against `expires_at` directly) makes this
    /// fail, which is the mutation check for it.
    #[test]
    fn a_token_inside_the_refresh_skew_is_not_served() {
        let now = SystemTime::now();
        assert!(
            !token_expiring_in(REFRESH_SKEW.as_secs() - 1, now).usable_at(now),
            "a token expiring inside the refresh margin must be treated as spent"
        );
        assert!(
            !token_expiring_in(0, now).usable_at(now),
            "a token expiring exactly now must not be served"
        );
    }

    #[test]
    fn a_token_exactly_at_the_margin_is_not_served() {
        let now = SystemTime::now();
        assert!(
            !token_expiring_in(REFRESH_SKEW.as_secs(), now).usable_at(now),
            "the comparison is strict: exactly at the margin is already too late"
        );
        assert!(
            token_expiring_in(REFRESH_SKEW.as_secs() + 1, now).usable_at(now),
            "one second past the margin is still usable — the guard must not be off by more \
             than it claims"
        );
    }

    /// An expiry so early that subtracting the skew would underflow must read
    /// as expired, not panic and not wrap into the far future.
    #[test]
    fn an_absurdly_early_expiry_is_not_usable() {
        let token = AccessToken {
            token: "ya29.test".to_owned(),
            expires_at: UNIX_EPOCH,
        };
        assert!(!token.usable_at(UNIX_EPOCH + Duration::from_secs(1)));
    }

    // ---------------------------------------------------------------------
    // The cache, driven over a real socket against a counting token endpoint.
    //
    // Written this way rather than against `usable_at` alone for the reason the
    // attestation tests in `wire/chat_completions.rs` give: the predicate being
    // correct proves nothing about the CACHE calling it. Every way this
    // regresses — the expiry check dropped from `token`, the second read after
    // the lock removed, a stale entry returned on a mint failure — leaves
    // `usable_at` perfectly correct and the behaviour broken.
    // ---------------------------------------------------------------------

    /// A token endpoint that counts requests and hands out tokens with a
    /// caller-chosen lifetime.
    async fn counting_token_endpoint(
        expires_in: u64,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let issued: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = Arc::clone(&issued);
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(move || {
                let recorder = Arc::clone(&recorder);
                async move {
                    let mut issued = recorder.lock().expect("recorder lock");
                    let token = format!("ya29.token-{}", issued.len() + 1);
                    issued.push(token.clone());
                    axum::Json(serde_json::json!({
                        "access_token": token,
                        "token_type": "Bearer",
                        "expires_in": expires_in,
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("token endpoint should bind");
        let address = listener
            .local_addr()
            .expect("token endpoint should report its address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}/token"), issued)
    }

    fn cache_for(token_uri: &str) -> TokenCache {
        let key = ServiceAccountKey::from_json(&test_key_json(token_uri))
            .expect("the fixture key must parse");
        TokenCache::new(key, reqwest::Client::new())
    }

    /// A live token is served from the cache — the endpoint is dialled once and
    /// not again. This is the property the cache exists for.
    #[tokio::test]
    async fn a_live_token_is_served_from_cache_without_a_second_mint() {
        let (uri, issued) = counting_token_endpoint(3600).await;
        let cache = cache_for(&uri);
        let now = SystemTime::now();

        let first = cache.token(now).await.expect("first mint");
        let second = cache
            .token(now + Duration::from_secs(60))
            .await
            .expect("second call");

        assert_eq!(first.secret(), second.secret());
        assert_eq!(
            issued.lock().expect("recorder").len(),
            1,
            "a token with an hour of life must not be re-minted a minute later"
        );
    }

    /// **The refresh path.** A cached token that has aged past the refresh
    /// margin is replaced rather than served.
    ///
    /// Deleting the expiry test in `TokenCache::token` — returning any cached
    /// token unconditionally — makes this fail, which is its mutation check.
    #[tokio::test]
    async fn an_expired_cached_token_is_refreshed_rather_than_served() {
        let (uri, issued) = counting_token_endpoint(3600).await;
        let cache = cache_for(&uri);
        let now = SystemTime::now();

        let first = cache.token(now).await.expect("first mint");
        // Well past expiry: an hour and a minute later.
        let second = cache
            .token(now + Duration::from_secs(3660))
            .await
            .expect("refresh");

        assert_ne!(
            first.secret(),
            second.secret(),
            "an expired token must be replaced, not handed out again"
        );
        assert_eq!(issued.lock().expect("recorder").len(), 2);
    }

    /// The refresh happens at the MARGIN, not at expiry. A token with four
    /// minutes of life left is inside `REFRESH_SKEW` and must be replaced even
    /// though it has not technically expired.
    ///
    /// This is the case a `now >= expires_at` comparison would get wrong, and
    /// it is the realistic one: a request that grabs a token with seconds left
    /// is rejected by Google after the TLS handshake, and the router reads that
    /// 401 as an upstream failure rather than as a credential problem.
    #[tokio::test]
    async fn a_token_inside_the_margin_is_refreshed_before_it_expires() {
        let (uri, issued) = counting_token_endpoint(3600).await;
        let cache = cache_for(&uri);
        let now = SystemTime::now();

        cache.token(now).await.expect("first mint");
        cache
            .token(now + Duration::from_secs(3600 - 240))
            .await
            .expect("refresh inside the margin");

        assert_eq!(
            issued.lock().expect("recorder").len(),
            2,
            "four minutes of remaining life is inside the five-minute margin"
        );
    }

    /// A token endpoint that returns a lifetime shorter than the refresh margin
    /// must not poison the cache: the token is returned to the caller that paid
    /// for it and the cell is left empty.
    #[tokio::test]
    async fn a_token_born_inside_the_margin_is_not_cached() {
        let (uri, issued) = counting_token_endpoint(60).await;
        let cache = cache_for(&uri);
        let now = SystemTime::now();

        let first = cache.token(now).await.expect("first mint");
        assert_eq!(first.secret(), "ya29.token-1");
        let second = cache.token(now).await.expect("second mint");
        assert_eq!(
            second.secret(),
            "ya29.token-2",
            "a token too short-lived to cache must be re-minted, never re-served"
        );
        assert_eq!(issued.lock().expect("recorder").len(), 2);
    }

    /// Concurrent callers arriving at a cold cache mint ONCE between them.
    ///
    /// This is what the mutex across the await buys, and it is the reason that
    /// lock is held the way it is. Without the re-read after acquiring it, each
    /// queued caller would mint its own token the moment it got in.
    #[tokio::test]
    async fn concurrent_callers_share_one_mint() {
        let (uri, issued) = counting_token_endpoint(3600).await;
        let cache = Arc::new(cache_for(&uri));
        let now = SystemTime::now();

        let calls = (0..8).map(|_| {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move {
                cache
                    .token(now)
                    .await
                    .map(|token| token.secret().to_owned())
            })
        });
        let results: Vec<String> = futures_util::future::join_all(calls)
            .await
            .into_iter()
            .map(|joined| joined.expect("task").expect("mint"))
            .collect();

        assert_eq!(
            issued.lock().expect("recorder").len(),
            1,
            "eight concurrent callers must not stampede the token endpoint"
        );
        assert!(
            results.iter().all(|token| token == &results[0]),
            "every caller must receive the same token: {results:?}"
        );
    }

    /// A refusal from the token endpoint surfaces as a refusal, carrying the
    /// status and body — not as an empty token that would 401 upstream later.
    #[tokio::test]
    async fn a_refused_assertion_is_reported_as_a_refusal() {
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    r#"{"error":"invalid_grant","error_description":"Invalid JWT Signature."}"#,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cache = cache_for(&format!("http://{address}/token"));
        let error = cache
            .token(SystemTime::now())
            .await
            .expect_err("a 400 must not become a token");
        match error {
            TokenError::Refused { status, body } => {
                assert_eq!(status, 400);
                assert!(body.contains("invalid_grant"), "{body}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}
