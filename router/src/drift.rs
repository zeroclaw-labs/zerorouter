//! Catalog drift detection: does `tiers.toml` still describe reality?
//!
//! Every price, context window and modality ZeroRouter knows is a static
//! belief written into `config/tiers.toml` by a human. Nothing in the running
//! service ever checks that belief against the upstreams, and the failure is
//! silent in the direction that costs money: if a provider raises a rate,
//! ZeroRouter keeps charging the old price AND keeps recording COGS at the old
//! basis, so the treasury view reports a healthy margin while the real invoice
//! is larger. You find out when the bill arrives.
//!
//! This module is the missing alarm. It fetches a public model catalog,
//! compares it against the shipped tier file, and reports. **It never writes.**
//! Prices are not data to be auto-applied — a bad fetch that repriced a live
//! billing catalog would be worse than the staleness it fixed, so the output
//! is a report a human acts on, and a non-zero exit so CI or cron can shout.
//!
//! Two comparisons matter and they answer different questions:
//!
//! - **basis vs upstream** — is our recorded cost still what we actually pay?
//!   This is the one that silently destroys margin.
//! - **sell vs upstream** — what markup is the customer actually paying? A
//!   pass-through tier claims this is zero. The load-time validator cannot
//!   check it, because it only proves the file is consistent with itself.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::{ModelMetadata, TierCatalog};
use crate::provider::ModelRates;

/// The public catalog ZeroRouter reconciles against.
pub const DEFAULT_SOURCE_URL: &str = "https://models.dev/api.json";

/// How closely a rate must match before it counts as agreement. Prices are
/// quoted in dollars per million tokens with at most a few decimals, so this
/// is float-comparison slack, not a tolerance for real drift.
const EPSILON: f64 = 1e-9;

/// One upstream's entry in the source catalog. Only the fields ZeroRouter
/// reconciles are modeled; the source carries far more.
#[derive(Debug, Deserialize)]
struct SourceProvider {
    #[serde(default)]
    models: BTreeMap<String, SourceModel>,
}

#[derive(Debug, Deserialize)]
struct SourceModel {
    #[serde(default)]
    cost: Option<SourceCost>,
    #[serde(default)]
    limit: Option<SourceLimit>,
    #[serde(default)]
    modalities: Option<SourceModalities>,
    #[serde(default)]
    tool_call: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct SourceCost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct SourceLimit {
    context: Option<u64>,
    output: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct SourceModalities {
    #[serde(default)]
    input: Vec<String>,
}

/// What the reconciliation found for one candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Recorded basis matches the upstream's published cost.
    Match,
    /// Recorded basis disagrees with the upstream. Margin is not what the
    /// treasury view believes.
    BasisDrift,
    /// The upstream publishes no cost for this model, so the basis cannot be
    /// checked at all. Not the same as agreement.
    Unpriced,
    /// The model is not in the source catalog. Could be a rename, a
    /// retirement, or a model the source does not cover.
    Missing,
}

impl Verdict {
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Match => "ok",
            Self::BasisDrift => "BASIS DRIFT",
            Self::Unpriced => "unpriced upstream",
            Self::Missing => "NOT IN SOURCE",
        }
    }

    /// Whether this verdict should fail the command. `Unpriced` does not: the
    /// source simply has nothing to say, and failing on it would train
    /// operators to ignore the alarm.
    #[must_use]
    pub const fn is_actionable(&self) -> bool {
        matches!(self, Self::BasisDrift | Self::Missing)
    }
}

/// One metadata field where the tier file and the source catalog disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataDrift {
    /// The `tiers.toml` field name, so an operator can grep for the line.
    pub field: &'static str,
    pub kind: MetadataDriftKind,
    /// What the file says, rendered. `"-"` when it says nothing.
    pub recorded: String,
    /// What the source publishes, rendered.
    pub upstream: String,
}

