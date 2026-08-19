//! The SECOND pricing source, driven through the real binary.
//!
//! Every test here runs `zerorouter admin catalog-drift` as a process and
//! reads its EXIT STATUS, not a `Result` from a library call. That is the
//! point: the operator's instruction is that a second, non-authoritative
//! source must never fail the command, and the only honest way to prove a
//! thing about an exit code is to observe one. A test that asserted on a
//! `Report` struct would prove that the struct carries no verdict, which is
//! not the same claim.
//!
//! Its own binary rather than an addition to `admin_catalog_drift.rs`: that
//! one drives the command IN-PROCESS to exercise the once-per-process operator
//! inventory install, and its first assertion depends on nothing having
//! installed one yet. Nothing here installs one either — both pins in
//! `corroboration_tiers.toml` are on shipped providers — but running out of
//! process keeps the two facts independent.
//!
//! Database-free, exactly like the command it exercises.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name)
}

fn write(name: &str, body: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, body).expect("a source fixture should write");
    path
}

/// models.dev's shape, agreeing with `corroboration_tiers.toml` exactly.
const PRIMARY_AGREEING: &str = r#"{
  "openai": { "models": { "gpt-5.6-luna": { "cost": {
    "input": 0.2, "output": 1.2, "cache_read": 0.02,
    "tiers": [ { "input": 0.4, "output": 1.8, "cache_read": 0.04,
                 "tier": { "type": "context", "size": 272000 } } ] } } } },
  "anthropic": { "models": { "claude-sonnet-5": { "cost": {
    "input": 2.0, "output": 10.0, "cache_read": 0.2 } } } }
}"#;

/// The same, with luna's base input rate moved. The catalog then disagrees
/// with the primary and the command must fail — whatever the second source
/// says.
const PRIMARY_DRIFTED: &str = r#"{
  "openai": { "models": { "gpt-5.6-luna": { "cost": {
    "input": 0.9, "output": 1.2, "cache_read": 0.02,
    "tiers": [ { "input": 0.4, "output": 1.8, "cache_read": 0.04,
                 "tier": { "type": "context", "size": 272000 } } ] } } } },
  "anthropic": { "models": { "claude-sonnet-5": { "cost": {
    "input": 2.0, "output": 10.0, "cache_read": 0.2 } } } }
}"#;

/// A second source that disagrees about everything it can: a different
/// boundary, rates 4.5x apart, and one of the two pins missing entirely.
const SECOND_HOSTILE: &str = r#"{"data": [
  { "id": "openai/gpt-5.6-luna", "pricing": {
      "prompt": "0.0000009", "completion": "0.0000054", "input_cache_read": "0.00000009",
      "overrides": [ { "min_prompt_tokens": 100000, "prompt": "0.0000018",
                       "completion": "0.0000108", "input_cache_read": "0.00000018" } ] } }
]}"#;

/// A second source that corroborates the DRIFTED primary perfectly — same
/// boundaries, same rates, both models present. It agrees with models.dev and
/// models.dev disagrees with `tiers.toml`, so this is the second source at its
/// most persuasive, and it still may not rescue the catalog.
const SECOND_AGREEING_WITH_DRIFTED_PRIMARY: &str = r#"{"data": [
  { "id": "openai/gpt-5.6-luna", "pricing": {
      "prompt": "0.0000009", "completion": "0.0000012", "input_cache_read": "0.00000002",
      "overrides": [ { "min_prompt_tokens": 272000, "prompt": "0.0000004",
                       "completion": "0.0000018", "input_cache_read": "0.00000004" } ] } },
  { "id": "anthropic/claude-sonnet-5", "pricing": {
      "prompt": "0.000002", "completion": "0.00001", "input_cache_read": "0.0000002" } }
]}"#;

