//! A SECOND pricing source, so one catalog can no longer be silently wrong.
//!
//! [`crate::drift`] reconciles `tiers.toml` against models.dev and nothing
//! else. Two failures came out of that single-source design on one day, and
//! both are the reason this module exists:
//!
//! 1. **A field was misread.** models.dev publishes each context band TWICE —
//!    as the structured `cost.tiers[].tier.size`, and again as a flat
//!    convenience key literally named `context_over_200k` whose NAME is wrong
//!    for any model whose boundary is elsewhere (`openai/gpt-5.6-luna`
//!    reprices at 272,000). Reading the misnamed key produced a confident,
//!    false report that models.dev was unreliable and unfit to reconcile
//!    against. One request to a second catalog would have settled it in
//!    seconds: OpenRouter publishes 272,000 too, and the two sources agree
//!    exactly.
//! 2. **A manual spot-check under-counted.** Someone eyeballed the catalog and
//!    reported one tiered model; the systematic reconcile later found three.
//!
//! Both are failures of corroboration, not of arithmetic, and neither is fixed
//! by looking harder at one document.
//!
//! # OpenRouter is a RESELLER, and that governs the whole design
//!
//! The second source is a marketplace that buys inference and resells it, and
//! it runs promotions. Its price is ITS price, not the vendor's list price.
//! Today `openai/gpt-5.6-sol` is 5.00/mtok on models.dev and 2.50 on
//! OpenRouter; ZeroRouter's catalog follows models.dev deliberately, because
//! models.dev tracks what the vendor charges the account ZeroRouter actually
//! bills against. A base-rate disagreement is therefore **not an error**, and
//! must never fail the command — an alarm that fires every time a reseller
//! discounts something is an alarm that gets muted, and then the real signal
//! goes with it.
//!
//! So the two halves of a price get very different treatment:
//!
//! - **STRUCTURE — where the boundaries are, and whether there are any — is
//!   high-confidence corroboration.** A threshold is a fact about the vendor's
//!   billing rule, not about a reseller's margin: OpenRouter has no commercial
//!   reason to move a boundary, and both sources today agree exactly on
//!   272,000 (OpenAI) and 200,000 (Google). Two independent catalogs
//!   disagreeing about WHERE a model reprices, or about whether it reprices at
//!   all, means one of them is wrong about the vendor's rule — which is
//!   precisely the failure that started this. Reported prominently.
//! - **RATES are informational.** The difference and the ratio are printed
//!   because "2.50 vs 5.00, 0.50x" is a fact worth an operator's eye, and
//!   because a 10x gap probably is a data error rather than a promotion. But
//!   nothing here decides anything.
//!
//! # Nothing in this module is actionable, by construction
//!
//! There is no `is_actionable` on any type here and no path from a
//! [`Report`] to an exit code. `admin catalog-drift` exits on
//! [`crate::drift::Verdict`] and metadata drift exactly as it did before this
//! module existed, and a test pins that. A flaky third party must never redden
//! CI or block an operator, so an unreachable, slow, or garbled second source
//! prints one line and the command carries on normally.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::drift::{CandidateDrift, Verdict};
use crate::provider::ModelRates;

/// The second catalog. Public, no authentication.
pub const DEFAULT_CORROBORATION_URL: &str = "https://openrouter.ai/api/v1/models";

/// Float-comparison slack, the same figure and the same meaning as
/// [`crate::drift`]'s: both sides of every comparison below arrive as the f64
/// nearest to a short decimal literal, so this is machine noise, not a
/// tolerance for a real difference. See [`per_mtok`] for why the two sides are
/// bit-identical whenever the underlying decimals are equal.
const EPSILON: f64 = 1e-9;

/// How long to wait for the second source before giving up.
///
/// Deliberately shorter than the primary fetch's 30s. The primary decides the
/// exit code and is worth waiting for; this one is advisory, and an operator
/// staring at a stalled terminal for half a minute to learn something optional
/// is a worse outcome than not learning it.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

// ---------------------------------------------------------------------------
// The wire shape
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SecondSourceDocument {
    #[serde(default)]
    data: Vec<SecondSourceModel>,
}

#[derive(Debug, Deserialize)]
struct SecondSourceModel {
    id: String,
    #[serde(default)]
    pricing: Option<SecondSourcePricing>,
}

/// Prices as the second source publishes them: per-TOKEN decimal STRINGS, not
/// the per-million-token numbers everything else in this crate speaks in. See
/// [`per_mtok`] for the conversion and why it is done the way it is.
#[derive(Debug, Deserialize)]
struct SecondSourcePricing {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
    #[serde(default)]
    input_cache_read: Option<String>,
    /// Rate tables that REPLACE the flat ones above some threshold — the
    /// second source's spelling of `cost.tiers[]`, keyed on the same
    /// `min_prompt_tokens` field name ZeroRouter's own catalog uses.
    #[serde(default)]
    overrides: Vec<SecondSourceOverride>,
}

#[derive(Debug, Deserialize)]
struct SecondSourceOverride {
    /// Absent on an override that is not a prompt-size band at all — see
    /// [`SecondSourceOverride::prompt_size_threshold`].
    #[serde(default)]
    min_prompt_tokens: Option<u64>,
    #[serde(default)]
    utc_start: Option<i64>,
    #[serde(default)]
    utc_end: Option<i64>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
    #[serde(default)]
    input_cache_read: Option<String>,
}

