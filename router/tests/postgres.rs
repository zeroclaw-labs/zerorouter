use std::str::FromStr;

use chrono::Utc;
use rust_decimal::Decimal;
use sqlx_core::{query::query, query_as::query_as, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use uuid::Uuid;
use zeroclaw_providers::pricing::ModelRates;
use zerorouter::{
    auth::{
        AuthenticatedKey, AuthenticationError, KeyAuthenticator, generate_api_key, hash_api_key,
    },
    db::{
        AttemptRecord, RequestTelemetry, UsageAdmission, UsageRecord, UsageSession,
        begin_usage_session, migrate,
    },
    openai::{OpenAiUsage, usage_cost},
};

const TEST_SIGNATURE: &str = "0123456789abcdef";

fn test_rates() -> ModelRates {
    ModelRates {
        input_per_mtok: Some(2.0),
        output_per_mtok: Some(10.0),
        cached_input_per_mtok: Some(0.2),
    }
}

/// Minimal telemetry with no served candidate — the sentinel/error shape.
fn sentinel_telemetry() -> RequestTelemetry {
    RequestTelemetry {
        requested_max_tokens: 4096,
        stream: false,
        prompt_bytes: 128,
        message_count: 1,
        tool_count: 0,
        candidate_id: None,
        basis_rates: None,
        sell_rates: test_rates(),
        finish_reason: None,
        shape_ok: None,
    }
}

#[tokio::test]
async fn postgres_enforces_reservations_revocation_and_append_only_usage() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let options = PgConnectOptions::from_str(&database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");

    let user_id = Uuid::new_v4();
    let key_id = Uuid::new_v4();
    let plaintext_key = generate_api_key();
    let email = format!("smoke-{user_id}@example.invalid");
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(email)
        .execute(&pool)
        .await
        .expect("test user must insert");
    query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name, spend_cap_usd, velocity_cap_tokens_per_min)
        VALUES ($1, $2, $3, 'integration', 20, 100000)
        "#,
    )
    .bind(key_id)
    .bind(user_id)
    .bind(hash_api_key(&plaintext_key))
    .execute(&pool)
    .await
    .expect("test API key must insert");
    let authenticator = KeyAuthenticator::new();
    let key = authenticator
        .authenticate(&pool, &plaintext_key)
        .await
        .expect("stored key must authenticate");
    assert_eq!(key.id, key_id);
    assert_eq!(key.user_id, user_id);
    assert!(matches!(
        authenticator.authenticate(&pool, &generate_api_key()).await,
        Err(AuthenticationError::Invalid)
    ));

    let session = match begin_usage_session(
        &pool,
        &key,
        1_000,
        500,
        Decimal::ONE,
        TEST_SIGNATURE.to_owned(),
        false,
    )
    .await
    .expect("first admission must query")
    {
        UsageAdmission::Allowed(session) => session,
        _ => panic!("first reservation should be admitted"),
    };
    assert!(matches!(
        begin_usage_session(
            &pool,
            &key,
            1_000,
            500,
            Decimal::from(20),
            TEST_SIGNATURE.to_owned(),
            false,
        )
        .await
        .expect("second admission must query"),
        UsageAdmission::SpendExceeded
    ));

    session
        .record(&UsageRecord {
            tier: "zero/test".to_owned(),
            upstream_provider: "test".to_owned(),
            upstream_model: "test/model".to_owned(),
            usage: OpenAiUsage {
                prompt_tokens: 100,
                completion_tokens: 25,
                total_tokens: 125,
                prompt_tokens_details: None,
            },
            cost_usd: Decimal::ONE,
            latency_ms: 10,
            status: 200,
            telemetry: sentinel_telemetry(),
            attempts: Vec::new(),
        })
        .await
        .expect("reservation must settle into usage");

    let usage_count =
        query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_events WHERE api_key_id = $1")
            .bind(key_id)
            .fetch_one(&pool)
            .await
            .expect("usage count must query");
    let reservation_count =
        query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_reservations WHERE api_key_id = $1")
            .bind(key_id)
            .fetch_one(&pool)
            .await
            .expect("reservation count must query");
    assert_eq!(usage_count, 1);
    assert_eq!(reservation_count, 0);

    query("UPDATE api_keys SET velocity_cap_tokens_per_min = 1000 WHERE id = $1")
        .bind(key_id)
        .execute(&pool)
        .await
        .expect("velocity cap must update");
    let (first, second) = tokio::join!(
        begin_usage_session(
            &pool,
            &key,
            800,
            400,
            Decimal::ZERO,
            TEST_SIGNATURE.to_owned(),
            false
        ),
        begin_usage_session(
            &pool,
            &key,
            800,
            400,
            Decimal::ZERO,
            TEST_SIGNATURE.to_owned(),
            false
        ),
    );
    let mut admitted = 0;
    let mut rejected = 0;
    for admission in [first, second] {
        match admission.expect("concurrent admission must query") {
            UsageAdmission::Allowed(session) => {
                admitted += 1;
                session
                    .record(&UsageRecord {
                        tier: "zero/test".to_owned(),
                        upstream_provider: "test".to_owned(),
                        upstream_model: "test/model".to_owned(),
                        usage: OpenAiUsage::default(),
                        cost_usd: Decimal::ZERO,
                        latency_ms: 0,
                        status: 499,
                        telemetry: sentinel_telemetry(),
                        attempts: Vec::new(),
                    })
                    .await
                    .expect("concurrent reservation must settle");
            }
            UsageAdmission::VelocityExceeded => rejected += 1,
            _ => panic!("unexpected concurrent admission result"),
        }
    }
    assert_eq!((admitted, rejected), (1, 1));

    assert!(
        query("UPDATE usage_events SET cost_usd = 0 WHERE api_key_id = $1")
            .bind(key_id)
            .execute(&pool)
            .await
            .is_err()
    );
    query("UPDATE api_keys SET disabled = TRUE WHERE id = $1")
        .bind(key_id)
        .execute(&pool)
        .await
        .expect("test key must revoke");
    let cached_key = authenticator
        .authenticate(&pool, &plaintext_key)
        .await
        .expect("immutable key identity may remain cached during its TTL");
    assert!(matches!(
        begin_usage_session(
            &pool,
            &cached_key,
            1,
            1,
            Decimal::ZERO,
            TEST_SIGNATURE.to_owned(),
            false
        )
        .await
        .expect("revoked admission must query"),
        UsageAdmission::Unauthorized
    ));
}