/// Why a metadata field needs attention. The two differ in severity, not just
/// in wording, so they are separate: one is a false claim, the other a gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataDriftKind {
    /// Both sides carry a value and they differ. The file states something
    /// untrue and every client is acting on it — the metadata equivalent of
    /// [`Verdict::BasisDrift`], and actionable for the same reason.
    Disagrees,
    /// The source publishes a value and the file declares none, so the listing
    /// omits the field and each client substitutes its own default (ZeroClaw:
    /// 32,000 tokens and no vision). Worth closing, but not a lie.
    Undeclared,
}

impl MetadataDriftKind {
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Disagrees => "METADATA DRIFT",
            Self::Undeclared => "undeclared",
        }
    }

    /// Whether this should fail the command.
    ///
    /// Only `Disagrees` does. `Undeclared` deliberately does not, for the same
    /// reason [`Verdict::Unpriced`] does not: the file is *allowed* to be
    /// silent — absence means "unknown", which is a true statement — and
    /// failing CI on a legal, honest omission is how an alarm gets ignored.
    /// It still prints, because the omission has a real cost downstream.
    #[must_use]
    pub const fn is_actionable(&self) -> bool {
        matches!(self, Self::Disagrees)
    }
}

/// One reconciled candidate.
#[derive(Debug, Clone)]
pub struct CandidateDrift {
    pub tier: String,
    pub candidate_id: String,
    pub provider: String,
    pub model: String,
    /// The model key actually matched in the source, when it differs from the
    /// dispatched model string (a dated Anthropic snapshot, say).
    pub matched_as: Option<String>,
    pub verdict: Verdict,
    pub recorded_basis: ModelRates,
    pub upstream_cost: ModelRates,
    /// The tier's sell rate — what the customer pays.
    pub sell: ModelRates,
    /// What `tiers.toml` claims this model can take and produce.
    pub recorded_metadata: ModelMetadata,
    /// What the source catalog publishes about the same model. Named to
    /// mirror `recorded_basis` / `upstream_cost`: the module compares two
    /// beliefs about one model, and both halves of each pair read alike.
    pub upstream_metadata: ModelMetadata,
    /// Metadata fields the two disagree about. Empty when they agree, when
    /// the source is silent, or when the model is not in the source at all.
    pub metadata_drift: Vec<MetadataDrift>,
}

impl CandidateDrift {
    /// Markup the customer pays over the upstream's published output rate, as
    /// a multiplier. `None` when either side is unknown. A pass-through tier
    /// claims this is 1.0.
    #[must_use]
    pub fn sell_markup(&self) -> Option<f64> {
        match (
            self.sell.output_per_mtok,
            self.upstream_cost.output_per_mtok,
        ) {
            (Some(sell), Some(cost)) if cost > EPSILON => Some(sell / cost),
            _ => None,
        }
    }

    /// Whether any metadata field is drifted badly enough to fail the command.
    #[must_use]
    pub fn has_actionable_metadata_drift(&self) -> bool {
        self.metadata_drift
            .iter()
            .any(|drift| drift.kind.is_actionable())
    }

    /// Whether the customer is paying materially more than the upstream costs
    /// while the tier is priced as pass-through.
    #[must_use]
    pub fn is_undisclosed_markup(&self) -> bool {
        self.sell_markup().is_some_and(|m| m > 1.0 + 1e-6)
    }
}

/// Anthropic dispatches dated snapshots (`claude-haiku-4-5-20251001`) that the
/// source catalogs under the undated family name. Strip one trailing
/// `-YYYYMMDD` so a snapshot still reconciles, and report what was matched so
/// the substitution is never invisible.
fn undated(model: &str) -> Option<&str> {
    let (head, tail) = model.rsplit_once('-')?;
    (tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit())).then_some(head)
}

fn rates_agree(recorded: ModelRates, upstream: ModelRates) -> bool {
    let close = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() < EPSILON,
        // The upstream not publishing a dimension is not disagreement about
        // it; `Verdict::Unpriced` covers the wholly-unpriced case separately.
        (_, None) => true,
        (None, Some(_)) => false,
    };
    close(recorded.input_per_mtok, upstream.input_per_mtok)
        && close(recorded.output_per_mtok, upstream.output_per_mtok)
        && close(
            recorded.cached_input_per_mtok,
            upstream.cached_input_per_mtok,
        )
}

