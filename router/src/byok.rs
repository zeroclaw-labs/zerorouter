//! Bring-your-own-key: customers attach their own upstream provider
//! credentials, ZeroRouter dispatches on them, and charges 5% of what the same
//! usage would have cost at catalog rates.
//!
//! # What is different about the secrets in this module
//!
//! Every other secret ZeroRouter stores is stored as a DIGEST — inference keys,
//! session tokens, device codes (`docs/SECURITY.md`, "Token namespaces"). A
//! digest is enough because ZeroRouter only ever needs to answer "is this the
//! same string I was given before?".
//!
//! A BYOK key is the first secret that must be REPLAYED: the whole point is to
//! present it to Anthropic or OpenAI on the customer's behalf, so it has to
//! come back out in plaintext at dispatch time. It is therefore encrypted
//! rather than hashed, and that is a genuine widening of the blast radius of a
//! database compromise, which is why the key that opens it lives outside the
//! database in a separate secret. `docs/SECURITY.md` records the exception
//! rather than leaving the "only digests" claim quietly false.
//!
//! The secret is also not ZeroRouter's. It is the customer's account with a
//! third party, so a defect here spends someone else's money at someone else's
//! vendor, and no amount of care on ZeroRouter's own balance would contain it.
//!
//! # Envelope
//!
//! `BYOK_ENCRYPTION_KEY` is the key-encryption key (KEK). Each stored
//! credential gets its own randomly generated 32-byte data-encryption key
//! (DEK): the credential is sealed under the DEK, and the DEK is sealed under
//! the KEK. Both seals are AES-256-GCM with a fresh random 96-bit nonce.
//!
//! An envelope rather than sealing straight under the KEK, for two reasons that
//! are about the future rather than about today's arithmetic:
//!
//! * **Rotation is reachable.** Rotating the KEK re-wraps the DEKs — a fixed
//!   amount of work per row that never touches the credential ciphertexts. A
//!   single-key design would have to decrypt and re-encrypt every customer's
//!   credential under the new key, which is a migration nobody runs, which is
//!   how a key ends up never being rotated.
//! * **One key encrypts one secret.** The KEK only ever seals 32-byte DEKs, so
//!   the volume of data under any single AES-GCM key stays trivially inside the
//!   safe bound for random nonces no matter how many customers attach keys.
//!
//! Both seals bind `(user_id, provider)` as additional authenticated data. AAD
//! is not encrypted — it is authenticated — so a ciphertext lifted out of one
//! row and written into another user's row, or the same user's row for a
//! different provider, fails to open rather than decrypting into a credential
//! the thief can then spend. Without it, a database write is enough to make
//! ZeroRouter dispatch one tenant's key on another tenant's request.

use std::{env, fmt, sync::Arc};

use anyhow::{Context, Result, bail};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::sqlx::{self, PgPool};

pub const ENCRYPTION_KEY_ENV: &str = "BYOK_ENCRYPTION_KEY";

/// Length of the key material this module works in, in bytes: AES-256 for both
/// the KEK and every DEK.
const KEY_BYTES: usize = 32;

/// The envelope format marker, so a future format can be told apart from this
/// one by reading rather than by guessing from a length.
const ENVELOPE_VERSION: u8 = 1;

/// AES-256-GCM's authentication tag length. `ring` appends it to the
/// ciphertext, so a sealed DEK is `KEY_BYTES + TAG_BYTES`.
const TAG_BYTES: usize = 16;

/// The fee ZeroRouter charges on BYOK traffic, as a fraction of what the same
/// usage would have cost at catalog rates.
///
/// A `Decimal` constructed from integers rather than a float literal: this
/// number multiplies money, and `0.05f64` is not 5%. `Decimal::new(5, 2)` is
/// exactly 5/100 and the arithmetic below stays in the same exact type the rest
/// of the money path uses.
#[must_use]
pub fn fee_rate() -> Decimal {
    Decimal::new(5, 2)
}