impl SecondSourceOverride {
    /// The prompt-token boundary this override reprices at, or `None` when it
    /// is not measured on the prompt.
    ///
    /// The second source's `overrides[]` is not exclusively a context ladder.
    /// It also carries TIME-OF-DAY promotions — `deepseek/deepseek-v4-pro`
    /// publishes four overrides keyed on `utc_start`/`utc_end` and no
    /// `min_prompt_tokens` at all, halving its rate outside business hours.
    /// That is the second source's exact analogue of models.dev's `tier.type`,
    /// and it deserves the same refusal for the same reason
    /// ([`crate::drift::Verdict::UnsupportedTierKind`]): an override measured
    /// on the clock is a different rule wearing the same shape, and counting
    /// it as a band would manufacture a structure disagreement out of nothing
    /// — or, worse, silence a real one by padding both lists.
    ///
    /// A band carrying BOTH a threshold and a clock window is refused too. No
    /// such row exists in the live document today, and if one appears it is a
    /// conditional-on-two-dimensions rule that this comparison genuinely
    /// cannot model, so guessing which half to honour would be inventing an
    /// answer.
    fn prompt_size_threshold(&self) -> Option<u64> {
        if self.utc_start.is_some() || self.utc_end.is_some() {
            return None;
        }
        self.min_prompt_tokens
    }

    fn rates(&self) -> ModelRates {
        ModelRates {
            input_per_mtok: self.prompt.as_deref().and_then(per_mtok),
            output_per_mtok: self.completion.as_deref().and_then(per_mtok),
            cached_input_per_mtok: self.input_cache_read.as_deref().and_then(per_mtok),
        }
    }
}

impl SecondSourcePricing {
    fn base(&self) -> ModelRates {
        ModelRates {
            input_per_mtok: self.prompt.as_deref().and_then(per_mtok),
            output_per_mtok: self.completion.as_deref().and_then(per_mtok),
            cached_input_per_mtok: self.input_cache_read.as_deref().and_then(per_mtok),
        }
    }

    /// Every prompt-size band, ascending. Sorted rather than taken in
    /// publication order: the question is WHERE the two catalogs put the
    /// boundaries, and two third-party documents have no reason to agree on
    /// the order they list them in. A reordered list is not a disagreement,
    /// the same call [`crate::drift`] already makes about modality ordering.
    fn bands(&self) -> Vec<(u64, ModelRates)> {
        let mut bands: Vec<_> = self
            .overrides
            .iter()
            .filter_map(|o| Some((o.prompt_size_threshold()?, o.rates())))
            .collect();
        bands.sort_by_key(|(threshold, _)| *threshold);
        bands
    }

    /// Overrides that are not prompt-size bands, counted so the report can say
    /// the second source prices on a dimension this comparison ignores rather
    /// than pretending it saw everything.
    fn unmodelled(&self) -> usize {
        self.overrides
            .iter()
            .filter(|o| o.prompt_size_threshold().is_none())
            .count()
    }
}

// ---------------------------------------------------------------------------
// Prices
// ---------------------------------------------------------------------------

/// Convert one of the second source's per-TOKEN decimal strings into the
/// per-million-token figure the rest of this crate speaks in.
///
/// # Why this shifts the decimal point instead of multiplying by 1e6
///
/// The obvious spelling is `s.parse::<f64>()? * 1e6`, and it introduces a
/// rounding step that the comparison then has to tolerate: the parse lands on
/// the double nearest `0.0000002`, and the multiply lands on the double
/// nearest THAT times a million, which need not be the double nearest `0.2` —
/// the one `input_per_mtok = 0.20` in `tiers.toml` produces. The two sides
/// would then differ in the last bits for values that are exactly equal as
/// decimals, and every comparison would rest on an epsilon being generous
/// enough to hide it.
///
/// Shifting the decimal point in the STRING removes the arithmetic entirely.
/// `"0.0000002"` becomes the text `"0000000.2"`, which parses to the double
/// nearest 0.2 — bit-identical to what the TOML literal `0.20` produces,
/// because it is the same decimal going through the same parser. Equal
/// decimals give equal doubles, so [`EPSILON`] is left doing only what
/// [`crate::drift`] already documents it as doing: absorbing the noise of
/// float representation, never masking a real difference. It also costs
/// nothing: this runs once per model in a CLI report.
///
/// A fixed-point integer form (nano-dollars per mtok, say) would be exact too,
/// but it would need a second representation for every rate in the crate and a
/// conversion at each boundary, to compare against an `f64` catalog that will
/// still be `f64` afterwards. The string shift buys the same exactness at the
/// one place the two representations actually meet.
///
/// # What is refused
///
/// `None` for anything that is not a plain non-negative decimal. In
/// particular a NEGATIVE price: the second source publishes `"-1"` for its own
/// meta-models (`openrouter/auto` and friends) to mean "varies, decided per
/// request", and `-1.0` per token is not a rate anyone charges. Treating a
/// sentinel as a number would print a spectacular fictional delta. `None`
/// means "this source said nothing usable", which is exactly how the
/// comparison below treats silence — as no comparison rather than as a
/// disagreement.
///
/// Exponent notation is refused too. The live document has never published it
/// (checked across all 415 models), and inventing a parse for a shape that has
/// never appeared is how a future format change becomes a wrong number instead
/// of a visible gap.
#[must_use]
fn per_mtok(price: &str) -> Option<f64> {
    const SHIFT: usize = 6;

    let price = price.trim();
    let price = price.strip_prefix('+').unwrap_or(price);
    if price.is_empty() || price.starts_with('-') {
        return None;
    }
    let (whole, fraction) = price.split_once('.').unwrap_or((price, ""));
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    // Move the point SHIFT places right by moving digits across it, padding
    // with zeros when the fraction runs out.
    let moved = fraction.len().min(SHIFT);
    let mut shifted = String::with_capacity(price.len() + SHIFT + 1);
    shifted.push_str(whole);
    shifted.push_str(&fraction[..moved]);
    for _ in moved..SHIFT {
        shifted.push('0');
    }
    if moved < fraction.len() {
        shifted.push('.');
        shifted.push_str(&fraction[moved..]);
    }
    shifted.parse().ok()
}

// ---------------------------------------------------------------------------
// What was found
// ---------------------------------------------------------------------------