async fn connect() -> Option<PgPool> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let options = PgConnectOptions::from_str(&database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");
    Some(pool)
}

/// A fresh user + funded-cap key, authenticated so it can admit sessions.
async fn seed_key(pool: &PgPool) -> AuthenticatedKey {
    let user_id = Uuid::new_v4();
    let key_id = Uuid::new_v4();
    let plaintext = generate_api_key();
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("telemetry-{user_id}@example.invalid"))
        .execute(pool)
        .await
        .expect("test user must insert");
    query(
        r#"
        INSERT INTO api_keys (id, user_id, key_hash, name, spend_cap_usd, velocity_cap_tokens_per_min)
        VALUES ($1, $2, $3, 'telemetry', 1000, 1000000)
        "#,
    )
    .bind(key_id)
    .bind(user_id)
    .bind(hash_api_key(&plaintext))
    .execute(pool)
    .await
    .expect("test API key must insert");
    KeyAuthenticator::new()
        .authenticate(pool, &plaintext)
        .await
        .expect("seeded key must authenticate")
}

async fn admit(pool: &PgPool, key: &AuthenticatedKey) -> UsageSession {
    match begin_usage_session(
        pool,
        key,
        4_596,
        500,
        Decimal::ONE,
        "00112233aabbccdd".to_owned(),
        false,
    )
    .await
    .expect("admission must query")
    {
        UsageAdmission::Allowed(session) => session,
        _ => panic!("admission should be allowed"),
    }
}