/// The full catalog price, for a request that dispatches on ZeroRouter's own
/// credentials. Named rather than written as a bare `Decimal::ONE` at the call
/// sites so the two arms of the fee decision read as the same decision.
#[must_use]
pub fn house_rate() -> Decimal {
    Decimal::ONE
}

/// Apply a fee rate to a catalog-priced amount.
///
/// Deliberately does NOT round. Nothing in ZeroRouter's money path rounds —
/// `crate::openai::usage_cost` returns an exact `Decimal` and the balance debit
/// consumes it as-is — so rounding here would make BYOK the only place where a
/// charge is not the arithmetic it claims to be. `checked_mul` for the same
/// reason `usage_cost` uses checked arithmetic: an overflow must become a
/// metering failure, never a wrapped charge.
#[must_use]
pub fn apply_fee(catalog_cost: Decimal, rate: Decimal) -> Option<Decimal> {
    catalog_cost.checked_mul(rate)
}

/// The key-encryption key this deployment seals BYOK credentials under.
///
/// Constructed only through [`Keyring::from_env`], so a `Keyring` existing at
/// all is the proof that the feature is configured. Every BYOK surface takes an
/// `Option<&Keyring>` and refuses when it is `None` — there is no code path
/// that stores or reads a credential without one.
pub struct Keyring {
    kek: LessSafeKey,
}

/// Never derive `Debug` here, and never print the key material. The manual impl
/// is the same discipline `StripeSettings` applies in `crate::web`: a struct
/// holding a secret must be safe to put in a log line by accident.
impl fmt::Debug for Keyring {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Keyring")
            .field("kek", &"<scrubbed>")
            .finish()
    }
}

impl Keyring {
    /// Read the KEK from the environment.
    ///
    /// `Ok(None)` is the feature being off, and an error is a misconfiguration
    /// — the same contract the web plane states in `crate::web`:
    /// **misconfiguration aborts, absence disables**. A deployment that has not
    /// provisioned the secret serves everything else exactly as before and
    /// refuses attach attempts with a clear reason; a deployment that has
    /// provisioned it WRONG must not start, because the alternative is a router
    /// that accepts a customer's key and cannot ever read it back.
    ///
    /// The encoding is 64 lowercase hex characters, and only that. `openssl
    /// rand -hex 32` produces it. Accepting base64 as well would mean a
    /// 44-character value and a 64-character value are both valid, so a
    /// truncated or double-encoded secret could land inside one of the two
    /// shapes and be accepted as a DIFFERENT key than the operator generated —
    /// which is undetectable until the day a customer's credential fails to
    /// open. One encoding makes every wrong value a startup error.
    pub fn from_env() -> Result<Option<Arc<Self>>> {
        let Some(raw) = env::var(ENCRYPTION_KEY_ENV)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        Ok(Some(Arc::new(Self::from_hex(&raw)?)))
    }

    /// Build a keyring from a hex key without going through the environment.
    ///
    /// Gated on the `testing` feature for the same reason
    /// [`crate::providers::ProviderCandidate::with_provider`] is: production
    /// reaches a keyring exactly one way, through [`Self::from_env`], and a
    /// binary that moves money should carry no second way to install the key
    /// that opens customers' vendor credentials.
    #[cfg(feature = "testing")]
    pub fn from_hex_for_tests(raw: &str) -> Result<Self> {
        Self::from_hex(raw)
    }