/// The whole cross-check. Carries no verdict and no exit code: see the module
/// docs for why that is the point rather than an omission.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub entries: Vec<Corroboration>,
    /// Candidates deliberately not looked up — $0 rungs on the operator's own
    /// hardware ([`Verdict::Unreconcilable`]). No public catalog covers them
    /// and none ever will, so a permanent "not listed by the second source"
    /// row would be the same forever-red noise the primary report already
    /// refuses to print.
    pub exempt: usize,
}

/// What the second source had to say about one candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct Corroboration {
    pub candidate_id: String,
    pub finding: Finding,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Finding {
    /// The second source does not list this id.
    ///
    /// Two very different things wear this shape and the document cannot tell
    /// them apart: a genuine gap in the second source's coverage, and an id
    /// that does not map into its namespace. The catalog is keyed on
    /// OpenRouter-format `{vendor}/{model}` ids precisely so the mapping is
    /// the identity function — but that is a claim about the file, and this is
    /// the check that keeps it honest. Today exactly one pin misses:
    /// `anthropic/claude-haiku-4-5`, which the second source spells
    /// `anthropic/claude-haiku-4.5`.
    NotListed,
    /// Listed, but publishing no price this comparison can read.
    Unpriced,
    /// The PRIMARY source priced nothing here, so there is nothing to
    /// corroborate — but the second source did, and a gap in the primary is
    /// exactly what a second opinion is for. Carried, never actionable: the
    /// primary decides, and one catalog's silence is not the other's licence.
    PrimarySilent {
        thresholds: Vec<u64>,
        rates: ModelRates,
    },
    /// Both sources describe the model.
    Checked(Checked),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Checked {
    /// Where each source says the model reprices. The corroboration that
    /// counts.
    pub structure: Structure,
    /// Where the two sources' numbers differ. Informational only.
    pub deltas: Vec<RateDelta>,
}

/// Where each catalog places the repricing boundaries, ascending. An empty
/// list means "one flat rate at every size", which is itself a claim the two
/// can disagree about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Structure {
    pub primary: Vec<u64>,
    pub second: Vec<u64>,
    /// What `tiers.toml` itself declares. Not part of the verdict — the
    /// catalog is already reconciled against the primary source, actionably,
    /// by [`crate::drift`] — but printed when the two sources disagree, so an
    /// operator can see which side the file is currently following without
    /// opening it.
    pub recorded: Vec<u64>,
    /// Second-source overrides that are not prompt-size bands at all
    /// ([`SecondSourceOverride::prompt_size_threshold`]). Excluded from
    /// `second` and counted here, so "we ignored something" is stated rather
    /// than implied.
    pub unmodelled: usize,
}

impl Structure {
    /// Whether the two catalogs place the boundaries in the same places.
    ///
    /// Both directions count and both are worth hearing about: one source
    /// carrying a band the other lacks is as strong a signal as two sources
    /// naming different numbers, because either way one of them is wrong about
    /// the vendor's billing rule.
    #[must_use]
    pub fn agrees(&self) -> bool {
        self.primary == self.second
    }
}

/// One dimension where the two sources quote different money.
///
/// **Informational, always.** OpenRouter is a reseller running promotions, so
/// this is frequently a real discount rather than anyone's mistake. It is
/// printed with its ratio because the ratio is what separates "a reseller is
/// 20% under list" from "somebody lost a decimal point", and that judgment
/// belongs to a human.
#[derive(Debug, Clone, PartialEq)]
pub struct RateDelta {
    /// `None` for the base table, `Some(threshold)` for a band both sources
    /// declare.
    pub band: Option<u64>,
    pub dimension: &'static str,
    pub primary: f64,
    pub second: f64,
}

impl RateDelta {
    /// The second source's price as a multiple of the primary's. `None` when
    /// the primary quotes zero, which no ratio can describe.
    #[must_use]
    pub fn ratio(&self) -> Option<f64> {
        (self.primary.abs() > EPSILON).then(|| self.second / self.primary)
    }
}

impl Report {
    /// Candidates whose two sources place the boundaries differently. The
    /// prominent part of the report.
    #[must_use]
    pub fn structure_disagreements(&self) -> Vec<&Corroboration> {
        self.entries
            .iter()
            .filter(|entry| match &entry.finding {
                Finding::Checked(checked) => !checked.structure.agrees(),
                _ => false,
            })
            .collect()
    }

    #[must_use]
    pub fn not_listed(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|entry| entry.finding == Finding::NotListed)
            .map(|entry| entry.candidate_id.as_str())
            .collect()
    }

    /// Every rate difference, candidate by candidate.
    #[must_use]
    pub fn rate_deltas(&self) -> Vec<(&str, &RateDelta)> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.finding {
                Finding::Checked(checked) => Some((entry.candidate_id.as_str(), checked)),
                _ => None,
            })
            .flat_map(|(id, checked)| checked.deltas.iter().map(move |delta| (id, delta)))
            .collect()
    }

    /// Candidates whose two sources were actually compared.
    #[must_use]
    pub fn checked(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.finding, Finding::Checked(_)))
            .count()
    }

    /// Candidates the second source lists but cannot price, plus those the
    /// primary could not price. Both are gaps rather than disagreements.
    #[must_use]
    pub fn unpriced(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.finding,
                    Finding::Unpriced | Finding::PrimarySilent { .. }
                )
            })
            .map(|entry| entry.candidate_id.as_str())
            .collect()
    }

    /// Whether everything the second source said agrees, so the report can
    /// collapse to one line. A report nobody reads is worth nothing, and a
    /// screen of `ok` rows is how a report stops being read.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.entries.iter().all(|entry| match &entry.finding {
            Finding::Checked(checked) => checked.structure.agrees() && checked.deltas.is_empty(),
            _ => false,
        })
    }
}

// ---------------------------------------------------------------------------
// The comparison
// ---------------------------------------------------------------------------