#[tokio::test]
async fn settled_row_carries_estimate_and_select_telemetry() {
    let Some(pool) = connect().await else {
        return;
    };
    let key = seed_key(&pool).await;
    let session = admit(&pool, &key).await;

    let basis = ModelRates {
        input_per_mtok: Some(0.3),
        output_per_mtok: Some(1.2),
        cached_input_per_mtok: Some(0.06),
    };
    let sell = test_rates();
    let served_usage = OpenAiUsage {
        prompt_tokens: 100,
        completion_tokens: 25,
        total_tokens: 125,
        prompt_tokens_details: None,
    };
    let loser_usage = OpenAiUsage {
        prompt_tokens: 0,
        completion_tokens: 40,
        total_tokens: 40,
        prompt_tokens_details: None,
    };
    let loser_basis_cost = usage_cost(basis, loser_usage);
    let served_basis_cost = usage_cost(basis, served_usage);

    let attempts = vec![
        AttemptRecord {
            attempt_no: 1,
            started_at: Utc::now(),
            candidate_id: "fireworks/loser".to_owned(),
            upstream_provider: "fireworks".to_owned(),
            upstream_model: "loser-model".to_owned(),
            outcome: "upstream_error".to_owned(),
            served: false,
            latency_ms: 12,
            usage: Some(loser_usage),
            tokens_estimated: true,
            cost_basis_usd: Some(loser_basis_cost),
            finish_reason: None,
            validator_kind: None,
        },
        AttemptRecord {
            attempt_no: 2,
            started_at: Utc::now(),
            candidate_id: "deepinfra/winner".to_owned(),
            upstream_provider: "deepinfra".to_owned(),
            upstream_model: "winner-model".to_owned(),
            outcome: "ok".to_owned(),
            served: true,
            latency_ms: 30,
            usage: Some(served_usage),
            tokens_estimated: false,
            cost_basis_usd: Some(served_basis_cost),
            finish_reason: Some("stop".to_owned()),
            validator_kind: None,
        },
    ];
    let telemetry = RequestTelemetry {
        requested_max_tokens: 8192,
        stream: true,
        prompt_bytes: 4096,
        message_count: 3,
        tool_count: 2,
        candidate_id: Some("deepinfra/winner".to_owned()),
        basis_rates: Some(basis),
        sell_rates: sell,
        finish_reason: Some("stop".to_owned()),
        shape_ok: Some(true),
    };
    session
        .record(&UsageRecord {
            tier: "zero/balanced".to_owned(),
            upstream_provider: "deepinfra".to_owned(),
            upstream_model: "winner-model".to_owned(),
            usage: served_usage,
            cost_usd: usage_cost(sell, served_usage),
            latency_ms: 42,
            status: 200,
            telemetry,
            attempts,
        })
        .await
        .expect("settle with telemetry must succeed");

    #[allow(clippy::type_complexity)]
    let (
        signature,
        requested_max_tokens,
        stream,
        prompt_bytes,
        message_count,
        tool_count,
        candidate_id,
        finish_reason,
        finish_reason_source,
        estimator_basis,
    ) = query_as::<
        _,
        (
            String,
            i32,
            bool,
            i64,
            i32,
            i32,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ),
    >(
        r#"
        SELECT task_signature, requested_max_tokens, stream, prompt_bytes,
               message_count, tool_count, candidate_id, finish_reason,
               finish_reason_source, estimator_basis
        FROM usage_events WHERE api_key_id = $1
        "#,
    )
    .bind(key.id)
    .fetch_one(&pool)
    .await
    .expect("settled event must query");

    assert_eq!(signature, "00112233aabbccdd");
    assert_eq!(requested_max_tokens, 8192);
    assert!(stream);
    assert_eq!(prompt_bytes, 4096);
    assert_eq!(message_count, 3);
    assert_eq!(tool_count, 2);
    assert_eq!(candidate_id.as_deref(), Some("deepinfra/winner"));
    assert_eq!(finish_reason.as_deref(), Some("stop"));
    assert_eq!(finish_reason_source.as_deref(), Some("synthetic"));
    assert_eq!(estimator_basis.as_deref(), Some("cold"));

    let (
        shape_ok,
        attempt_count,
        reserved_output_tokens,
        reserved_cost_usd,
        cost_basis_usd,
        attempts_cost_basis_usd,
        sell_rates,
        basis_rates,
    ) = query_as::<
        _,
        (
            Option<bool>,
            Option<i16>,
            Option<i32>,
            Option<Decimal>,
            Option<Decimal>,
            Option<Decimal>,
            serde_json::Value,
            Option<serde_json::Value>,
        ),
    >(
        r#"
        SELECT shape_ok, attempt_count, reserved_output_tokens, reserved_cost_usd,
               cost_basis_usd, attempts_cost_basis_usd, sell_rates, basis_rates
        FROM usage_events WHERE api_key_id = $1
        "#,
    )
    .bind(key.id)
    .fetch_one(&pool)
    .await
    .expect("settled provenance must query");

    assert_eq!(shape_ok, Some(true));
    assert_eq!(attempt_count, Some(2));
    assert_eq!(reserved_output_tokens, Some(500));
    assert_eq!(reserved_cost_usd, Some(Decimal::ONE));
    assert_eq!(
        cost_basis_usd.map(|value| value.normalize()),
        Some(served_basis_cost.normalize()),
        "served-attempt COGS is the candidate cost-basis price"
    );
    assert_eq!(
        attempts_cost_basis_usd.map(|value| value.normalize()),
        Some(loser_basis_cost.normalize()),
        "losing-attempt COGS sums into attempts_cost_basis_usd"
    );
    assert_eq!(sell_rates["output_per_mtok"], serde_json::json!(10.0));
    assert_eq!(
        basis_rates.expect("basis snapshot present")["output_per_mtok"],
        serde_json::json!(1.2)
    );

    let attempt_total =
        query_scalar::<_, i64>("SELECT COUNT(*) FROM request_attempts WHERE api_key_id = $1")
            .bind(key.id)
            .fetch_one(&pool)
            .await
            .expect("attempt count must query");
    let served_total = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM request_attempts WHERE api_key_id = $1 AND served",
    )
    .bind(key.id)
    .fetch_one(&pool)
    .await
    .expect("served count must query");
    assert_eq!(attempt_total, 2, "one row per candidate tried");
    assert_eq!(served_total, 1, "exactly one served attempt per request");
}