    fn from_hex(raw: &str) -> Result<Self> {
        if raw.len() != KEY_BYTES * 2 {
            bail!(
                "{ENCRYPTION_KEY_ENV} must be {} hex characters ({KEY_BYTES} bytes); \
                 got {} characters. Generate one with `openssl rand -hex 32`",
                KEY_BYTES * 2,
                raw.len()
            );
        }
        if !raw
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            bail!(
                "{ENCRYPTION_KEY_ENV} must be lowercase hex; \
                 generate one with `openssl rand -hex 32`"
            );
        }
        let bytes =
            hex::decode(raw).with_context(|| format!("{ENCRYPTION_KEY_ENV} is not valid hex"))?;
        Self::from_bytes(&bytes)
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let unbound = UnboundKey::new(&AES_256_GCM, bytes)
            .map_err(|_| anyhow::anyhow!("{ENCRYPTION_KEY_ENV} is not a usable AES-256 key"))?;
        Ok(Self {
            kek: LessSafeKey::new(unbound),
        })
    }

    /// Seal `credential` into a storable envelope bound to `(user_id, provider)`.
    pub fn seal(&self, user_id: Uuid, provider: &str, credential: &str) -> Result<Vec<u8>> {
        let aad = aad_bytes(user_id, provider);

        let dek_bytes: [u8; KEY_BYTES] = rand::random();
        let dek = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &dek_bytes)
                .map_err(|_| anyhow::anyhow!("generated data key is unusable"))?,
        );

        let dek_nonce: [u8; NONCE_LEN] = rand::random();
        let mut wrapped_dek = dek_bytes.to_vec();
        self.kek
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(dek_nonce),
                Aad::from(&aad),
                &mut wrapped_dek,
            )
            .map_err(|_| anyhow::anyhow!("wrapping the data key failed"))?;

        let data_nonce: [u8; NONCE_LEN] = rand::random();
        let mut sealed = credential.as_bytes().to_vec();
        dek.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(data_nonce),
            Aad::from(&aad),
            &mut sealed,
        )
        .map_err(|_| anyhow::anyhow!("sealing the credential failed"))?;

        // version | dek nonce | wrapped dek (+tag) | data nonce | ciphertext (+tag)
        let mut envelope =
            Vec::with_capacity(1 + NONCE_LEN + KEY_BYTES + TAG_BYTES + NONCE_LEN + sealed.len());
        envelope.push(ENVELOPE_VERSION);
        envelope.extend_from_slice(&dek_nonce);
        envelope.extend_from_slice(&wrapped_dek);
        envelope.extend_from_slice(&data_nonce);
        envelope.extend_from_slice(&sealed);
        Ok(envelope)
    }

    /// Open an envelope sealed by [`Self::seal`] for the same
    /// `(user_id, provider)`.
    ///
    /// Every failure is one error, deliberately: a wrong KEK, a tampered
    /// ciphertext, and an envelope lifted from another tenant's row are all
    /// "this did not open", and telling them apart in the message would tell an
    /// attacker with database read access which of their guesses was closer.
    pub fn open(&self, user_id: Uuid, provider: &str, envelope: &[u8]) -> Result<String> {
        let aad = aad_bytes(user_id, provider);
        let wrapped_len = KEY_BYTES + TAG_BYTES;
        let header_len = 1 + NONCE_LEN + wrapped_len + NONCE_LEN;
        if envelope.len() < header_len || envelope[0] != ENVELOPE_VERSION {
            bail!("stored BYOK credential could not be opened");
        }

        let dek_nonce: [u8; NONCE_LEN] = envelope[1..1 + NONCE_LEN]
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored BYOK credential could not be opened"))?;
        let mut wrapped_dek = envelope[1 + NONCE_LEN..1 + NONCE_LEN + wrapped_len].to_vec();
        let dek_bytes = self
            .kek
            .open_in_place(
                Nonce::assume_unique_for_key(dek_nonce),
                Aad::from(&aad),
                &mut wrapped_dek,
            )
            .map_err(|_| anyhow::anyhow!("stored BYOK credential could not be opened"))?;
        let dek = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, dek_bytes)
                .map_err(|_| anyhow::anyhow!("stored BYOK credential could not be opened"))?,
        );

        let data_nonce_at = 1 + NONCE_LEN + wrapped_len;
        let data_nonce: [u8; NONCE_LEN] = envelope[data_nonce_at..data_nonce_at + NONCE_LEN]
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored BYOK credential could not be opened"))?;
        let mut sealed = envelope[header_len..].to_vec();
        let plaintext = dek
            .open_in_place(
                Nonce::assume_unique_for_key(data_nonce),
                Aad::from(&aad),
                &mut sealed,
            )
            .map_err(|_| anyhow::anyhow!("stored BYOK credential could not be opened"))?;
        String::from_utf8(plaintext.to_vec())
            .map_err(|_| anyhow::anyhow!("stored BYOK credential could not be opened"))
    }
}

