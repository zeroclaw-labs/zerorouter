//! `zerorouter user` — the end-user CLI: RFC 8628 device-flow login, and the
//! account surface that the resulting credential can actually reach.
//!
//! # Scope boundary
//!
//! This CLI is deliberately smaller than an account-management CLI usually is,
//! and the reason is a property of the server, not an omission here.
//!
//! [`crate::device`]'s `POST /auth/device/token` mints a **`zcr_` inference API
//! key** and returns it as `access_token` with `scope: "inference"`. It does
//! not mint a portal session. Every `/api/*` endpoint — `/api/me`, `/api/keys`,
//! `/api/usage`, `/api/billing/ledger`, and the Stripe billing routes — is
//! guarded by [`crate::session::PortalUser`], which resolves *only* the
//! `zr_session` cookie (a `zcs_` token hashed into `portal_sessions`) and, on
//! mutating methods, additionally requires the `x-zerorouter-portal` CSRF
//! header. There is no bearer path into `/api/*`: the CSRF header and the
//! cookie parser appear nowhere but `session.rs`, and `bearer_token()` exists
//! only in `api.rs`, for `/v1/chat/completions`.
//!
//! So a device-flow credential authorizes exactly one endpoint,
//! `/v1/chat/completions`, and the commands here are the ones that boundary
//! permits:
//!
//! | command  | transport                                    |
//! |----------|----------------------------------------------|
//! | `login`  | `/auth/device/code` + `/auth/device/token`    |
//! | `logout` | local only                                   |
//! | `whoami` | local only (the stored credential's metadata) |
//! | `models` | `GET /v1/models` — unauthenticated on the server |
//!
//! `keys`, `balance`, `ledger`, and `usage` are **not** implemented because no
//! credential this CLI can obtain will authenticate them. They are not stubbed
//! out either: a subcommand that always fails is worse than an absent one for
//! the agents this surface is meant to serve. Adding them requires a
//! server-side credential with portal scope — a decision about what approving a
//! device grant hands over, which is documented in `docs/USER-CLI.md`.
//!
//! # Agent contract
//!
//! - `--json` puts **exactly one** JSON object on stdout per invocation.
//!   Progress, prompts, and errors go to stderr. `login --json > cred.json` is
//!   therefore safe.
//! - Exit codes are meaningful; see [`exit_code`] and the table in
//!   `docs/USER-CLI.md`.
//! - Nothing prompts for input. `login` prints a URL and a code and polls;
//!   it never opens a browser unless `--open` is passed.

use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Where the hosted service lives; overridable for self-hosted / edge mode.
pub const DEFAULT_BASE_URL: &str = "https://zerorouter.ai";
pub const BASE_URL_ENV: &str = "ZEROROUTER_BASE_URL";
/// Overrides the whole config directory (not just the file), so a test or a
/// sandboxed agent can keep credentials out of the real home directory.
pub const CONFIG_DIR_ENV: &str = "ZEROROUTER_CONFIG_DIR";
/// The device-flow client id. The router allowlists these through
/// `ZEROROUTER_DEVICE_CLIENT_IDS`, whose default is exactly `zeroclaw` — so
/// this CLI presents `zeroclaw` to work against an unmodified deployment
/// rather than requiring every operator to reconfigure before logging in.
const DEFAULT_CLIENT_ID: &str = "zeroclaw";
pub const CLIENT_ID_ENV: &str = "ZEROROUTER_DEVICE_CLIENT_ID";
const DEFAULT_KEY_NAME: &str = "zerorouter cli";
const CREDENTIALS_FILE: &str = "credentials";

/// Owner-only. Read and written in one place ([`CredentialStore::save`]) so
/// there is a single point that decides how exposed a plaintext API key on
/// disk is.
#[cfg(unix)]
const CREDENTIAL_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const CONFIG_DIR_MODE: u32 = 0o700;

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// RFC 8628 §3.5: a `slow_down` response lengthens the poll interval by five
/// seconds, and the client must keep the new interval for the rest of the flow.
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);
/// Poll a touch later than the server's stated interval. The server refuses a
/// poll that lands inside `poll_interval_secs` of the previous one, so polling
/// at exactly the interval sits on the boundary and earns a needless
/// `slow_down`.
const POLL_MARGIN: Duration = Duration::from_millis(250);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Fallback only; the server's `expires_in` governs when it is present.
const DEFAULT_GRANT_TTL: Duration = Duration::from_secs(900);

