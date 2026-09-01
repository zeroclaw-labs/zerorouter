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
use crate::provider::{ModelRates, RateSchedule};

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
    /// What the source says a cache WRITE costs. Read because the source does
    /// publish it — for every Claude lane, and for the OpenAI family too — and
    /// a dimension ZeroRouter now bills on is a dimension CI should be able to
    /// check. See [`rates_agree`] for why silence on EITHER side ends the
    /// comparison rather than producing a finding.
    cache_write: Option<f64>,
    /// Conditional rates that REPLACE the flat ones above some measured
    /// threshold. Read from the structured array and from nowhere else.
    ///
    /// The source publishes each tier twice: here, with an explicit
    /// `tier.size`, and again as a flat convenience key named literally
    /// `context_over_200k`. That name is hardcoded and is WRONG for models
    /// whose boundary is elsewhere — `openai/gpt-5.6-luna` reprices at 272,000
    /// tokens, not 200,000, while carrying a `context_over_200k` key holding
    /// the correct rates under an incorrect name. Reading it produced a false
    /// report that this source disagreed with OpenRouter and was unfit to
    /// reconcile against; it agrees exactly, on both the rates and the
    /// threshold, once the structured field is the one consulted. Never read
    /// the convenience key.
    #[serde(default)]
    tiers: Vec<SourceCostTier>,
}

/// One conditional rate table: what the upstream charges once
/// [`SourceCostTierBound::size`] is exceeded.
#[derive(Debug, Deserialize)]
struct SourceCostTier {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    tier: SourceCostTierBound,
}

#[derive(Debug, Deserialize)]
struct SourceCostTierBound {
    /// What is being measured — `context` for every tier seen so far. Kept so
    /// an unrecognized kind can be reported rather than assumed.
    #[serde(rename = "type")]
    kind: String,
    size: u64,
}

/// The only bound kind ZeroRouter can price against, because
/// [`crate::provider::ConditionalRate`] conditions on prompt size and nothing
/// else. The source spells it `context`.
const PROMPT_SIZE_TIER_KIND: &str = "context";

impl SourceCostTier {
    /// Whether this tier's bound is measured on the prompt.
    ///
    /// The `type` field exists because the source does not promise every tier
    /// measures the same thing, so a comparison that reads only `size` is
    /// comparing two numbers that may not denote the same quantity. An
    /// output-length bound of 272,000 is not a prompt bound of 272,000, and
    /// treating it as one reconciles a catalog band against a rule it does not
    /// implement — a green row over a real mispricing.
    fn measures_prompt_size(&self) -> bool {
        self.tier.kind == PROMPT_SIZE_TIER_KIND
    }
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
    /// A $0 rung on an operator-declared upstream: a model running on the
    /// operator's own hardware, which no public catalog covers and none ever
    /// will (edge mode, stage 2: `docs/design/edge-mode-local-rung.md`).
    ///
    /// Reported, never actionable — the same treatment as [`Self::Unpriced`],
    /// for the same reason. This module exists to protect margin, and there is
    /// no margin to drift on a rung that costs nothing; meanwhile a
    /// `NOT IN SOURCE` row that can never be fixed, on every CI run, is the
    /// fastest way to teach an operator to stop reading this report. It still
    /// prints, so the exemption is visible in the table rather than silently
    /// skipping the candidate.
    ///
    /// The exemption needs BOTH halves — the provider's `"settlement": "free"`
    /// declaration and the candidate's $0 price, the same pair
    /// [`crate::config::TierCandidate::is_free`] requires — and is deliberately
    /// the narrowest thing that works. A free-declaring provider's *priced*
    /// candidate is a real cost basis and deserves a real answer, even if that
    /// answer is `NOT IN SOURCE`. And $0 alone is very much not enough: a
    /// fat-fingered zero on a metered provider is precisely the silent-margin
    /// failure this module was written to catch, and exempting it would disable
    /// the alarm at the moment it matters most. (Such a candidate cannot load
    /// at all since stage 2 — `validate_zero_price` refuses it — but this file
    /// does not depend on that being true.)
    Unreconcilable,
    /// The upstream has DECLARED that the reconciliation source cannot price
    /// it, and said why (`unreconcilable_reason`, `config/providers.json`).
    ///
    /// Distinct from [`Self::Unreconcilable`], which covers a rung that costs
    /// nothing and that no catalog will ever cover. This one is a rung that
    /// costs real money and that the source *appears* to cover — the dangerous
    /// case, and the reason the exemption has to be declared rather than
    /// inferred. Bedrock is the instance: models.dev carries
    /// `amazon-bedrock/anthropic.claude-sonnet-5` at 2.00/10.00, which is the
    /// GLOBAL cross-region rate, while ZeroRouter dials the in-region-only
    /// mantle endpoint that AWS meters at the unqualified `_standard` SKU,
    /// 2.20/11.00. A join on the provider key was implemented and then removed:
    /// it produced `ok` on a basis 10% below the invoice, which is worse than
    /// no check at all, because it is a check that says everything is fine.
    ///
    /// **Not actionable, and it prints its reason on every run.** Not
    /// actionable because there is nothing an operator could edit to make it
    /// go away — the source does not carry this SKU class at any key — and a
    /// permanently red row is how a report stops being read. Printing the
    /// declared reason every time is what stops it becoming a silent skip: the
    /// argument for the gap, and the authority the rates actually came from,
    /// are in front of whoever reads the report.
    ///
    /// The honest cost, recorded here rather than left to be discovered: these
    /// candidates have NO automated price check. If AWS moves the in-region
    /// rate, nothing in CI will notice. That is a real regression in coverage
    /// for this lane, accepted because the alternative on offer was a false
    /// green, and mitigated only by the re-verification command in the
    /// declaration. Wiring the AWS Price List API as a second drift source
    /// would close it properly and is the right next piece of work.
    NotCoveredBySource,
    /// The upstream reprices the whole request above a measured threshold and
    /// the CATALOG declares no conditional rates at all, so every request past
    /// the boundary is billed at a basis ZeroRouter does not actually pay.
    ///
    /// Its own verdict rather than [`Self::BasisDrift`] because no single
    /// number in the file is wrong: the recorded rate is the vendor's real
    /// base rate, and the fault is a missing table rather than a mistyped one.
    /// Once the catalog DOES declare conditional rates the two are comparable
    /// like anything else — matching thresholds and rates give
    /// [`Self::Match`], any disagreement about either gives
    /// [`Self::BasisDrift`] — so this variant means exactly "the file has
    /// nothing to say about a boundary the vendor charges at".
    ///
    /// **Actionable, since conditional rates landed.** It was not, and the
    /// reason it was not is written into its own history: `openai/gpt-5.6-luna`
    /// shipped tiered before the catalog could express the shape, so failing on
    /// it would have reddened every PR with a fault nobody could fix, and a
    /// check that is always red is a check nobody reads. That reason is gone.
    /// There is now a four-line table an operator can add, and until they add
    /// it the request path is quoting a basis the invoice will not match — the
    /// precise silent-margin failure this module exists to break. It fails the
    /// command for the same reason [`Self::BasisDrift`] does, and it fires the
    /// day an upstream introduces a boundary it did not have before, which is
    /// when someone needs to hear about it.
    TieredUpstream,
    /// The upstream reprices on a dimension ZeroRouter does not model.
    ///
    /// A [`crate::provider::ConditionalRate`] conditions on PROMPT SIZE and
    /// nothing else. The source's tier bound carries a `type` alongside its
    /// `size`, and every tier seen so far says `context` — but the field
    /// exists precisely because that is not guaranteed, and a bound measured
    /// on output length, or on anything else the source grows a name for, is a
    /// different rule wearing the same shape. Compared on `size` alone it
    /// would reconcile against a prompt-token band number-for-number while
    /// pricing something else entirely, and report `ok`.
    ///
    /// Its own verdict rather than [`Self::TieredUpstream`] because the advice
    /// differs: there is no band an operator could add that would make this
    /// right. And emphatically not [`Self::Unreconcilable`], which is the
    /// non-actionable local-$0-rung exemption — filing this there would turn
    /// "we cannot price what this vendor charges" into a silent pass, which is
    /// the exact failure mode this module exists to prevent. It fails the
    /// command; what it asks for is a human decision, not a config edit.
    UnsupportedTierKind,
}