/// Cross-check an already-reconciled catalog against a second source document.
///
/// Split from the fetch so it is testable without a network, exactly like
/// [`crate::drift::reconcile`].
///
/// # Errors
///
/// When the document cannot be read as a catalog at all. That is deliberately
/// an error rather than an empty result: a document that parsed to nothing
/// would otherwise report every candidate as `NotListed`, which reads as ten
/// simultaneous delistings — a loud, specific, and completely false claim. One
/// line saying the second source could not be read is the honest output, and
/// the caller prints it and carries on.
pub fn corroborate(findings: &[CandidateDrift], document: &str) -> Result<Report> {
    let parsed: SecondSourceDocument = match serde_json::from_str(document) {
        Ok(parsed) => parsed,
        Err(error) => bail!("could not be read as JSON ({error})"),
    };
    if parsed.data.is_empty() {
        bail!("published no models");
    }

    // First wins on a duplicate id. The live document has none; if one appears
    // it is the source's own inconsistency, and picking deterministically
    // keeps this report reproducible rather than ordering-dependent.
    let mut catalog: BTreeMap<&str, &SecondSourceModel> = BTreeMap::new();
    for model in &parsed.data {
        catalog.entry(model.id.as_str()).or_insert(model);
    }

    let mut entries = Vec::new();
    let mut exempt = 0;
    for found in findings {
        // Both primary exemptions carry across, for the reason the `exempt`
        // field records: a candidate the PRIMARY source structurally cannot
        // cover is not a candidate a second opinion has anything to add about,
        // and a permanent "not listed" row is the forever-red noise this whole
        // design refuses to print. `NotCoveredBySource` joins `Unreconcilable`
        // here on that rule and on no weaker one — it is a declared, argued gap
        // in the primary, not a lane anybody chose to stop looking at.
        if matches!(
            found.verdict,
            Verdict::Unreconcilable | Verdict::NotCoveredBySource
        ) {
            exempt += 1;
            continue;
        }
        entries.push(Corroboration {
            candidate_id: found.candidate_id.clone(),
            finding: compare(found, catalog.get(found.candidate_id.as_str()).copied()),
        });
    }
    Ok(Report { entries, exempt })
}

fn compare(found: &CandidateDrift, second: Option<&SecondSourceModel>) -> Finding {
    let Some(second) = second else {
        return Finding::NotListed;
    };
    let Some(pricing) = second.pricing.as_ref() else {
        return Finding::Unpriced;
    };
    let second_base = pricing.base();
    let second_bands = pricing.bands();
    if second_base.is_empty() && second_bands.is_empty() {
        return Finding::Unpriced;
    }

    if found.upstream_cost.is_empty() && found.upstream_conditional.is_empty() {
        return Finding::PrimarySilent {
            thresholds: second_bands.iter().map(|(t, _)| *t).collect(),
            rates: second_base,
        };
    }

    // The primary's bands arrive in publication order; sort for the same
    // reason the second source's are sorted.
    let mut primary_bands = found.upstream_conditional.clone();
    primary_bands.sort_by_key(|(threshold, _)| *threshold);

    let structure = Structure {
        primary: primary_bands.iter().map(|(t, _)| *t).collect(),
        second: second_bands.iter().map(|(t, _)| *t).collect(),
        recorded: found.recorded_conditional.iter().map(|(t, _)| *t).collect(),
        unmodelled: pricing.unmodelled(),
    };

    let mut deltas = Vec::new();
    push_deltas(&mut deltas, None, found.upstream_cost, second_base);
    // Only bands BOTH sources declare. A band only one of them has is not a
    // rate difference — there is no second number — and the structure line
    // above has already said so, more precisely than a delta could.
    for (threshold, primary) in &primary_bands {
        if let Some((_, second)) = second_bands.iter().find(|(t, _)| t == threshold) {
            push_deltas(&mut deltas, Some(*threshold), *primary, *second);
        }
    }

    Finding::Checked(Checked { structure, deltas })
}

/// Every dimension where two rate tables quote different money.
///
/// Silence on either side is not a difference: a catalog that does not publish
/// a cached-input rate has not disagreed about one. That is the same asymmetry
/// [`crate::drift::rates_agree`] makes, and for the same reason — a verdict
/// manufactured out of missing data is a guess.
fn push_deltas(
    out: &mut Vec<RateDelta>,
    band: Option<u64>,
    primary: ModelRates,
    second: ModelRates,
) {
    let dimensions = [
        ("input", primary.input_per_mtok, second.input_per_mtok),
        (
            "cached_input",
            primary.cached_input_per_mtok,
            second.cached_input_per_mtok,
        ),
        ("output", primary.output_per_mtok, second.output_per_mtok),
    ];
    for (dimension, primary, second) in dimensions {
        let (Some(primary), Some(second)) = (primary, second) else {
            continue;
        };
        if (primary - second).abs() >= EPSILON {
            out.push(RateDelta {
                band,
                dimension,
                primary,
                second,
            });
        }
    }
}