/// The authenticated-but-not-encrypted binding for one stored credential.
///
/// A separator that cannot occur in a UUID's text form keeps
/// `(user, "ab") | (user, "c")` from colliding with `(user, "a") | (user, "bc")`
/// — the classic concatenation ambiguity, which here would let a credential be
/// replayed against a provider it was not attached for.
fn aad_bytes(user_id: Uuid, provider: &str) -> Vec<u8> {
    format!("zerorouter:byok:v1:{user_id}:{provider}").into_bytes()
}

/// What the portal may show about a stored credential: enough for a human to
/// recognize which key this is, and nothing an attacker can spend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredKey {
    pub provider: String,
    /// A SHA-256 prefix of the credential. Stable across re-attachment of the
    /// same key, which is what makes it useful for support ("is the key you
    /// pasted the one we hold?") without disclosing anything.
    pub fingerprint: String,
    /// The credential's last four characters — the affordance every provider
    /// dashboard uses, and how a customer tells two of their own keys apart.
    pub last4: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A stable, non-reversible label for a credential.
///
/// Truncated to 16 hex characters (64 bits). It is an identifier, not an
/// authenticator: nothing is ever admitted because a fingerprint matched, so
/// collision resistance only has to beat accidental collision within one
/// customer's handful of keys, and a shorter string is one a human can actually
/// compare by eye against a provider dashboard.
#[must_use]
pub fn fingerprint(credential: &str) -> String {
    hex::encode(Sha256::digest(credential.as_bytes()))
        .chars()
        .take(16)
        .collect()
}

/// The trailing characters a customer recognizes their key by.
///
/// Counted in `char`s rather than bytes so a non-ASCII credential cannot panic
/// on a split inside a multi-byte sequence, and capped at the length of the
/// credential itself so a short string yields a short label rather than
/// disclosing that it was short by erroring.
#[must_use]
pub fn last4(credential: &str) -> String {
    let chars: Vec<char> = credential.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

/// Refuse a credential that cannot be a provider key before it is ever sealed.
///
/// The bounds are shape checks, not a claim to know any vendor's format:
/// providers change their prefixes and lengths, and a router that hardcoded
/// them would reject a valid new-format key. What it does refuse is the set of
/// values that are certainly mistakes — blank, whitespace-bearing (a paste that
/// caught a newline or a shell prompt), implausibly short, or implausibly long
/// — because each of those becomes an upstream 401 the customer has no way to
/// diagnose from the outside.
pub fn validate_credential(credential: &str) -> Result<(), &'static str> {
    if credential.is_empty() {
        return Err("Paste the provider API key.");
    }
    if credential.chars().any(char::is_whitespace) {
        return Err(
            "That key contains whitespace — check for a stray newline or space in the paste.",
        );
    }
    if credential.chars().count() < 16 {
        return Err("That does not look like a provider API key: it is too short.");
    }
    if credential.chars().count() > 512 {
        return Err("That does not look like a provider API key: it is too long.");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Attach (or replace) `user_id`'s credential for `provider`.
///
/// Replacement rather than a second row: the table's UNIQUE `(user_id,
/// provider)` makes "which of my two Anthropic keys is being used?" a question
/// that cannot be asked, and re-pasting is how a customer rotates.
///
/// `last_used_at` resets to NULL on replacement, because it describes the
/// credential rather than the row — carrying the old key's timestamp forward
/// would tell the customer their new key had been used before it ever was.
pub async fn attach_key(
    pool: &PgPool,
    keyring: &Keyring,
    user_id: Uuid,
    provider: &str,
    credential: &str,
) -> Result<StoredKey> {
    let sealed = keyring.seal(user_id, provider, credential)?;
    let fingerprint = fingerprint(credential);
    let last4 = last4(credential);
    let (created_at,) = sqlx::query_as::<_, (chrono::DateTime<chrono::Utc>,)>(
        r#"
        INSERT INTO byok_provider_keys (
            user_id, provider, sealed_credential, fingerprint, last4
        )
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (user_id, provider) DO UPDATE
        SET sealed_credential = EXCLUDED.sealed_credential,
            fingerprint       = EXCLUDED.fingerprint,
            last4             = EXCLUDED.last4,
            created_at        = NOW(),
            last_used_at      = NULL
        RETURNING created_at
        "#,
    )
    .bind(user_id)
    .bind(provider)
    .bind(&sealed)
    .bind(&fingerprint)
    .bind(&last4)
    .fetch_one(pool)
    .await?;
    Ok(StoredKey {
        provider: provider.to_owned(),
        fingerprint,
        last4,
        created_at,
        last_used_at: None,
    })
}

/// Every credential this user has attached, without opening any of them.
///
/// The sealed column is deliberately not in the SELECT list. A listing has no
/// use for it, and not fetching it means no code path between the database and
/// the portal response is ever holding one.
pub async fn list_keys(pool: &PgPool, user_id: Uuid) -> Result<Vec<StoredKey>, sqlx::Error> {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            chrono::DateTime<chrono::Utc>,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
    >(
        r#"
        SELECT provider, fingerprint, last4, created_at, last_used_at
        FROM byok_provider_keys
        WHERE user_id = $1
        ORDER BY provider
        "#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(provider, fingerprint, last4, created_at, last_used_at)| StoredKey {
                provider,
                fingerprint,
                last4,
                created_at,
                last_used_at,
            },
        )
        .collect())
}