#[derive(Debug, Args)]
pub struct UserArgs {
    #[command(subcommand)]
    command: UserCommand,

    /// Emit one machine-readable JSON object on stdout; progress and errors
    /// go to stderr.
    #[arg(long, global = true)]
    json: bool,

    /// Router base URL. Falls back to $ZEROROUTER_BASE_URL, then the stored
    /// credential's own base URL, then the hosted service.
    #[arg(long, global = true, value_name = "URL")]
    base_url: Option<String>,
}

#[derive(Debug, Subcommand)]
enum UserCommand {
    /// Log in with the device-authorization flow and store the credential.
    Login(LoginArgs),
    /// Remove the stored credential from this machine.
    Logout,
    /// Show which router this machine is logged in to. Never prints the key.
    Whoami,
    /// List the models the router serves, with rates and context windows.
    Models,
}

#[derive(Debug, Args)]
struct LoginArgs {
    /// Label recorded on the minted key, shown in the portal's key list.
    #[arg(long, value_name = "NAME")]
    key_name: Option<String>,

    /// Device-flow client id. Must be allowlisted by the router's
    /// ZEROROUTER_DEVICE_CLIENT_IDS.
    #[arg(long, value_name = "ID")]
    client_id: Option<String>,

    /// Print the verification URL only. This is the default; the flag exists
    /// so scripts can state the intent explicitly.
    #[arg(long)]
    no_browser: bool,

    /// Attempt to open the verification URL in a browser. Never happens
    /// without this flag.
    #[arg(long, conflicts_with = "no_browser")]
    open: bool,
}

/// Process exit statuses. Stable — agents branch on these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// I/O, configuration, or an otherwise unclassified local error.
    Runtime,
    /// No credential is stored (or it is unreadable).
    NotAuthenticated,
    /// The router answered, but with an error status.
    Http,
    /// The device authorization was denied, expired, or never approved.
    DeviceAuthorization,
}

impl Failure {
    #[must_use]
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Runtime => 1,
            // 2 is clap's usage-error code; skipped so the two never collide.
            Self::NotAuthenticated => 3,
            Self::Http => 4,
            Self::DeviceAuthorization => 5,
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::Runtime => "runtime_error",
            Self::NotAuthenticated => "not_authenticated",
            Self::Http => "http_error",
            Self::DeviceAuthorization => "device_authorization_failed",
        }
    }
}

/// An error that carries the exit status it should produce.
#[derive(Debug)]
pub struct CliError {
    pub failure: Failure,
    pub source: anyhow::Error,
}

impl CliError {
    fn new(failure: Failure, source: anyhow::Error) -> Self {
        Self { failure, source }
    }
}

trait FailureContext<T> {
    fn failing(self, failure: Failure) -> Result<T, CliError>;
}

impl<T, E: Into<anyhow::Error>> FailureContext<T> for Result<T, E> {
    fn failing(self, failure: Failure) -> Result<T, CliError> {
        self.map_err(|error| CliError::new(failure, error.into()))
    }
}

/// Run a `zerorouter user` invocation and return the process exit code.
///
/// Every path prints its own output; the caller only propagates the status.
pub async fn run(args: UserArgs) -> i32 {
    let json = args.json;
    match dispatch(args).await {
        Ok(()) => {
            // Explicit: the caller exits the process directly, which skips the
            // flush an ordinary return would perform.
            let _ = std::io::stdout().flush();
            0
        }
        Err(error) => {
            report_error(&error, json);
            let _ = std::io::stdout().flush();
            error.failure.exit_code()
        }
    }
}

fn report_error(error: &CliError, json: bool) {
    let mut stderr = std::io::stderr();
    if json {
        let payload = serde_json::json!({
            "error": {
                "code": error.failure.code(),
                "message": format!("{:#}", error.source),
            }
        });
        let _ = writeln!(
            stderr,
            "{}",
            serde_json::to_string(&payload).unwrap_or_else(|_| {
                "{\"error\":{\"code\":\"runtime_error\",\"message\":\"unserializable\"}}".to_owned()
            })
        );
    } else {
        let _ = writeln!(stderr, "error: {:#}", error.source);
    }
    let _ = stderr.flush();
}