/// A second source that corroborates the AGREEING primary perfectly. The
/// quiet path: one summary line and nothing else.
const SECOND_AGREEING: &str = r#"{"data": [
  { "id": "openai/gpt-5.6-luna", "pricing": {
      "prompt": "0.0000002", "completion": "0.0000012", "input_cache_read": "0.00000002",
      "overrides": [ { "min_prompt_tokens": 272000, "prompt": "0.0000004",
                       "completion": "0.0000018", "input_cache_read": "0.00000004" } ] } },
  { "id": "anthropic/claude-sonnet-5", "pricing": {
      "prompt": "0.000002", "completion": "0.00001", "input_cache_read": "0.0000002" } }
]}"#;

struct Run {
    ok: bool,
    stdout: String,
}

fn run(primary: &Path, extra: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_zerorouter"))
        .args(["admin", "catalog-drift"])
        .arg("--tiers")
        .arg(fixture("corroboration_tiers.toml"))
        .arg("--source-file")
        .arg(primary)
        .args(extra)
        // The ambient environment must not decide what this test reconciles.
        .env_remove("ZEROROUTER_TIERS_PATH")
        .env_remove("ZEROROUTER_PROVIDERS_PATH")
        .env_remove("DATABASE_URL")
        .output()
        .expect("the zerorouter binary should run");
    Run {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    }
}

/// THE test the whole design rests on.
///
/// The second source contradicts the reconciliation on every axis it has: it
/// puts the boundary at 100,000 where the primary says 272,000, quotes rates
/// 4.5x apart, and has never heard of one of the two pins. All of it is
/// reported. None of it fails the command.
#[test]
fn nothing_the_second_source_says_can_fail_the_command() {
    let run = run(
        &write("corroboration_primary_agreeing.json", PRIMARY_AGREEING),
        &[
            "--corroborate-file",
            write("corroboration_second_hostile.json", SECOND_HOSTILE)
                .to_str()
                .expect("a tmpdir path is UTF-8"),
        ],
    );

    assert!(
        run.ok,
        "a disagreeing second source must never redden the command:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("no drift"),
        "and the verdict is still the primary's alone:\n{}",
        run.stdout
    );

    // Reported, though — an exit code that ignores the second source is only
    // half the requirement. If these stop appearing, the assertion above is
    // passing because nothing ran.
    assert!(
        run.stdout.contains("BOUNDARIES DISAGREE"),
        "a threshold disagreement is the prominent signal:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("100000") && run.stdout.contains("272000"),
        "with both catalogs' numbers:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("not listed by the second source")
            && run.stdout.contains("anthropic/claude-sonnet-5"),
        "an id that does not map is named:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("rate differences"),
        "and rate differences print as information:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("INFORMATIONAL") && run.stdout.contains("RESELLER"),
        "labelled so nobody reads them as a finding:\n{}",
        run.stdout
    );
}

/// The converse, and the half that would be easy to lose. A second source that
/// agrees with the primary in every particular is still not a vote: the
/// catalog drifted from the primary, so the command fails.
#[test]
fn a_second_source_that_agrees_cannot_rescue_a_drifted_catalog() {
    let run = run(
        &write("corroboration_primary_drifted.json", PRIMARY_DRIFTED),
        &[
            "--corroborate-file",
            write(
                "corroboration_second_agreeing_drifted.json",
                SECOND_AGREEING_WITH_DRIFTED_PRIMARY,
            )
            .to_str()
            .expect("a tmpdir path is UTF-8"),
        ],
    );

    assert!(
        !run.ok,
        "a drifted basis fails the command regardless of corroboration:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("BASIS DRIFT"),
        "for the primary's reason:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("boundaries agree, no rate differences"),
        "while the second source reports itself entirely satisfied:\n{}",
        run.stdout
    );
}

/// And the same run without the second source at all, so the exit codes above
/// are known to be the ones the command already produced rather than ones the
/// new section happens to reproduce.
#[test]
fn the_exit_code_is_the_same_with_the_second_source_and_without_it() {
    let agreeing = write("corroboration_primary_agreeing_bare.json", PRIMARY_AGREEING);
    let drifted = write("corroboration_primary_drifted_bare.json", PRIMARY_DRIFTED);
    let second = write("corroboration_second_hostile_bare.json", SECOND_HOSTILE);
    let second = second.to_str().expect("a tmpdir path is UTF-8");

    assert!(run(&agreeing, &[]).ok);
    assert!(run(&agreeing, &["--corroborate-file", second]).ok);
    assert!(!run(&drifted, &[]).ok);
    assert!(!run(&drifted, &["--corroborate-file", second]).ok);
}

