//! Migration 0030: the per-key retention switch column (`api_keys.retention_policy`).
//!
//! Gated on `DATABASE_URL` like the other DB suites: unset → the test returns
//! early (skips) rather than failing.

use std::str::FromStr;

use sqlx_core::{query::query, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use uuid::Uuid;
use zerorouter::auth::{generate_api_key, hash_api_key};
use zerorouter::db::migrate;

async fn connect() -> Option<PgPool> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let options = PgConnectOptions::from_str(&database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");
    Some(pool)
}

/// Insert a user and return its id.
async fn seed_user(pool: &PgPool) -> Uuid {
    let user_id = Uuid::new_v4();
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("retention-policy-{user_id}@example.invalid"))
        .execute(pool)
        .await
        .expect("test user must insert");
    user_id
}

#[tokio::test]
async fn a_key_created_without_a_policy_backfills_to_zdr_only() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = seed_user(&pool).await;
    let key_id = Uuid::new_v4();
    // No `retention_policy` named: the column's NOT NULL DEFAULT is what every
    // pre-existing key gets under the migration, and it is the SAFE end of the
    // switch, `zdr_only` — never a quietly weaker guarantee.
    query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name, spend_cap_usd, velocity_cap_tokens_per_min)
        VALUES ($1, $2, $3, 'retention-policy', 20, 1000000)
        "#,
    )
    .bind(key_id)
    .bind(user_id)
    .bind(hash_api_key(&generate_api_key()))
    .execute(&pool)
    .await
    .expect("key insert must succeed");

    let policy = query_scalar::<_, String>("SELECT retention_policy FROM api_keys WHERE id = $1")
        .bind(key_id)
        .fetch_one(&pool)
        .await
        .expect("retention_policy must read");
    assert_eq!(
        policy, "zdr_only",
        "a key created without a policy must default to the safe, zero-retention-only switch"
    );
}

#[tokio::test]
async fn allow_non_zdr_is_a_representable_deliberate_value() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = seed_user(&pool).await;
    let key_id = Uuid::new_v4();
    query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name, spend_cap_usd, velocity_cap_tokens_per_min, retention_policy)
        VALUES ($1, $2, $3, 'retention-policy', 20, 1000000, 'allow_non_zdr')
        "#,
    )
    .bind(key_id)
    .bind(user_id)
    .bind(hash_api_key(&generate_api_key()))
    .execute(&pool)
    .await
    .expect("a key may be created on the loosened switch");

    let policy = query_scalar::<_, String>("SELECT retention_policy FROM api_keys WHERE id = $1")
        .bind(key_id)
        .fetch_one(&pool)
        .await
        .expect("retention_policy must read");
    assert_eq!(policy, "allow_non_zdr");
}

#[tokio::test]
async fn an_unknown_policy_keyword_is_refused_by_the_check() {
    let Some(pool) = connect().await else {
        return;
    };
    let user_id = seed_user(&pool).await;
    let key_id = Uuid::new_v4();
    // A value outside the switch's vocabulary is rejected at the database, so a
    // hand-written UPDATE cannot park a key on a policy the router has no branch
    // for — which would read as configured and enforce something undefined.
    let result = query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name, spend_cap_usd, velocity_cap_tokens_per_min, retention_policy)
        VALUES ($1, $2, $3, 'retention-policy', 20, 1000000, 'sometimes')
        "#,
    )
    .bind(key_id)
    .bind(user_id)
    .bind(hash_api_key(&generate_api_key()))
    .execute(&pool)
    .await;
    assert!(
        result.is_err(),
        "the CHECK must refuse a retention_policy keyword the router cannot switch on"
    );
}