async fn dispatch(args: UserArgs) -> Result<(), CliError> {
    let UserArgs {
        command,
        json,
        base_url,
    } = args;
    let output = Output { json };
    let store = CredentialStore::discover().failing(Failure::Runtime)?;
    match command {
        UserCommand::Login(login) => {
            login_command(&store, &output, base_url.as_deref(), login).await
        }
        UserCommand::Logout => logout_command(&store, &output),
        UserCommand::Whoami => whoami_command(&store, &output),
        UserCommand::Models => models_command(&store, &output, base_url.as_deref()).await,
    }
}

/// Where output goes, and in which shape.
struct Output {
    json: bool,
}

impl Output {
    /// The single JSON object an invocation is allowed to put on stdout.
    fn emit(&self, value: &serde_json::Value) -> Result<(), CliError> {
        if !self.json {
            return Ok(());
        }
        let text = serde_json::to_string_pretty(value)
            .context("serializing the command result")
            .failing(Failure::Runtime)?;
        println!("{text}");
        Ok(())
    }

    /// Human-readable progress. Suppressed in JSON mode's stdout by going to
    /// stderr, so `--json` output stays a single parseable object.
    fn note(&self, message: &str) {
        let mut stderr = std::io::stderr();
        let _ = writeln!(stderr, "{message}");
        let _ = stderr.flush();
    }

    /// Human-readable result on stdout, printed only when not in JSON mode.
    fn line(&self, message: &str) {
        if !self.json {
            println!("{message}");
        }
    }
}

// ---------------------------------------------------------------------------
// Credential storage
// ---------------------------------------------------------------------------

/// The stored credential. `api_key` is the plaintext `zcr_` key the device
/// flow minted — it exists nowhere else, since the server keeps only its
/// SHA-256 digest, so losing this file means re-running `login`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    pub version: u32,
    pub base_url: String,
    pub api_key: String,
    pub token_type: String,
    pub scope: String,
    pub key_name: String,
    pub created_at: DateTime<Utc>,
}

impl Credential {
    /// A stable, non-reversible handle for the key, safe to print and to log.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.api_key.as_bytes());
        format!("sha256:{}", &hex::encode(digest)[..16])
    }
}

pub struct CredentialStore {
    directory: PathBuf,
}

impl CredentialStore {
    /// Resolve the config directory: `$ZEROROUTER_CONFIG_DIR`, else
    /// `$XDG_CONFIG_HOME/zerorouter`, else `$HOME/.config/zerorouter`.
    pub fn discover() -> Result<Self> {
        if let Some(directory) = non_empty_env(CONFIG_DIR_ENV) {
            return Ok(Self {
                directory: PathBuf::from(directory),
            });
        }
        if let Some(xdg) = non_empty_env("XDG_CONFIG_HOME") {
            return Ok(Self {
                directory: PathBuf::from(xdg).join("zerorouter"),
            });
        }
        let home = non_empty_env("HOME").ok_or_else(|| {
            anyhow!("cannot locate a config directory: neither {CONFIG_DIR_ENV}, XDG_CONFIG_HOME, nor HOME is set")
        })?;
        Ok(Self {
            directory: PathBuf::from(home).join(".config").join("zerorouter"),
        })
    }

    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.directory.join(CREDENTIALS_FILE)
    }

    pub fn load(&self) -> Result<Option<Credential>> {
        let path = self.path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };
        let credential = serde_json::from_str::<Credential>(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(credential))
    }

    /// Write the credential, owner-readable only, and replace any previous one
    /// atomically.
    ///
    /// The mode is applied twice on purpose. `OpenOptions::mode` only takes
    /// effect when the call actually creates the file, so a leftover temp file
    /// from a killed run would otherwise keep whatever mode it already had;
    /// the explicit `set_permissions` closes that window. Both read
    /// [`CREDENTIAL_FILE_MODE`], so there is exactly one place to change — and
    /// exactly one place to break, which is what the permissions test pins.
    pub fn save(&self, credential: &Credential) -> Result<PathBuf> {
        fs::create_dir_all(&self.directory)
            .with_context(|| format!("creating {}", self.directory.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.directory, fs::Permissions::from_mode(CONFIG_DIR_MODE))
                .with_context(|| format!("restricting {}", self.directory.display()))?;
        }

        let path = self.path();
        let temporary = self
            .directory
            .join(format!(".{CREDENTIALS_FILE}.{}.tmp", std::process::id()));

        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(CREDENTIAL_FILE_MODE);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("creating {}", temporary.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(CREDENTIAL_FILE_MODE))
                .with_context(|| format!("restricting {}", temporary.display()))?;
        }

        let mut document =
            serde_json::to_string_pretty(credential).context("serializing the credential")?;
        document.push('\n');
        file.write_all(document.as_bytes())
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("flushing {}", temporary.display()))?;
        drop(file);

        fs::rename(&temporary, &path).with_context(|| format!("installing {}", path.display()))?;
        Ok(path)
    }

    /// Remove the credential. Returns whether one was there to remove.
    pub fn clear(&self) -> Result<bool> {
        let path = self.path();
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
        }
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    let value = env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