impl Verdict {
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Match => "ok",
            Self::BasisDrift => "BASIS DRIFT",
            Self::Unpriced => "unpriced upstream",
            Self::Missing => "NOT IN SOURCE",
            Self::Unreconcilable => "local $0 rung",
            Self::NotCoveredBySource => "source cannot price",
            Self::TieredUpstream => "TIERED UPSTREAM",
            Self::UnsupportedTierKind => "UNMODELLED TIER",
        }
    }

    /// Whether this verdict should fail the command. `Unpriced` does not: the
    /// source simply has nothing to say, and failing on it would train
    /// operators to ignore the alarm. Neither does `Unreconcilable`, for the
    /// reason spelled out on that variant. `TieredUpstream` now does — see its
    /// variant for why the call changed. Neither does `NotCoveredBySource`:
    /// there is no edit that would resolve it, and its declared reason prints
    /// on every run so the gap is argued rather than hidden.
    #[must_use]
    pub const fn is_actionable(&self) -> bool {
        matches!(
            self,
            Self::BasisDrift | Self::Missing | Self::TieredUpstream | Self::UnsupportedTierKind
        )
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
    /// The file claims LESS than the source does, on a field where claiming
    /// less is a true statement.
    ///
    /// **models.dev describes the MODEL; `tiers.toml` describes the LANE**, and
    /// a lane may carry less than the model it dispatches to. The catalog
    /// advertised `pdf`, `video` and `audio` on rows whose wire had no mapping
    /// for any of them, so a client that believed the row built a request that
    /// 400'd; `#102` removed those claims from the lanes rather than teaching
    /// the wire to carry them, and recorded the reasoning in `tiers.toml`'s own
    /// header. That left thirteen rows where the file is deliberately, honestly
    /// narrower than the source — and reporting them as `Disagrees` made the
    /// command permanently red on the state the project had just decided was
    /// correct.
    ///
    /// **The relaxation is strictly one-directional and that is the whole
    /// safety argument.** A lane claiming a subset of what the model can take
    /// under-promises, and the request path enforces it: a modality the lane
    /// does not declare is refused at the capability gate before any reserve or
    /// dial. A lane claiming a SUPERSET is the false advertisement that broke
    /// clients, so extra modalities remain [`Self::Disagrees`] and remain
    /// actionable. Widening this to a set-difference in both directions would
    /// re-open exactly the defect `#102` closed.
    ///
    /// It PRINTS on every run rather than passing silently, for the reason
    /// [`Verdict::NotCoveredBySource`] prints its reason: this is a place the
    /// reconciliation is deliberately not asking a question, and an exemption
    /// nobody re-reads is indistinguishable from a lane somebody forgot. The
    /// row shows what the lane declines to carry, so the day a wire learns to
    /// carry it, the gap to close is already on screen.
    Narrowed,
}

impl MetadataDriftKind {
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Disagrees => "METADATA DRIFT",
            Self::Undeclared => "undeclared",
            Self::Narrowed => "lane narrower",
        }
    }

    /// Whether this should fail the command.
    ///
    /// Only `Disagrees` does. `Undeclared` deliberately does not, for the same
    /// reason [`Verdict::Unpriced`] does not: the file is *allowed* to be
    /// silent — absence means "unknown", which is a true statement — and
    /// failing CI on a legal, honest omission is how an alarm gets ignored.
    /// It still prints, because the omission has a real cost downstream.
    ///
    /// Neither does `Narrowed`, and for a stronger reason than either: it is
    /// not a defect at all. A lane that carries less than its model is the
    /// catalog telling the truth about the wire, which is the state `#102`
    /// worked to reach — see that variant.
    #[must_use]
    pub const fn is_actionable(&self) -> bool {
        matches!(self, Self::Disagrees)
    }
}

/// How the upstream reprices this model past a threshold, rendered for the
/// report, or `None` when it charges one flat rate at every size.
///
/// Quantifying it in the row is the point: "TIERED UPSTREAM" alone says a
/// margin is wrong somewhere, while `≥272000 tok: 0.4/0.04/1.8` says how much
/// and from where, which is what someone deciding whether to prioritize
/// conditional rates actually needs.
fn upstream_tier_note(entry: &SourceModel) -> Option<String> {
    let cost = entry.cost.as_ref()?;
    let mut notes: Vec<String> = Vec::new();
    for tier in &cost.tiers {
        let show = |rate: Option<f64>| rate.map_or_else(|| "-".to_owned(), |r| format!("{r}"));
        notes.push(format!(
            "≥{} {}: {}/{}/{}",
            tier.tier.size,
            tier.tier.kind,
            show(tier.input),
            show(tier.cache_read),
            show(tier.output)
        ));
    }
    (!notes.is_empty()).then(|| notes.join(", "))
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
    /// The BASE cost basis — what the file records for a request under every
    /// threshold. Paired with `recorded_conditional`, which carries the rest.
    pub recorded_basis: ModelRates,
    /// The conditional bands the file records, as `(min_prompt_tokens, rates)`
    /// in declaration order. Empty for a model priced flat, which is every
    /// model this file described before conditional rates existed.
    pub recorded_conditional: Vec<(u64, ModelRates)>,
    pub upstream_cost: ModelRates,
    /// The conditional bands the SOURCE publishes, as `(size, rates)` in the
    /// order it publishes them — the structured `cost.tiers[]` and never the
    /// misnamed convenience key ([`SourceCost::tiers`]).
    ///
    /// Only tiers measured on prompt size ([`SourceCostTier::measures_prompt_size`])
    /// appear here. A bound measured on something else is not a prompt-token
    /// threshold however well its number lines up, and putting it in this list
    /// would let anything reading the list compare two quantities that do not
    /// denote the same thing — the exact confusion [`Verdict::UnsupportedTierKind`]
    /// exists to refuse. That verdict fires on the same input, so nothing is
    /// lost by leaving it out here.
    ///
    /// Machine-readable beside the already-rendered [`Self::upstream_tier`],
    /// because a second catalog can only be cross-checked against numbers, not
    /// against a display string.
    pub upstream_conditional: Vec<(u64, ModelRates)>,
    /// The tier's BASE sell rate — what the customer pays below every
    /// threshold.
    pub sell: ModelRates,
    /// The bands the tier SELLS at, as `(min_prompt_tokens, rates)`.
    ///
    /// Reported beside `recorded_conditional` because the two answer different
    /// questions and only this one is about the customer. A tier can record a
    /// perfectly correct cost basis above the boundary and still charge the
    /// wrong price there — nothing in this file forces basis and sell to
    /// declare the same bands, deliberately, since a tier's rungs need not
    /// reprice where the tier does.
    pub sell_conditional: Vec<(u64, ModelRates)>,
    /// What `tiers.toml` claims this model can take and produce.
    pub recorded_metadata: ModelMetadata,
    /// What the source catalog publishes about the same model. Named to
    /// mirror `recorded_basis` / `upstream_cost`: the module compares two
    /// beliefs about one model, and both halves of each pair read alike.
    pub upstream_metadata: ModelMetadata,
    /// Metadata fields the two disagree about. Empty when they agree, when
    /// the source is silent, or when the model is not in the source at all.
    pub metadata_drift: Vec<MetadataDrift>,
    /// The upstream's conditional rates past a threshold, already rendered —
    /// see [`upstream_tier_note`]. `None` means one flat rate at every size,
    /// which is what a single-rate catalog row can honestly express.
    pub upstream_tier: Option<String>,
    /// This upstream's declared reason for being outside the source's coverage,
    /// carried so the report can print it beside the row.
    ///
    /// `Some` exactly when the verdict is [`Verdict::NotCoveredBySource`]. It
    /// travels on the finding rather than being looked up again at print time
    /// so the reason shown is the one the verdict was decided from.
    pub unreconcilable_reason: Option<String>,
}

