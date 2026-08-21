//! End-to-end tests for `zerorouter user`, driven against a real router
//! process in `serve` mode — not an in-process `oneshot` router, because the
//! thing under test is a separate binary talking HTTP, and the credential file
//! it writes is an artifact of that process, not of a handler.
//!
//! Skipped (return early) unless `DATABASE_URL` is set, matching the
//! convention in `tests/postgres.rs`. Every flow uses a unique key name and
//! its own config directory, so parallel runs do not collide.

use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    str::FromStr,
    time::Duration,
};

use serde_json::Value;
use sqlx_core::{query::query, query_scalar::query_scalar};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use uuid::Uuid;
use zerorouter::{
    auth::KeyAuthenticator,
    db::migrate,
    session::{CSRF_HEADER, SESSION_COOKIE, create_session},
};

/// Long enough for a debug-profile binary to bind and migrate on a cold cache.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
/// The server's poll interval is 5s, so a login that has to wait one full
/// round trip takes ~5s; allow several.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(90);

async fn connect(database_url: &str) -> PgPool {
    let options = PgConnectOptions::from_str(database_url).expect("test database URL must parse");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("test database must connect");
    migrate(&pool).await.expect("migration must succeed");
    pool
}

/// A router child process, killed on drop so a failing assertion cannot leak
/// a listener into the rest of the run.
struct Router {
    child: Child,
    base_url: String,
}

impl Drop for Router {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Claim an ephemeral port, then release it for the child to bind. A racing
/// process could steal it between the two, which is why startup is polled
/// rather than assumed.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port must be available");
    listener
        .local_addr()
        .expect("the bound listener must report an address")
        .port()
}