// ---------------------------------------------------------------------------
// Base URL resolution
// ---------------------------------------------------------------------------

fn normalize_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("the base URL is empty");
    }
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        bail!("the base URL must start with http:// or https://, got {raw:?}");
    }
    Ok(trimmed.to_owned())
}

/// Flag, then environment, then the default. Used by `login`, which is
/// establishing a credential rather than reading one.
fn base_url_for_login(flag: Option<&str>) -> Result<String> {
    let raw = flag
        .map(str::to_owned)
        .or_else(|| non_empty_env(BASE_URL_ENV))
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    normalize_base_url(&raw)
}

/// Flag, then environment, then whichever router this machine logged in to,
/// then the default — so `models` follows a self-hosted login without needing
/// the flag repeated.
fn base_url_for_request(flag: Option<&str>, credential: Option<&Credential>) -> Result<String> {
    let raw = flag
        .map(str::to_owned)
        .or_else(|| non_empty_env(BASE_URL_ENV))
        .or_else(|| credential.map(|credential| credential.base_url.clone()))
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    normalize_base_url(&raw)
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("building the HTTP client")
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    interval: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenResponse {
    access_token: String,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

async fn login_command(
    store: &CredentialStore,
    output: &Output,
    base_url_flag: Option<&str>,
    args: LoginArgs,
) -> Result<(), CliError> {
    let base_url = base_url_for_login(base_url_flag).failing(Failure::Runtime)?;
    let client_id = args
        .client_id
        .or_else(|| non_empty_env(CLIENT_ID_ENV))
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_owned());
    let key_name = args
        .key_name
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_KEY_NAME.to_owned());
    let client = http_client().failing(Failure::Runtime)?;

    let grant = start_device_authorization(&client, &base_url, &client_id, &key_name).await?;

    let verification_url = grant
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| grant.verification_uri.clone());
    announce_verification(output, &grant, &verification_url);
    if args.open {
        open_in_browser(output, &verification_url);
    }

    let interval = grant
        .interval
        .and_then(|seconds| u64::try_from(seconds).ok())
        .map_or(DEFAULT_POLL_INTERVAL, Duration::from_secs);
    let ttl = grant
        .expires_in
        .and_then(|seconds| u64::try_from(seconds).ok())
        .map_or(DEFAULT_GRANT_TTL, Duration::from_secs);

    let token = poll_for_token(
        &client,
        &base_url,
        &client_id,
        &grant.device_code,
        interval,
        ttl,
    )
    .await?;

    let credential = Credential {
        version: 1,
        base_url: base_url.clone(),
        api_key: token.access_token,
        token_type: token.token_type.unwrap_or_else(|| "bearer".to_owned()),
        scope: token.scope.unwrap_or_else(|| "inference".to_owned()),
        key_name,
        created_at: Utc::now(),
    };
    let path = store.save(&credential).failing(Failure::Runtime)?;

    // The plaintext key leaves this process exactly once, here. The server
    // stores only its digest, so this is the single opportunity to capture it;
    // `whoami` deliberately prints a fingerprint instead.
    output.emit(&serde_json::json!({
        "status": "logged_in",
        "base_url": credential.base_url,
        "api_key": credential.api_key,
        "token_type": credential.token_type,
        "scope": credential.scope,
        "key_name": credential.key_name,
        "key_fingerprint": credential.fingerprint(),
        "credentials_path": path.display().to_string(),
        "created_at": credential.created_at,
    }))?;
    if !output.json {
        output.line(&format!("Logged in to {}.", credential.base_url));
        output.line(&format!("Credential stored at {}", path.display()));
        output.line("");
        output.line("This key is shown once and stored only as a digest on the server:");
        output.line("");
        output.line(&format!("    {}", credential.api_key));
        output.line("");
    }
    Ok(())
}