impl CandidateDrift {
    /// The single word for this candidate's whole row — everything wrong with
    /// it, not just its rates.
    ///
    /// [`Verdict`] answers only "does the recorded basis match", so a
    /// candidate priced correctly but describing a 1M-token model as 32k
    /// labels itself `ok`. That row is precisely the one an operator scanning
    /// for problems skips, and the detail section below then contradicts it.
    ///
    /// Rates win the cell when both drifted: a wrong basis is the more
    /// expensive fact, and no metadata detail is lost — every field still
    /// prints below.
    #[must_use]
    pub fn row_label(&self) -> &'static str {
        if !self.verdict.is_actionable() && self.has_actionable_metadata_drift() {
            MetadataDriftKind::Disagrees.label()
        } else {
            self.verdict.label()
        }
    }

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

/// A model id with its PUBLISHER prefix removed, when it carries one.
///
/// The third and last join attempt, for the same reason [`undated`] is the
/// second: the vendor and the source spell the same model differently, and
/// refusing to reconcile over a naming convention would switch off the one
/// alarm that guards margin.
///
/// **The case this exists for is Vertex.** Google's OpenAI-compatible surface
/// names its own models `google/gemini-3.7-flash` — the prefix is required, and
/// dropping it returns a 404 — while models.dev files the same model under
/// `google-vertex` as the bare `gemini-3.7-flash`. Without this the three
/// Vertex lanes report NOT IN SOURCE and their prices are checked by nothing.
///
/// Three properties keep this from becoming a way to match the wrong row:
///
/// - It is tried **last**, only after an exact match and the undated one have
///   both failed, so no lane that already reconciles can change what it matches.
/// - The lookup stays inside the SAME provider's model map, so a stripped id
///   can only ever find another model of the same vendor at the same source key.
/// - The stripped form is reported through `matched_as`, so the drift output
///   says what it matched as rather than quietly claiming an exact hit.
///
/// It deliberately takes only the LAST segment, so Fireworks'
/// `accounts/fireworks/models/kimi-k3` would reduce to `kimi-k3` — which is
/// harmless, because that lane matches exactly and never reaches this arm.
fn unpublished(model: &str) -> Option<&str> {
    let (_, bare) = model.rsplit_once('/')?;
    (!bare.is_empty()).then_some(bare)
}

/// Whether the catalog's conditional rates say the same thing as the source's.
///
/// Both halves count. **Thresholds** must match one for one and in order: a
/// catalog repricing at 200,000 against an upstream repricing at 272,000 is
/// wrong for every request in between, and it is wrong in a way no rate
/// comparison would ever notice because both sides' numbers are individually
/// correct. **Rates** are then compared band by band with the same
/// [`rates_agree`] the base tables use, so an upstream silent about a
/// dimension is silence rather than disagreement, exactly as it is at the
/// base.
///
/// Reads `cost.tiers[]` and never the flat `context_over_200k` convenience key
/// — see [`SourceCost::tiers`] for why that key's name is a lie.
fn conditional_agrees(recorded: &RateSchedule, upstream: &[SourceCostTier]) -> bool {
    if recorded.conditional().len() != upstream.len() {
        return false;
    }
    recorded
        .conditional()
        .iter()
        .zip(upstream)
        .all(|(recorded, upstream)| {
            // The kind is checked again here, not only at the call site. This
            // predicate answers "do these two say the same thing", and two
            // bounds measuring different quantities do not say the same thing
            // however well their numbers line up — so the guarantee belongs to
            // the comparison rather than to the order its callers happen to
            // run their checks in.
            upstream.measures_prompt_size()
                && recorded.min_prompt_tokens == upstream.tier.size
                && rates_agree(
                    recorded.rates,
                    ModelRates {
                        input_per_mtok: upstream.input,
                        output_per_mtok: upstream.output,
                        cached_input_per_mtok: upstream.cache_read,
                        cache_write_per_mtok: upstream.cache_write,
                    },
                )
        })
}

fn rates_agree(recorded: ModelRates, upstream: ModelRates) -> bool {
    let close = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() < EPSILON,
        // The upstream not publishing a dimension is not disagreement about
        // it; `Verdict::Unpriced` covers the wholly-unpriced case separately.
        (_, None) => true,
        (None, Some(_)) => false,
    };
    // The cache-write dimension is compared ONLY when both sides state it, so
    // its own comparator is deliberately weaker than `close` in one direction.
    //
    // For the other three, a file that declares nothing where the source
    // publishes a rate is drift: the dimension exists on every lane, so silence
    // is an omission. Cache write is not like that. An absent
    // `cache_write_per_mtok` is a CAPABILITY decision — it is how a lane says
    // it does not sell client prompt caching (see `ModelRates`) — and the
    // source publishes a cache-write rate for lanes ZeroRouter deliberately
    // does not sell it on. Reusing `close` here would turn every one of those
    // deliberate omissions into a red CI run demanding a price for a feature
    // the lane refuses.
    //
    // What survives is the check that matters: once a lane DOES transcribe the
    // rate, it is reconciled against the source like any other number, so a
    // vendor moving the multiplier is caught. Lanes that declare nothing are
    // reported informationally instead — `cache_write_undeclared`.
    let close_if_both = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() < EPSILON,
        _ => true,
    };
    close(recorded.input_per_mtok, upstream.input_per_mtok)
        && close(recorded.output_per_mtok, upstream.output_per_mtok)
        && close(
            recorded.cached_input_per_mtok,
            upstream.cached_input_per_mtok,
        )
        && close_if_both(recorded.cache_write_per_mtok, upstream.cache_write_per_mtok)
}