/// Detach one credential. Returns whether a row was removed.
///
/// A hard DELETE, unlike `api_keys` which are disabled and kept: nothing
/// references this row (usage history records the PROVIDER and a `byok` flag,
/// never the credential), and a customer asking ZeroRouter to stop holding
/// their vendor credential should be left holding nothing.
pub async fn remove_key(pool: &PgPool, user_id: Uuid, provider: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM byok_provider_keys WHERE user_id = $1 AND provider = $2")
        .bind(user_id)
        .bind(provider)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// The providers this user holds a credential for, without opening any of them.
///
/// Admission needs this and only this: the fee a request reserves against
/// depends on WHICH providers are covered, not on the credentials themselves.
/// Keeping the two lookups separate means the reservation path never decrypts.
pub async fn covered_providers(pool: &PgPool, user_id: Uuid) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT provider FROM byok_provider_keys WHERE user_id = $1")
        .bind(user_id)
        .fetch_all(pool)
        .await
}

/// Open every credential this user has attached, for one request's dispatch.
///
/// Returns `(provider, credential)` pairs. The plaintexts live only in the
/// route being built and are dropped with it — nothing caches them across
/// requests, which is what keeps a rotated or detached key from continuing to
/// dispatch.
///
/// A row that fails to open is DROPPED with a warning rather than failing the
/// request. That is the one place this module chooses availability, and it is
/// bounded: the customer loses their own BYOK dispatch for that provider and
/// the request either serves on a house candidate at full catalog price or
/// finds no candidate at all. The alternative — failing the whole request —
/// would turn one unreadable row into a total outage for that customer, and the
/// unreadable case is an operator error (a rotated KEK) rather than an attack.
pub async fn open_credentials(
    pool: &PgPool,
    keyring: &Keyring,
    user_id: Uuid,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, Vec<u8>)>(
        "SELECT provider, sealed_credential FROM byok_provider_keys WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let mut opened = Vec::with_capacity(rows.len());
    for (provider, sealed) in rows {
        match keyring.open(user_id, &provider, &sealed) {
            Ok(credential) => opened.push((provider, credential)),
            Err(_) => {
                // No credential, no ciphertext, no user id in the event: the
                // provider and the fact of the failure are the whole
                // diagnosable content, and they are enough to tell an operator
                // a KEK rotation left rows behind.
                tracing::warn!(
                    provider = %provider,
                    "a stored BYOK credential could not be opened; dispatching without it"
                );
            }
        }
    }
    Ok(opened)
}