fn announce_verification(output: &Output, grant: &DeviceCodeResponse, verification_url: &str) {
    output.note("");
    output.note(&format!("    Visit: {}", grant.verification_uri));
    output.note(&format!("    Code:  {}", grant.user_code));
    if grant.verification_uri_complete.is_some() {
        output.note("");
        output.note(&format!("    Or open directly: {verification_url}"));
    }
    output.note("");
    output.note("Waiting for approval...");
}

/// Best effort, and only ever on `--open`. A failure to launch is not a login
/// failure — the URL has already been printed.
fn open_in_browser(output: &Output, url: &str) {
    let launcher = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    match std::process::Command::new(launcher).arg(url).spawn() {
        Ok(_) => {}
        Err(error) => output.note(&format!(
            "could not launch {launcher} ({error}); open the URL above yourself"
        )),
    }
}

async fn start_device_authorization(
    client: &reqwest::Client,
    base_url: &str,
    client_id: &str,
    key_name: &str,
) -> Result<DeviceCodeResponse, CliError> {
    let url = format!("{base_url}/auth/device/code");
    let response = client
        .post(&url)
        .form(&[("client_id", client_id), ("key_name", key_name)])
        .send()
        .await
        .with_context(|| format!("requesting a device code from {url}"))
        .failing(Failure::Http)?;

    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("reading the response from {url}"))
        .failing(Failure::Http)?;
    if !status.is_success() {
        let detail = oauth_error_code(&body).unwrap_or_else(|| truncate(&body));
        if detail == "invalid_client" {
            return Err(CliError::new(
                Failure::Http,
                anyhow!(
                    "the router rejected client id {client_id:?}. It must be listed in the \
                     router's ZEROROUTER_DEVICE_CLIENT_IDS (default: \"zeroclaw\"); \
                     pass --client-id to match."
                ),
            ));
        }
        return Err(CliError::new(
            Failure::Http,
            anyhow!("{url} returned {status}: {detail}"),
        ));
    }
    serde_json::from_str::<DeviceCodeResponse>(&body)
        .with_context(|| format!("parsing the device code response from {url}"))
        .failing(Failure::Http)
}

async fn poll_for_token(
    client: &reqwest::Client,
    base_url: &str,
    client_id: &str,
    device_code: &str,
    initial_interval: Duration,
    ttl: Duration,
) -> Result<DeviceTokenResponse, CliError> {
    let url = format!("{base_url}/auth/device/token");
    let deadline = tokio::time::Instant::now() + ttl;
    let mut interval = initial_interval;

    loop {
        let response = client
            .post(&url)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
                ("client_id", client_id),
            ])
            .send()
            .await
            .with_context(|| format!("polling {url}"))
            .failing(Failure::Http)?;

        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("reading the response from {url}"))
            .failing(Failure::Http)?;

        if status.is_success() {
            return serde_json::from_str::<DeviceTokenResponse>(&body)
                .with_context(|| format!("parsing the token response from {url}"))
                .failing(Failure::Http);
        }

        match oauth_error_code(&body).as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += SLOW_DOWN_INCREMENT,
            Some("access_denied") => {
                return Err(CliError::new(
                    Failure::DeviceAuthorization,
                    anyhow!(
                        "the authorization was denied. If you did approve it, the account may be \
                         frozen or at its API key limit."
                    ),
                ));
            }
            Some("expired_token") => {
                return Err(CliError::new(
                    Failure::DeviceAuthorization,
                    anyhow!("the device code expired before it was approved; run login again"),
                ));
            }
            Some(other) => {
                return Err(CliError::new(
                    Failure::DeviceAuthorization,
                    anyhow!("the router refused the device grant: {other}"),
                ));
            }
            None => {
                return Err(CliError::new(
                    Failure::Http,
                    anyhow!("{url} returned {status}: {}", truncate(&body)),
                ));
            }
        }

        let wait = interval + POLL_MARGIN;
        if tokio::time::Instant::now() + wait >= deadline {
            return Err(CliError::new(
                Failure::DeviceAuthorization,
                anyhow!("timed out waiting for the authorization to be approved"),
            ));
        }
        tokio::time::sleep(wait).await;
    }
}

/// Pull `error` out of an RFC 6749 §5.2 error body.
fn oauth_error_code(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("error")?
        .as_str()
        .map(str::to_owned)
}

