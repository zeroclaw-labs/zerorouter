//! Build transparency: the running deployment identifies which public build
//! it claims to be, in a form anyone can check against the public record.
//!
//! # What this is, and the exact strength of the claim
//!
//! ZeroRouter reads customer prompts in plaintext — it has to, to meter and
//! route them — so "we don't store your data" is a claim customers currently
//! take on faith. The full fix is confidential computing (remote attestation,
//! TLS terminated inside a measured enclave); the staged plan and threat
//! model live in `docs/VERIFIABILITY.md`. This module is stage one:
//! **provenance**. Every deployed image is attested at build time by the
//! public deploy workflow (GitHub artifact attestations — a Sigstore
//! signature binding the image digest to the exact public commit and workflow
//! run that built it), and this endpoint lets the running service say which
//! digest and commit it is, so the chain
//!
//! ```text
//! running service → image digest → signed attestation → public commit → source
//! ```
//!
//! is walkable by anyone, with no account and no help from the operator.
//!
//! **What it proves:** the digest this deployment reports was built by the
//! public workflow from the public source, unmodified, at the commit it
//! names. **What it deliberately does not prove:** that the host is actually
//! running that image. The report is self-issued — a malicious operator's
//! binary could recite an honest build's digest — and nothing here rules out
//! a listener outside the process (the TLS-terminating load balancer above
//! all). Those are exactly the gaps remote attestation closes in the next
//! stage, and the endpoint says so in its own payload rather than letting
//! the strong-sounding word "attestation" oversell what stage one is.
//!
//! # Where the fields come from
//!
//! - `source_commit` is baked into the image at build time
//!   (`ZEROROUTER_SOURCE_COMMIT`, set by the deploy workflow's build args).
//! - `image` / `image_digest` come from the ECS container metadata endpoint
//!   (`ECS_CONTAINER_METADATA_URI_V4`), which reports the digest the task is
//!   actually pinned to — better evidence than the binary's own opinion,
//!   though still served through the binary. Off ECS (local runs, tests) the
//!   fields are null and the endpoint says why.
//!
//! The metadata lookup happens once, on first request, and only a successful
//! answer is cached: the metadata service being briefly unreachable should
//! not permanently blind the endpoint, and its absence (no env var) is a
//! stable fact worth caching.

use std::time::Duration;

use axum::Json;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::OnceCell;

/// Baked into the image by the deploy workflow; absent on local builds.
pub const SOURCE_COMMIT_ENV: &str = "ZEROROUTER_SOURCE_COMMIT";

/// Injected by ECS on Fargate/EC2; absent everywhere else.
pub const ECS_METADATA_ENV: &str = "ECS_CONTAINER_METADATA_URI_V4";

/// The public home of the source and the attestations. A constant, not
/// configuration: this binary is built from this repository (AGPL-3.0), and a
/// fork that changes the constant is exactly the kind of divergence the
/// attestation chain is built to expose.
const REPOSITORY: &str = "zeroclaw-labs/zerorouter";

const METADATA_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Serialize)]
pub struct TransparencyReport {
    /// The commit this image claims to be built from.
    pub source_commit: Option<String>,
    /// Browsable source at exactly that commit.
    pub source: Option<String>,
    /// The image reference and digest the container runtime reports.
    pub image: Option<String>,
    pub image_digest: Option<String>,
    /// The public attestation record for that digest: a Sigstore signature by
    /// the deploy workflow binding digest → commit → workflow. Fetchable by
    /// anyone (`gh api`, or plain HTTPS — no authentication for public
    /// repositories).
    pub attestations: Option<String>,
    /// The one-line verification for someone with the `gh` CLI.
    pub verify: Option<String>,
    /// The honest scope of the claim, stated where the claim is made.
    pub caveat: &'static str,
}

const CAVEAT: &str = "Self-reported provenance: the digest above is attested (Sigstore, via the \
     public deploy workflow) to be built from the named public commit, but \
     nothing here yet proves this host runs that image unmodified or that \
     plaintext is not observed outside the process. See docs/VERIFIABILITY.md \
     for what each stage proves and what comes next.";

/// Assemble the report from explicit inputs. Separated from the handler so
/// tests can drive it against a fixture metadata server without mutating
/// process environment or fighting the handler's cache.
pub async fn build_report(
    metadata_uri: Option<&str>,
    source_commit: Option<&str>,
) -> TransparencyReport {
    let source_commit = source_commit
        .map(str::trim)
        .filter(|commit| !commit.is_empty())
        .map(str::to_owned);
    let (image, image_digest) = match metadata_uri {
        Some(uri) => container_image(uri).await,
        None => (None, None),
    };
    TransparencyReport {
        source: source_commit
            .as_deref()
            .map(|commit| format!("https://github.com/{REPOSITORY}/tree/{commit}")),
        attestations: image_digest.as_deref().map(|digest| {
            format!("https://api.github.com/repos/{REPOSITORY}/attestations/{digest}")
        }),
        verify: match (&image, &image_digest) {
            (Some(image), Some(digest)) => {
                // The tag half of the reference is irrelevant once a digest is
                // pinned; strip it so the command works verbatim.
                let repository = image.split('@').next().unwrap_or(image);
                let repository = repository.split(':').next().unwrap_or(repository);
                Some(format!(
                    "gh attestation verify oci://{repository}@{digest} --repo {REPOSITORY}"
                ))
            }
            _ => None,
        },
        source_commit,
        image,
        image_digest,
        caveat: CAVEAT,
    }
}

/// Ask the ECS container metadata endpoint which image this container runs.
///
/// Failure shapes are all the same answer — (None, None) — because the
/// endpoint must degrade to "unknown" rather than fail: transparency being
/// unavailable must never make the service look less healthy than it is.
async fn container_image(metadata_uri: &str) -> (Option<String>, Option<String>) {
    let Ok(client) = reqwest::Client::builder().timeout(METADATA_TIMEOUT).build() else {
        return (None, None);
    };
    let Ok(response) = client.get(metadata_uri).send().await else {
        return (None, None);
    };
    if !response.status().is_success() {
        return (None, None);
    }
    let Ok(body) = response.json::<Value>().await else {
        return (None, None);
    };
    let field = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    (field("Image"), field("ImageDigest"))
}

/// The cached report. Only a complete answer is cached: if the metadata env
/// var is present but the lookup failed, the next request retries instead of
/// pinning "unknown" for the process lifetime. A missing env var is itself a
/// stable, cacheable answer (local runs are not going to become ECS tasks).
static REPORT: OnceCell<TransparencyReport> = OnceCell::const_new();

// GET /transparency — public and unauthenticated on purpose: the audience is
// someone who has NOT decided to trust this deployment yet.
pub async fn transparency() -> Json<TransparencyReport> {
    if let Some(report) = REPORT.get() {
        return Json(report.clone());
    }
    let metadata_uri = std::env::var(ECS_METADATA_ENV).ok();
    let source_commit = std::env::var(SOURCE_COMMIT_ENV).ok();
    let report = build_report(metadata_uri.as_deref(), source_commit.as_deref()).await;
    let complete = metadata_uri.is_none() || report.image_digest.is_some();
    if complete {
        let _ = REPORT.set(report.clone());
    }
    Json(report)
}
