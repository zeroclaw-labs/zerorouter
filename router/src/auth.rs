use std::{collections::HashMap, time::Duration};

use sha2::{Digest, Sha256};
use tokio::{sync::RwLock, time::Instant};
use uuid::Uuid;

use crate::{
    priority::Priority,
    sqlx::{self, PgPool},
};

const KEY_PREFIX: &str = "zcr_";
const KEY_BYTES: usize = 32;
const KEY_HEX_LENGTH: usize = KEY_BYTES * 2;
const CACHE_TTL: Duration = Duration::from_secs(30);
const DUMMY_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Clone, Debug)]
pub struct AuthenticatedKey {
    pub id: Uuid,
    pub user_id: Uuid,
    /// The key's `default_priority` (migration 0004), carried on the
    /// authenticated identity because it must be known before candidate
    /// ordering — which runs ahead of the admission SELECT, so admission
    /// cannot fetch it in time. Rides the same 30-second cache as the key
    /// itself: a changed default has exactly the staleness contract of a
    /// disablement. `None` means balanced.
    pub default_priority: Option<Priority>,
}

#[derive(Clone, Debug)]
struct CacheEntry {
    key: AuthenticatedKey,
    expires_at: Instant,
}

#[derive(Debug)]
pub enum AuthenticationError {
    Invalid,
    Database(sqlx::Error),
}

pub struct KeyAuthenticator {
    cache: RwLock<HashMap<String, CacheEntry>>,
}

impl KeyAuthenticator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
        }
    }

    pub async fn authenticate(
        &self,
        pool: &PgPool,
        token: &str,
    ) -> Result<AuthenticatedKey, AuthenticationError> {
        let syntactically_valid = valid_key_shape(token);
        let hash = hash_api_key(token);
        let now = Instant::now();

        let cached_key = if syntactically_valid {
            let cache = self.cache.read().await;
            cache
                .get(&hash)
                .filter(|entry| entry.expires_at > now)
                .map(|entry| entry.key.clone())
        } else {
            None
        };
        if let Some(key) = cached_key {
            return Ok(key);
        }

        let row = sqlx::query_as::<_, (Uuid, Uuid, String, bool, Option<String>)>(
            r#"
            SELECT id, user_id, key_hash, disabled, default_priority
            FROM api_keys
            WHERE key_hash = $1
            "#,
        )
        .bind(&hash)
        .fetch_optional(pool)
        .await
        .map_err(AuthenticationError::Database)?;

        let stored_hash = row.as_ref().map_or(DUMMY_HASH, |record| record.2.as_str());
        let hashes_match = constant_time_eq(stored_hash, &hash);
        let Some((id, user_id, _, _, default_priority)) =
            row.filter(|row| syntactically_valid && hashes_match && !row.3)
        else {
            return Err(AuthenticationError::Invalid);
        };

        let key = AuthenticatedKey {
            id,
            user_id,
            // The column is CHECK-constrained to the three keywords, so a
            // `None` here is a genuine NULL — balanced — not a parse loss.
            default_priority: default_priority.as_deref().and_then(Priority::from_keyword),
        };
        let mut cache = self.cache.write().await;
        cache.retain(|_, entry| entry.expires_at > now);
        cache.insert(
            hash,
            CacheEntry {
                key: key.clone(),
                expires_at: now + CACHE_TTL,
            },
        );
        Ok(key)
    }
}

impl Default for KeyAuthenticator {
    fn default() -> Self {
        Self::new()
    }
}

#[must_use]
pub fn generate_api_key() -> String {
    let bytes: [u8; KEY_BYTES] = rand::random();
    format!("{KEY_PREFIX}{}", hex::encode(bytes))
}

#[must_use]
pub fn hash_api_key(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn valid_key_shape(token: &str) -> bool {
    token.len() == KEY_PREFIX.len() + KEY_HEX_LENGTH
        && token.starts_with(KEY_PREFIX)
        && token[KEY_PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
}

#[allow(clippy::needless_bitwise_bool)]
fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let max_len = left.len().max(right.len());
    let mut difference = left.len() ^ right.len();
    for index in 0..max_len {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_key_has_required_shape_and_entropy_length() {
        let key = generate_api_key();
        assert!(valid_key_shape(&key));
        assert_eq!(key.len(), 68);
        assert_ne!(key, generate_api_key());
    }

    #[test]
    fn hashing_is_stable_lowercase_sha256() {
        let hash = hash_api_key("zcr_test");
        assert_eq!(hash.len(), 64);
        assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(hash, hash_api_key("zcr_test"));
    }

    #[test]
    fn constant_time_comparison_handles_length_mismatch() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abc0"));
    }

    #[test]
    fn revocation_cache_ttl_stays_within_contract() {
        assert!(CACHE_TTL <= Duration::from_secs(60));
    }
}