async fn start_router(database_url: &str) -> Router {
    let port = free_port();
    let base_url = format!("http://127.0.0.1:{port}");
    let child = Command::new(env!("CARGO_BIN_EXE_zerorouter"))
        .arg("serve")
        .env("DATABASE_URL", database_url)
        .env("ZEROROUTER_BIND", format!("127.0.0.1:{port}"))
        // Enables the web plane, which is what carries the device endpoints.
        // OIDC and Stripe stay entirely absent, which the feature-group check
        // in `WebConfig::from_env` permits.
        .env("ZEROROUTER_PUBLIC_BASE_URL", &base_url)
        .env("ZEROROUTER_DEVICE_CLIENT_IDS", "zeroclaw")
        .env("ZEROROUTER_TIERS_PATH", "config/tiers.toml")
        // `/v1/models` publishes only lanes this deployment can dispatch to, so
        // a server started with no provider secrets serves an EMPTY catalog —
        // correctly, and this test is about the CLI rendering a populated one.
        // These stand for "the secret is provisioned" and nothing more: the
        // catalog route never dials an upstream, so no value here is ever sent
        // anywhere. Placeholders rather than plausible keys, so nobody reads
        // them as credentials that could work.
        .env("ANTHROPIC_API_KEY", "not-a-real-key")
        .env("OPENAI_API_KEY", "not-a-real-key")
        .env("GEMINI_API_KEY", "not-a-real-key")
        .env("BEDROCK_API_KEY", "not-a-real-key")
        .env("BEDROCK_REGION", "us-east-1")
        // Present so the CLI renders a lane whose metadata is deliberately
        // PARTIAL: three Fireworks lanes state no max output and one states no
        // modalities at all, because the vendor's own pages contradict each
        // other there. Rendering a full table is the easy case; rendering one
        // with holes in it is where a formatter reaches into a missing key.
        .env("FIREWORKS_API_KEY", "not-a-real-key")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the router binary should start");
    let router = Router { child, base_url };

    let client = reqwest::Client::new();
    let health = format!("{}/healthz", router.base_url);
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Ok(response) = client.get(&health).send().await
            && response.status().is_success()
        {
            return router;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the router did not become healthy within {STARTUP_TIMEOUT:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A throwaway config directory under `target/`, so the tests never read or
/// write the developer's real `~/.config/zerorouter`.
fn scratch_config_dir(label: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("user-cli-{label}-{}", Uuid::new_v4().simple()));
    fs::create_dir_all(&directory).expect("scratch config directory must create");
    directory
}

fn user_cli(config_dir: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zerorouter"));
    command
        .arg("user")
        .args(arguments)
        .env("ZEROROUTER_CONFIG_DIR", config_dir)
        // Never let the ambient environment reach into a test.
        .env_remove("ZEROROUTER_BASE_URL")
        .env_remove("ZEROROUTER_DEVICE_CLIENT_ID")
        .env("RUST_LOG", "warn");
    command
}

fn run_user_cli(config_dir: &Path, arguments: &[&str]) -> Output {
    user_cli(config_dir, arguments)
        .output()
        .expect("the user CLI should run")
}

fn stdout_json(output: &Output) -> Value {
    let text = String::from_utf8(output.stdout.clone()).expect("stdout should be UTF-8");
    serde_json::from_str(&text).unwrap_or_else(|error| {
        panic!("stdout should be a single JSON object ({error}); got: {text}")
    })
}

/// Create a portal user and a session token for it — the only credential that
/// can approve a device grant, since `/api/device/approve` takes `PortalUser`.
async fn portal_session(pool: &PgPool) -> (Uuid, String) {
    let user_id = Uuid::new_v4();
    let email = format!("user-cli-{user_id}@example.invalid");
    query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(&email)
        .execute(pool)
        .await
        .expect("test user must insert");
    let (token, _expires_at) = create_session(pool, user_id, Duration::from_secs(3600))
        .await
        .expect("portal session must create");
    (user_id, token)
}

/// Wait for the CLI's `POST /auth/device/code` to land, then approve (or deny)
/// it the way the portal does.
async fn settle_pending_authorization(
    pool: &PgPool,
    base_url: &str,
    session_token: &str,
    key_name: &str,
    action: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let user_code = loop {
        let found = query_scalar::<_, String>(
            "SELECT user_code FROM device_authorizations \
             WHERE key_name = $1 AND status = 'pending' AND expires_at > NOW()",
        )
        .bind(key_name)
        .fetch_optional(pool)
        .await
        .expect("pending authorization lookup must query");
        if let Some(user_code) = found {
            break user_code;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the CLI never created a device authorization for {key_name}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let response = reqwest::Client::new()
        .post(format!("{base_url}/api/device/{action}"))
        .header(
            reqwest::header::COOKIE,
            format!("{SESSION_COOKIE}={session_token}"),
        )
        // The portal's CSRF guard: `PortalUser` refuses a mutating request
        // without this header even when the cookie is valid.
        .header(CSRF_HEADER, "1")
        .json(&serde_json::json!({ "user_code": user_code }))
        .send()
        .await
        .expect("the approval request should complete");
    assert!(
        response.status().is_success(),
        "{action} should succeed, got {}",
        response.status()
    );
    user_code
}

#[tokio::test]
async fn user_cli_logs_in_through_the_device_flow_then_reports_and_clears_the_credential() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = connect(&database_url).await;
    let router = start_router(&database_url).await;
    let config_dir = scratch_config_dir("login");
    let key_name = format!("cli-test-{}", Uuid::new_v4().simple());
    let (user_id, session_token) = portal_session(&pool).await;

    // Drive the real login: the CLI polls while the test plays the portal.
    let login = user_cli(
        &config_dir,
        &[
            "login",
            "--json",
            "--no-browser",
            "--base-url",
            &router.base_url,
            "--key-name",
            &key_name,
        ],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("login should start");

    settle_pending_authorization(
        &pool,
        &router.base_url,
        &session_token,
        &key_name,
        "approve",
    )
    .await;

    let output = tokio::time::timeout(
        LOGIN_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            login.wait_with_output().expect("login should complete")
        }),
    )
    .await
    .expect("login should finish before the timeout")
    .expect("the login task should not panic");

    assert!(
        output.status.success(),
        "login should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let body = stdout_json(&output);
    assert_eq!(body["status"], "logged_in");
    assert_eq!(body["base_url"], router.base_url);
    assert_eq!(body["scope"], "inference");
    assert_eq!(body["token_type"], "bearer");
    assert_eq!(body["key_name"], key_name);
    let api_key = body["api_key"]
        .as_str()
        .expect("login should return the plaintext key")
        .to_owned();
    assert!(api_key.starts_with("zcr_"));
    assert_eq!(api_key.len(), 68);

    // GUARD: the plaintext key is printed exactly once. The server keeps only
    // a digest, so this output is the sole copy — but every extra echo is
    // another place it can be scraped from a log.
    let stdout_text = String::from_utf8(output.stdout.clone()).expect("stdout should be UTF-8");
    assert_eq!(
        stdout_text.matches(api_key.as_str()).count(),
        1,
        "the plaintext key must appear exactly once on stdout"
    );
    let stderr_text = String::from_utf8(output.stderr.clone()).expect("stderr should be UTF-8");
    assert!(
        !stderr_text.contains(api_key.as_str()),
        "the plaintext key must never reach stderr"
    );

    // The minted key is real, enabled, and belongs to the approving user.
    let authenticated = KeyAuthenticator::new()
        .authenticate(&pool, &api_key)
        .await
        .expect("the stored key must authenticate against the router");
    assert_eq!(authenticated.user_id, user_id);

    // GUARD: the credential file is owner-only. A plaintext API key at 0644 is
    // readable by every process on a shared box.
    let credentials = config_dir.join("credentials");
    let metadata = fs::metadata(&credentials).expect("the credential file must exist");
    assert!(metadata.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "credentials must be owner read/write only, found {mode:o}"
        );
    }
    // No temp file survived the atomic install.
    let leftovers: Vec<_> = fs::read_dir(&config_dir)
        .expect("config dir must list")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "credentials")
        .collect();
    assert!(leftovers.is_empty(), "unexpected files left: {leftovers:?}");

    // whoami reports the login without ever restating the secret.
    let whoami = run_user_cli(&config_dir, &["whoami", "--json"]);
    assert!(whoami.status.success(), "whoami should exit 0");
    let identity = stdout_json(&whoami);
    assert_eq!(identity["status"], "authenticated");
    assert_eq!(identity["base_url"], router.base_url);
    assert_eq!(identity["key_name"], key_name);
    assert!(
        identity["key_fingerprint"]
            .as_str()
            .is_some_and(|fingerprint| fingerprint.starts_with("sha256:"))
    );
    // GUARD: whoami must not disclose the key, in any field.
    let whoami_text = String::from_utf8(whoami.stdout).expect("stdout should be UTF-8");
    assert!(
        !whoami_text.contains(api_key.as_str()),
        "whoami must never print the plaintext key: {whoami_text}"
    );
    assert!(identity.get("api_key").is_none());

    // The human rendering must not leak it either.
    let whoami_human = run_user_cli(&config_dir, &["whoami"]);
    assert!(whoami_human.status.success());
    let human_text = String::from_utf8(whoami_human.stdout).expect("stdout should be UTF-8");
    assert!(!human_text.contains(api_key.as_str()));
    assert!(human_text.contains("sha256:"));

    // models is served by the unauthenticated /v1/models, and passes the
    // server's document through unchanged.
    let models = run_user_cli(&config_dir, &["models", "--json"]);
    assert!(
        models.status.success(),
        "models should exit 0; stderr: {}",
        String::from_utf8_lossy(&models.stderr)
    );
    let catalog = stdout_json(&models);
    assert_eq!(catalog["object"], "list");
    let rows = catalog["data"]
        .as_array()
        .expect("the model list should carry a data array");
    assert!(!rows.is_empty(), "the shipped catalog should list models");
    assert!(rows.iter().all(|row| row["id"].is_string()));
    // The retention block rides through the pass-through unchanged, so `--json`
    // needed no CLI change to carry it. Asserted here because "unchanged
    // pass-through" is the property that makes that true, and a future reshape
    // of this command would break it silently.
    assert!(
        rows.iter().all(|row| matches!(
            row["retention"]["posture"].as_str(),
            Some("zero" | "standard")
        )),
        "every listed lane must carry a retention posture in --json"
    );

    // The human table renders the posture as a column and explains it beneath.
    let table = run_user_cli(&config_dir, &["models"]);
    assert!(table.status.success(), "models should exit 0");
    let rendered = String::from_utf8(table.stdout).expect("stdout should be UTF-8");
    assert!(
        rendered.contains("RETENTION"),
        "the table needs a retention column: {rendered}"
    );
    assert!(
        rendered.contains("Retention:") && rendered.contains("zero-retention, listed first"),
        "the table needs the retention footnote: {rendered}"
    );

    // logout removes the file, and is idempotent.
    let logout = run_user_cli(&config_dir, &["logout", "--json"]);
    assert!(logout.status.success());
    assert_eq!(stdout_json(&logout)["removed"], true);
    assert!(!credentials.exists());
    let again = run_user_cli(&config_dir, &["logout", "--json"]);
    assert!(
        again.status.success(),
        "logging out twice must stay a success"
    );
    assert_eq!(stdout_json(&again)["removed"], false);

    // ...and once logged out, whoami reports the dedicated status.
    let after = run_user_cli(&config_dir, &["whoami", "--json"]);
    assert_eq!(after.status.code(), Some(3));
    let error = String::from_utf8(after.stderr).expect("stderr should be UTF-8");
    let parsed: Value = serde_json::from_str(error.trim().lines().next_back().unwrap_or_default())
        .expect("a --json failure should put a JSON error on stderr");
    assert_eq!(parsed["error"]["code"], "not_authenticated");
    assert!(after.stdout.is_empty(), "a failure must not write stdout");

    // Housekeeping: leave no enabled key behind on the shared scratch database.
    query("UPDATE api_keys SET disabled = TRUE WHERE user_id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("test keys should cleanly disable");
}

#[tokio::test]
async fn user_cli_reports_a_denied_device_authorization_with_its_own_exit_code() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = connect(&database_url).await;
    let router = start_router(&database_url).await;
    let config_dir = scratch_config_dir("denied");
    let key_name = format!("cli-deny-{}", Uuid::new_v4().simple());
    let (_user_id, session_token) = portal_session(&pool).await;

    let login = user_cli(
        &config_dir,
        &[
            "login",
            "--json",
            "--base-url",
            &router.base_url,
            "--key-name",
            &key_name,
        ],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("login should start");

    settle_pending_authorization(&pool, &router.base_url, &session_token, &key_name, "deny").await;

    let output = tokio::time::timeout(
        LOGIN_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            login.wait_with_output().expect("login should complete")
        }),
    )
    .await
    .expect("a denied login should finish promptly")
    .expect("the login task should not panic");

    assert_eq!(
        output.status.code(),
        Some(5),
        "a denied grant has its own exit code; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "a failed login must not write a credential document to stdout"
    );
    // Nothing was stored, so a later command still reports "not logged in".
    assert!(!config_dir.join("credentials").exists());
}