#[tokio::test]
async fn request_attempts_are_append_only_and_one_served_per_request() {
    let Some(pool) = connect().await else {
        return;
    };
    let key = seed_key(&pool).await;
    let session = admit(&pool, &key).await;
    session
        .record(&UsageRecord {
            tier: "zero/balanced".to_owned(),
            upstream_provider: "deepinfra".to_owned(),
            upstream_model: "winner-model".to_owned(),
            usage: OpenAiUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                prompt_tokens_details: None,
            },
            cost_usd: Decimal::ZERO,
            latency_ms: 5,
            status: 200,
            telemetry: sentinel_telemetry(),
            attempts: vec![AttemptRecord {
                attempt_no: 1,
                started_at: Utc::now(),
                candidate_id: "deepinfra/winner".to_owned(),
                upstream_provider: "deepinfra".to_owned(),
                upstream_model: "winner-model".to_owned(),
                outcome: "ok".to_owned(),
                served: true,
                latency_ms: 5,
                usage: None,
                tokens_estimated: false,
                cost_basis_usd: None,
                finish_reason: Some("stop".to_owned()),
                validator_kind: None,
            }],
        })
        .await
        .expect("settle must succeed");

    // The append-only trigger rejects any row UPDATE.
    assert!(
        query("UPDATE request_attempts SET outcome = 'timeout' WHERE api_key_id = $1")
            .bind(key.id)
            .execute(&pool)
            .await
            .is_err(),
        "request_attempts must reject UPDATE"
    );

    // The partial unique index forbids a second served attempt per request.
    let request_id =
        query_scalar::<_, Uuid>("SELECT request_id FROM usage_events WHERE api_key_id = $1")
            .bind(key.id)
            .fetch_one(&pool)
            .await
            .expect("request id must query");
    assert!(
        query(
            r#"
            INSERT INTO request_attempts (
                request_id, api_key_id, user_id, attempt_no, candidate_id,
                upstream_provider, upstream_model, outcome, served, latency_ms
            )
            VALUES ($1, $2, $3, 9, 'deepinfra/other', 'deepinfra', 'other', 'ok', TRUE, 1)
            "#,
        )
        .bind(request_id)
        .bind(key.id)
        .bind(key.user_id)
        .execute(&pool)
        .await
        .is_err(),
        "a second served attempt must violate the one-served-per-request index"
    );
}

