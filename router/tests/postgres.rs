use std::str::FromStr;

use chrono::Utc;
use rust_decimal::Decimal;
use sqlx_core::{query::query, query_as::query_as, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use uuid::Uuid;
use zerorouter::provider::ModelRates;
use zerorouter::{
    auth::{
        AuthenticatedKey, AuthenticationError, KeyAuthenticator, generate_api_key, hash_api_key,
    },
    db::{
        AttemptRecord, AttemptTokens, RequestTelemetry, ReservationBasis, UsageAdmission,
        UsageRecord, UsageSession, begin_usage_session, migrate, output_token_percentiles,
        provider_cogs, segment_clamp_stats, user_clamp_loss,
    },
    openai::{OpenAiUsage, TASK_SIGNATURE_SCHEME, TaskSignature, tool_names_digest, usage_cost},
    priority::Priority,
};

/// A fixed segment key for tests that only need the reservation to carry one.
fn test_signature(hex: &str) -> TaskSignature {
    TaskSignature {
        hex: hex.to_owned(),
        scheme: TASK_SIGNATURE_SCHEME,
        tool_names_sha256: tool_names_digest(&["read".to_owned(), "shell".to_owned()]),
    }
}

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
        priority: Some(Priority::Balanced),
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
        test_signature("0123456789abcdef"),
        ReservationBasis::Cold,
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
            test_signature("0123456789abcdef"),
            ReservationBasis::Cold,
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
            test_signature("0123456789abcdef"),
            ReservationBasis::Cold,
            false
        ),
        begin_usage_session(
            &pool,
            &key,
            800,
            400,
            Decimal::ZERO,
            test_signature("0123456789abcdef"),
            ReservationBasis::Cold,
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
            test_signature("0123456789abcdef"),
            ReservationBasis::Cold,
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
        test_signature("00112233aabbccdd"),
        ReservationBasis::Cold,
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
    let loser_basis_cost = usage_cost(basis, loser_usage).expect("basis rates must price");
    let served_basis_cost = usage_cost(basis, served_usage).expect("basis rates must price");

    let attempts = vec![
        AttemptRecord {
            attempt_no: 1,
            started_at: Utc::now(),
            candidate_id: "openai/loser".to_owned(),
            upstream_provider: "openai".to_owned(),
            upstream_model: "loser-model".to_owned(),
            outcome: "upstream_error".to_owned(),
            served: false,
            latency_ms: 12,
            tokens: AttemptTokens::measured(loser_usage),
            tokens_estimated: false,
            cost_basis_usd: Some(loser_basis_cost),
            finish_reason: None,
            validator_kind: None,
        },
        AttemptRecord {
            attempt_no: 2,
            started_at: Utc::now(),
            candidate_id: "openai/winner".to_owned(),
            upstream_provider: "openai".to_owned(),
            upstream_model: "winner-model".to_owned(),
            outcome: "ok".to_owned(),
            served: true,
            latency_ms: 30,
            tokens: AttemptTokens::measured(served_usage),
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
        candidate_id: Some("openai/winner".to_owned()),
        basis_rates: Some(basis),
        sell_rates: sell,
        finish_reason: Some("stop".to_owned()),
        shape_ok: Some(true),
        priority: Some(Priority::Success),
    };
    session
        .record(&UsageRecord {
            tier: "zero/balanced".to_owned(),
            upstream_provider: "openai".to_owned(),
            upstream_model: "winner-model".to_owned(),
            usage: served_usage,
            cost_usd: usage_cost(sell, served_usage).expect("sell rates must price"),
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
    assert_eq!(candidate_id.as_deref(), Some("openai/winner"));
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

    let (attempts_complete, scheme, tool_digest) =
        query_as::<_, (Option<bool>, Option<i16>, Option<String>)>(
            r#"
        SELECT attempts_cost_basis_complete, task_signature_scheme, tool_names_sha256
        FROM usage_events WHERE api_key_id = $1
        "#,
        )
        .bind(key.id)
        .fetch_one(&pool)
        .await
        .expect("0007 provenance must query");
    assert_eq!(
        attempts_complete,
        Some(true),
        "every losing attempt here was metered, so the sum really is the total"
    );
    assert_eq!(scheme, Some(TASK_SIGNATURE_SCHEME));
    assert_eq!(
        tool_digest,
        Some(tool_names_digest(&["read".to_owned(), "shell".to_owned()])),
        "the settled row carries the exact tool digest the signature was built from"
    );
}

/// `attempts_cost_basis_usd` used to be a `filter_map(...).sum()`, which
/// dropped every attempt whose COGS was unknown and reported the remainder as
/// though it were the total: three burnt upstream calls of which two were never
/// metered settled as "the attempts cost what the one metered attempt cost".
/// The sum now declares whether it is a total or a floor.
#[tokio::test]
async fn a_partly_unknown_attempt_cogs_sum_reports_itself_as_a_lower_bound() {
    let Some(pool) = connect().await else {
        return;
    };
    let basis = ModelRates {
        input_per_mtok: Some(0.3),
        output_per_mtok: Some(1.2),
        cached_input_per_mtok: Some(0.06),
    };
    let metered = OpenAiUsage {
        prompt_tokens: 100,
        completion_tokens: 25,
        total_tokens: 125,
        prompt_tokens_details: None,
    };
    let metered_cost = usage_cost(basis, metered).expect("basis rates must price");

    /// One losing attempt, described by what is known about its tokens.
    fn loser(attempt_no: i16, tokens: AttemptTokens, cost: Option<Decimal>) -> AttemptRecord {
        AttemptRecord {
            attempt_no,
            started_at: Utc::now(),
            candidate_id: format!("openai/loser-{attempt_no}"),
            upstream_provider: "openai".to_owned(),
            upstream_model: "loser-model".to_owned(),
            outcome: "upstream_error".to_owned(),
            served: false,
            latency_ms: 12,
            tokens,
            tokens_estimated: !tokens.is_complete(),
            cost_basis_usd: cost,
            finish_reason: None,
            validator_kind: None,
        }
    }

    async fn settle_with(
        pool: &PgPool,
        attempts: Vec<AttemptRecord>,
        basis: ModelRates,
    ) -> (Option<Decimal>, Option<bool>) {
        let key = seed_key(pool).await;
        let session = admit(pool, &key).await;
        let mut telemetry = sentinel_telemetry();
        telemetry.basis_rates = Some(basis);
        telemetry.candidate_id = Some("openai/winner".to_owned());
        session
            .record(&UsageRecord {
                tier: "zero/balanced".to_owned(),
                upstream_provider: "openai".to_owned(),
                upstream_model: "winner-model".to_owned(),
                usage: OpenAiUsage::default(),
                cost_usd: Decimal::ZERO,
                latency_ms: 42,
                status: 502,
                telemetry,
                attempts,
            })
            .await
            .expect("settle must succeed");
        query_as::<_, (Option<Decimal>, Option<bool>)>(
            r#"
            SELECT attempts_cost_basis_usd, attempts_cost_basis_complete
            FROM usage_events WHERE api_key_id = $1
            "#,
        )
        .bind(key.id)
        .fetch_one(pool)
        .await
        .expect("attempt COGS summary must query")
    }

    // Both losers metered: the sum is the whole story.
    let (total, complete) = settle_with(
        &pool,
        vec![
            loser(1, AttemptTokens::measured(metered), Some(metered_cost)),
            loser(2, AttemptTokens::measured(metered), Some(metered_cost)),
        ],
        basis,
    )
    .await;
    assert_eq!(
        total.map(|value| value.normalize()),
        Some((metered_cost * Decimal::from(2)).normalize())
    );
    assert_eq!(complete, Some(true), "two metered losers sum to a total");

    // One metered, one the upstream never reported on. The known part is still
    // reported — throwing it away would lose real information — but the row now
    // says so instead of passing a partial off as a total.
    let (total, complete) = settle_with(
        &pool,
        vec![
            loser(1, AttemptTokens::measured(metered), Some(metered_cost)),
            loser(2, AttemptTokens::unknown(), None),
        ],
        basis,
    )
    .await;
    assert_eq!(
        total.map(|value| value.normalize()),
        Some(metered_cost.normalize()),
        "the known part survives as a lower bound"
    );
    assert_eq!(
        complete,
        Some(false),
        "an unmetered losing attempt makes the sum a floor, not a total"
    );

    // A floor-priced attempt is not a measurement either: its prompt side is
    // unknown, so its COGS is a lower bound and so is the sum containing it.
    let floor = AttemptTokens::output_floor(40);
    let floor_cost = usage_cost(
        basis,
        floor.priceable().expect("an output floor is priceable"),
    )
    .expect("basis rates must price");
    let (total, complete) =
        settle_with(&pool, vec![loser(1, floor, Some(floor_cost))], basis).await;
    assert_eq!(
        total.map(|value| value.normalize()),
        Some(floor_cost.normalize())
    );
    assert_eq!(
        complete,
        Some(false),
        "an attempt priced from the per-chunk floor knows nothing about the prompt it consumed"
    );
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
            upstream_provider: "openai".to_owned(),
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
                candidate_id: "openai/winner".to_owned(),
                upstream_provider: "openai".to_owned(),
                upstream_model: "winner-model".to_owned(),
                outcome: "ok".to_owned(),
                served: true,
                latency_ms: 5,
                tokens: AttemptTokens::unknown(),
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
            VALUES ($1, $2, $3, 9, 'openai/other', 'deepinfra', 'other', 'ok', TRUE, 1)
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
    let outcome: anyhow::Result<(bool, bool, bool, bool, bool, bool, i64)> = async {
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
        let intents_exists = query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'stripe_checkout_intents')",
        )
        .fetch_one(&pool)
        .await?;
        let settlement_outbox_exists = query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = 'usage_reservations' AND column_name = 'settlement_intent')",
        )
        .fetch_one(&pool)
        .await?;
        let ledger_honesty_exists = query_scalar::<_, bool>(
            r#"
            SELECT COUNT(*) = 3
            FROM information_schema.columns
            WHERE table_name = 'usage_events'
              AND column_name IN ('attempts_cost_basis_complete', 'task_signature_scheme',
                                  'tool_names_sha256')
            "#,
        )
        .fetch_one(&pool)
        .await?;
        let freeze_state_exists = query_scalar::<_, bool>(
            r#"
            SELECT COUNT(*) = 2
            FROM information_schema.columns
            WHERE table_name = 'users'
              AND column_name IN ('frozen_at', 'frozen_reason')
            "#,
        )
        .fetch_one(&pool)
        .await?;
        let version = query_scalar::<_, i64>("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await?;
        pool.close().await;
        Ok((
            attempts_exists,
            column_exists,
            intents_exists,
            settlement_outbox_exists,
            ledger_honesty_exists,
            freeze_state_exists,
            version,
        ))
    }
    .await;

    // Always drop the throwaway database, even if a probe above failed.
    let _ = query(&format!("DROP DATABASE {fresh_db}"))
        .execute(&admin)
        .await;

    let outcome = outcome.expect("the 0001->0009 chain must apply on a fresh database");
    assert!(outcome.0, "request_attempts exists after the fresh chain");
    assert!(
        outcome.1,
        "the 0004 telemetry columns exist after the chain"
    );
    assert!(
        outcome.2,
        "the 0005 stripe_checkout_intents table exists after the chain"
    );
    assert!(
        outcome.3,
        "the 0006 settlement-outbox columns exist after the chain"
    );
    assert!(
        outcome.4,
        "the 0007 ledger-honesty columns exist after the chain"
    );
    assert!(
        outcome.5,
        "the 0009 freeze-state columns exist after the chain"
    );
    // 13, not 10: 0013 (dispute resolution) is numbered with a gap so that
    // 0010-0012 stay available to branches in flight. The chain's head is the
    // highest version applied, not a count of files.
    assert_eq!(outcome.6, 13, "the chain reaches migration version 13");
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

/// One settled row shaped for the estimator scan, inserted directly — the
/// scan's input contract is the table, not the serve path.
#[allow(clippy::too_many_arguments)]
async fn seed_settled_output(
    pool: &PgPool,
    api_key_id: Uuid,
    signature: &str,
    scheme: Option<i16>,
    candidate: Option<&str>,
    output_tokens: i32,
    status: i16,
) {
    query(
        r#"
        INSERT INTO usage_events (
            request_id, api_key_id, tier, upstream_provider, upstream_model,
            input_tokens, cached_input_tokens, output_tokens, cost_usd,
            latency_ms, status, task_signature, task_signature_scheme,
            candidate_id
        )
        VALUES ($1, $2, 'zero/estimator-test', 'fireworks', 'upstream/est',
                100, 0, $3, 0.001, 10, $4, $5, $6, $7)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(api_key_id)
    .bind(output_tokens)
    .bind(status)
    .bind(signature)
    .bind(scheme)
    .bind(candidate)
    .execute(pool)
    .await
    .expect("estimator seed row must insert");
}

/// The estimator's percentile scan reads exactly its cell: the signature at
/// the current scheme, optionally one candidate, status-200 rows only —
/// with pre-0007 NULL-scheme rows invisible to it.
#[tokio::test]
async fn output_percentiles_scan_measures_only_the_cell_it_is_asked_about() {
    let Some(pool) = connect().await else {
        return;
    };
    let key = seed_key(&pool).await;
    // Random per run: usage_events is append-only (no cleanup is possible),
    // so a fixed signature would accrete rows across local runs against a
    // persistent database and break the exact counts below on the second
    // run.
    let signature = Uuid::new_v4().simple().to_string()[..16].to_owned();
    let signature = signature.as_str();

    // Candidate A: 101 settled rows with outputs 0..=100, so the quantile
    // index lands on whole ranks and p50/p90/p99 are exact.
    for output in 0..=100 {
        seed_settled_output(
            &pool,
            key.id,
            signature,
            Some(TASK_SIGNATURE_SCHEME),
            Some("openai/est-a"),
            output,
            200,
        )
        .await;
    }
    // Candidate B: 10 rows at 1000 — visible to its own cell and to the
    // per-signature cell, never to A's.
    for _ in 0..10 {
        seed_settled_output(
            &pool,
            key.id,
            signature,
            Some(TASK_SIGNATURE_SCHEME),
            Some("anthropic/est-b"),
            1_000,
            200,
        )
        .await;
    }
    // Noise the scan must not see: pre-0007 rows (NULL scheme) and non-200
    // settles.
    for _ in 0..7 {
        seed_settled_output(
            &pool,
            key.id,
            signature,
            None,
            Some("openai/est-a"),
            5_000,
            200,
        )
        .await;
        seed_settled_output(
            &pool,
            key.id,
            signature,
            Some(TASK_SIGNATURE_SCHEME),
            Some("openai/est-a"),
            7_777,
            502,
        )
        .await;
    }
    // And a row outside the trailing window: same cell, right status, wrong
    // era — the 14-day clause is what keeps a segment's months-old output
    // regime from steering today's ordering, so it gets the same negative
    // coverage as the scheme/status/candidate filters.
    query(
        r#"
        INSERT INTO usage_events (
            request_id, api_key_id, tier, upstream_provider, upstream_model,
            input_tokens, cached_input_tokens, output_tokens, cost_usd,
            latency_ms, status, task_signature, task_signature_scheme,
            candidate_id, ts
        )
        VALUES ($1, $2, 'zero/estimator-test', 'fireworks', 'upstream/est',
                100, 0, 9999, 0.001, 10, 200, $3, $4, 'openai/est-a',
                NOW() - INTERVAL '15 days')
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(key.id)
    .bind(signature)
    .bind(TASK_SIGNATURE_SCHEME)
    .execute(&pool)
    .await
    .expect("out-of-window seed row must insert");

    let cell_a = output_token_percentiles(
        &pool,
        signature,
        TASK_SIGNATURE_SCHEME,
        Some("openai/est-a"),
    )
    .await
    .expect("scan must run")
    .expect("candidate A has rows");
    assert_eq!(cell_a.rows, 101);
    assert!((cell_a.p50 - 50.0).abs() < 1e-9, "p50 = {}", cell_a.p50);
    assert!((cell_a.p90 - 90.0).abs() < 1e-9, "p90 = {}", cell_a.p90);
    assert!((cell_a.p99 - 99.0).abs() < 1e-9, "p99 = {}", cell_a.p99);
    assert!(cell_a.is_warm());

    let cell_b = output_token_percentiles(
        &pool,
        signature,
        TASK_SIGNATURE_SCHEME,
        Some("anthropic/est-b"),
    )
    .await
    .expect("scan must run")
    .expect("candidate B has rows");
    assert_eq!(cell_b.rows, 10);
    assert!((cell_b.p50 - 1_000.0).abs() < 1e-9);
    assert!(!cell_b.is_warm(), "10 rows is under the warm gate");

    let whole_signature = output_token_percentiles(&pool, signature, TASK_SIGNATURE_SCHEME, None)
        .await
        .expect("scan must run")
        .expect("the signature has rows");
    assert_eq!(
        whole_signature.rows, 111,
        "the candidate-agnostic cell sees both candidates and none of the noise"
    );

    assert!(
        output_token_percentiles(&pool, "ffff000011112222", TASK_SIGNATURE_SCHEME, None)
            .await
            .expect("scan must run")
            .is_none(),
        "an unmeasured signature answers None, not zeros"
    );
}

/// One learned-basis row shaped for the clamp-stats scan, aged as asked.
#[allow(clippy::too_many_arguments)]
async fn seed_clamp_row(
    pool: &PgPool,
    api_key_id: Uuid,
    signature: &str,
    basis: &str,
    cost_usd: &str,
    reserved_cost_usd: &str,
    age_days: i32,
) {
    query(
        r#"
        INSERT INTO usage_events (
            request_id, api_key_id, tier, upstream_provider, upstream_model,
            input_tokens, cached_input_tokens, output_tokens, cost_usd,
            latency_ms, status, task_signature, task_signature_scheme,
            estimator_basis, reserved_cost_usd, ts
        )
        VALUES ($1, $2, 'zero/clamp-test', 'fireworks', 'upstream/clamp',
                100, 0, 100, $3::NUMERIC, 10, 200, $4, $5, $6, $7::NUMERIC,
                NOW() - ($8 * INTERVAL '1 day'))
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(api_key_id)
    .bind(cost_usd)
    .bind(signature)
    .bind(TASK_SIGNATURE_SCHEME)
    .bind(basis)
    .bind(reserved_cost_usd)
    .bind(age_days)
    .execute(pool)
    .await
    .expect("clamp seed row must insert");
}

/// The clamp-stats scan's arithmetic, pinned row by row: losses sum through
/// GREATEST so over-reserved rows cannot offset them, cold rows are
/// invisible, and each window sees exactly its own era.
#[tokio::test]
async fn clamp_stats_sum_losses_only_and_respect_their_windows() {
    let Some(pool) = connect().await else {
        return;
    };
    let key = seed_key(&pool).await;
    let signature = Uuid::new_v4().simple().to_string()[..16].to_owned();
    let signature = signature.as_str();

    // Trigger window (< 7d): a $0.40 loss, an over-reserved row (reserved
    // exceeds cost by $5 — must NOT offset), and a clean exact-cost row.
    seed_clamp_row(&pool, key.id, signature, "learned", "0.50", "0.10", 1).await;
    seed_clamp_row(&pool, key.id, signature, "learned", "1.00", "6.00", 1).await;
    seed_clamp_row(&pool, key.id, signature, "learned", "0.20", "0.20", 1).await;
    // Re-derivation era (7–14d): a $1.50 single-row loss.
    seed_clamp_row(&pool, key.id, signature, "learned", "1.60", "0.10", 10).await;
    // Outside both windows.
    seed_clamp_row(&pool, key.id, signature, "learned", "9.00", "0.10", 20).await;
    // Cold-basis loss-shaped row: invisible to the aggregates entirely.
    seed_clamp_row(&pool, key.id, signature, "cold", "9.00", "0.10", 1).await;

    let stats = segment_clamp_stats(&pool, signature, TASK_SIGNATURE_SCHEME)
        .await
        .expect("stats must scan");
    assert_eq!(stats.loss_7d_usd, "0.40".parse().unwrap());
    assert_eq!(stats.max_row_loss_7d_usd, "0.40".parse().unwrap());
    assert_eq!(stats.clamped_rows_7d, 1);
    assert_eq!(stats.learned_rows_7d, 3);
    assert_eq!(
        stats.loss_14d_usd,
        "1.90".parse().unwrap(),
        "the re-derivation window adds the aged loss and still no offsets"
    );
    assert_eq!(stats.max_row_loss_14d_usd, "1.50".parse().unwrap());
    assert_eq!(stats.clamped_rows_14d, 2);
    assert_eq!(stats.learned_rows_14d, 4);

    // The user aggregate over the same rows: 30d catches everything above
    // except nothing (all under 30d except the 20d row IS under 30d) — so
    // 30d = 0.40 + 1.50 + 8.90; the 37d window matches here. An out-of-era
    // row at 33 days lands only in the 37-day re-derivation sum.
    let user_id = query_scalar::<_, Uuid>("SELECT user_id FROM api_keys WHERE id = $1")
        .bind(key.id)
        .fetch_one(&pool)
        .await
        .expect("owner must query");
    seed_clamp_row(&pool, key.id, signature, "learned", "5.00", "0.50", 33).await;
    let (loss_30d, loss_37d) = user_clamp_loss(&pool, user_id)
        .await
        .expect("user loss must scan");
    assert_eq!(loss_30d, "10.80".parse().unwrap());
    assert_eq!(loss_37d, "15.30".parse().unwrap());
}

/// The treasury aggregation, pinned row by row: served COGS from settled
/// rows, walk COGS from non-served attempts, unpriced rows counted and
/// never silently folded into a sum, windows respected.
#[tokio::test]
async fn provider_cogs_split_served_from_walk_and_count_the_unpriced() {
    let Some(pool) = connect().await else {
        return;
    };
    let key = seed_key(&pool).await;
    let provider = format!("prov-{}", Uuid::new_v4().simple());

    // Two served rows with basis, one without (unpriced), one out-of-window.
    for (basis, age_days) in [
        (Some("0.10"), 0),
        (Some("0.25"), 0),
        (None, 0),
        (Some("9.00"), 40),
    ] {
        query(
            r#"
            INSERT INTO usage_events (
                request_id, api_key_id, tier, upstream_provider, upstream_model,
                input_tokens, cached_input_tokens, output_tokens, cost_usd,
                latency_ms, status, cost_basis_usd, ts
            )
            VALUES ($1, $2, 'zero/treasury-test', $3, 'upstream/t',
                    10, 0, 10, 0.001, 5, 200, $4::NUMERIC,
                    NOW() - ($5 * INTERVAL '1 day'))
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(key.id)
        .bind(&provider)
        .bind(basis)
        .bind(age_days)
        .execute(&pool)
        .await
        .expect("treasury usage row must insert");
    }
    // A non-served walk attempt: the losing rung's COGS.
    let request_id = query_scalar::<_, Uuid>(
        "SELECT request_id FROM usage_events WHERE api_key_id = $1 LIMIT 1",
    )
    .bind(key.id)
    .fetch_one(&pool)
    .await
    .expect("anchor request must exist");
    query(
        r#"
        INSERT INTO request_attempts (
            request_id, api_key_id, user_id, attempt_no, candidate_id,
            upstream_provider, upstream_model, outcome, served, latency_ms,
            cost_basis_usd
        )
        SELECT $1, $2, k.user_id, 1, 'x/y', $3, 'upstream/t',
               'upstream_error', FALSE, 9, 0.07
        FROM api_keys k WHERE k.id = $2
        "#,
    )
    .bind(request_id)
    .bind(key.id)
    .bind(&provider)
    .execute(&pool)
    .await
    .expect("treasury attempt row must insert");

    let rows = provider_cogs(&pool, 7)
        .await
        .expect("treasury must aggregate");
    let row = rows
        .iter()
        .find(|row| row.provider == provider)
        .expect("provider present");
    assert_eq!(row.served_cogs_usd, "0.35".parse().unwrap());
    assert_eq!(row.walk_cogs_usd, "0.07".parse().unwrap());
    assert_eq!(
        row.served_requests, 3,
        "in-window settled rows, priced or not"
    );
    assert_eq!(
        row.unpriced_rows, 1,
        "the basis-less row is counted, not hidden"
    );
}