/// Compare the metadata a candidate records against what the source publishes.
///
/// The asymmetry mirrors [`rates_agree`], and for the same reason. The
/// *source* being silent is never disagreement: it simply has nothing to say
/// about that field, and manufacturing a verdict out of its silence would be
/// guessing. The *file* being silent while the source publishes is worth
/// reporting — the listing then omits the field and every client substitutes
/// its own default — but it is a gap rather than a false claim, which is why
/// [`MetadataDriftKind`] keeps the two apart.
///
/// Modalities compare as sets. The two catalogs have no reason to agree on
/// ordering, and a reordered list is not drift.
#[must_use]
fn metadata_drift(recorded: &ModelMetadata, upstream: &ModelMetadata) -> Vec<MetadataDrift> {
    let mut out = Vec::new();

    let mut compare =
        |field: &'static str, recorded: Option<String>, upstream: Option<String>, agree: bool| {
            let Some(upstream) = upstream else {
                return;
            };
            match recorded {
                None => out.push(MetadataDrift {
                    field,
                    kind: MetadataDriftKind::Undeclared,
                    recorded: "-".to_owned(),
                    upstream,
                }),
                Some(recorded) if !agree => out.push(MetadataDrift {
                    field,
                    kind: MetadataDriftKind::Disagrees,
                    recorded,
                    upstream,
                }),
                Some(_) => {}
            }
        };

    compare(
        "context_window",
        recorded.context_window.map(|v| v.to_string()),
        upstream.context_window.map(|v| v.to_string()),
        recorded.context_window == upstream.context_window,
    );
    compare(
        "max_output_tokens",
        recorded.max_output_tokens.map(|v| v.to_string()),
        upstream.max_output_tokens.map(|v| v.to_string()),
        recorded.max_output_tokens == upstream.max_output_tokens,
    );
    let sorted = |modalities: &Option<Vec<String>>| {
        modalities.as_ref().map(|modalities| {
            let mut modalities = modalities.clone();
            modalities.sort();
            modalities
        })
    };
    compare(
        "input_modalities",
        recorded
            .input_modalities
            .as_ref()
            .map(|modalities| modalities.join(",")),
        upstream
            .input_modalities
            .as_ref()
            .map(|modalities| modalities.join(",")),
        sorted(&recorded.input_modalities) == sorted(&upstream.input_modalities),
    );
    compare(
        "tool_call",
        recorded.tool_call.map(|v| v.to_string()),
        upstream.tool_call.map(|v| v.to_string()),
        recorded.tool_call == upstream.tool_call,
    );

    out
}

/// Reconcile a loaded catalog against an already-fetched source document.
///
/// Split from the fetch so the comparison is testable without a network.
/// Withheld tiers are reconciled too: a tier withheld for below-cost pricing
/// is precisely the one an operator most wants an upstream number for.
#[must_use]
pub fn reconcile(catalog: &TierCatalog, source: &str) -> Vec<CandidateDrift> {
    let providers: BTreeMap<String, SourceProvider> =
        serde_json::from_str(source).unwrap_or_default();

    let mut out = Vec::new();
    let definitions = catalog.tiers.iter().chain(
        catalog
            .unavailable
            .iter()
            .map(|(id, withheld)| (id, &withheld.definition)),
    );

    for (tier_id, definition) in definitions {
        for candidate in &definition.candidates {
            let models = providers.get(&candidate.provider).map(|p| &p.models);
            let (entry, matched_as) = match models {
                Some(models) => match models.get(&candidate.model) {
                    Some(entry) => (Some(entry), None),
                    None => match undated(&candidate.model).and_then(|base| models.get(base)) {
                        Some(entry) => (
                            Some(entry),
                            undated(&candidate.model).map(ToOwned::to_owned),
                        ),
                        None => (None, None),
                    },
                },
                None => (None, None),
            };

            let upstream_cost = entry
                .and_then(|e| e.cost.as_ref())
                .map(|c| ModelRates {
                    input_per_mtok: c.input,
                    output_per_mtok: c.output,
                    cached_input_per_mtok: c.cache_read,
                })
                .unwrap_or_default();

            let verdict = match entry {
                None => Verdict::Missing,
                Some(entry) if entry.cost.is_none() => Verdict::Unpriced,
                Some(_) if rates_agree(candidate.rates, upstream_cost) => Verdict::Match,
                Some(_) => Verdict::BasisDrift,
            };

            let upstream_metadata = ModelMetadata {
                context_window: entry.and_then(|e| e.limit.as_ref()).and_then(|l| l.context),
                max_output_tokens: entry.and_then(|e| e.limit.as_ref()).and_then(|l| l.output),
                // `None` here means the source said nothing about modalities,
                // which is not the same as it publishing an empty list.
                input_modalities: entry
                    .and_then(|e| e.modalities.as_ref())
                    .map(|m| m.input.clone()),
                tool_call: entry.and_then(|e| e.tool_call),
            };

            out.push(CandidateDrift {
                tier: tier_id.clone(),
                candidate_id: candidate.id.clone(),
                provider: candidate.provider.clone(),
                model: candidate.model.clone(),
                matched_as,
                verdict,
                recorded_basis: candidate.rates,
                upstream_cost,
                sell: definition.rates,
                metadata_drift: metadata_drift(&candidate.metadata, &upstream_metadata),
                recorded_metadata: candidate.metadata.clone(),
                upstream_metadata,
            });
        }
    }
    out
}

