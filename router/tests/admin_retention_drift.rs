//! `zerorouter admin retention-drift`, end to end and offline.
//!
//! Every case here runs through `--source-dir`, so the suite touches no network
//! and cannot be reddened by a vendor's marketing site. That is also the mode CI
//! can pin a fixture in; the daily job runs the fetching mode against the real
//! pages, where a change is the signal rather than a flake.
//!
//! Database-free, exactly like the command it exercises.

use std::path::{Path, PathBuf};

use zerorouter::{
    admin::{AdminArgs, AdminCommand, RetentionDriftArgs},
    retention::digest,
};

/// A policy page whose visible text is `body`.
const fn page(body: &str) -> &str {
    body
}

fn tiers_source(anthropic_sha: &str, openai_sha: &str) -> String {
    format!(
        r#"
schema_version = 1

[retention.anthropic]
posture = "standard"
description = "retains inputs and outputs for 30 days"
source_url = "https://example.invalid/anthropic-policy"
verified = "2026-08-20"
source_sha256 = "{anthropic_sha}"

[retention.openai]
posture = "standard"
description = "retains abuse-monitoring logs for 30 days"
source_url = "https://example.invalid/openai-policy"
verified = "2026-08-20"
source_sha256 = "{openai_sha}"

[tiers."anthropic/pin"]
[tiers."anthropic/pin".rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
[[tiers."anthropic/pin".candidates]]
id = "anthropic/pin"
provider = "anthropic"
model = "pin"
[tiers."anthropic/pin".candidates.rates]
input_per_mtok = 1.00
output_per_mtok = 2.00

[tiers."openai/pin"]
[tiers."openai/pin".rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
[[tiers."openai/pin".candidates]]
id = "openai/pin"
provider = "openai"
model = "pin"
[tiers."openai/pin".candidates.rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
"#
    )
}

const ANTHROPIC_PAGE: &str =
    page("<html><body><p>We delete inputs and outputs within 30 days.</p></body></html>");
const OPENAI_PAGE: &str =
    page("<html><body><p>Abuse monitoring logs are retained for up to 30 days.</p></body></html>");

/// Write a tier file and a `--source-dir` beside it, both unique to `name` so
/// tests running in parallel never share a path.
async fn scratch(name: &str, tiers: &str, pages: &[(&str, &str)]) -> (PathBuf, PathBuf) {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let dir = root.join("pages");
    tokio::fs::create_dir_all(&dir)
        .await
        .expect("the fixture directory should be created");
    let tiers_path = root.join("tiers.toml");
    tokio::fs::write(&tiers_path, tiers)
        .await
        .expect("the tier fixture should write");
    for (provider, body) in pages {
        tokio::fs::write(dir.join(format!("{provider}.html")), body)
            .await
            .expect("the page fixture should write");
    }
    (tiers_path, dir)
}

fn args(tiers: &Path, source_dir: &Path) -> AdminArgs {
    AdminArgs {
        command: AdminCommand::RetentionDrift(RetentionDriftArgs {
            source_dir: Some(source_dir.to_path_buf()),
            // The second source stays OFF: this suite is about the primary
            // check and must keep proving it touches no network.
            corroborate: false,
            corroborate_url: zerorouter::retention::DEFAULT_CORROBORATION_URL.to_owned(),
            corroborate_file: None,
            tiers: Some(tiers.to_path_buf()),
            providers: None,
            max_age_days: None,
            allow_drift: false,
        }),
    }
}

/// The green path: every pinned digest still matches the page behind it.
#[tokio::test]
async fn retention_drift_exits_zero_when_every_policy_page_is_unchanged() {
    let (tiers, dir) = scratch(
        "retention_unchanged",
        &tiers_source(&digest(ANTHROPIC_PAGE), &digest(OPENAI_PAGE)),
        &[("anthropic", ANTHROPIC_PAGE), ("openai", OPENAI_PAGE)],
    )
    .await;

    zerorouter::admin::run(args(&tiers, &dir))
        .await
        .expect("an unchanged set of policy pages must exit zero");
}

/// THE MAINTENANCE LOOP: a page whose wording moved fails the command.
///
/// This is what the daily CI job is for, and the case a tampered pin must also
/// produce — a pinned digest that does not match the page is the same condition
/// whether the page moved or the pin was edited.
#[tokio::test]
async fn a_policy_page_that_changed_since_verification_fails_the_command() {
    // The pin still records the OLD page; the directory serves a reworded one.
    let reworded = "<html><body><p>We delete inputs and outputs within 90 days.</p></body></html>";
    let (tiers, dir) = scratch(
        "retention_changed",
        &tiers_source(&digest(ANTHROPIC_PAGE), &digest(OPENAI_PAGE)),
        &[("anthropic", reworded), ("openai", OPENAI_PAGE)],
    )
    .await;

    let error = zerorouter::admin::run(args(&tiers, &dir))
        .await
        .expect_err("a changed policy page must fail the command");
    let detail = format!("{error:#}");
    assert!(
        detail.contains("re-verification") || detail.contains("re-pin"),
        "the failure must send a human back to the page: {detail}"
    );
}

/// A tampered pin is caught for the same reason a changed page is.
///
/// Stated as its own test because it is a different threat: the page is
/// untouched and the FILE was edited. A label whose digest can be edited to
/// silence the check would make the whole mechanism decorative.
#[tokio::test]
async fn a_tampered_pinned_digest_fails_the_command() {
    let (tiers, dir) = scratch(
        "retention_tampered",
        // Both pages are served exactly as pinned except that anthropic's
        // recorded digest has been swapped for a plausible-looking wrong one.
        &tiers_source(&"4".repeat(64), &digest(OPENAI_PAGE)),
        &[("anthropic", ANTHROPIC_PAGE), ("openai", OPENAI_PAGE)],
    )
    .await;

    zerorouter::admin::run(args(&tiers, &dir))
        .await
        .expect_err("a pinned digest that does not match its page must fail the command");
}

/// A missing page is actionable, not a pass.
///
/// The failure that would otherwise look fine forever: a pin whose evidence
/// cannot be reached has silently lost its re-verification loop, and treating
/// that as success would let the label assert itself against nothing.
#[tokio::test]
async fn a_policy_page_that_cannot_be_read_fails_the_command() {
    let (tiers, dir) = scratch(
        "retention_unreachable",
        &tiers_source(&digest(ANTHROPIC_PAGE), &digest(OPENAI_PAGE)),
        // openai.html is deliberately absent.
        &[("anthropic", ANTHROPIC_PAGE)],
    )
    .await;

    zerorouter::admin::run(args(&tiers, &dir))
        .await
        .expect_err("a policy page that cannot be read must fail the command");
}

/// `--allow-drift` is the release valve, and it must not be the default.
#[tokio::test]
async fn allow_drift_reports_the_change_and_exits_zero() {
    let reworded = "<html><body><p>Rewritten entirely.</p></body></html>";
    let (tiers, dir) = scratch(
        "retention_allow_drift",
        &tiers_source(&digest(ANTHROPIC_PAGE), &digest(OPENAI_PAGE)),
        &[("anthropic", reworded), ("openai", OPENAI_PAGE)],
    )
    .await;

    let mut allowed = args(&tiers, &dir);
    if let AdminCommand::RetentionDrift(ref mut inner) = allowed.command {
        inner.allow_drift = true;
    }
    zerorouter::admin::run(allowed)
        .await
        .expect("--allow-drift must report the change and still exit zero");
}

/// Cosmetic churn must NOT fail the command.
///
/// The property that decides whether this check survives contact with real
/// marketing pages: if a redeploy that changes only a build id reddens CI, the
/// job gets muted and the mechanism is worse than nothing.
#[tokio::test]
async fn a_redeployed_page_with_the_same_words_still_passes() {
    let deployed = "<html><head><script>window.__BUILD__='7f3a9c';</script>\
                    <style>.x{color:#111}</style></head>\
                    <body>\n\n  <!-- rendered 2026-08-20 -->\n  \
                    <p>We   delete inputs and outputs\n within 30 days.</p></body></html>";
    // Pinned against the plain page; served as a full CMS render of the same
    // sentence.
    let (tiers, dir) = scratch(
        "retention_redeploy",
        &tiers_source(&digest(ANTHROPIC_PAGE), &digest(OPENAI_PAGE)),
        &[("anthropic", deployed), ("openai", OPENAI_PAGE)],
    )
    .await;

    zerorouter::admin::run(args(&tiers, &dir))
        .await
        .expect("a redeploy that changes no words must not fail the command");
}