#[test]
fn corroboration_is_off_unless_it_is_asked_for() {
    // The daily CI workflow runs this command bare. It must keep answering one
    // question about `tiers.toml` and must not acquire a dependency on a third
    // party's availability as a side effect of this feature existing.
    let run = run(
        &write("corroboration_primary_optin.json", PRIMARY_AGREEING),
        &[],
    );
    assert!(run.ok);
    assert!(
        !run.stdout.contains("Second source"),
        "no flag, no second fetch:\n{}",
        run.stdout
    );
}

#[test]
fn two_agreeing_sources_collapse_to_a_single_quiet_line() {
    // A report nobody reads is worth nothing, and a screenful of `ok` rows is
    // how a report stops being read.
    let run = run(
        &write("corroboration_primary_quiet.json", PRIMARY_AGREEING),
        &[
            "--corroborate-file",
            write("corroboration_second_quiet.json", SECOND_AGREEING)
                .to_str()
                .expect("a tmpdir path is UTF-8"),
        ],
    );

    assert!(run.ok);
    assert!(
        run.stdout
            .contains("2 candidate(s) cross-checked, boundaries agree, no rate differences"),
        "one line when everything agrees:\n{}",
        run.stdout
    );
    // "no rate differences" is the clean line itself, so the noisy marker has
    // to be the section header rather than the phrase.
    for noisy in [
        "BOUNDARIES DISAGREE",
        "not listed",
        "rate differences (INFORMATIONAL",
        "override(s) ignored",
    ] {
        assert!(
            !run.stdout.contains(noisy),
            "and nothing else: {noisy}\n{}",
            run.stdout
        );
    }
}

/// Every way the second source can be useless, one after another. A flaky
/// third party must never redden CI or block an operator, so each of these
/// prints one line and the command finishes exactly as it would have.
#[test]
fn a_second_source_that_cannot_be_read_is_skipped_and_the_command_finishes_normally() {
    let primary = write("corroboration_primary_skips.json", PRIMARY_AGREEING);
    let garbage = write(
        "corroboration_second_garbage.json",
        "<html><body>503 Service Unavailable</body></html>",
    );
    let empty = write("corroboration_second_empty.json", r#"{"data": []}"#);

    let cases: Vec<(&str, Vec<String>)> = vec![
        (
            "a file that is not there",
            vec![
                "--corroborate-file".to_owned(),
                PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
                    .join("corroboration_second_absent.json")
                    .display()
                    .to_string(),
            ],
        ),
        (
            "an HTML error page where JSON was promised",
            vec![
                "--corroborate-file".to_owned(),
                garbage.display().to_string(),
            ],
        ),
        (
            "a document listing no models at all",
            vec!["--corroborate-file".to_owned(), empty.display().to_string()],
        ),
        (
            "an unreachable host",
            vec![
                "--corroborate".to_owned(),
                "--corroborate-url".to_owned(),
                // Port 1 on loopback: refused immediately, no DNS, no waiting.
                "http://127.0.0.1:1/api/v1/models".to_owned(),
            ],
        ),
    ];

    for (why, extra) in cases {
        let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
        let run = run(&primary, &extra);
        assert!(run.ok, "{why} must not fail the command:\n{}", run.stdout);
        assert!(
            run.stdout.contains("SKIPPED"),
            "{why} must say so in one line:\n{}",
            run.stdout
        );
        assert!(
            run.stdout.contains("the verdict above is unchanged"),
            "{why} must say the report still stands:\n{}",
            run.stdout
        );
        assert!(
            run.stdout.contains("no drift"),
            "{why} must leave the primary reconciliation intact:\n{}",
            run.stdout
        );
    }
}