fn truncate(body: &str) -> String {
    const LIMIT: usize = 300;
    let trimmed = body.trim();
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_owned();
    }
    let clipped: String = trimmed.chars().take(LIMIT).collect();
    format!("{clipped}...")
}

// ---------------------------------------------------------------------------
// logout / whoami
// ---------------------------------------------------------------------------

fn logout_command(store: &CredentialStore, output: &Output) -> Result<(), CliError> {
    let path = store.path();
    let removed = store.clear().failing(Failure::Runtime)?;
    // Idempotent: logging out when already logged out is a success, so a
    // cleanup script does not have to test first.
    output.emit(&serde_json::json!({
        "status": if removed { "logged_out" } else { "not_authenticated" },
        "removed": removed,
        "credentials_path": path.display().to_string(),
    }))?;
    if removed {
        output.line(&format!("Removed {}", path.display()));
    } else {
        output.line("No stored credential; nothing to remove.");
    }
    Ok(())
}

fn whoami_command(store: &CredentialStore, output: &Output) -> Result<(), CliError> {
    let credential = require_credential(store)?;
    let path = store.path();
    // No `api_key` field here, by contract: this command is the one an agent
    // runs repeatedly and pipes into logs.
    output.emit(&serde_json::json!({
        "status": "authenticated",
        "base_url": credential.base_url,
        "scope": credential.scope,
        "token_type": credential.token_type,
        "key_name": credential.key_name,
        "key_fingerprint": credential.fingerprint(),
        "credentials_path": path.display().to_string(),
        "created_at": credential.created_at,
    }))?;
    if !output.json {
        output.line(&format!("Router:      {}", credential.base_url));
        output.line(&format!("Scope:       {}", credential.scope));
        output.line(&format!("Key name:    {}", credential.key_name));
        output.line(&format!("Fingerprint: {}", credential.fingerprint()));
        output.line(&format!("Logged in:   {}", credential.created_at));
        output.line(&format!("Credential:  {}", path.display()));
    }
    Ok(())
}

fn require_credential(store: &CredentialStore) -> Result<Credential, CliError> {
    match store.load().failing(Failure::Runtime)? {
        Some(credential) => Ok(credential),
        None => Err(CliError::new(
            Failure::NotAuthenticated,
            anyhow!(
                "not logged in: no credential at {}. Run `zerorouter user login`.",
                store.path().display()
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// models
// ---------------------------------------------------------------------------

async fn models_command(
    store: &CredentialStore,
    output: &Output,
    base_url_flag: Option<&str>,
) -> Result<(), CliError> {
    // A stored credential only supplies the default base URL here; the
    // endpoint itself is unauthenticated, so `models` works before `login`.
    let credential = store.load().failing(Failure::Runtime)?;
    let base_url =
        base_url_for_request(base_url_flag, credential.as_ref()).failing(Failure::Runtime)?;
    let client = http_client().failing(Failure::Runtime)?;
    let url = format!("{base_url}/v1/models");
    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))
        .failing(Failure::Http)?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("reading the response from {url}"))
        .failing(Failure::Http)?;
    if !status.is_success() {
        return Err(CliError::new(
            Failure::Http,
            anyhow!("{url} returned {status}: {}", truncate(&body)),
        ));
    }
    let document = serde_json::from_str::<serde_json::Value>(&body)
        .with_context(|| format!("parsing the model list from {url}"))
        .failing(Failure::Http)?;

    // Pass the server's document through unchanged: it is already the stable
    // OpenAI/OpenRouter-shaped contract, and reshaping it here would create a
    // second one to keep in step.
    output.emit(&document)?;
    if !output.json {
        render_models(output, &document);
    }
    Ok(())
}

fn render_models(output: &Output, document: &serde_json::Value) {
    let rows = document
        .get("data")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if rows.is_empty() {
        output.line("No models are currently served.");
        return;
    }
    let width = rows
        .iter()
        .filter_map(|row| row.get("id").and_then(serde_json::Value::as_str))
        .map(str::len)
        .max()
        .unwrap_or(0)
        .max("MODEL".len());

    output.line(&format!(
        "{:<width$}  {:>12}  {:>12}  {:>10}",
        "MODEL",
        "IN $/MTOK",
        "OUT $/MTOK",
        "CONTEXT",
        width = width
    ));
    for row in rows {
        let id = row
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let pricing = row.get("pricing");
        let input = pricing
            .and_then(|pricing| pricing.get("prompt"))
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| "-".to_owned(), per_million);
        let completion = pricing
            .and_then(|pricing| pricing.get("completion"))
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| "-".to_owned(), per_million);
        let context = row
            .get("context_length")
            .and_then(serde_json::Value::as_u64)
            .map_or_else(|| "-".to_owned(), |value| value.to_string());
        output.line(&format!(
            "{id:<width$}  {input:>12}  {completion:>12}  {context:>10}",
            width = width
        ));
    }

    let repricing = rows
        .iter()
        .filter(|row| {
            row.get("pricing")
                .and_then(|pricing| pricing.get("overrides"))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|overrides| !overrides.is_empty())
        })
        .count();
    if repricing > 0 {
        // The rate shown is the base band; saying so matters because the
        // catalog reprices some models at 2x past a prompt threshold.
        output.line("");
        output.line(&format!(
            "{repricing} model(s) reprice above a prompt-size threshold; \
             the rates above are the base band. Use --json for the full schedule."
        ));
    }
}