#[tokio::test]
async fn migration_chain_applies_on_a_fresh_database() {
    let Some(base) = std::env::var("DATABASE_URL").ok() else {
        return;
    };
    let admin_url = swap_database(&base, "postgres");
    let fresh_db = format!("zr_migchain_{}", Uuid::new_v4().simple());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(PgConnectOptions::from_str(&admin_url).expect("admin url must parse"))
        .await
        .expect("maintenance database must connect");
    query(&format!("CREATE DATABASE {fresh_db}"))
        .execute(&admin)
        .await
        .expect("fresh database must be created");

    let fresh_url = swap_database(&base, &fresh_db);
    // Nothing inside may panic: the DROP below is the only cleanup, so every
    // step reports through the Result and the assertions run after the drop.
    let outcome: anyhow::Result<(bool, bool, i64)> = async {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(PgConnectOptions::from_str(&fresh_url)?)
            .await?;
        migrate(&pool).await?;
        let attempts_exists = query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'request_attempts')",
        )
        .fetch_one(&pool)
        .await?;
        let column_exists = query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = 'usage_events' AND column_name = 'task_signature')",
        )
        .fetch_one(&pool)
        .await?;
        let version = query_scalar::<_, i64>("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await?;
        pool.close().await;
        Ok((attempts_exists, column_exists, version))
    }
    .await;

    // Always drop the throwaway database, even if a probe above failed.
    let _ = query(&format!("DROP DATABASE {fresh_db}"))
        .execute(&admin)
        .await;

    let outcome = outcome.expect("the 0001->0004 chain must apply on a fresh database");
    assert!(outcome.0, "request_attempts exists after the fresh chain");
    assert!(
        outcome.1,
        "the 0004 telemetry columns exist after the chain"
    );
    assert_eq!(outcome.2, 4, "the chain reaches migration version 4");
}

/// Rewrite the database name in a Postgres URL, keeping any query string
/// (`?sslmode=require`) attached to the swapped target instead of folding it
/// into the database name.
fn swap_database(url: &str, database: &str) -> String {
    let (path, query) = url
        .split_once('?')
        .map_or((url, None), |(path, query)| (path, Some(query)));
    let (prefix, _) = path
        .rsplit_once('/')
        .expect("database url must have a path segment");
    match query {
        Some(query) => format!("{prefix}/{database}?{query}"),
        None => format!("{prefix}/{database}"),
    }
}

#[test]
fn swap_database_keeps_connection_parameters() {
    assert_eq!(
        swap_database("postgres://zr@127.0.0.1:55432/zerorouter_test", "postgres"),
        "postgres://zr@127.0.0.1:55432/postgres"
    );
    assert_eq!(
        swap_database(
            "postgres://zr@db.example.invalid/zerorouter?sslmode=require",
            "zr_migchain_1"
        ),
        "postgres://zr@db.example.invalid/zr_migchain_1?sslmode=require"
    );
}
