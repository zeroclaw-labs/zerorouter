//! `zerorouter admin catalog-drift` over an EDGE catalog (edge mode, stage 2).
//!
//! Its own binary, and for a specific reason: this drives the real command, and
//! the command installs the operator provider inventory — a once-per-process
//! act. `tests/local_candidates.rs` installs one in its own binary before any
//! test runs there, which would make the "without `--providers` this fails"
//! half of the case below unreachable. Here nothing is installed until the
//! command does it.
//!
//! Database-free, exactly like the command it exercises.

use std::path::PathBuf;

use zerorouter::admin::{AdminArgs, AdminCommand, CatalogDriftArgs};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name)
}

fn args(providers: Option<PathBuf>, source_file: PathBuf) -> AdminArgs {
    AdminArgs {
        command: AdminCommand::CatalogDrift(CatalogDriftArgs {
            source_url: zerorouter::drift::DEFAULT_SOURCE_URL.to_owned(),
            source_file: Some(source_file),
            // The second source stays OFF here. This test is about the edge
            // catalog's provider inventory, and it must keep proving what it
            // proved before corroboration existed — including that the command
            // touches no network. `tests/admin_catalog_drift_corroboration.rs`
            // is where the second source is exercised.
            corroborate: false,
            corroborate_url: zerorouter::corroborate::DEFAULT_CORROBORATION_URL.to_owned(),
            corroborate_file: None,
            tiers: Some(fixture("local_candidates_tiers.toml")),
            providers,
            allow_drift: false,
        }),
    }
}

/// The command an edge operator runs in CI, end to end.
///
/// Both halves in one test because the install is once per process and the
/// order matters: the failure must be demonstrated before the inventory exists.
///
/// Before this stage wired the inventory into the command, the first half was
/// the ONLY reachable behaviour — an edge catalog could not be reconciled at
/// all, because its tier file names an upstream the shipped inventory does not
/// have. The deployment shape with the most local configuration to get wrong
/// was the one that could not run the check.
#[tokio::test]
async fn catalog_drift_reconciles_an_edge_catalog_once_it_is_told_the_providers() {
    let source = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("edge_drift_source.json");
    tokio::fs::write(
        &source,
        r#"{"openai": {"models": {
            "upstream/edge-burst": {"cost": {"input": 1.0, "output": 2.0, "cache_read": 0.2}},
            "upstream/toolless-burst": {"cost": {"input": 1.0, "output": 2.0, "cache_read": 0.2}}
        }}}"#,
    )
    .await
    .expect("the drift source fixture should write");

    let without = zerorouter::admin::run(args(None, source.clone()))
        .await
        .expect_err("an edge catalog cannot even be loaded without its provider inventory");
    let detail = format!("{without:#}");
    assert!(
        detail.contains("local-llama") || detail.contains("tier catalog"),
        "the failure should name the catalog it could not load: {detail}"
    );

    // With the inventory, the catalog loads, the cloud rung reconciles against
    // the source, and the local rungs are reported without failing the command
    // — the exit code CI reads.
    zerorouter::admin::run(args(Some(fixture("local_providers.json")), source))
        .await
        .expect("an entirely correct edge catalog must reconcile and exit zero");
}
