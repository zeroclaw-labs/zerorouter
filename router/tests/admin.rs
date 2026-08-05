use std::{process::Command, str::FromStr};

use serde_json::Value;
use sqlx_core::{query::query, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;
use zerorouter::{auth::hash_api_key, db::migrate};

fn run_admin(database_url: &str, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_zerorouter"))
        .env("DATABASE_URL", database_url)
        .arg("admin")
        .args(arguments)
        .output()
        .expect("admin command should start")
}

#[tokio::test]
async fn admin_cli_mints_lists_and_revokes_user_keys_without_storing_plaintext() {
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

    let identity = Uuid::new_v4();
    let email = format!("admin-smoke-{identity}@example.invalid");
    let first = run_admin(
        &database_url,
        &[
            "mint-key",
            "--email",
            &email,
            "--name",
            "first",
            "--spend-cap-usd",
            "3.50",
            "--velocity-cap-tokens-per-min",
            "2000",
        ],
    );
    assert!(first.status.success(), "first mint should succeed");
    let first: Value =
        serde_json::from_slice(&first.stdout).expect("mint output should be valid JSON");
    let first_id = Uuid::parse_str(
        first["key_id"]
            .as_str()
            .expect("mint output should contain a key ID"),
    )
    .expect("minted key ID should be a UUID");
    let first_key = first["api_key"]
        .as_str()
        .expect("mint output should contain a plaintext key")
        .to_owned();
    assert!(first_key.starts_with("zcr_"));
    assert_eq!(first_key.len(), 68);

    let first_hash = hash_api_key(&first_key);
    let stored_hash = query_scalar::<_, String>("SELECT key_hash FROM api_keys WHERE id = $1")
        .bind(first_id)
        .fetch_one(&pool)
        .await
        .expect("stored key digest should query");
    assert_eq!(stored_hash, first_hash);
    assert_ne!(stored_hash, first_key);

    let uppercase_email = email.to_uppercase();
    let second = run_admin(
        &database_url,
        &["mint-key", "--email", &uppercase_email, "--name", "second"],
    );
    assert!(second.status.success(), "second mint should succeed");
    let second: Value =
        serde_json::from_slice(&second.stdout).expect("second mint output should be valid JSON");
    let second_id = Uuid::parse_str(
        second["key_id"]
            .as_str()
            .expect("second mint output should contain a key ID"),
    )
    .expect("second minted key ID should be a UUID");
    let second_key = second["api_key"]
        .as_str()
        .expect("second mint output should contain a plaintext key")
        .to_owned();
    let second_hash = hash_api_key(&second_key);
    assert_ne!(first_id, second_id);
    assert_ne!(first_key, second_key);

    let user_count = query_scalar::<_, i64>("SELECT COUNT(*) FROM users WHERE email = $1")
        .bind(&email)
        .fetch_one(&pool)
        .await
        .expect("normalized user count should query");
    assert_eq!(user_count, 1);

    let list = run_admin(&database_url, &["list-keys", "--email", &email]);
    assert!(list.status.success(), "list should succeed");
    let list_text = String::from_utf8(list.stdout).expect("list output should be UTF-8");
    let listed: Value = serde_json::from_str(&list_text).expect("list output should be JSON");
    assert_eq!(listed.as_array().map(Vec::len), Some(2));
    assert!(list_text.contains(&first_id.to_string()));
    assert!(list_text.contains(&second_id.to_string()));
    assert!(!list_text.contains(&first_key));
    assert!(!list_text.contains(&second_key));
    assert!(!list_text.contains(&first_hash));
    assert!(!list_text.contains(&second_hash));

    let revoke = run_admin(
        &database_url,
        &["revoke-key", "--key-id", &first_id.to_string()],
    );
    assert!(revoke.status.success(), "revoke should succeed");
    let disabled = query_scalar::<_, bool>("SELECT disabled FROM api_keys WHERE id = $1")
        .bind(first_id)
        .fetch_one(&pool)
        .await
        .expect("revoked key state should query");
    assert!(disabled);

    let invalid_email = format!("invalid-{identity}@example.invalid");
    let invalid = run_admin(
        &database_url,
        &[
            "mint-key",
            "--email",
            &invalid_email,
            "--name",
            "invalid",
            "--spend-cap-usd=-1",
        ],
    );
    assert!(!invalid.status.success(), "negative cap should be rejected");
    let invalid_rows = query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM api_keys INNER JOIN users ON users.id = api_keys.user_id WHERE users.email = $1",
    )
    .bind(&invalid_email)
    .fetch_one(&pool)
    .await
    .expect("invalid mint rows should query");
    assert_eq!(invalid_rows, 0);

    query("UPDATE api_keys SET disabled = TRUE WHERE id = $1")
        .bind(second_id)
        .execute(&pool)
        .await
        .expect("second test key should cleanly disable");
}

#[tokio::test]
async fn admin_cli_grants_promo_credit_to_existing_users_only() {
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

    let identity = Uuid::new_v4();
    let email = format!("grant-smoke-{identity}@example.invalid");

    // A typo'd email must fail loudly, never mint a funded ghost account.
    let missing = run_admin(
        &database_url,
        &["grant-credit", "--email", &email, "--amount-usd", "5"],
    );
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("no user with email"),
        "unknown user is refused"
    );

    run_admin(
        &database_url,
        &["mint-key", "--email", &email, "--name", "seed"],
    );

    let granted = run_admin(
        &database_url,
        &["grant-credit", "--email", &email, "--amount-usd", "5.25"],
    );
    assert!(granted.status.success());
    let output: Value = serde_json::from_slice(&granted.stdout).expect("grant output is JSON");
    assert_eq!(output["granted_usd"], "5.25");
    assert_eq!(output["balance_usd"], "5.25");

    // The grant is the same money path purchases use: a 'promo' ledger row
    // snapshotting balance_after, and the users balance moved with it.
    let user_id = Uuid::parse_str(output["user_id"].as_str().unwrap()).unwrap();
    let (entry_type, amount, after): (String, rust_decimal::Decimal, rust_decimal::Decimal) =
        query_as_row(&pool, user_id).await;
    assert_eq!(entry_type, "promo");
    assert_eq!(amount.to_string(), "5.25");
    assert_eq!(after.to_string(), "5.25");

    // Zero and negative amounts are refused at the CLI boundary.
    for bad in ["0", "-3"] {
        let refused = run_admin(
            &database_url,
            &["grant-credit", "--email", &email, "--amount-usd", bad],
        );
        assert!(!refused.status.success(), "amount {bad} must be refused");
    }
}

async fn query_as_row(
    pool: &sqlx_postgres::PgPool,
    user_id: Uuid,
) -> (String, rust_decimal::Decimal, rust_decimal::Decimal) {
    query_scalar::<_, (String, rust_decimal::Decimal, rust_decimal::Decimal)>(
        "SELECT (entry_type, amount_usd, balance_after_usd) FROM credit_ledger \
         WHERE user_id = $1 AND entry_type = 'promo' ORDER BY id DESC LIMIT 1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("promo ledger row exists")
}