/// Lanes the source prices cache writes for while `tiers.toml` declares none.
///
/// Not a finding and never actionable — see [`rates_agree`] for why silence is
/// a decision here rather than an omission. It is printed anyway, because the
/// alternative is that the reconciliation quietly knows something the operator
/// does not: these are exactly the lanes that COULD sell prompt caching, with
/// the vendor's own number already in hand, and the only thing standing between
/// them and the feature is somebody transcribing it.
#[must_use]
pub fn cache_write_undeclared(findings: &[CandidateDrift]) -> Vec<(&str, f64)> {
    findings
        .iter()
        .filter(|found| found.unreconcilable_reason.is_none())
        .filter(|found| !found.recorded_basis.prices_cache_writes())
        .filter_map(|found| {
            found
                .upstream_cost
                .cache_write_per_mtok
                .map(|rate| (found.candidate_id.as_str(), rate))
        })
        .collect()
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
/// Modalities compare as sets, and ASYMMETRICALLY. Ordering is never drift —
/// the two catalogs have no reason to agree on it — and neither is the file
/// claiming a subset of what the source publishes, because the two are
/// describing different things: models.dev describes the model, `tiers.toml`
/// describes the lane, and a lane may carry less than the model it dispatches
/// to. A lane claiming MORE than the model still is drift. See
/// [`MetadataDriftKind::Narrowed`].
#[must_use]
fn metadata_drift(recorded: &ModelMetadata, upstream: &ModelMetadata) -> Vec<MetadataDrift> {
    let mut out = Vec::new();

    // A free function rather than a closure capturing `out`: the modality arm
    // below pushes to `out` too, and a capturing closure would hold the only
    // mutable borrow for the whole body — forcing modalities to be compared
    // last and silently reordering the report.
    fn compare(
        field: &'static str,
        recorded: Option<String>,
        upstream: Option<String>,
        agree: bool,
    ) -> Option<MetadataDrift> {
        let upstream = upstream?;
        match recorded {
            None => Some(MetadataDrift {
                field,
                kind: MetadataDriftKind::Undeclared,
                recorded: "-".to_owned(),
                upstream,
            }),
            Some(recorded) if !agree => Some(MetadataDrift {
                field,
                kind: MetadataDriftKind::Disagrees,
                recorded,
                upstream,
            }),
            Some(_) => None,
        }
    }

    out.extend(compare(
        "context_window",
        recorded.context_window.map(|v| v.to_string()),
        upstream.context_window.map(|v| v.to_string()),
        recorded.context_window == upstream.context_window,
    ));
    out.extend(compare(
        "max_output_tokens",
        recorded.max_output_tokens.map(|v| v.to_string()),
        upstream.max_output_tokens.map(|v| v.to_string()),
        recorded.max_output_tokens == upstream.max_output_tokens,
    ));
    // Modalities are the one field where the two catalogs are not making the
    // same KIND of claim, so they cannot be compared for equality. models.dev
    // describes what the MODEL accepts; this file describes what the LANE
    // carries, and a lane is free to carry less. See `MetadataDriftKind::
    // Narrowed` for the argument and for why the relaxation runs one way only.
    match (&recorded.input_modalities, &upstream.input_modalities) {
        // Source silent: nothing to check, exactly as everywhere else here.
        (_, None) => {}
        (None, Some(upstream)) => out.push(MetadataDrift {
            field: "input_modalities",
            kind: MetadataDriftKind::Undeclared,
            recorded: "-".to_owned(),
            upstream: upstream.join(","),
        }),
        (Some(recorded_list), Some(upstream_list)) => {
            // What the lane claims that the model does not: the false
            // advertisement, and the only direction that is a defect.
            let overclaimed: Vec<&str> = recorded_list
                .iter()
                .filter(|modality| !upstream_list.contains(modality))
                .map(String::as_str)
                .collect();
            // What the model takes that the lane declines to carry.
            let withheld: Vec<&str> = upstream_list
                .iter()
                .filter(|modality| !recorded_list.contains(modality))
                .map(String::as_str)
                .collect();
            if !overclaimed.is_empty() {
                out.push(MetadataDrift {
                    field: "input_modalities",
                    kind: MetadataDriftKind::Disagrees,
                    recorded: recorded_list.join(","),
                    upstream: upstream_list.join(","),
                });
            } else if !withheld.is_empty() {
                out.push(MetadataDrift {
                    field: "input_modalities",
                    kind: MetadataDriftKind::Narrowed,
                    recorded: recorded_list.join(","),
                    upstream: upstream_list.join(","),
                });
            }
        }
    }
    out.extend(compare(
        "tool_call",
        recorded.tool_call.map(|v| v.to_string()),
        upstream.tool_call.map(|v| v.to_string()),
        recorded.tool_call == upstream.tool_call,
    ));

    out
}

/// Reconcile a loaded catalog against an already-fetched source document.
///
/// Split from the fetch so the comparison is testable without a network.
/// Withheld tiers are reconciled too: a tier withheld for below-cost pricing
/// is precisely the one an operator most wants an upstream number for.
#[must_use]
pub fn reconcile(catalog: &TierCatalog, source: &str) -> Vec<CandidateDrift> {
    reconcile_with(
        catalog,
        source,
        &|candidate| crate::providers::provider_settles_free(&candidate.provider),
        &crate::providers::provider_unreconcilable_reason,
        &crate::providers::provider_source_key,
    )
}

/// [`reconcile`], with the "does this candidate's provider settle free?"
/// question supplied rather than read from the process-wide inventory.
///
/// The seam exists so the exemption can be tested for what it is — a rule
/// about candidates — without a test having to install a global inventory to
/// make one true, which would leak into every other test in the binary.
/// `pub(crate)` for exactly that reason and no other: [`crate::corroborate`]'s
/// tests need a real [`Verdict::Unreconcilable`] finding to prove the
/// exemption carries across to the second source, and hand-building one would
/// prove only that the test can build a struct.
///
/// `unreconcilable_reason` is supplied the same way and for the same reason: it
/// asks the provider inventory whether this upstream has DECLARED that the
/// source cannot price it ([`crate::providers::provider_unreconcilable_reason`]),
/// which is a fact about a deployment's configuration rather than something
/// this module can derive from a rate table.
pub(crate) fn reconcile_with(
    catalog: &TierCatalog,
    source: &str,
    settles_free: &dyn Fn(&crate::config::TierCandidate) -> bool,
    unreconcilable_reason: &dyn Fn(&str) -> Option<String>,
    source_provider_key: &dyn Fn(&str) -> Option<String>,
) -> Vec<CandidateDrift> {
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
            // The provider key is the join UNLESS the inventory declares
            // otherwise. ZeroRouter's key and the source's agree for
            // `anthropic`, `openai`, and `google` and that is a coincidence of
            // naming, not a rule — models.dev files Fireworks under
            // `fireworks-ai`. Falling back to the provider key keeps every
            // pre-existing entry reconciling exactly as it did.
            let declared_source_key = source_provider_key(&candidate.provider);
            let source_key = declared_source_key
                .as_deref()
                .unwrap_or(&candidate.provider);
            let models = providers.get(source_key).map(|p| &p.models);
            let (entry, matched_as) = match models {
                Some(models) => match models.get(&candidate.model) {
                    Some(entry) => (Some(entry), None),
                    None => match undated(&candidate.model).and_then(|base| models.get(base)) {
                        Some(entry) => (
                            Some(entry),
                            undated(&candidate.model).map(ToOwned::to_owned),
                        ),
                        None => {
                            match unpublished(&candidate.model).and_then(|bare| models.get(bare)) {
                                Some(entry) => (
                                    Some(entry),
                                    unpublished(&candidate.model).map(ToOwned::to_owned),
                                ),
                                None => (None, None),
                            }
                        }
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
                    cache_write_per_mtok: c.cache_write,
                })
                .unwrap_or_default();

            let upstream_conditional = entry
                .and_then(|entry| entry.cost.as_ref())
                .map(|cost| cost.tiers.as_slice())
                .unwrap_or_default();

            // Both exemptions are checked before the source is consulted at
            // all, and they are checked FIRST for the same reason: whether a
            // row exists over there decides nothing when the row could not
            // mean what it appears to mean. A $0 rung on an upstream that bills
            // nobody is not a model the source failed to mention, it is a model
            // the source is not about; a declared-unreconcilable upstream may
            // well have a row, and comparing against it is the failure.
            let declared_unreconcilable = unreconcilable_reason(&candidate.provider);
            let verdict = if candidate.rates_are_zero() && settles_free(candidate) {
                Verdict::Unreconcilable
            } else if declared_unreconcilable.is_some() {
                Verdict::NotCoveredBySource
            } else {
                match entry {
                    None => Verdict::Missing,
                    Some(entry) if entry.cost.is_none() => Verdict::Unpriced,
                    // Before anything else about the bands: if the upstream is
                    // measuring something ZeroRouter cannot condition on, no
                    // comparison below means what it appears to mean, and the
                    // advice "add a conditional band" would be wrong.
                    Some(_)
                        if upstream_conditional
                            .iter()
                            .any(|tier| !tier.measures_prompt_size()) =>
                    {
                        Verdict::UnsupportedTierKind
                    }
                    // Checked before `Match`: a row whose flat rate agrees is
                    // still mispriced past the threshold, and saying "ok" to it
                    // is exactly the silence this module exists to break. Only
                    // when the CATALOG has nothing to say about the boundary,
                    // though — once it declares its own conditional rates the
                    // two are comparable and this is an ordinary rate
                    // comparison again.
                    Some(_) if !upstream_conditional.is_empty() && candidate.rates.is_flat() => {
                        Verdict::TieredUpstream
                    }
                    Some(_)
                        if rates_agree(candidate.rates.base(), upstream_cost)
                            && conditional_agrees(&candidate.rates, upstream_conditional) =>
                    {
                        Verdict::Match
                    }
                    Some(_) => Verdict::BasisDrift,
                }
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
                recorded_basis: candidate.rates.base(),
                recorded_conditional: candidate
                    .rates
                    .conditional()
                    .iter()
                    .map(|conditional| (conditional.min_prompt_tokens, conditional.rates))
                    .collect(),
                upstream_cost,
                upstream_conditional: upstream_conditional
                    .iter()
                    .filter(|tier| tier.measures_prompt_size())
                    .map(|tier| {
                        (
                            tier.tier.size,
                            ModelRates {
                                input_per_mtok: tier.input,
                                output_per_mtok: tier.output,
                                cached_input_per_mtok: tier.cache_read,
                                cache_write_per_mtok: tier.cache_write,
                            },
                        )
                    })
                    .collect(),
                sell: definition.rates.base(),
                sell_conditional: definition
                    .rates
                    .conditional()
                    .iter()
                    .map(|conditional| (conditional.min_prompt_tokens, conditional.rates))
                    .collect(),
                metadata_drift: metadata_drift(&candidate.metadata, &upstream_metadata),
                upstream_tier: entry.and_then(upstream_tier_note),
                unreconcilable_reason: declared_unreconcilable,
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
    use crate::provider::ConditionalRate;

    fn rates(input: f64, cached: f64, output: f64) -> ModelRates {
        ModelRates {
            cache_write_per_mtok: None,
            input_per_mtok: Some(input),
            cached_input_per_mtok: Some(cached),
            output_per_mtok: Some(output),
        }
    }

    fn catalog_with(
        model: &str,
        basis: impl Into<RateSchedule>,
        sell: impl Into<RateSchedule>,
    ) -> TierCatalog {
        catalog_declaring(model, basis, sell, ModelMetadata::default())
    }

    /// One conditional band, for a catalog that reprices past `threshold`.
    fn conditional(threshold: u64, rates: ModelRates) -> ConditionalRate {
        ConditionalRate {
            min_prompt_tokens: threshold,
            rates,
        }
    }

    /// `catalog_with`, plus what the file claims the model can take and
    /// produce — the half of the comparison that used to have no input.
    fn catalog_declaring(
        model: &str,
        basis: impl Into<RateSchedule>,
        sell: impl Into<RateSchedule>,
        metadata: ModelMetadata,
    ) -> TierCatalog {
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "zero/test".to_owned(),
            TierDefinition {
                rates: sell.into(),
                retention: None,
                candidates: vec![TierCandidate {
                    id: format!("anthropic/{model}"),
                    provider: "anthropic".to_owned(),
                    model: model.to_owned(),
                    surface: None,
                    rates: basis.into(),
                    metadata,
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

    /// The source publishes each tier twice and the two spellings disagree by
    /// construction: `cost.tiers[].tier.size` carries the model's real
    /// boundary, while the flat `context_over_200k` key states that boundary in
    /// its NAME and is simply wrong whenever it is not 200k — `gpt-5.6-luna`
    /// reprices at 272,000. Reading the convenience key once produced a false
    /// report that the source disagreed with OpenRouter and was unfit to
    /// reconcile against, so the fixture below encodes the trap: a model whose
    /// real threshold is 272,000 while a `context_over_200k` key sits beside it
    /// holding a different number. The reported boundary must be the structured
    /// one.
    const TIERED_SOURCE: &str = r#"{
      "anthropic": { "models": {
        "gpt-5.6-luna": {
          "cost": {
            "input": 0.2, "output": 1.2, "cache_read": 0.02,
            "tiers": [
              { "input": 0.4, "output": 1.8, "cache_read": 0.04,
                "tier": { "type": "context", "size": 272000 } }
            ],
            "context_over_200k": { "input": 9.9, "output": 9.9, "cache_read": 9.9 }
          },
          "limit": { "context": 1050000, "output": 128000 }
        }
      } } }"#;

    #[test]
    fn a_tiered_upstream_is_reported_from_the_structured_field_not_the_named_key() {
        let catalog = catalog_with("gpt-5.6-luna", rates(0.2, 0.02, 1.2), rates(0.2, 0.02, 1.2));
        let found = reconcile(&catalog, TIERED_SOURCE);

        // The flat rates agree exactly, so the pre-existing comparison would
        // have said `ok` and moved on. It must not: the row is right below the
        // boundary and wrong above it.
        assert!(rates_agree(found[0].recorded_basis, found[0].upstream_cost));
        assert_eq!(found[0].verdict, Verdict::TieredUpstream);

        let note = found[0]
            .upstream_tier
            .as_deref()
            .expect("a tiered upstream reports its boundary");
        assert!(
            note.contains("272000"),
            "the boundary must come from cost.tiers[].tier.size: {note}"
        );
        assert!(
            !note.contains("200000") && !note.contains("9.9"),
            "context_over_200k must never be read — its name is the lie: {note}"
        );
        assert!(
            note.contains("0.4") && note.contains("1.8"),
            "the tier's own rates belong in the report: {note}"
        );
    }

    #[test]
    fn the_sources_bands_reach_the_report_as_numbers_not_only_as_a_display_string() {
        // `upstream_tier` is a sentence for a human. A SECOND catalog can only
        // be cross-checked against numbers, so the same bands travel
        // structured — and from the same structured field, never the
        // convenience key whose name is the lie.
        let catalog = catalog_with("gpt-5.6-luna", rates(0.2, 0.02, 1.2), rates(0.2, 0.02, 1.2));
        let found = reconcile(&catalog, TIERED_SOURCE);
        assert_eq!(
            found[0].upstream_conditional,
            vec![(272_000, rates(0.4, 0.04, 1.8))],
            "the structured cost.tiers[] band, at its real 272,000 boundary"
        );
    }

    #[test]
    fn a_band_measured_on_something_other_than_prompt_size_is_not_a_prompt_threshold() {
        // `upstream_conditional` is a list of PROMPT-token thresholds. An
        // output-length bound of 272,000 is not one, and letting it into the
        // list would hand every later reader — the second-source cross-check
        // included — two numbers that do not denote the same quantity and look
        // like they do. `UnsupportedTierKind` fires on this same input, so the
        // fact is not lost, only kept out of a list it does not belong in.
        let catalog = catalog_with("gpt-5.6-luna", rates(0.2, 0.02, 1.2), rates(0.2, 0.02, 1.2));
        let found = reconcile(&catalog, OUTPUT_TIERED_SOURCE);
        assert_eq!(found[0].verdict, Verdict::UnsupportedTierKind);
        assert_eq!(found[0].upstream_conditional, vec![]);
    }

    #[test]
    fn a_flat_catalog_against_a_tiered_upstream_is_now_actionable() {
        // The verdict was reported-but-tolerated only because the catalog
        // could not express the shape. It can now, so a file that still says
        // nothing about a boundary the vendor charges at has a four-line fix
        // and must fail the command until someone applies it.
        let catalog = catalog_with("gpt-5.6-luna", rates(0.2, 0.02, 1.2), rates(0.2, 0.02, 1.2));
        let found = reconcile(&catalog, TIERED_SOURCE);
        assert_eq!(found[0].verdict, Verdict::TieredUpstream);
        assert!(
            found[0].verdict.is_actionable(),
            "an unexpressed boundary bills at a basis ZeroRouter does not pay"
        );
        assert!(found[0].recorded_conditional.is_empty());
    }

    /// The same model, repriced on a dimension that is NOT prompt size. The
    /// rates and the boundary number are identical to `TIERED_SOURCE`; only
    /// `tier.type` differs, so anything that ignores that field reconciles
    /// this as a perfect match.
    const OUTPUT_TIERED_SOURCE: &str = r#"{
      "anthropic": { "models": {
        "gpt-5.6-luna": {
          "cost": {
            "input": 0.2, "output": 1.2, "cache_read": 0.02,
            "tiers": [
              { "input": 0.4, "output": 1.8, "cache_read": 0.04,
                "tier": { "type": "output", "size": 272000 } }
            ]
          },
          "limit": { "context": 1050000, "output": 128000 }
        }
      } } }"#;

    #[test]
    fn a_tier_measured_on_something_other_than_prompt_size_is_never_ok() {
        // `min_prompt_tokens` conditions on the PROMPT. An upstream repricing
        // on output length — or on time of day, or on anything else the source
        // grows a `type` for — is a different rule that happens to be written
        // in the same shape, and a catalog band would reconcile against it
        // number-for-number while pricing the wrong thing entirely.
        //
        // The `size` and every rate here match what the catalog declares, so
        // this row says `ok` unless `tier.type` is actually compared. It must
        // not: the honest answer is that ZeroRouter cannot model this, and
        // that has to fail the command rather than pass silently.
        let schedule = RateSchedule::new(
            rates(0.2, 0.02, 1.2),
            vec![conditional(272_000, rates(0.4, 0.04, 1.8))],
        );
        let catalog = catalog_with("gpt-5.6-luna", schedule.clone(), schedule);
        let found = reconcile(&catalog, OUTPUT_TIERED_SOURCE);
        assert_ne!(
            found[0].verdict,
            Verdict::Match,
            "an output-length tier must never reconcile against a prompt-token band"
        );
        assert_eq!(found[0].verdict, Verdict::UnsupportedTierKind);
        assert!(
            found[0].verdict.is_actionable(),
            "a repricing rule ZeroRouter cannot model is not something to pass over quietly"
        );
    }

    #[test]
    fn a_flat_catalog_against_an_unmodellable_tier_says_so_rather_than_asking_for_a_band() {
        // And the same on a catalog that declares nothing: the advice
        // "add a conditional band" would be wrong, because no band ZeroRouter
        // can express would price an output-length rule.
        let catalog = catalog_with("gpt-5.6-luna", rates(0.2, 0.02, 1.2), rates(0.2, 0.02, 1.2));
        let found = reconcile(&catalog, OUTPUT_TIERED_SOURCE);
        assert_eq!(found[0].verdict, Verdict::UnsupportedTierKind);
    }

    #[test]
    fn a_catalog_that_matches_the_upstreams_conditional_rates_reconciles() {
        // Once both sides declare the shape, this is an ordinary rate
        // comparison again: same boundary, same rates, `ok`.
        let schedule = RateSchedule::new(
            rates(0.2, 0.02, 1.2),
            vec![conditional(272_000, rates(0.4, 0.04, 1.8))],
        );
        let catalog = catalog_with("gpt-5.6-luna", schedule.clone(), schedule);
        let found = reconcile(&catalog, TIERED_SOURCE);
        assert_eq!(found[0].verdict, Verdict::Match);
        assert!(!found[0].verdict.is_actionable());
        assert_eq!(
            found[0].recorded_conditional,
            vec![(272_000, rates(0.4, 0.04, 1.8))],
            "the report carries the band so an operator can read what is being trusted"
        );
    }

    #[test]
    fn the_report_carries_the_sell_bands_not_only_the_basis_bands() {
        // What the customer pays above the boundary is a different fact from
        // what ZeroRouter pays, and the file is free to state them
        // differently. A report that showed only the basis would let a tier
        // charge 9.00 above 272,000 tokens while every row read `ok`, because
        // the basis it reconciles against is impeccable.
        let basis = RateSchedule::new(
            rates(0.2, 0.02, 1.2),
            vec![conditional(272_000, rates(0.4, 0.04, 1.8))],
        );
        let sell = RateSchedule::new(
            rates(0.2, 0.02, 1.2),
            vec![conditional(272_000, rates(9.0, 0.9, 9.0))],
        );
        let catalog = catalog_with("gpt-5.6-luna", basis, sell);
        let found = reconcile(&catalog, TIERED_SOURCE);

        assert_eq!(
            found[0].verdict,
            Verdict::Match,
            "the BASIS matches the upstream exactly — that is the point"
        );
        assert_eq!(
            found[0].recorded_conditional,
            vec![(272_000, rates(0.4, 0.04, 1.8))]
        );
        assert_eq!(
            found[0].sell_conditional,
            vec![(272_000, rates(9.0, 0.9, 9.0))],
            "the sell band must reach the report, or a markup above the boundary is invisible"
        );
    }

    #[test]
    fn a_catalog_whose_boundary_differs_from_the_upstreams_is_basis_drift() {
        // The failure a rate-only comparison cannot see: every individual
        // number matches, and the file is still wrong for every request
        // between 200,000 and 272,000 tokens.
        let schedule = RateSchedule::new(
            rates(0.2, 0.02, 1.2),
            vec![conditional(200_000, rates(0.4, 0.04, 1.8))],
        );
        let catalog = catalog_with("gpt-5.6-luna", schedule.clone(), schedule);
        let found = reconcile(&catalog, TIERED_SOURCE);
        assert_eq!(found[0].verdict, Verdict::BasisDrift);
        assert!(found[0].verdict.is_actionable());
    }

    #[test]
    fn a_catalog_whose_conditional_rates_differ_is_basis_drift() {
        let schedule = RateSchedule::new(
            rates(0.2, 0.02, 1.2),
            vec![conditional(272_000, rates(0.4, 0.04, 2.4))],
        );
        let catalog = catalog_with("gpt-5.6-luna", schedule.clone(), schedule);
        assert_eq!(
            reconcile(&catalog, TIERED_SOURCE)[0].verdict,
            Verdict::BasisDrift
        );
    }

    #[test]
    fn a_catalog_that_invents_a_boundary_the_upstream_does_not_have_is_basis_drift() {
        // The reverse direction, and it matters as much: a vendor that DROPS
        // its long-context surcharge leaves ZeroRouter charging customers for
        // a repricing it no longer pays, which the sell-side of this report is
        // there to catch.
        let schedule = RateSchedule::new(
            rates(2.0, 0.2, 10.0),
            vec![conditional(272_000, rates(4.0, 0.4, 18.0))],
        );
        let catalog = catalog_with("claude-sonnet-5", schedule.clone(), schedule);
        let found = reconcile(&catalog, SOURCE);
        assert_eq!(found[0].verdict, Verdict::BasisDrift);
        assert_eq!(found[0].upstream_tier, None);
    }

    /// A flat upstream must stay flat: the tier machinery may not invent a
    /// boundary for the models that do not have one, or every row would carry a
    /// warning and none would mean anything.
    #[test]
    fn an_untiered_upstream_reports_no_boundary() {
        let catalog = catalog_with(
            "claude-sonnet-5",
            rates(2.0, 0.2, 10.0),
            rates(2.0, 0.2, 10.0),
        );
        let found = reconcile(&catalog, SOURCE);
        assert_eq!(found[0].upstream_tier, None);
        assert_eq!(found[0].verdict, Verdict::Match);
    }

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

    /// The row label is the only part of the report an operator reads for
    /// every candidate; the detail section below is what they read for the
    /// ones it flags. So a row must never claim `ok` while carrying drift
    /// that the section then lists — the table would be sending them past
    /// the very candidate they are scanning for.
    #[test]
    fn a_row_whose_rates_are_fine_but_metadata_drifted_does_not_claim_ok() {
        let catalog = catalog_declaring(
            "claude-sonnet-5",
            rates(2.0, 0.2, 10.0),
            rates(2.0, 0.2, 10.0),
            ModelMetadata {
                context_window: Some(32_000),
                ..sonnet_metadata()
            },
        );
        let found = reconcile(&catalog, SOURCE);

        assert_eq!(
            found[0].verdict,
            Verdict::Match,
            "the basis genuinely does match; that is what makes this the trap"
        );
        assert!(found[0].has_actionable_metadata_drift());
        assert_eq!(found[0].row_label(), "METADATA DRIFT");
    }

    /// Rates win the cell when both drifted — the more expensive fact leads,
    /// and the metadata still prints in full below.
    #[test]
    fn a_drifted_basis_still_leads_the_row_over_drifted_metadata() {
        let catalog = catalog_declaring(
            "claude-sonnet-5",
            rates(9.0, 0.2, 10.0),
            rates(9.0, 0.2, 10.0),
            ModelMetadata {
                context_window: Some(32_000),
                ..sonnet_metadata()
            },
        );
        let found = reconcile(&catalog, SOURCE);

        assert_eq!(found[0].verdict, Verdict::BasisDrift);
        assert!(found[0].has_actionable_metadata_drift());
        assert_eq!(found[0].row_label(), "BASIS DRIFT");
    }

    /// A clean candidate still reads `ok`: the label was not widened into
    /// something that flags everything.
    #[test]
    fn a_candidate_that_agrees_on_both_axes_still_reads_ok() {
        let catalog = catalog_declaring(
            "claude-sonnet-5",
            rates(2.0, 0.2, 10.0),
            rates(2.0, 0.2, 10.0),
            sonnet_metadata(),
        );
        let found = reconcile(&catalog, SOURCE);

        assert_eq!(found[0].row_label(), "ok");
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

    // -----------------------------------------------------------------------
    // Lane modalities vs model modalities. The relaxation runs ONE WAY, and
    // both directions are asserted here together on purpose: the value of the
    // rule is the difference between them, and a pair of tests in separate
    // places would let someone "fix" the first by deleting the second.
    // -----------------------------------------------------------------------

    /// A lane that carries less than its model is the catalog telling the truth
    /// about the wire — the state `#102` worked to reach, when `pdf` came off
    /// thirteen rows no adapter could carry it on. It must not fail the command.
    #[test]
    fn a_lane_carrying_less_than_the_model_is_not_actionable_drift() {
        let catalog = catalog_declaring(
            "claude-sonnet-5",
            rates(2.0, 0.2, 10.0),
            rates(2.0, 0.2, 10.0),
            ModelMetadata {
                // The source publishes text,image,pdf; the wire carries no PDF.
                input_modalities: Some(vec!["text".to_owned(), "image".to_owned()]),
                ..sonnet_metadata()
            },
        );
        let found = reconcile(&catalog, SOURCE);

        assert!(
            !found[0].has_actionable_metadata_drift(),
            "a lane may carry less than the model it dispatches to"
        );
        assert_eq!(found[0].row_label(), "ok");
        // It still PRINTS, so the gap an operator could close one day stays
        // visible rather than becoming a silent skip.
        assert_eq!(
            found[0].metadata_drift,
            vec![MetadataDrift {
                field: "input_modalities",
                kind: MetadataDriftKind::Narrowed,
                recorded: "text,image".to_owned(),
                upstream: "text,image,pdf".to_owned(),
            }]
        );
    }

    /// The direction that must stay red: a lane advertising a modality the
    /// model does not take is the false claim that broke clients before `#102`
    /// — a request built on that row reaches an upstream that refuses it.
    #[test]
    fn a_lane_claiming_more_than_the_model_is_still_actionable_drift() {
        // `claude-sonnet-5` in SOURCE takes text,image,pdf — never video.
        let catalog = catalog_declaring(
            "claude-sonnet-5",
            rates(2.0, 0.2, 10.0),
            rates(2.0, 0.2, 10.0),
            ModelMetadata {
                input_modalities: Some(vec![
                    "text".to_owned(),
                    "image".to_owned(),
                    "pdf".to_owned(),
                    "video".to_owned(),
                ]),
                ..sonnet_metadata()
            },
        );
        let found = reconcile(&catalog, SOURCE);

        assert!(
            found[0].has_actionable_metadata_drift(),
            "advertising a modality no wire carries is the defect #102 closed"
        );
        assert_eq!(
            found[0].metadata_drift[0].kind,
            MetadataDriftKind::Disagrees
        );
        assert_eq!(found[0].row_label(), "METADATA DRIFT");
    }

    /// Overclaiming AND withholding at once is still a defect. The false claim
    /// is the fact that matters, so it wins the row rather than being masked by
    /// the honest narrowing sitting beside it.
    #[test]
    fn overclaiming_wins_over_narrowing_when_a_lane_does_both() {
        let catalog = catalog_declaring(
            "claude-sonnet-5",
            rates(2.0, 0.2, 10.0),
            rates(2.0, 0.2, 10.0),
            ModelMetadata {
                // Drops `pdf` (honest) and adds `video` (a false claim).
                input_modalities: Some(vec![
                    "text".to_owned(),
                    "image".to_owned(),
                    "video".to_owned(),
                ]),
                ..sonnet_metadata()
            },
        );
        let found = reconcile(&catalog, SOURCE);
        assert_eq!(found[0].metadata_drift.len(), 1);
        assert_eq!(
            found[0].metadata_drift[0].kind,
            MetadataDriftKind::Disagrees
        );
        assert!(found[0].has_actionable_metadata_drift());
    }

    /// An exact match stays silent: the narrowing did not turn agreement into a
    /// permanent advisory row.
    #[test]
    fn a_lane_matching_the_model_exactly_reports_nothing_at_all() {
        let catalog = catalog_declaring(
            "claude-sonnet-5",
            rates(2.0, 0.2, 10.0),
            rates(2.0, 0.2, 10.0),
            sonnet_metadata(),
        );
        assert_eq!(reconcile(&catalog, SOURCE)[0].metadata_drift, vec![]);
    }

    /// The other fields did NOT get the subset treatment. A context window is a
    /// single number describing one capability, not a set of capabilities a
    /// lane may decline to carry, and a lane understating it means long-context
    /// work quietly truncated — a defect, not a conservative claim.
    #[test]
    fn a_smaller_context_window_is_still_drift_and_not_a_narrowing() {
        let catalog = catalog_declaring(
            "claude-sonnet-5",
            rates(2.0, 0.2, 10.0),
            rates(2.0, 0.2, 10.0),
            ModelMetadata {
                context_window: Some(200_000),
                ..sonnet_metadata()
            },
        );
        let found = reconcile(&catalog, SOURCE);
        assert!(found[0].has_actionable_metadata_drift());
        assert_eq!(found[0].metadata_drift[0].field, "context_window");
        assert_eq!(
            found[0].metadata_drift[0].kind,
            MetadataDriftKind::Disagrees
        );
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

    /// The publisher-prefix join, as a unit — the three cases that decide
    /// whether it can ever match the wrong row.
    #[test]
    fn a_publisher_prefix_is_stripped_only_when_there_is_one() {
        assert_eq!(
            unpublished("google/gemini-3.7-flash"),
            Some("gemini-3.7-flash")
        );
        assert_eq!(
            unpublished("accounts/fireworks/models/kimi-k3"),
            Some("kimi-k3"),
            "only the last segment; this lane matches exactly and never reaches the arm"
        );
        assert_eq!(
            unpublished("gemini-3.7-flash"),
            None,
            "an id with no prefix has nothing to strip"
        );
        assert_eq!(
            unpublished("google/"),
            None,
            "a trailing slash must not produce an empty lookup key"
        );
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

    /// A one-candidate catalog on `provider`, priced at `basis`.
    fn catalog_on(provider: &str, basis: ModelRates) -> TierCatalog {
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "zero/edge".to_owned(),
            TierDefinition {
                rates: RateSchedule::flat(rates(2.0, 0.2, 10.0)),
                retention: None,
                candidates: vec![TierCandidate {
                    id: format!("{provider}/qwen3-8b"),
                    provider: provider.to_owned(),
                    model: "qwen3-8b".to_owned(),
                    surface: None,
                    rates: RateSchedule::flat(basis),
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

    fn free() -> ModelRates {
        ModelRates {
            cache_write_per_mtok: None,
            input_per_mtok: Some(0.0),
            cached_input_per_mtok: None,
            output_per_mtok: Some(0.0),
        }
    }

    #[test]
    fn a_local_zero_rung_is_reported_but_never_fails_the_command() {
        // Edge mode, stage 2. A model on the operator's own hardware is absent
        // from models.dev and always will be, so `NOT IN SOURCE` — which fails
        // the command — would fire forever on a correct config, and an alarm
        // that fires forever is an alarm nobody reads. It still gets a row, so
        // the exemption is visible in the table rather than a silent skip, and
        // there is no metadata noise to go with it: the source has nothing to
        // say about this model, and silence upstream is never disagreement.
        let catalog = catalog_on("local-llama", free());
        let found = reconcile_with(&catalog, SOURCE, &|_| true, &|_| None, &|_| None);

        assert_eq!(found[0].verdict, Verdict::Unreconcilable);
        assert!(!found[0].verdict.is_actionable());
        assert_eq!(found[0].verdict.label(), "local $0 rung");
        assert_eq!(found[0].metadata_drift, vec![]);
        assert!(!found[0].has_actionable_metadata_drift());
    }

    #[test]
    fn the_exemption_needs_both_halves_or_it_would_disable_the_alarm() {
        // A $0 basis on a SHIPPED provider is the silent-margin failure this
        // module exists to catch — the fat-fingered rate that records zero
        // COGS against real spend — so it must keep reconciling and keep
        // failing. (`validate_zero_price` refuses such a file outright since
        // stage 2; this does not depend on that.)
        let shipped_but_free = catalog_on("anthropic", free());
        let found = reconcile_with(&shipped_but_free, SOURCE, &|_| false, &|_| None, &|_| None);
        assert_eq!(found[0].verdict, Verdict::Missing);
        assert!(found[0].verdict.is_actionable());

        // And an operator upstream that is PRICED is a real cost basis, so it
        // gets a real answer even though that answer is `NOT IN SOURCE`. Only
        // the intersection is exempt.
        let operator_but_priced = catalog_on("local-llama", rates(1.0, 0.1, 5.0));
        let found = reconcile_with(&operator_but_priced, SOURCE, &|_| true, &|_| None, &|_| {
            None
        });
        assert_eq!(found[0].verdict, Verdict::Missing);
        assert!(found[0].verdict.is_actionable());
    }

    /// The Bedrock exemption, and the specific trap it exists to avoid.
    ///
    /// A declaring upstream is NOT one the source has never heard of. models.dev
    /// does carry Bedrock, at a key a mapping could reach, priced at the GLOBAL
    /// cross-region rate — while ZeroRouter dials the in-region-only mantle
    /// endpoint AWS meters 10% higher. So the fixture below deliberately makes
    /// the source ANSWER, and answer with the wrong SKU class: it is built to
    /// reconcile cleanly against a basis that is 10% under the invoice.
    #[test]
    fn a_declared_exemption_beats_a_source_row_that_would_have_matched() {
        const GLOBAL_RATE_SOURCE: &str = r#"{
            "bedrock": {"models": {"anthropic.claude-sonnet-5": {
                "cost": {"input": 2.0, "output": 10.0, "cache_read": 0.2}
            }}}
        }"#;
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "bedrock/claude-sonnet-5".to_owned(),
            TierDefinition {
                rates: RateSchedule::flat(rates(2.2, 0.22, 11.0)),
                retention: None,
                candidates: vec![TierCandidate {
                    id: "bedrock/claude-sonnet-5".to_owned(),
                    provider: "bedrock".to_owned(),
                    model: "anthropic.claude-sonnet-5".to_owned(),
                    surface: None,
                    rates: RateSchedule::flat(rates(2.2, 0.22, 11.0)),
                    metadata: ModelMetadata::default(),
                }],
            },
        );
        let catalog = TierCatalog {
            schema_version: 1,
            tiers,
            retention: BTreeMap::new(),
            unavailable: BTreeMap::new(),
        };

        // WITHOUT the declaration the source disagrees, and it disagrees in the
        // direction that reads as our file being wrong — 2.2 recorded against
        // 2.0 published. An operator "fixing" that would move the basis 10%
        // BELOW what AWS invoices. This is the row the exemption removes.
        let unguarded =
            reconcile_with(&catalog, GLOBAL_RATE_SOURCE, &|_| false, &|_| None, &|_| {
                None
            });
        assert_eq!(unguarded[0].verdict, Verdict::BasisDrift);
        assert!(unguarded[0].verdict.is_actionable());

        // WITH it the verdict is the declared gap, it is not actionable, and
        // the reason travels on the finding so the report can argue the case.
        let guarded = reconcile_with(
            &catalog,
            GLOBAL_RATE_SOURCE,
            &|_| false,
            &|provider| (provider == "bedrock").then(|| "global rate, we pay in-region".to_owned()),
            &|_| None,
        );
        assert_eq!(guarded[0].verdict, Verdict::NotCoveredBySource);
        assert!(!guarded[0].verdict.is_actionable());
        assert_eq!(guarded[0].verdict.label(), "source cannot price");
        assert_eq!(
            guarded[0].unreconcilable_reason.as_deref(),
            Some("global rate, we pay in-region")
        );
    }

    /// The reconciliation source files some upstreams under a key that is not
    /// ZeroRouter's, and Fireworks is the first: models.dev calls it
    /// `fireworks-ai`. The whole join is one `BTreeMap::get`, so without a
    /// declared key every candidate on such an upstream reports `NOT IN
    /// SOURCE` — actionable, permanently, on rates that are in fact published
    /// and checkable.
    ///
    /// Both halves are asserted in one test on purpose: the value of the
    /// mechanism is the DIFFERENCE between them, and a pair of tests in
    /// different places would let someone "fix" the first by deleting the
    /// second's premise.
    #[test]
    fn a_declared_source_key_joins_an_upstream_the_source_files_under_another_name() {
        const RENAMED_SOURCE: &str = r#"{
            "fireworks-ai": {"models": {"accounts/fireworks/models/kimi-k3": {
                "cost": {"input": 3.0, "output": 15.0, "cache_read": 0.3}
            }}}
        }"#;
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "fireworks/kimi-k3".to_owned(),
            TierDefinition {
                rates: RateSchedule::flat(rates(3.0, 0.3, 15.0)),
                retention: None,
                candidates: vec![TierCandidate {
                    id: "fireworks/kimi-k3".to_owned(),
                    provider: "fireworks".to_owned(),
                    model: "accounts/fireworks/models/kimi-k3".to_owned(),
                    surface: None,
                    rates: RateSchedule::flat(rates(3.0, 0.3, 15.0)),
                    metadata: ModelMetadata::default(),
                }],
            },
        );
        let catalog = TierCatalog {
            schema_version: 1,
            tiers,
            retention: BTreeMap::new(),
            unavailable: BTreeMap::new(),
        };

        // WITHOUT the declaration: the row exists, at a key one character off,
        // and the report says the model is not in the source at all. This is
        // the state the shipped inventory would have been in.
        let unmapped = reconcile_with(&catalog, RENAMED_SOURCE, &|_| false, &|_| None, &|_| None);
        assert_eq!(unmapped[0].verdict, Verdict::Missing);
        assert!(unmapped[0].verdict.is_actionable());

        // WITH it: an ordinary rate comparison, and it agrees.
        let mapped = reconcile_with(
            &catalog,
            RENAMED_SOURCE,
            &|_| false,
            &|_| None,
            &|provider| (provider == "fireworks").then(|| "fireworks-ai".to_owned()),
        );
        assert_eq!(mapped[0].verdict, Verdict::Match);
        assert!(!mapped[0].verdict.is_actionable());
    }

    /// The mapping redirects the join; it does not weaken it. A declared key
    /// pointing at a real row whose prices differ must still report drift —
    /// otherwise the field would be a quiet way to make an upstream stop being
    /// checked, which is what `unreconcilable_reason` is for and what this
    /// field must never become.
    #[test]
    fn a_declared_source_key_still_reports_drift_at_the_key_it_names() {
        const RENAMED_SOURCE: &str = r#"{
            "fireworks-ai": {"models": {"accounts/fireworks/models/kimi-k3": {
                "cost": {"input": 9.0, "output": 15.0, "cache_read": 0.3}
            }}}
        }"#;
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "fireworks/kimi-k3".to_owned(),
            TierDefinition {
                rates: RateSchedule::flat(rates(3.0, 0.3, 15.0)),
                retention: None,
                candidates: vec![TierCandidate {
                    id: "fireworks/kimi-k3".to_owned(),
                    provider: "fireworks".to_owned(),
                    model: "accounts/fireworks/models/kimi-k3".to_owned(),
                    surface: None,
                    rates: RateSchedule::flat(rates(3.0, 0.3, 15.0)),
                    metadata: ModelMetadata::default(),
                }],
            },
        );
        let catalog = TierCatalog {
            schema_version: 1,
            tiers,
            retention: BTreeMap::new(),
            unavailable: BTreeMap::new(),
        };

        let found = reconcile_with(
            &catalog,
            RENAMED_SOURCE,
            &|_| false,
            &|_| None,
            &|provider| (provider == "fireworks").then(|| "fireworks-ai".to_owned()),
        );
        assert_eq!(found[0].verdict, Verdict::BasisDrift);
        assert!(found[0].verdict.is_actionable());
    }

    /// An upstream that declares no source key joins on its own key, which is
    /// what every entry did before the field existed. Pinned so that adding
    /// the mapping cannot quietly change the three providers that never needed
    /// one.
    #[test]
    fn an_undeclared_source_key_still_joins_on_the_provider_key() {
        let catalog = catalog_with(
            "claude-sonnet-5",
            rates(2.0, 0.2, 10.0),
            rates(2.0, 0.2, 10.0),
        );
        let found = reconcile_with(&catalog, SOURCE, &|_| false, &|_| None, &|_| None);
        assert_eq!(found[0].verdict, Verdict::Match);
    }

    #[test]
    fn the_exemption_is_scoped_to_the_declaring_provider_alone() {
        // A resolver that answers for one upstream must not quiet another. If
        // this ever fails, one provider's declared gap has become everybody's
        // silence, which is the whole reconciliation switched off by accident.
        let catalog = catalog_with(
            "claude-sonnet-5",
            rates(9.0, 0.9, 9.0),
            rates(9.0, 0.9, 9.0),
        );
        let found = reconcile_with(
            &catalog,
            SOURCE,
            &|_| false,
            &|provider| (provider == "bedrock").then(|| "declared elsewhere".to_owned()),
            &|_| None,
        );
        assert_eq!(found[0].verdict, Verdict::BasisDrift);
        assert!(found[0].verdict.is_actionable());
        assert_eq!(found[0].unreconcilable_reason, None);
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