/// `/v1/models` quotes per single token (OpenRouter's shape); a human table
/// reads better per million.
fn per_million(rate: &str) -> String {
    match rate.parse::<f64>() {
        Ok(value) => format!("{:.2}", value * 1_000_000.0),
        Err(_) => rate.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls_are_normalized_and_validated() {
        assert_eq!(
            normalize_base_url("https://zerorouter.ai/").expect("valid"),
            "https://zerorouter.ai"
        );
        assert_eq!(
            normalize_base_url("  http://127.0.0.1:8080  ").expect("valid"),
            "http://127.0.0.1:8080"
        );
        assert!(normalize_base_url("zerorouter.ai").is_err());
        assert!(normalize_base_url("").is_err());
        assert!(normalize_base_url("ftp://zerorouter.ai").is_err());
    }

    #[test]
    fn exit_codes_are_distinct_nonzero_and_avoid_claps_usage_code() {
        let failures = [
            Failure::Runtime,
            Failure::NotAuthenticated,
            Failure::Http,
            Failure::DeviceAuthorization,
        ];
        let mut seen = std::collections::HashSet::new();
        for failure in failures {
            let code = failure.exit_code();
            assert!(code > 0, "{failure:?} must be a failure code");
            assert_ne!(
                code, 2,
                "{failure:?} must not collide with clap's usage code"
            );
            assert!(seen.insert(code), "{failure:?} duplicates exit code {code}");
        }
    }

    #[test]
    fn oauth_error_codes_are_extracted_and_non_oauth_bodies_are_not() {
        assert_eq!(
            oauth_error_code(r#"{"error":"authorization_pending"}"#).as_deref(),
            Some("authorization_pending")
        );
        assert_eq!(oauth_error_code("not json"), None);
        assert_eq!(oauth_error_code(r#"{"other":"x"}"#), None);
        // The portal envelope nests an object under `error`, not a string;
        // treating that as a code would print "[object]" to the user.
        assert_eq!(
            oauth_error_code(r#"{"error":{"message":"m","code":"c"}}"#),
            None
        );
    }

    #[test]
    fn fingerprints_are_stable_short_digests_that_are_not_the_key() {
        let credential = Credential {
            version: 1,
            base_url: "https://zerorouter.ai".to_owned(),
            api_key: "zcr_0123456789abcdef".to_owned(),
            token_type: "bearer".to_owned(),
            scope: "inference".to_owned(),
            key_name: "test".to_owned(),
            created_at: Utc::now(),
        };
        let fingerprint = credential.fingerprint();
        assert_eq!(fingerprint, credential.fingerprint());
        assert!(fingerprint.starts_with("sha256:"));
        assert_eq!(fingerprint.len(), "sha256:".len() + 16);
        assert!(!fingerprint.contains(&credential.api_key));
    }

    #[test]
    fn per_million_scales_single_token_rates() {
        assert_eq!(per_million("0.000003"), "3.00");
        assert_eq!(per_million("0.0000006"), "0.60");
        // An unparseable rate is shown verbatim rather than silently zeroed.
        assert_eq!(per_million("n/a"), "n/a");
    }

    #[test]
    fn truncation_bounds_untrusted_error_bodies() {
        let long = "x".repeat(1000);
        let truncated = truncate(&long);
        assert!(truncated.len() < long.len());
        assert!(truncated.ends_with("..."));
        assert_eq!(truncate("  short  "), "short");
    }
}