#[tokio::test]
async fn user_cli_exit_codes_distinguish_missing_credentials_from_transport_failures() {
    // Deliberately server-free: these paths must not need a database or a
    // router, so an agent can probe them cheaply.
    let config_dir = scratch_config_dir("codes");

    let whoami = run_user_cli(&config_dir, &["whoami", "--json"]);
    assert_eq!(whoami.status.code(), Some(3));
    assert!(whoami.stdout.is_empty());

    // A router that is not listening is an HTTP-class failure, not a
    // credential one — the distinction is the whole point of the codes.
    let port = free_port();
    let models = run_user_cli(
        &config_dir,
        &[
            "models",
            "--json",
            "--base-url",
            &format!("http://127.0.0.1:{port}"),
        ],
    );
    assert_eq!(models.status.code(), Some(4));
    assert!(models.stdout.is_empty());

    // A malformed base URL is refused locally, before any request.
    let malformed = run_user_cli(&config_dir, &["models", "--base-url", "zerorouter.ai"]);
    assert_eq!(malformed.status.code(), Some(1));

    // An unknown subcommand stays clap's usage error, which the CLI's own
    // codes deliberately avoid colliding with.
    let unknown = run_user_cli(&config_dir, &["keys"]);
    assert_eq!(unknown.status.code(), Some(2));
}