/// Fetch the second source.
///
/// # Errors
///
/// On any transport failure, non-success status, or unreadable body. Every one
/// of them is advisory: the caller prints the reason and finishes normally.
pub async fn fetch(url: &str) -> Result<String> {
    let response = reqwest::Client::new()
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("could not be fetched from {url} ({error})"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("answered HTTP {status} from {url}");
    }
    response
        .text()
        .await
        .map_err(|error| anyhow::anyhow!("returned an unreadable body from {url} ({error})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelMetadata, TierCandidate, TierCatalog, TierDefinition};
    use crate::drift::reconcile;
    use crate::provider::{ConditionalRate, RateSchedule};

    fn rates(input: f64, cached: f64, output: f64) -> ModelRates {
        ModelRates {
            input_per_mtok: Some(input),
            cached_input_per_mtok: Some(cached),
            output_per_mtok: Some(output),
        }
    }

    /// A one-pin catalog whose candidate id is the OpenRouter-format id the
    /// second source is looked up by — the shape `tiers.toml` actually uses.
    fn catalog(id: &str, provider: &str, model: &str, schedule: RateSchedule) -> TierCatalog {
        let mut tiers = BTreeMap::new();
        tiers.insert(
            id.to_owned(),
            TierDefinition {
                rates: schedule.clone(),
                retention: None,
                candidates: vec![TierCandidate {
                    id: id.to_owned(),
                    provider: provider.to_owned(),
                    model: model.to_owned(),
                    surface: None,
                    rates: schedule,
                    metadata: ModelMetadata::default(),
                }],
            },
        );
        TierCatalog {
            schema_version: 1,
            tiers,
            retention: BTreeMap::new(),
            unavailable: BTreeMap::new(),
        }
    }

    fn luna_catalog() -> TierCatalog {
        catalog(
            "openai/gpt-5.6-luna",
            "openai",
            "gpt-5.6-luna",
            RateSchedule::new(
                rates(0.2, 0.02, 1.2),
                vec![ConditionalRate {
                    min_prompt_tokens: 272_000,
                    rates: rates(0.4, 0.04, 1.8),
                }],
            ),
        )
    }

    /// models.dev's shape, carrying the trap the primary module documents: the
    /// real boundary lives in `cost.tiers[].tier.size`, while a flat
    /// `context_over_200k` key states a different one in its NAME.
    const PRIMARY: &str = r#"{
      "openai": { "models": {
        "gpt-5.6-luna": {
          "cost": {
            "input": 0.2, "output": 1.2, "cache_read": 0.02,
            "tiers": [
              { "input": 0.4, "output": 1.8, "cache_read": 0.04,
                "tier": { "type": "context", "size": 272000 } }
            ],
            "context_over_200k": { "input": 0.4, "output": 1.8, "cache_read": 0.04 }
          },
          "limit": { "context": 1050000, "output": 128000 }
        }
      } } }"#;

    /// The second source's shape: per-TOKEN decimal strings, `overrides[]`
    /// keyed on `min_prompt_tokens`, and an id that IS the pin id.
    const SECOND: &str = r#"{"data": [
      { "id": "openai/gpt-5.6-luna", "context_length": 1050000,
        "pricing": { "prompt": "0.0000002", "completion": "0.0000012",
                     "input_cache_read": "0.00000002",
                     "overrides": [ { "min_prompt_tokens": 272000,
                                      "prompt": "0.0000004",
                                      "completion": "0.0000018",
                                      "input_cache_read": "0.00000004" } ] } }
    ]}"#;

    fn only(report: &Report) -> &Finding {
        assert_eq!(report.entries.len(), 1, "one candidate");
        &report.entries[0].finding
    }

    fn checked(report: &Report) -> &Checked {
        match only(report) {
            Finding::Checked(checked) => checked,
            other => panic!("expected a comparison, got {other:?}"),
        }
    }

    // -- prices ------------------------------------------------------------

    #[test]
    fn a_per_token_price_becomes_the_exact_same_double_the_tier_file_produces() {
        // The float-precision decision, pinned. `0.0000002` per token and
        // `0.20` per million tokens are the same price, so they must be the
        // same f64 — bit for bit, not merely within an epsilon. If this ever
        // becomes a parse-and-multiply, the assertions below start failing on
        // the last bits and the comparison quietly starts leaning on EPSILON
        // to hide arithmetic error rather than representation noise.
        for (per_token, per_mtok_literal) in [
            ("0.0000002", 0.20_f64),
            ("0.0000012", 1.20),
            ("0.00000002", 0.02),
            ("0.000002", 2.0),
            ("0.00001", 10.0),
            ("0.0000000375", 0.0375),
            ("0.0000025", 2.5),
            ("0.000015", 15.0),
            ("0.0000225", 22.5),
            ("0.00000003", 0.03),
            ("0.0000003", 0.30),
            ("0", 0.0),
        ] {
            let converted = per_mtok(per_token).expect("a plain decimal converts");
            assert_eq!(
                converted.to_bits(),
                per_mtok_literal.to_bits(),
                "{per_token}/token must be bit-identical to {per_mtok_literal}/mtok, got {converted}"
            );
        }
    }

    #[test]
    fn a_price_that_is_not_a_plain_non_negative_decimal_is_not_a_price() {
        // `-1` is the second source's sentinel for "varies, decided per
        // request" on its own meta-models. Reading it as a number would print
        // a fictional -1,000,000/mtok delta with a spectacular ratio beside
        // it. Silence is the honest answer, and the comparison already treats
        // silence as no comparison rather than as disagreement.
        for refused in [
            "-1",
            "-0.0000002",
            "",
            "  ",
            "abc",
            "1e-7",
            "0.2e1",
            ".",
            "1.2.3",
        ] {
            assert_eq!(
                per_mtok(refused),
                None,
                "{refused:?} must not read as a price"
            );
        }
    }

    #[test]
    fn a_price_with_more_precision_than_the_shift_keeps_all_of_it() {
        // The second source publishes cache-write rates with sixteen
        // significant digits. Nothing here reads that dimension today, but the
        // conversion must not truncate a price to make the shift work.
        assert_eq!(
            per_mtok("0.0000000208333333333333"),
            Some(0.0208333333333333)
        );
        assert_eq!(per_mtok("12.5"), Some(12_500_000.0));
    }

    // -- agreement ---------------------------------------------------------

    #[test]
    fn two_sources_that_agree_report_one_clean_line_and_nothing_else() {
        let found = reconcile(&luna_catalog(), PRIMARY);
        let report = corroborate(&found, SECOND).expect("a well-formed second source is readable");

        assert!(report.is_clean());
        assert_eq!(report.checked(), 1);
        assert_eq!(report.structure_disagreements().len(), 0);
        assert_eq!(report.rate_deltas().len(), 0);
        assert_eq!(report.not_listed(), Vec::<&str>::new());

        let checked = checked(&report);
        assert!(checked.structure.agrees());
        assert_eq!(checked.structure.primary, vec![272_000]);
        assert_eq!(checked.structure.second, vec![272_000]);
        assert_eq!(checked.structure.unmodelled, 0);
    }

    #[test]
    fn two_flat_sources_agree_about_being_flat() {
        // Absence of bands is a claim the two can corroborate, not an absence
        // of information. A source that quietly grew a boundary the other
        // lacks is the signal this whole section exists for.
        let flat_primary = r#"{"anthropic": {"models": {"claude-sonnet-5":
            {"cost": {"input": 2.0, "output": 10.0, "cache_read": 0.2}}}}}"#;
        let flat_second = r#"{"data": [{"id": "anthropic/claude-sonnet-5",
            "pricing": {"prompt": "0.000002", "completion": "0.00001",
                        "input_cache_read": "0.0000002"}}]}"#;
        let catalog = catalog(
            "anthropic/claude-sonnet-5",
            "anthropic",
            "claude-sonnet-5",
            RateSchedule::flat(rates(2.0, 0.2, 10.0)),
        );
        let report = corroborate(&reconcile(&catalog, flat_primary), flat_second)
            .expect("a well-formed second source is readable");

        assert!(report.is_clean());
        let checked = checked(&report);
        assert!(checked.structure.agrees());
        assert_eq!(checked.structure.primary, Vec::<u64>::new());
        assert_eq!(checked.structure.second, Vec::<u64>::new());
    }

    // -- structure disagreement (the high-confidence signal) ---------------

    #[test]
    fn sources_that_place_the_boundary_differently_are_reported_and_never_actionable() {
        // The exact failure that started this. If someone reads models.dev's
        // misnamed `context_over_200k` key again and the primary starts
        // reporting 200,000 for a model that reprices at 272,000, the second
        // source contradicts it in one line — and every RATE still matches, so
        // nothing but the structure comparison could have caught it.
        let misread_primary = PRIMARY.replace("\"size\": 272000", "\"size\": 200000");
        let found = reconcile(&luna_catalog(), &misread_primary);
        let report = corroborate(&found, SECOND).expect("a well-formed second source is readable");

        assert!(!report.is_clean());
        assert_eq!(report.structure_disagreements().len(), 1);
        let checked = checked(&report);
        assert_eq!(checked.structure.primary, vec![200_000]);
        assert_eq!(checked.structure.second, vec![272_000]);
        assert!(!checked.structure.agrees());
        // No band is shared, so there is no rate to compare in one — and the
        // structure line has already said the more precise thing.
        assert_eq!(checked.deltas, vec![]);
        // The catalog's own threshold rides along so an operator can see which
        // source the file is following. It is not part of the verdict.
        assert_eq!(checked.structure.recorded, vec![272_000]);
    }

    #[test]
    fn one_source_having_a_band_the_other_lacks_is_a_structure_disagreement() {
        let flat_primary = r#"{"openai": {"models": {"gpt-5.6-luna":
            {"cost": {"input": 0.2, "output": 1.2, "cache_read": 0.02}}}}}"#;
        let found = reconcile(&luna_catalog(), flat_primary);
        let report = corroborate(&found, SECOND).expect("a well-formed second source is readable");

        let checked = checked(&report);
        assert!(!checked.structure.agrees());
        assert_eq!(checked.structure.primary, Vec::<u64>::new());
        assert_eq!(checked.structure.second, vec![272_000]);
        assert_eq!(report.structure_disagreements().len(), 1);
    }

    #[test]
    fn bands_published_out_of_order_are_not_a_disagreement() {
        // Two third-party documents have no reason to agree on the order they
        // list bands in, and a reordered list is not a moved boundary. BOTH
        // sides are shuffled here, and differently: each source is sorted by
        // its own code path, so a fixture that disordered only one of them
        // would leave the other's sort untested.
        let laddered_primary = r#"{"openai": {"models": {"gpt-5.6-luna": {"cost": {
            "input": 0.2, "output": 1.2, "cache_read": 0.02,
            "tiers": [
              { "input": 0.6, "output": 2.4, "cache_read": 0.06,
                "tier": { "type": "context", "size": 512000 } },
              { "input": 0.4, "output": 1.8, "cache_read": 0.04,
                "tier": { "type": "context", "size": 272000 } }
            ]}}}}}"#;
        let laddered_second = r#"{"data": [{"id": "openai/gpt-5.6-luna", "pricing": {
            "prompt": "0.0000002", "completion": "0.0000012", "input_cache_read": "0.00000002",
            "overrides": [
              { "min_prompt_tokens": 512000, "prompt": "0.0000006",
                "completion": "0.0000024", "input_cache_read": "0.00000006" },
              { "min_prompt_tokens": 272000, "prompt": "0.0000004",
                "completion": "0.0000018", "input_cache_read": "0.00000004" } ] } }]}"#;
        let report = corroborate(
            &reconcile(&luna_catalog(), laddered_primary),
            laddered_second,
        )
        .expect("a well-formed second source is readable");

        let checked = checked(&report);
        assert_eq!(checked.structure.primary, vec![272_000, 512_000]);
        assert_eq!(checked.structure.second, vec![272_000, 512_000]);
        assert!(checked.structure.agrees());
        assert_eq!(checked.deltas, vec![], "and every band's rates line up");
    }

    #[test]
    fn a_time_of_day_promotion_is_not_a_context_band() {
        // The second source's `overrides[]` is not exclusively a context
        // ladder: `deepseek/deepseek-v4-pro` publishes four overrides keyed on
        // `utc_start`/`utc_end` with no `min_prompt_tokens` at all. Counting
        // one as a band would invent a structure disagreement out of a
        // discount schedule — the second source's version of the `tier.type`
        // confusion the primary module refuses.
        let clock_second = r#"{"data": [{"id": "openai/gpt-5.6-luna", "pricing": {
            "prompt": "0.0000002", "completion": "0.0000012", "input_cache_read": "0.00000002",
            "overrides": [
              { "utc_start": 100, "utc_end": 400, "prompt": "0.0000001",
                "completion": "0.0000006", "input_cache_read": "0.00000001" },
              { "min_prompt_tokens": 272000, "prompt": "0.0000004",
                "completion": "0.0000018", "input_cache_read": "0.00000004" } ] } }]}"#;
        let report = corroborate(&reconcile(&luna_catalog(), PRIMARY), clock_second)
            .expect("a well-formed second source is readable");

        let checked = checked(&report);
        assert_eq!(
            checked.structure.second,
            vec![272_000],
            "the clock override is not a prompt-token boundary"
        );
        assert!(checked.structure.agrees());
        assert_eq!(
            checked.structure.unmodelled, 1,
            "and the report says it ignored one, rather than implying it saw everything"
        );
        assert_eq!(checked.deltas, vec![]);
    }

    // -- rate disagreement (informational) ---------------------------------

    #[test]
    fn a_reseller_discount_is_reported_with_its_ratio_and_changes_nothing() {
        // THE governing constraint. `openai/gpt-5.6-sol` really is 5.00 on
        // models.dev and 2.50 on OpenRouter today, because OpenRouter is a
        // reseller running a promotion. The catalog follows models.dev
        // deliberately. So this must be visible and must be inert: the
        // structure still corroborates, and there is nothing here to act on.
        let sol_primary = r#"{"openai": {"models": {"gpt-5.6-sol": {"cost": {
            "input": 5.0, "output": 30.0, "cache_read": 0.5,
            "tiers": [ { "input": 10.0, "output": 45.0, "cache_read": 1.0,
                         "tier": { "type": "context", "size": 272000 } } ] } }}}}"#;
        let sol_second = r#"{"data": [{"id": "openai/gpt-5.6-sol", "pricing": {
            "prompt": "0.0000025", "completion": "0.000015", "input_cache_read": "0.00000025",
            "overrides": [ { "min_prompt_tokens": 272000, "prompt": "0.000005",
                             "completion": "0.0000225", "input_cache_read": "0.0000005" } ] } }]}"#;
        let sol = catalog(
            "openai/gpt-5.6-sol",
            "openai",
            "gpt-5.6-sol",
            RateSchedule::new(
                rates(5.0, 0.5, 30.0),
                vec![ConditionalRate {
                    min_prompt_tokens: 272_000,
                    rates: rates(10.0, 1.0, 45.0),
                }],
            ),
        );
        let found = reconcile(&sol, sol_primary);
        assert_eq!(
            found[0].verdict,
            Verdict::Match,
            "the catalog follows models.dev, exactly"
        );

        let report =
            corroborate(&found, sol_second).expect("a well-formed second source is readable");
        let checked = checked(&report);
        assert!(
            checked.structure.agrees(),
            "a reseller discounts a price; it does not move the vendor's boundary"
        );
        assert_eq!(
            checked.deltas.len(),
            6,
            "three dimensions in the base table and three in the band"
        );
        for delta in &checked.deltas {
            let ratio = delta.ratio().expect("a non-zero primary rate has a ratio");
            assert!(
                (ratio - 0.5).abs() < 1e-9,
                "every dimension is half, exactly: {delta:?} -> {ratio}"
            );
        }
        assert!(!report.is_clean(), "it prints");
        assert_eq!(
            report.structure_disagreements().len(),
            0,
            "and it is not the part that would ever be prominent"
        );
    }

    #[test]
    fn silence_on_a_dimension_is_not_a_rate_difference() {
        // The same asymmetry the primary reconciliation makes. A catalog that
        // does not publish a cached-input rate has not disagreed about one,
        // and manufacturing a delta from missing data would be a guess.
        let quiet_second = r#"{"data": [{"id": "openai/gpt-5.6-luna", "pricing": {
            "prompt": "0.0000002", "completion": "0.0000012",
            "overrides": [ { "min_prompt_tokens": 272000, "prompt": "0.0000004",
                             "completion": "0.0000018" } ] } }]}"#;
        let report = corroborate(&reconcile(&luna_catalog(), PRIMARY), quiet_second)
            .expect("a well-formed second source is readable");
        assert_eq!(checked(&report).deltas, vec![]);
        assert!(report.is_clean());
    }

    #[test]
    fn a_negative_sentinel_price_is_silence_rather_than_a_gigantic_delta() {
        let sentinel_second = r#"{"data": [{"id": "openai/gpt-5.6-luna", "pricing": {
            "prompt": "-1", "completion": "-1",
            "overrides": [ { "min_prompt_tokens": 272000, "prompt": "0.0000004",
                             "completion": "0.0000018", "input_cache_read": "0.00000004" } ] } }]}"#;
        let report = corroborate(&reconcile(&luna_catalog(), PRIMARY), sentinel_second)
            .expect("a well-formed second source is readable");
        let checked = checked(&report);
        assert_eq!(
            checked.deltas,
            vec![],
            "a sentinel is not a price, so there is nothing to differ about"
        );
        assert!(checked.structure.agrees());
    }

    // -- gaps --------------------------------------------------------------

    #[test]
    fn a_model_the_second_source_does_not_list_is_named_and_never_actionable() {
        // Both the "OpenRouter has never heard of this" case and the "our id
        // does not map into its namespace" case land here, and the document
        // cannot tell them apart. It is reported so a human can — and today it
        // is real: the catalog pins `anthropic/claude-haiku-4-5`, which the
        // second source spells `anthropic/claude-haiku-4.5`.
        let haiku_primary = r#"{"anthropic": {"models": {"claude-haiku-4-5":
            {"cost": {"input": 1.0, "output": 5.0, "cache_read": 0.1}}}}}"#;
        let haiku = catalog(
            "anthropic/claude-haiku-4-5",
            "anthropic",
            "claude-haiku-4-5",
            RateSchedule::flat(rates(1.0, 0.1, 5.0)),
        );
        let found = reconcile(&haiku, haiku_primary);
        assert_eq!(
            found[0].verdict,
            Verdict::Match,
            "the primary reconciles it fine"
        );

        let report = corroborate(&found, SECOND).expect("a well-formed second source is readable");
        assert_eq!(report.not_listed(), vec!["anthropic/claude-haiku-4-5"]);
        assert!(!report.is_clean());
        assert_eq!(report.checked(), 0);
    }

    #[test]
    fn a_model_the_second_source_lists_without_prices_is_a_gap_not_a_disagreement() {
        let unpriced = r#"{"data": [{"id": "openai/gpt-5.6-luna", "context_length": 1050000}]}"#;
        let report = corroborate(&reconcile(&luna_catalog(), PRIMARY), unpriced)
            .expect("a well-formed second source is readable");
        assert_eq!(only(&report), &Finding::Unpriced);
        assert_eq!(report.unpriced(), vec!["openai/gpt-5.6-luna"]);
    }

    #[test]
    fn the_second_source_speaks_where_the_primary_is_silent() {
        // A gap in the primary is exactly what a second opinion is for. It is
        // carried and it is inert: the primary already reports `NOT IN SOURCE`
        // actionably, and one catalog's silence is not the other's licence to
        // become authoritative.
        let silent_primary = r#"{"openai": {"models": {}}}"#;
        let found = reconcile(&luna_catalog(), silent_primary);
        assert_eq!(found[0].verdict, Verdict::Missing);

        let report = corroborate(&found, SECOND).expect("a well-formed second source is readable");
        assert_eq!(
            only(&report),
            &Finding::PrimarySilent {
                thresholds: vec![272_000],
                rates: rates(0.2, 0.02, 1.2),
            }
        );
        assert_eq!(report.structure_disagreements().len(), 0);
        assert_eq!(report.rate_deltas().len(), 0);
    }

    #[test]
    fn a_local_zero_rung_is_never_looked_up_at_all() {
        // Same rule, same reason as the primary's `Unreconcilable` exemption:
        // no public catalog covers a model on the operator's own hardware and
        // none ever will, so a permanent "not listed" row on every run is the
        // fastest way to teach an operator to stop reading this section.
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "zero/edge".to_owned(),
            TierDefinition {
                rates: RateSchedule::flat(rates(2.0, 0.2, 10.0)),
                retention: None,
                candidates: vec![TierCandidate {
                    id: "local-llama/qwen3-8b".to_owned(),
                    provider: "local-llama".to_owned(),
                    model: "qwen3-8b".to_owned(),
                    surface: None,
                    rates: RateSchedule::flat(ModelRates {
                        input_per_mtok: Some(0.0),
                        cached_input_per_mtok: None,
                        output_per_mtok: Some(0.0),
                    }),
                    metadata: ModelMetadata::default(),
                }],
            },
        );
        let edge = TierCatalog {
            schema_version: 1,
            tiers,
            retention: BTreeMap::new(),
            unavailable: BTreeMap::new(),
        };
        let found = crate::drift::reconcile_with(&edge, PRIMARY, &|_| true, &|_| None, &|_| None);
        assert_eq!(found[0].verdict, Verdict::Unreconcilable);

        let report = corroborate(&found, SECOND).expect("a well-formed second source is readable");
        assert_eq!(report.entries, vec![], "not looked up, so not reported");
        assert_eq!(report.exempt, 1, "but counted, so the exemption is visible");
        assert_eq!(report.not_listed(), Vec::<&str>::new());
    }

    // -- a source that cannot be read at all -------------------------------

    #[test]
    fn an_unreadable_second_source_is_one_error_not_a_wall_of_delistings() {
        // A document that parsed to nothing would report every candidate as
        // NotListed — a loud, specific, completely false claim that ten models
        // were delisted at once. The honest output is one line saying the
        // second source could not be read, which the caller prints before
        // finishing normally.
        let found = reconcile(&luna_catalog(), PRIMARY);
        for junk in [
            "",
            "not json at all",
            "null",
            "{}",
            r#"{"data": []}"#,
            r#"{"data": "nope"}"#,
            "<html>503 Service Unavailable</html>",
        ] {
            let error = corroborate(&found, junk)
                .expect_err("an unreadable second source must not read as agreement");
            let rendered = format!("{error:#}");
            assert!(
                rendered.contains("JSON") || rendered.contains("published no models"),
                "the reason must be sayable in one line: {rendered}"
            );
        }
    }

    #[test]
    fn a_second_source_that_lost_one_model_still_reports_the_rest() {
        // Partial data is usable data. Losing corroboration on one model is
        // not a reason to lose it on the other nine.
        let mut tiers = luna_catalog().tiers;
        tiers.insert(
            "anthropic/claude-sonnet-5".to_owned(),
            TierDefinition {
                rates: RateSchedule::flat(rates(2.0, 0.2, 10.0)),
                retention: None,
                candidates: vec![TierCandidate {
                    id: "anthropic/claude-sonnet-5".to_owned(),
                    provider: "anthropic".to_owned(),
                    model: "claude-sonnet-5".to_owned(),
                    surface: None,
                    rates: RateSchedule::flat(rates(2.0, 0.2, 10.0)),
                    metadata: ModelMetadata::default(),
                }],
            },
        );
        let both = TierCatalog {
            schema_version: 1,
            tiers,
            retention: BTreeMap::new(),
            unavailable: BTreeMap::new(),
        };
        let primary = PRIMARY.replace(
            "\"openai\": {",
            r#""anthropic": {"models": {"claude-sonnet-5":
              {"cost": {"input": 2.0, "output": 10.0, "cache_read": 0.2}}}}, "openai": {"#,
        );
        let report = corroborate(&reconcile(&both, &primary), SECOND)
            .expect("a well-formed second source is readable");

        assert_eq!(report.checked(), 1, "luna still corroborates");
        assert_eq!(report.not_listed(), vec!["anthropic/claude-sonnet-5"]);
    }
}