/// Fetch the source catalog. Kept separate from [`reconcile`] so a caller can
/// supply a cached document, and so the request path never touches this.
pub async fn fetch_source(url: &str) -> Result<String> {
    let response = reqwest::Client::new()
        .get(url)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("fetching the model catalog from {url}"))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("model catalog source {url} answered HTTP {status}");
    }
    response
        .text()
        .await
        .with_context(|| format!("reading the model catalog body from {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TierCandidate, TierDefinition};

    fn rates(input: f64, cached: f64, output: f64) -> ModelRates {
        ModelRates {
            input_per_mtok: Some(input),
            cached_input_per_mtok: Some(cached),
            output_per_mtok: Some(output),
        }
    }

    fn catalog_with(model: &str, basis: ModelRates, sell: ModelRates) -> TierCatalog {
        catalog_declaring(model, basis, sell, ModelMetadata::default())
    }

    /// `catalog_with`, plus what the file claims the model can take and
    /// produce — the half of the comparison that used to have no input.
    fn catalog_declaring(
        model: &str,
        basis: ModelRates,
        sell: ModelRates,
        metadata: ModelMetadata,
    ) -> TierCatalog {
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "zero/test".to_owned(),
            TierDefinition {
                rates: sell,
                candidates: vec![TierCandidate {
                    id: format!("anthropic/{model}"),
                    provider: "anthropic".to_owned(),
                    model: model.to_owned(),
                    rates: basis,
                    metadata,
                }],
            },
        );
        TierCatalog {
            schema_version: 1,
            tiers,
            unavailable: BTreeMap::new(),
        }
    }

    /// The metadata models.dev publishes for `claude-sonnet-5` in `SOURCE`.
    fn sonnet_metadata() -> ModelMetadata {
        ModelMetadata {
            context_window: Some(1_000_000),
            max_output_tokens: Some(128_000),
            input_modalities: Some(vec![
                "text".to_owned(),
                "image".to_owned(),
                "pdf".to_owned(),
            ]),
            tool_call: Some(true),
        }
    }

    const SOURCE: &str = r#"{
      "anthropic": { "models": {
        "claude-sonnet-5": {
          "cost": { "input": 2.0, "output": 10.0, "cache_read": 0.2 },
          "limit": { "context": 1000000, "output": 128000 },
          "modalities": { "input": ["text", "image", "pdf"] },
          "tool_call": true
        },
        "claude-haiku-4-5": {
          "cost": { "input": 1.0, "output": 5.0, "cache_read": 0.1 },
          "limit": { "context": 200000, "output": 64000 }
        },
        "claude-unpriced": { "limit": { "context": 1000 } }
      } } }"#;

    #[test]
    fn a_matching_basis_reconciles() {
        let catalog = catalog_with(
            "claude-sonnet-5",
            rates(2.0, 0.2, 10.0),
            rates(2.0, 0.2, 10.0),
        );
        let found = reconcile(&catalog, SOURCE);
        assert_eq!(found[0].verdict, Verdict::Match);
        assert_eq!(found[0].upstream_metadata, sonnet_metadata());
    }

    #[test]
    fn matching_metadata_is_not_drift() {
        // The happy path for the second axis: a file that declares exactly
        // what the source publishes reports nothing at all, in any order —
        // the two catalogs have no reason to agree on modality ordering, and
        // a reordered list is not a change in what the model accepts.
        let mut declared = sonnet_metadata();
        declared.input_modalities = Some(vec![
            "pdf".to_owned(),
            "text".to_owned(),
            "image".to_owned(),
        ]);
        let catalog = catalog_declaring(
            "claude-sonnet-5",
            rates(2.0, 0.2, 10.0),
            rates(2.0, 0.2, 10.0),
            declared,
        );
        let found = reconcile(&catalog, SOURCE);
        assert_eq!(found[0].metadata_drift, vec![]);
        assert!(!found[0].has_actionable_metadata_drift());
    }

    #[test]
    fn a_context_window_that_moved_upstream_is_reported_like_a_price() {
        // The whole point of extending this command. A window the file
        // overstates becomes requests the upstream rejects; one it understates
        // becomes long-context work quietly truncated. Neither appears in the
        // ledger, so the reconciliation is the only place it can surface — and
        // it is a false claim, so it fails the command exactly as a drifted
        // basis does.
        let mut stale = sonnet_metadata();
        stale.context_window = Some(200_000);
        let catalog = catalog_declaring(
            "claude-sonnet-5",
            rates(2.0, 0.2, 10.0),
            rates(2.0, 0.2, 10.0),
            stale,
        );
        let found = reconcile(&catalog, SOURCE);

        // The basis still agrees: this is drift the price check cannot see.
        assert_eq!(found[0].verdict, Verdict::Match);
        assert_eq!(
            found[0].metadata_drift,
            vec![MetadataDrift {
                field: "context_window",
                kind: MetadataDriftKind::Disagrees,
                recorded: "200000".to_owned(),
                upstream: "1000000".to_owned(),
            }]
        );
        assert!(found[0].has_actionable_metadata_drift());
    }

    #[test]
    fn a_field_the_file_never_declared_is_reported_but_does_not_fail() {
        // The severity split. A silent file is legal — absence means unknown,
        // which is true — but it still costs something downstream, because
        // every client then substitutes its own default. So it prints and does
        // NOT fail, exactly like `Unpriced`: an alarm that fires on honest
        // silence is an alarm operators learn to skip.
        let catalog = catalog_with(
            "claude-sonnet-5",
            rates(2.0, 0.2, 10.0),
            rates(2.0, 0.2, 10.0),
        );
        let found = reconcile(&catalog, SOURCE);

        assert_eq!(
            found[0].metadata_drift.len(),
            4,
            "every field is undeclared"
        );
        assert!(
            found[0]
                .metadata_drift
                .iter()
                .all(|drift| drift.kind == MetadataDriftKind::Undeclared)
        );
        assert!(
            !found[0].has_actionable_metadata_drift(),
            "an honest omission must not fail the command"
        );
    }

    #[test]
    fn a_source_that_says_nothing_about_a_field_is_not_drift() {
        // The other half of the asymmetry, and the one that matters most for
        // trust in the alarm. `claude-haiku-4-5` carries limits in SOURCE but
        // no modalities and no tool_call, so those two fields cannot be
        // checked — and silence upstream is not evidence against the file.
        // Manufacturing a verdict from it would be guessing.
        let catalog = catalog_declaring(
            "claude-haiku-4-5-20251001",
            rates(1.0, 0.1, 5.0),
            rates(1.0, 0.1, 5.0),
            ModelMetadata {
                context_window: Some(200_000),
                max_output_tokens: Some(64_000),
                input_modalities: Some(vec!["text".to_owned(), "image".to_owned()]),
                tool_call: Some(true),
            },
        );
        let found = reconcile(&catalog, SOURCE);

        assert_eq!(found[0].matched_as.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(
            found[0].metadata_drift,
            vec![],
            "the source publishing nothing about a field is not disagreement"
        );
    }

    #[test]
    fn a_model_absent_from_the_source_reports_no_metadata_drift() {
        // A retired or renamed model, which is the case `Verdict::Missing`
        // exists for — it already says so, loudly and actionably. Adding four
        // Undeclared rows on top would repeat one fact four times in the
        // section an operator reads for *fixable* differences, and none of
        // them would be fixable: there is no upstream value to copy.
        let catalog = catalog_declaring(
            "claude-retired",
            rates(1.0, 0.1, 5.0),
            rates(1.0, 0.1, 5.0),
            sonnet_metadata(),
        );
        let found = reconcile(&catalog, SOURCE);

        assert_eq!(found[0].verdict, Verdict::Missing);
        assert!(found[0].verdict.is_actionable());
        assert_eq!(found[0].metadata_drift, vec![]);
    }

    #[test]
    fn a_stale_basis_is_reported_as_drift() {
        // The real case this was written for: sonnet's basis was raised to the
        // post-expiry rate ahead of the calendar, so the file believes the
        // model costs 3.00/15.00 while the upstream still charges 2.00/10.00.
        let catalog = catalog_with(
            "claude-sonnet-5",
            rates(3.0, 0.3, 15.0),
            rates(3.0, 0.3, 15.0),
        );
        let found = reconcile(&catalog, SOURCE);
        assert_eq!(found[0].verdict, Verdict::BasisDrift);
        assert!(found[0].verdict.is_actionable());
    }

    #[test]
    fn a_pass_through_tier_selling_above_upstream_is_an_undisclosed_markup() {
        // basis == sell, so the load-time validator is satisfied and the
        // structural test passes — the file is perfectly self-consistent. It
        // is just not true. Only an upstream number can catch this.
        let catalog = catalog_with(
            "claude-sonnet-5",
            rates(3.0, 0.3, 15.0),
            rates(3.0, 0.3, 15.0),
        );
        let found = reconcile(&catalog, SOURCE);
        assert_eq!(found[0].sell_markup(), Some(1.5));
        assert!(found[0].is_undisclosed_markup());
    }

    #[test]
    fn a_dated_snapshot_matches_its_undated_family_and_says_so() {
        let catalog = catalog_with(
            "claude-haiku-4-5-20251001",
            rates(1.0, 0.1, 5.0),
            rates(1.0, 0.1, 5.0),
        );
        let found = reconcile(&catalog, SOURCE);
        assert_eq!(found[0].verdict, Verdict::Match);
        assert_eq!(found[0].matched_as.as_deref(), Some("claude-haiku-4-5"));
    }

    #[test]
    fn an_unknown_model_is_actionable_but_an_unpriced_one_is_not() {
        let gone = catalog_with("claude-retired", rates(1.0, 0.1, 5.0), rates(1.0, 0.1, 5.0));
        assert_eq!(reconcile(&gone, SOURCE)[0].verdict, Verdict::Missing);
        assert!(reconcile(&gone, SOURCE)[0].verdict.is_actionable());

        // The source having no price is not evidence our price is wrong, and
        // failing on it would teach operators to ignore this command.
        let quiet = catalog_with(
            "claude-unpriced",
            rates(1.0, 0.1, 5.0),
            rates(1.0, 0.1, 5.0),
        );
        assert_eq!(reconcile(&quiet, SOURCE)[0].verdict, Verdict::Unpriced);
        assert!(!reconcile(&quiet, SOURCE)[0].verdict.is_actionable());
    }

    #[test]
    fn an_unreachable_or_malformed_source_reports_missing_never_agreement() {
        // A failed fetch must never read as "everything matches". That is the
        // one way an alarm can be worse than no alarm at all.
        let catalog = catalog_with(
            "claude-sonnet-5",
            rates(2.0, 0.2, 10.0),
            rates(2.0, 0.2, 10.0),
        );
        for junk in ["", "null", "{}", "not json at all"] {
            let found = reconcile(&catalog, junk);
            assert_eq!(
                found[0].verdict,
                Verdict::Missing,
                "malformed source {junk:?} must not read as agreement"
            );
        }
    }
}