/// Record that a credential was dispatched on.
///
/// Best-effort and off the request's critical path by design: this is a display
/// field on a settings page, and failing a customer's inference because a
/// timestamp did not update would be an absurd trade. The caller logs and
/// continues.
pub async fn mark_used(pool: &PgPool, user_id: Uuid, provider: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE byok_provider_keys SET last_used_at = NOW() WHERE user_id = $1 AND provider = $2",
    )
    .bind(user_id)
    .bind(provider)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_keyring() -> Keyring {
        Keyring::from_bytes(&[7u8; KEY_BYTES]).expect("a 32-byte key must build a keyring")
    }

    #[test]
    fn a_sealed_credential_round_trips() {
        let keyring = test_keyring();
        let user = Uuid::new_v4();
        let sealed = keyring
            .seal(user, "anthropic", "sk-ant-secret-value")
            .expect("sealing must succeed");
        assert_eq!(
            keyring
                .open(user, "anthropic", &sealed)
                .expect("opening must succeed"),
            "sk-ant-secret-value"
        );
    }

    #[test]
    fn the_envelope_does_not_contain_the_plaintext() {
        // The property the storage mutation test asserts against a real column,
        // asserted here against the bytes themselves so a change to the
        // envelope format cannot quietly start storing plaintext.
        let keyring = test_keyring();
        let user = Uuid::new_v4();
        let secret = "sk-ant-a-very-recognizable-secret";
        let sealed = keyring.seal(user, "anthropic", secret).expect("seal");
        let haystack = String::from_utf8_lossy(&sealed);
        assert!(
            !haystack.contains(secret),
            "the sealed envelope must not contain the credential"
        );
        assert!(
            !haystack.contains("a-very-recognizable-secret"),
            "the sealed envelope must not contain any run of the credential"
        );
    }

    #[test]
    fn two_seals_of_one_credential_differ() {
        // Fresh nonces and a fresh DEK per seal. Equal ciphertexts would let
        // anyone with database read access tell which customers pasted the same
        // key — including whether a customer re-attached the key they already
        // had.
        let keyring = test_keyring();
        let user = Uuid::new_v4();
        let first = keyring
            .seal(user, "openai", "sk-the-same-key")
            .expect("seal");
        let second = keyring
            .seal(user, "openai", "sk-the-same-key")
            .expect("seal");
        assert_ne!(first, second);
    }

    #[test]
    fn an_envelope_does_not_open_for_another_tenant_or_provider() {
        // The AAD binding. Without it, a database write moving one row's
        // ciphertext into another row would make ZeroRouter dispatch one
        // customer's key on another customer's request.
        let keyring = test_keyring();
        let owner = Uuid::new_v4();
        let thief = Uuid::new_v4();
        let sealed = keyring
            .seal(owner, "anthropic", "sk-ant-secret")
            .expect("seal");
        assert!(
            keyring.open(thief, "anthropic", &sealed).is_err(),
            "another user's id must not open the envelope"
        );
        assert!(
            keyring.open(owner, "openai", &sealed).is_err(),
            "another provider must not open the envelope"
        );
        assert!(
            keyring.open(owner, "anthropic", &sealed).is_ok(),
            "the owning pair must still open it"
        );
    }

    #[test]
    fn a_different_kek_does_not_open_the_envelope() {
        let sealed = test_keyring()
            .seal(Uuid::nil(), "anthropic", "sk-ant-secret")
            .expect("seal");
        let other = Keyring::from_bytes(&[9u8; KEY_BYTES]).expect("keyring");
        assert!(other.open(Uuid::nil(), "anthropic", &sealed).is_err());
    }

    #[test]
    fn a_tampered_envelope_does_not_open() {
        let keyring = test_keyring();
        let mut sealed = keyring
            .seal(Uuid::nil(), "anthropic", "sk-ant-secret")
            .expect("seal");
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert!(keyring.open(Uuid::nil(), "anthropic", &sealed).is_err());
    }

    #[test]
    fn the_kek_is_accepted_only_as_lowercase_hex_of_the_right_length() {
        assert!(Keyring::from_hex(&"a".repeat(64)).is_ok());
        // Too short, too long, uppercase, and non-hex all abort rather than
        // being coerced into some key.
        assert!(Keyring::from_hex(&"a".repeat(63)).is_err());
        assert!(Keyring::from_hex(&"a".repeat(65)).is_err());
        assert!(Keyring::from_hex(&"A".repeat(64)).is_err());
        assert!(Keyring::from_hex(&"z".repeat(64)).is_err());
        // A base64 secret of the right byte length is the plausible operator
        // mistake, and it must not be accepted as some other key.
        assert!(Keyring::from_hex("YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXoxMjM0NTY=").is_err());
    }

    #[test]
    fn the_fee_is_five_percent_and_exact() {
        // The arithmetic a customer is charged. Written against values whose
        // exact answer is known by hand, because "about 5%" is not a thing a
        // ledger can record.
        let catalog = Decimal::new(123_456, 6); // $0.123456
        assert_eq!(
            apply_fee(catalog, fee_rate()).expect("the fee must compute"),
            Decimal::new(61_728, 7), // $0.0061728 — 0.123456 / 20, exactly
        );
        assert_eq!(
            apply_fee(catalog, house_rate()).expect("the house rate must compute"),
            catalog,
            "the house rate must leave a catalog price untouched"
        );
        assert_eq!(
            apply_fee(Decimal::ZERO, fee_rate()).expect("zero must compute"),
            Decimal::ZERO
        );
    }

    #[test]
    fn the_fee_never_exceeds_the_catalog_price() {
        // The direction that matters: a fee arm that ever charged MORE than the
        // house price would be worse than no BYOK at all.
        for cents in [1_i64, 7, 99, 100_000, 987_654_321] {
            let catalog = Decimal::new(cents, 6);
            let fee = apply_fee(catalog, fee_rate()).expect("the fee must compute");
            assert!(
                fee < catalog,
                "the BYOK fee must be below catalog for {catalog}"
            );
            assert!(fee >= Decimal::ZERO, "the BYOK fee must never be negative");
        }
    }

    #[test]
    fn fingerprints_are_stable_and_last4_is_the_tail() {
        assert_eq!(fingerprint("sk-ant-abcd"), fingerprint("sk-ant-abcd"));
        assert_ne!(fingerprint("sk-ant-abcd"), fingerprint("sk-ant-abce"));
        assert_eq!(fingerprint("sk-ant-abcd").len(), 16);
        assert_eq!(last4("sk-ant-abcd"), "abcd");
        // Shorter than four characters must not panic, and a multi-byte
        // credential must not split a character.
        assert_eq!(last4("ab"), "ab");
        assert_eq!(last4(""), "");
        assert_eq!(last4("kéy—ünicode"), "code");
    }

    #[test]
    fn implausible_credentials_are_refused_before_they_are_sealed() {
        assert!(validate_credential("sk-ant-api03-0123456789abcdef").is_ok());
        assert!(validate_credential("").is_err());
        assert!(validate_credential("short").is_err());
        // The paste that caught a newline — the most common real mistake, and
        // one that otherwise becomes an undiagnosable upstream 401.
        assert!(validate_credential("sk-ant-api03-0123456789abcdef\n").is_err());
        assert!(validate_credential("sk-ant api03-0123456789abcdef").is_err());
        assert!(validate_credential(&"a".repeat(513)).is_err());
    }

    #[test]
    fn the_aad_cannot_be_confused_by_concatenation() {
        // (user, "ab") and (user, "a") + "b" must not produce the same AAD, or a
        // credential could open for a provider it was never attached for.
        let user = Uuid::nil();
        assert_ne!(aad_bytes(user, "ab"), aad_bytes(user, "a:b"));
        assert_ne!(aad_bytes(user, "anthropic"), aad_bytes(user, "anthropi"));
    }
}
