use std::str::FromStr;

use rust_decimal::Decimal;
use sqlx_core::{query::query, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;
use zerorouter::{
    auth::{AuthenticationError, KeyAuthenticator, generate_api_key, hash_api_key},
    db::{UsageAdmission, UsageRecord, begin_usage_session, migrate},
    openai::OpenAiUsage,
};

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
    assert!(matches!(
        authenticator.authenticate(&pool, &generate_api_key()).await,
        Err(AuthenticationError::Invalid)
    ));

    let session = match begin_usage_session(&pool, &key, 1_000, Decimal::ONE)
        .await
        .expect("first admission must query")
    {
        UsageAdmission::Allowed(session) => session,
        _ => panic!("first reservation should be admitted"),
    };
    assert!(matches!(
        begin_usage_session(&pool, &key, 1_000, Decimal::from(20))
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
        begin_usage_session(&pool, &key, 800, Decimal::ZERO),
        begin_usage_session(&pool, &key, 800, Decimal::ZERO),
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
        begin_usage_session(&pool, &cached_key, 1, Decimal::ZERO)
            .await
            .expect("revoked admission must query"),
        UsageAdmission::Unauthorized
    ));
}
