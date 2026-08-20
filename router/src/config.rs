use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock, PoisonError},
};

use crate::provider::{ConditionalRate, ModelRates, RateSchedule};
use serde::{Deserialize, Deserializer};
use thiserror::Error;

use crate::{
    openai::{MAX_RATE_PER_MTOK, billable_rate},
    priority::Priority,
    providers::{is_supported_provider, provider_settles_free},
};

pub const DEFAULT_TIER_CONFIG_PATH: &str = "config/tiers.toml";
pub const TIER_CONFIG_PATH_ENV: &str = "ZEROROUTER_TIERS_PATH";

/// The parsed catalog, split into what may be sold and what may not.
///
/// `tiers` holds only tiers that are safe to serve. Everything that routes or
/// advertises — [`TierCatalog::resolve`], [`TierCatalog::model_listing`] —
/// reads that map and nothing else, so a withheld tier cannot leak back into
/// the product by a caller forgetting a check.
///
/// A tier that parsed cleanly and is structurally sound but is priced below
/// its own cost basis moves to `unavailable` instead of failing the load. It
/// is kept rather than dropped so a request for it can answer *which* tier is
/// broken and why (see [`TierCatalog::unavailable_for`]) rather than claiming
/// a model that plainly exists in the file is not there.
#[derive(Clone, Debug, Deserialize)]
pub struct TierCatalog {
    pub schema_version: u32,
    pub tiers: BTreeMap<String, TierDefinition>,
    /// What each upstream does with a request after it answers, keyed by the
    /// provider key a candidate names (`anthropic`, `openai`, `google`).
    ///
    /// Provider-level because retention is a property of **the operator's
    /// account with that provider**, not of a model: every model reached on
    /// one API key rests under one arrangement, and a per-model claim would
    /// invite the file to say two different things about one account.
    /// [`TierDefinition::retention`] overrides it for the one case the account
    /// rule cannot express — a lane bought under a separate agreement.
    ///
    /// `#[serde(default)]` here is *not* a default posture. An empty map
    /// parses and then fails [`validate_tier_catalog`] at the first candidate
    /// whose provider is unlabelled, which is how an unlabelled lane is made
    /// impossible rather than merely discouraged: the refusal carries the
    /// provider's name, where a serde error would only say a key was missing.
    #[serde(default)]
    pub retention: BTreeMap<String, RetentionPin>,
    /// Tiers withheld for below-cost pricing, keyed by tier id. Never
    /// deserialized from the file — it is a verdict about the file, not a
    /// field in it.
    #[serde(skip)]
    pub unavailable: BTreeMap<String, UnavailableTier>,
}

/// A tier that parses and is structurally valid but must not be served,
/// because at least one of its candidates costs more than the tier sells for.
#[derive(Clone, Debug)]
pub struct UnavailableTier {
    pub tier: String,
    /// The rendered [`TierConfigError::NegativeMargin`] that withheld it. It
    /// carries the cost basis and the sell rate, so it belongs in operator
    /// logs and never in a customer-facing response body.
    pub reason: String,
    /// Kept so a request that pins one of this tier's concrete candidates gets
    /// the same honest answer as a request for the tier id itself.
    pub definition: TierDefinition,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TierDefinition {
    #[serde(default)]
    pub candidates: Vec<TierCandidate>,
    #[serde(deserialize_with = "deserialize_rate_schedule")]
    pub rates: RateSchedule,
    /// Retention posture for this tier alone, overriding the provider-level
    /// pin its candidates would otherwise resolve to.
    ///
    /// A COMPLETE replacement, never a patch — the same rule a conditional
    /// rate band follows. A tier that overrides states its own posture, its own
    /// evidence, and its own verification date, because a half-inherited claim
    /// would cite a page that was never read for this lane.
    ///
    /// The case it exists for: one lane bought under a separate agreement (a
    /// negotiated ZDR endpoint) while the rest of that provider's account stays
    /// standard. Absent on every shipped tier today.
    #[serde(default)]
    pub retention: Option<RetentionPin>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TierCandidate {
    pub id: String,
    pub provider: String,
    pub model: String,
    #[serde(deserialize_with = "deserialize_rate_schedule")]
    pub rates: RateSchedule,
    /// What this model can take and produce. Declared here, beside the rates,
    /// because it is the same kind of claim: static, human-owned, and true
    /// only because someone checked. A candidate that declares none is
    /// `ModelMetadata::default()` — every field unknown — which is exactly how
    /// the file read before this table existed.
    #[serde(default)]
    pub metadata: ModelMetadata,
}

impl TierCandidate {
    /// Whether this candidate's COST BASIS is zero — the tier-file half of
    /// "free", and a claim about what ZeroRouter pays, never about what the
    /// customer is charged (that is the owning tier's sell rate; see
    /// [`ResolvedRoute::sells_free`]).
    ///
    /// The arithmetic, and why the cached dimension may be absent, is
    /// [`ModelRates::are_zero`] — one definition, read from both sides of the
    /// money. A local server has no cache pricing, so requiring the operator
    /// to write `= 0` for it would make the commonest honest config silently
    /// *not* free, which is a worse failure than the one strictness buys.
    ///
    /// On its own this says nothing about whether anyone is billed — a rate
    /// table can be wrong, and a zero in it is as likely to be a typo as a
    /// claim. [`Self::is_free`] is the question with an answer.
    ///
    /// A schedule carrying conditional tables answers this only if EVERY one
    /// of them is zero too ([`RateSchedule::are_zero`]) — a free rung has no
    /// repricing to declare, and a conditional table that charges past a
    /// threshold is a priced rung whatever its base row says.
    #[must_use]
    pub fn rates_are_zero(&self) -> bool {
        self.rates.are_zero()
    }

    /// Whether this candidate is a **$0 rung** (edge mode, stage 2:
    /// `docs/design/edge-mode-local-rung.md`).
    ///
    /// The one definition of free, and it requires TWO declarations that live
    /// in two different files: the provider says its traffic bills nobody
    /// (`"settlement": "free"`, [`crate::providers::provider_settles_free`]) and
    /// the candidate is priced at zero here. Either alone is refused — a $0
    /// price on a metered provider does not load at all (`validate_zero_price`),
    /// and a free-settling provider whose candidate carries real rates is
    /// simply a priced rung.
    ///
    /// Both are needed because neither file can be trusted to mean it alone.
    /// A zero in a rate table is indistinguishable from a fat-fingered rate,
    /// which is the silent-margin failure this repo fears most; and a provider
    /// declaration says nothing about what any particular candidate charges.
    /// Requiring both means the free lane is only ever entered on purpose —
    /// never by a typo, never by picking a wire, never by forgetting a
    /// credential. It cannot make an operator's claim TRUE (see
    /// [`crate::providers::SettlementDeclaration`]); it makes the claim
    /// deliberate.
    ///
    /// Cost-mode ordering reads this to put free rungs first, and `drift.rs`
    /// reads it to know which candidates a public model catalog cannot speak
    /// to. Stage 3's metering skip is specified to key on "the candidate's
    /// configured price at selection time, in one place" — this is that place.
    /// It is a property of server-side configuration only: nothing about a
    /// request, a header, or a model alias can reach it.
    #[must_use]
    pub fn is_free(&self) -> bool {
        self.rates_are_zero() && provider_settles_free(&self.provider)
    }
}

/// What a request mechanically needs from whatever serves it (edge mode,
/// stage 2).
///
/// Every field is a fact about the request as received — how much prompt it
/// carries, whether it declares tools, which input modalities it contains.
/// Nothing here is, or may become, a judgment about how *well* a model would
/// answer: that is the design's B-line ("No quality prediction. No cascades.
/// No judge models."), and the reason this type carries measurements rather
/// than scores. A candidate is eligible or it is not, and the answer is a
/// comparison against what the operator declared in `tiers.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestNeeds {
    /// The byte-length prompt bound — the same number admission reserves
    /// against (`ChatCompletionRequest::reservation_usage`), deliberately, so
    /// selection and reservation describe the same request.
    pub prompt_bound: u64,
    /// Whether the request declares tools.
    pub tools: bool,
    /// Input modalities present in the request, in models.dev's vocabulary —
    /// the vocabulary `tiers.toml` declares in.
    pub modalities: BTreeSet<String>,
}

/// What a model can take and produce, as declared in `config/tiers.toml`.
///
/// Every field is optional and every one of them means *unknown* when absent,
/// never a default. That distinction is the whole point of the type: a
/// consumer has to be able to tell "this model has a small window" from "no
/// one has told me what this model's window is", because the two call for
/// opposite behaviour. ZeroClaw's `ModelInfo.context_window` is an `Option`
/// for the same reason — and when it is `None` that client falls back to
/// `UNCONFIGURED_CONTEXT_WINDOW_FALLBACK` (32,000 tokens), which is why an
/// omitted field here is a real cost and a *wrong* field here is a worse one.
///
/// Names follow models.dev, the catalog `zerorouter admin catalog-drift`
/// reconciles against, so a drift check is a direct comparison rather than a
/// translation. The wire spelling differs in one place — see
/// [`crate::openai::ModelObject`].
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ModelMetadata {
    /// Maximum input window, in tokens.
    pub context_window: Option<u64>,
    /// Maximum tokens the model will generate in one response.
    pub max_output_tokens: Option<u64>,
    /// Input modalities the model accepts, in models.dev's vocabulary
    /// (`text`, `image`, `pdf`, `audio`).
    pub input_modalities: Option<Vec<String>>,
    /// Whether the model supports native tool calling.
    pub tool_call: Option<bool>,
}

impl ModelMetadata {
    /// Whether a candidate declaring this metadata can mechanically serve a
    /// request (edge mode, stage 2).
    ///
    /// The rule the design doc states in full: *"Selection inputs are
    /// mechanical only: does the request fit context, need tools/modalities
    /// the candidate lacks, is the endpoint healthy."* Health lives in
    /// [`crate::health`]; the other three are here.
    ///
    /// **Unknown is never a refusal.** An undeclared field means no one has
    /// described that model, which is exactly the state every candidate was in
    /// before the metadata table existed — so a silent candidate is eligible
    /// for everything, precisely as it is today. Only a *declared* limit can
    /// exclude a candidate, and only against a fact about the request. That
    /// keeps the operator in charge: the router never decides a model is
    /// unsuitable, it only believes what the operator wrote down.
    ///
    /// Two deliberate conservatisms in the context comparison:
    ///
    /// - `prompt_bound` is a BYTE bound, not a token count. There is no
    ///   tokenizer on this path — billing policy is metered actuals only, and
    ///   the only prompt measure the router has is the byte bound admission
    ///   reserves against — so it over-counts tokens by roughly 3-4x and a
    ///   local rung yields to cloud earlier than it strictly must. That is the
    ///   right direction to be wrong in: the walk's response to an upstream
    ///   context rejection is to truncate the prompt in place and retry, so an
    ///   over-eager local rung would silently drop the customer's content,
    ///   while an over-cautious one only spends money.
    /// - `max_output_tokens` is NOT part of eligibility. Every request carries
    ///   an output limit (defaulted to the baseline when absent), so gating on
    ///   it would exclude any local model whose declared ceiling sits under
    ///   that default for every request, including the ones that would have
    ///   generated forty tokens. Servers on this wire stop at their own limit;
    ///   overflowing it is not a refusal the way an oversized prompt is.
    #[must_use]
    pub fn can_serve(&self, needs: &RequestNeeds) -> bool {
        if self
            .context_window
            .is_some_and(|window| needs.prompt_bound > window)
        {
            return false;
        }
        if needs.tools && self.tool_call == Some(false) {
            return false;
        }
        if let Some(declared) = &self.input_modalities
            && !needs
                .modalities
                .iter()
                .all(|modality| declared.contains(modality))
        {
            return false;
        }
        true
    }

    /// The metadata a *tier* may honestly advertise, given every candidate a
    /// request for that tier could land on.
    ///
    /// A tier id is a promise about the worst rung the walk can reach, not the
    /// best. Advertising the flagship's 1M window on a tier that can fail over
    /// to a 200k rung would hand the customer a number that is wrong precisely
    /// when failover happens — the moment they are least able to notice. So
    /// limits take the minimum, modalities take the intersection, and tool
    /// calling holds only if it holds for every candidate.
    ///
    /// A single undeclared candidate makes the whole field unknown rather than
    /// being skipped. Narrowing over the rest would publish a bound this tier
    /// cannot keep: the candidate nobody described might be the 32k one.
    ///
    /// Today every shipped tier has exactly one candidate, so this reduces to
    /// copying that candidate's metadata. The rule is written for the tier
    /// that gains a second rung, which is the change that would otherwise turn
    /// a correct listing into a confident lie.
    #[must_use]
    fn narrowed(candidates: &[TierCandidate]) -> Self {
        // `try_fold` over `Option` is the "one unknown poisons the field" rule
        // itself: a `None` from any candidate short-circuits the whole fold.
        let smallest = |pick: fn(&Self) -> Option<u64>| {
            candidates
                .iter()
                .map(|candidate| pick(&candidate.metadata))
                .try_fold(None::<u64>, |narrowed, declared| {
                    declared.map(|declared| Some(narrowed.map_or(declared, |n| n.min(declared))))
                })
                .flatten()
        };

        Self {
            context_window: smallest(|metadata| metadata.context_window),
            max_output_tokens: smallest(|metadata| metadata.max_output_tokens),
            input_modalities: candidates
                .iter()
                .map(|candidate| candidate.metadata.input_modalities.as_deref())
                .try_fold(None::<Vec<String>>, |shared, declared| {
                    let declared = declared?;
                    Some(Some(match shared {
                        // The first candidate's order is kept, so the listing
                        // reads the way the file does.
                        None => declared.to_vec(),
                        Some(shared) => shared
                            .into_iter()
                            .filter(|modality| declared.contains(modality))
                            .collect(),
                    }))
                })
                .flatten(),
            tool_call: candidates
                .iter()
                .map(|candidate| candidate.metadata.tool_call)
                .try_fold(None::<bool>, |narrowed, declared| {
                    declared.map(|declared| Some(narrowed.is_none_or(|n| n) && declared))
                })
                .flatten(),
        }
    }
}

/// What an upstream does with a request once it has answered it.
///
/// Two values and no third, because the catalog publishes this to customers as
/// a claim about their data and "probably not retained" is not a claim anyone
/// can act on. Absence is not a value either — a lane with no resolvable
/// posture does not load (see [`TierConfigError::UnlabelledLane`]).
#[derive(Clone, Copy, Debug, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RetentionPosture {
    /// The upstream retains nothing: prompts and completions are not written
    /// to durable storage on their side, including for abuse monitoring.
    ///
    /// **Only pinnable against a signed or confirmed zero-retention
    /// arrangement with that provider.** Every major vendor offers ZDR by
    /// negotiated agreement rather than by default, so the posture is a fact
    /// about the operator's contract, never about the vendor's marketing. See
    /// `docs/DEPLOY.md`, "Changing a retention posture".
    Zero,
    /// The upstream retains prompts and completions for some period — commonly
    /// for abuse monitoring, whether or not it trains on them.
    ///
    /// This is the honest default posture for an ordinary API account, and it
    /// is what every lane in the shipped catalog carries today.
    Standard,
}

impl RetentionPosture {
    /// Sort rank for the public catalog: zero-retention lanes come first.
    ///
    /// Written out rather than derived from the variant order, because a
    /// `#[derive(Ord)]` would make the catalog's ordering a silent consequence
    /// of how the enum happens to be typed — reordering two variants during an
    /// unrelated edit would quietly demote every zero-retention lane. This
    /// function is the ordering, it is greppable, and
    /// `zero_retention_lanes_sort_before_standard_ones` fails if it is flipped.
    #[must_use]
    pub const fn ordering_rank(self) -> u8 {
        match self {
            Self::Zero => 0,
            Self::Standard => 1,
        }
    }

    /// The short label the catalog publishes for this posture.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Zero => "zero retention",
            Self::Standard => "provider retains data",
        }
    }

    /// The weaker of two postures: `standard` always wins.
    ///
    /// The retention analogue of [`ModelMetadata::narrowed`], and it fails in
    /// the same direction. A tier that can route to a zero-retention rung *and*
    /// a retaining one retains data, because the customer cannot tell which
    /// rung served them — so the tier may only advertise the weaker claim.
    #[must_use]
    pub const fn weaker(self, other: Self) -> Self {
        match (self, other) {
            (Self::Zero, Self::Zero) => Self::Zero,
            _ => Self::Standard,
        }
    }
}

/// A pinned retention claim: what the posture is, and the evidence it was read
/// from.
///
/// Every field is required. A half-written pin is refused at load rather than
/// accepted with blanks, because the three evidence fields are what make this a
/// checkable claim instead of an assertion: `source_url` is where a human
/// verified it, `verified` is when, and `source_sha256` is what that page said
/// at the time, so `zerorouter admin retention-drift` can tell a human the page
/// has moved since (see `src/retention.rs`).
///
/// The pin is never written back by any tool, exactly as prices are never
/// written back by `catalog-drift` — a retention label is a legal-adjacent
/// claim about a customer's data, and the one thing worse than a stale label is
/// one a network fetch invented.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RetentionPin {
    pub posture: RetentionPosture,
    /// Short human text shown beside the label, e.g. "provider retains prompts
    /// up to 30 days". Qualitative when the vendor publishes no window —
    /// inventing a number is worse than declining to state one.
    pub description: String,
    /// The provider policy page the claim was verified against.
    pub source_url: String,
    /// ISO date (`YYYY-MM-DD`) a human last read `source_url` and confirmed the
    /// posture.
    pub verified: String,
    /// SHA-256 of the normalized visible text of `source_url` as of `verified`.
    /// `retention-drift` re-fetches and compares; a mismatch means the page
    /// changed and a human must look, never that the posture flipped.
    pub source_sha256: String,
    /// This provider's slug in OpenRouter's provider directory, when one
    /// corresponds. Advisory only — it feeds the corroboration pass and can
    /// never change an exit code.
    ///
    /// Explicit rather than inferred from the provider key because the mapping
    /// is genuinely not mechanical: ZeroRouter's `google` lane is the Gemini
    /// *Developer* API, which OpenRouter calls `google-ai-studio`, while its
    /// `google-vertex` slug is a different product under a different data
    /// policy. Guessing `google` would have corroborated the wrong account.
    #[serde(default)]
    pub openrouter_slug: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ResolvedRoute {
    pub requested_model: String,
    pub candidates: Vec<TierCandidate>,
    /// What the customer is billed at. A SCHEDULE rather than one rate table,
    /// because the tier may reprice past a prompt-size threshold: admission
    /// sizes its reservation at [`RateSchedule::worst_case`] and settlement
    /// charges [`RateSchedule::at_prompt_tokens`] against the measured prompt.
    pub sell_rates: RateSchedule,
}

impl ResolvedRoute {
    /// Whether serving this route bills the CUSTOMER nothing (edge mode,
    /// stage 3: `docs/design/edge-mode-local-rung.md`).
    ///
    /// The sell-side half of the metering skip, and the half
    /// [`TierCandidate::is_free`] cannot answer. `is_free` says ZeroRouter's
    /// COST is zero; every candidate bills at its owning TIER's sell rate
    /// whichever one serves ([`TierCatalog::resolve`]), and
    /// `validate_candidate_margin` only forbids a basis ABOVE the sell rate —
    /// so a $0 basis under a $3.00 sell rate is a legal, deliberate,
    /// 100%-margin configuration. Reading candidate freeness as customer
    /// freeness would hand that tier away for nothing.
    ///
    /// Two shapes make the difference reachable rather than theoretical:
    ///
    /// - A single-candidate priced tier over a local rung — margin by
    ///   construction, exactly what the pricing model invites.
    /// - A MIXED tier (paid cloud rung + free local rung) whose cloud rung is
    ///   dropped for a missing credential. [`crate::providers::ProviderRoute`]
    ///   removes a candidate whose credential is absent from the environment,
    ///   so an unset environment variable can collapse a mixed route to an
    ///   all-free one. A skip keyed on candidates alone would turn a
    ///   deployment mistake into free paid-tier inference; this conjunct means
    ///   it can only ever turn into a cloud outage, which is what it is.
    ///
    /// Conjoining this NARROWS the skip and can never widen it. What survives
    /// is exactly the set of routes on which the metered path provably prices
    /// `cost_usd = 0` and debits nothing — so the skip is a latency change
    /// and never a billing one.
    ///
    /// Quantified over the whole schedule ([`RateSchedule::are_zero`]): a tier
    /// that gives its base rate away and reprices past a threshold sells
    /// something, and the skip must not engage on it — the skipped path writes
    /// no reservation and no ledger row, so a request past the boundary would
    /// be delivered with nothing to charge it against.
    #[must_use]
    pub fn sells_free(&self) -> bool {
        self.sell_rates.are_zero()
    }
}

#[derive(Debug, Deserialize)]
struct RawModelRates {
    input_per_mtok: Option<f64>,
    output_per_mtok: Option<f64>,
    cached_input_per_mtok: Option<f64>,
    /// Conditional rate tables, each replacing the three rates above for the
    /// whole request once the prompt reaches its `min_prompt_tokens`. Absent
    /// means the table charges one price at every size, which is what every
    /// rate table in this file meant before the key existed.
    #[serde(default)]
    conditional: Vec<RawConditionalRate>,
}

/// One `[[...rates.conditional]]` block.
///
/// `deny_unknown_fields` where [`RawModelRates`] does not have it, and the
/// asymmetry is the point. A misspelled key inside a conditional block —
/// `input_per_mtoks`, say — would otherwise deserialize to `None` and the
/// block would price that dimension at zero for every request past the
/// boundary. The required-dimension check in `validate_rate_schedule` catches
/// exactly that case, but a misspelling of the OPTIONAL cached dimension would
/// slip past it, and a rejected file is a better answer than a silently
/// cheaper one. Nothing that loads today carries one of these blocks, so this
/// cannot refuse a file that used to load.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConditionalRate {
    min_prompt_tokens: u64,
    input_per_mtok: Option<f64>,
    output_per_mtok: Option<f64>,
    cached_input_per_mtok: Option<f64>,
}

fn deserialize_rate_schedule<'de, D>(deserializer: D) -> Result<RateSchedule, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = RawModelRates::deserialize(deserializer)?;
    let base = ModelRates {
        input_per_mtok: raw.input_per_mtok,
        output_per_mtok: raw.output_per_mtok,
        cached_input_per_mtok: raw.cached_input_per_mtok,
    };
    if raw.conditional.is_empty() {
        return Ok(RateSchedule::flat(base));
    }
    Ok(RateSchedule::new(
        base,
        raw.conditional
            .into_iter()
            .map(|conditional| ConditionalRate {
                min_prompt_tokens: conditional.min_prompt_tokens,
                rates: ModelRates {
                    input_per_mtok: conditional.input_per_mtok,
                    output_per_mtok: conditional.output_per_mtok,
                    cached_input_per_mtok: conditional.cached_input_per_mtok,
                },
            })
            .collect(),
    ))
}

#[derive(Debug, Error)]
pub enum TierConfigError {
    #[error("failed to read tier configuration at {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse tier configuration at {path}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("unsupported tier configuration schema version {0}")]
    UnsupportedSchema(u32),
    #[error(
        "tier id must be a reserved 'zero/…' routing alias or a '{{vendor}}/{{model}}' pin id \
         matching one of its own candidates: {0}"
    )]
    InvalidTierId(String),
    #[error("tier {tier} has an invalid {dimension} rate")]
    InvalidRate {
        tier: String,
        dimension: &'static str,
    },
    #[error(
        "tier {tier} has an unbillable {dimension} rate {rate:e}: a rate must convert to an \
         exact billing decimal, so it may not exceed {MAX_RATE_PER_MTOK:e} USD per million \
         tokens and, if nonzero, may not round to zero"
    )]
    UnbillableRate {
        tier: String,
        dimension: &'static str,
        rate: f64,
    },
    #[error("tier {tier} contains an invalid candidate")]
    InvalidCandidate { tier: String },
    #[error("tier {tier} must define at least one candidate")]
    EmptyTier { tier: String },
    #[error("unsupported provider {provider} in tier {tier}")]
    UnsupportedProvider { tier: String, provider: String },
    #[error("duplicate concrete model id {0}")]
    DuplicateModelId(String),
    #[error(
        "model id {0} ends in a priority keyword after ':', which the model-suffix carrier \
         (design doc: 'Model-suffix carrier') would mask — rename the id"
    )]
    PrioritySuffixCollision(String),
    #[error(
        "candidate {candidate} in tier {tier} declares {field} = 0: a zero limit is not a small \
         limit, it is a broken one — omit the field to say the limit is unknown"
    )]
    ZeroModelLimit {
        tier: String,
        candidate: String,
        field: &'static str,
    },
    #[error(
        "candidate {candidate} in tier {tier} declares an empty or blank input_modalities entry — \
         omit the field to say the modalities are unknown"
    )]
    InvalidModalities { tier: String, candidate: String },
    #[error(
        "candidate {candidate} in tier {tier} is priced at $0, but provider {provider} does not \
         declare \"settlement\": \"free\": a $0 cost basis is only legal on an upstream whose \
         inventory entry states that its traffic bills nobody. On a metered upstream a zero rate \
         records no COGS against real spend, which no ledger ever notices"
    )]
    ZeroPriceWithoutFreeSettlement {
        tier: String,
        candidate: String,
        provider: String,
    },
    #[error(
        "candidate {candidate} in tier {tier} costs more than the tier sells: \
         {dimension} cost basis {basis} exceeds tier sell rate {sell}{at}"
    )]
    NegativeMargin {
        tier: String,
        candidate: String,
        dimension: &'static str,
        basis: f64,
        sell: f64,
        /// Which rate table the violation is in, rendered for the message:
        /// empty for the base table, `" above N prompt tokens"` for a
        /// conditional one. A tier can be profitable at its base rate and lose
        /// money on every long request, so a margin refusal that did not say
        /// which band it meant would send an operator to the wrong line.
        at: String,
    },
    #[error(
        "tier {tier} declares conditional rate thresholds {thresholds:?}: they must be strictly \
         ascending and above zero — a threshold of 0 redefines the base rate rather than \
         conditioning on anything, and a repeated or out-of-order one makes the file's reading \
         order disagree with its pricing order"
    )]
    InvalidConditionalThresholds { tier: String, thresholds: Vec<u64> },
    #[error(
        "tier {tier} prices cached input at {cached} against an input rate of {input}: a cache \
         read is a DISCOUNT on a fresh read, never dearer than one. A reservation prices its \
         whole prompt bound at the input rate while settlement splits the measured prompt into \
         cached and uncached parts, so a cached rate above the input rate is the one shape in \
         which a settled charge can exceed what was reserved for it"
    )]
    CachedRateAboveInputRate {
        tier: String,
        input: f64,
        cached: f64,
    },
    #[error(
        "candidate {candidate} in tier {tier} dispatches to provider {provider}, which declares no \
         retention posture: add a [retention.{provider}] block naming the posture, the policy page \
         it was verified against, and the date. An unlabelled lane is refused rather than defaulted \
         — the catalog publishes this claim to customers, and a lane whose posture nobody wrote \
         down is one nobody checked"
    )]
    UnlabelledLane {
        tier: String,
        candidate: String,
        provider: String,
    },
    #[error(
        "retention pin for {subject} leaves {field} blank: every field of a retention pin is \
         evidence for a claim about a customer's data, and a blank one asserts the claim without it"
    )]
    IncompleteRetentionPin {
        subject: String,
        field: &'static str,
    },
    #[error(
        "retention pin for {subject} records verified = {verified}: it must be an ISO calendar date \
         (YYYY-MM-DD) naming the day a human last read the policy page, because \
         `admin retention-drift` reports staleness against it"
    )]
    InvalidRetentionDate { subject: String, verified: String },
    #[error(
        "retention pin for {subject} records source_url {url}: it must be an http(s) URL that \
         `admin retention-drift` can fetch, or the claim has no re-verification loop"
    )]
    InvalidRetentionSourceUrl { subject: String, url: String },
    #[error(
        "retention pin for {subject} records source_sha256 {digest}: it must be 64 hexadecimal \
         characters — the normalized-text digest `admin retention-drift` prints for that page"
    )]
    InvalidRetentionDigest { subject: String, digest: String },
}

impl TierCatalog {
    /// Route a requested model, considering only tiers that may be served.
    ///
    /// A withheld tier is absent from `tiers`, so it resolves to `None` here
    /// exactly like an unknown id; [`TierCatalog::unavailable_for`] tells the
    /// two apart.
    #[must_use]
    pub fn resolve(&self, requested_model: &str) -> Option<ResolvedRoute> {
        if let Some(tier) = self
            .tiers
            .get(requested_model)
            .filter(|tier| !tier.candidates.is_empty())
        {
            return Some(ResolvedRoute {
                requested_model: requested_model.to_owned(),
                candidates: tier.candidates.clone(),
                sell_rates: tier.rates.clone(),
            });
        }

        self.tiers.values().find_map(|tier| {
            tier.candidates
                .iter()
                .find(|candidate| candidate.id == requested_model)
                .cloned()
                .map(|candidate| ResolvedRoute {
                    requested_model: requested_model.to_owned(),
                    // A pinned concrete candidate meters at its owning tier's
                    // sell rate, never the per-candidate cost basis, so pinning
                    // a model cannot undercut the tier price. The whole
                    // SCHEDULE is inherited, thresholds included, so a pin
                    // cannot dodge the tier's repricing either.
                    sell_rates: tier.rates.clone(),
                    candidates: vec![candidate],
                })
        })
    }

    /// The withheld tier a requested model belongs to, if any.
    ///
    /// Both the tier id and any concrete candidate pinned inside it answer
    /// here: `zero/high-end` and `anthropic/claude-sonnet-5` are the same
    /// fault and deserve the same answer. Concrete ids are unique across the
    /// entire file (a duplicate is fatal — see `validate_tier_catalog`), so
    /// this lookup can never shadow a candidate that a healthy tier serves.
    #[must_use]
    pub fn unavailable_for(&self, requested_model: &str) -> Option<&UnavailableTier> {
        self.unavailable.get(requested_model).or_else(|| {
            self.unavailable.values().find(|withheld| {
                withheld
                    .definition
                    .candidates
                    .iter()
                    .any(|candidate| candidate.id == requested_model)
            })
        })
    }

    /// Move the condemned tiers out of the servable map.
    ///
    /// Withholding rather than deleting keeps the tier addressable for
    /// diagnosis while removing it from every path that routes or advertises,
    /// which is the whole point: a model a customer cannot use must not appear
    /// in `/v1/models`, and a request for it must not read as "no such model".
    fn withhold(&mut self, reasons: BTreeMap<String, String>) {
        for (tier, reason) in reasons {
            let Some(definition) = self.tiers.remove(&tier) else {
                continue;
            };
            self.unavailable.insert(
                tier.clone(),
                UnavailableTier {
                    tier,
                    reason,
                    definition,
                },
            );
        }
    }

    /// The public `/v1/models` catalog: every candidate id, paired with who
    /// serves it and what it bills at, plus a row for any reserved `zero/*`
    /// routing alias whose id differs from its candidates'. A vendor-named pin's
    /// tier id equals its single candidate's id, so it contributes exactly one
    /// row, owned by the vendor — no duplicate ZeroRouter-branded row. A
    /// candidate's `sell_rates` is always its *owning* tier's rate, never its
    /// own cost basis — mirroring `resolve`'s pinned-candidate rule above, so a
    /// model listing can never advertise a price cheaper than what a request for
    /// that id is actually metered at.
    ///
    /// Withheld tiers are absent from `tiers` and so are their candidates:
    /// the catalog never offers a model that a request for it would refuse.
    ///
    /// `dispatchable` extends that same promise to the OTHER reason a request
    /// would be refused, and it is a required argument rather than an option
    /// because forgetting it is precisely the bug it exists to fix. Withholding
    /// answers "is this tier priced so we cannot serve it"; this answers "does
    /// this deployment hold the credential needed to serve it". Both are
    /// "a customer cannot use this", and a catalog that filters one and not the
    /// other still advertises models that fail.
    ///
    /// It was not always here, and the omission shipped. `/v1/models` was
    /// deliberately credential-blind — documented as publishing "the stable full
    /// catalog rather than changing with credential availability" — which was a
    /// defensible choice while every shipped provider's key was always present
    /// in production. The first lane deployed without its secret broke it: the
    /// storefront advertised both Bedrock zero-retention lanes, the flagship of
    /// the whole product, while every call to them returned 503. A stable
    /// catalog is worth less than a truthful one.
    ///
    /// Pass the predicate rather than reading the environment here so this stays
    /// a pure function of the catalog plus one declared fact — the same reason
    /// [`crate::drift::reconcile_with`] takes its questions as arguments.
    ///
    /// Metadata does *not* follow the sell-rate rule, and the asymmetry is
    /// deliberate. A price is a property of the tier a request is billed
    /// through, so a pinned candidate inherits its tier's. A context window is
    /// a property of the model itself: a request for
    /// `anthropic/claude-sonnet-5` reaches sonnet and gets sonnet's window
    /// whatever tier the id happens to sit under. So a candidate row carries
    /// its own metadata and a tier row carries
    /// [`ModelMetadata::narrowed`] across everything it can route to.
    #[must_use]
    pub fn model_listing(
        &self,
        dispatchable: &dyn Fn(&str) -> bool,
    ) -> BTreeMap<String, ModelListing> {
        let mut models = BTreeMap::new();
        for (tier_id, definition) in &self.tiers {
            // The rungs this deployment could actually reach. Everything below
            // is computed over THESE and not over the tier's full candidate
            // list, which matters for more than the row's existence: a routing
            // alias advertises the narrowest metadata and the weakest retention
            // posture across what it can route to, and including a rung with no
            // credential would let the published claim describe a lane no
            // request can land on.
            let reachable: Vec<TierCandidate> = definition
                .candidates
                .iter()
                .filter(|candidate| dispatchable(&candidate.provider))
                .cloned()
                .collect();
            if reachable.is_empty() {
                continue;
            }
            // A vendor-named pin's tier id equals its single candidate's id, so
            // the catalog publishes one row for it — owned by the vendor, from
            // the candidate loop below — matching OpenRouter, where `owned_by`
            // is the vendor and there is no separate ZeroRouter-branded row.
            // Only a routing alias whose id differs from every candidate (the
            // reserved `zero/*` namespace) gets its own zerorouter-owned row.
            let is_routing_alias = definition
                .candidates
                .iter()
                .all(|candidate| candidate.id != *tier_id);
            if is_routing_alias {
                let reachable_definition = TierDefinition {
                    candidates: reachable.clone(),
                    ..definition.clone()
                };
                // A load-time invariant, not a runtime possibility: the catalog
                // does not load unless every candidate resolves a posture, so
                // this cannot be `None` for a tier that is being served.
                let Some(retention) = self.tier_retention(&reachable_definition) else {
                    continue;
                };
                models.insert(
                    tier_id.clone(),
                    ModelListing {
                        owned_by: "zerorouter".to_owned(),
                        sell_rates: definition.rates.clone(),
                        metadata: ModelMetadata::narrowed(&reachable),
                        retention,
                    },
                );
            }
            for candidate in &reachable {
                let Some(retention) = self.candidate_retention(definition, candidate) else {
                    continue;
                };
                models
                    .entry(candidate.id.clone())
                    .or_insert_with(|| ModelListing {
                        owned_by: candidate.provider.clone(),
                        sell_rates: definition.rates.clone(),
                        metadata: candidate.metadata.clone(),
                        retention,
                    });
            }
        }
        models
    }

    /// The posture one candidate serves under: its tier's override if the tier
    /// declares one, otherwise the pin for the provider it dispatches to.
    ///
    /// `None` means the file never should have loaded — see
    /// [`TierConfigError::UnlabelledLane`], which is raised for exactly this
    /// condition at validation time.
    #[must_use]
    pub fn candidate_retention(
        &self,
        definition: &TierDefinition,
        candidate: &TierCandidate,
    ) -> Option<RetentionPin> {
        definition
            .retention
            .clone()
            .or_else(|| self.retention.get(&candidate.provider).cloned())
    }

    /// The posture a whole TIER may advertise: its override if it declares one,
    /// otherwise the weakest posture across every candidate a request for it
    /// could land on.
    ///
    /// Weakest rather than first, for the reason [`RetentionPosture::weaker`]
    /// gives: a customer cannot tell which rung served them, so a tier with one
    /// retaining rung retains. The *description* carried alongside is the one
    /// belonging to the rung that set the posture, so the text a customer reads
    /// always names a real arrangement rather than a blend of two.
    #[must_use]
    pub fn tier_retention(&self, definition: &TierDefinition) -> Option<RetentionPin> {
        if let Some(override_pin) = &definition.retention {
            return Some(override_pin.clone());
        }
        definition
            .candidates
            .iter()
            .map(|candidate| self.retention.get(&candidate.provider).cloned())
            .try_fold(None::<RetentionPin>, |weakest, pin| {
                let pin = pin?;
                Some(Some(match weakest {
                    None => pin,
                    Some(weakest) => {
                        if weakest.posture.weaker(pin.posture) == weakest.posture {
                            weakest
                        } else {
                            pin
                        }
                    }
                }))
            })
            .flatten()
    }
}

/// One row of the public catalog: the provider that serves this id, the sell
/// rate a request for it is billed at, and what the model can take and
/// produce.
#[derive(Clone, Debug)]
pub struct ModelListing {
    pub owned_by: String,
    /// The tier's whole sell SCHEDULE — the base rate and every band it
    /// reprices at.
    ///
    /// The schedule rather than the base table, because a price a customer
    /// cannot see is a price they cannot check: four of the ten models this
    /// catalog lists reprice at 2x, and publishing only the base rate quoted
    /// half the real price on exactly the requests where the gap is largest.
    /// [`crate::openai::ModelPricing`] renders the bands as OpenRouter's
    /// `pricing.overrides[]`, which is additive — a flat tier's JSON is
    /// unchanged.
    pub sell_rates: RateSchedule,
    pub metadata: ModelMetadata,
    /// What the upstream serving this row does with the request afterwards.
    ///
    /// Not an `Option`. Every row the catalog publishes carries a posture,
    /// because the whole point of the label is that a customer never has to
    /// wonder about a lane that omitted one — and `validate_tier_catalog`
    /// refuses to load a file in which any candidate could reach this point
    /// unlabelled.
    ///
    /// Follows the METADATA rule rather than the sell-rate rule: a candidate
    /// row carries its own provider's posture, because a request for
    /// `anthropic/claude-sonnet-5` reaches Anthropic's account whatever tier the
    /// id sits under. A routing-alias row carries the weakest posture across
    /// everything it can route to ([`RetentionPosture::weaker`]).
    pub retention: RetentionPin,
}

pub async fn load_tier_catalog(path: &Path) -> Result<TierCatalog, TierConfigError> {
    let source = tokio::fs::read_to_string(path)
        .await
        .map_err(|source| TierConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    let mut catalog: TierCatalog =
        toml::from_str(&source).map_err(|source| TierConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    let withheld = validate_tier_catalog(&catalog)?;
    report_withheld_tiers(&withheld);
    catalog.withhold(withheld);
    Ok(catalog)
}

/// The unavailability state this process has already reported.
static REPORTED_WITHHELD: OnceLock<Mutex<BTreeMap<String, String>>> = OnceLock::new();

/// Log every *change* in which tiers are withheld, and nothing else.
///
/// `load_tier_catalog` runs on every request, so an unconditional `error!`
/// here would emit one line per request for as long as a tier stays mispriced:
/// the operator's signal buried under its own repetition, at a log volume that
/// scales with customer traffic. Instead the process remembers the exact
/// (tier → reason) map it last reported and logs only the delta.
///
/// - The first load that withholds a tier logs `error!` naming the tier and
///   the full reason — candidate, dimension, cost basis, sell rate.
/// - Every load after it that finds the same state is silent.
/// - A *different* state logs again: another tier going below cost, or the
///   same tier failing on a new dimension, is a new entry.
/// - A tier returning to the catalog logs `info!`, so a fix is as visible as
///   the break and an operator can confirm the repair landed.
///
/// Keying on content rather than on a timer is what makes this safe: there is
/// no cooldown window in which a genuinely new fault goes unreported, and no
/// clock to reason about. The state is per process, so a restart re-reports
/// everything still wrong — nothing is suppressed across a deploy.
fn report_withheld_tiers(withheld: &BTreeMap<String, String>) {
    let reported = REPORTED_WITHHELD.get_or_init(|| Mutex::new(BTreeMap::new()));
    // The guarded value is replaced wholesale, so a panic elsewhere cannot
    // leave it half-updated; recover the lock rather than propagate, because
    // log bookkeeping must never be the thing that fails a request.
    let mut reported = reported.lock().unwrap_or_else(PoisonError::into_inner);
    if *reported == *withheld {
        return;
    }
    for (tier, reason) in withheld {
        if reported.get(tier) != Some(reason) {
            tracing::error!(
                tier = %tier,
                reason = %reason,
                "tier withheld from the catalog: every request it served would lose money; \
                 raise this tier's sell rates or move the candidate. Other tiers keep serving."
            );
        }
    }
    for tier in reported.keys() {
        if !withheld.contains_key(tier) {
            tracing::info!(tier = %tier, "tier restored to the catalog");
        }
    }
    reported.clone_from(withheld);
}

/// Validate a catalog, separating faults that condemn the *file* from faults
/// that condemn a single *tier*.
///
/// Two classes with deliberately different blast radii:
///
/// - **Structural** faults — an unsupported schema version, a malformed tier
///   id, a tier with no candidates, a missing or negative or non-finite rate,
///   a malformed candidate, an unsupported provider, a duplicate concrete id,
///   or a conditional rate threshold that is zero or out of order — mean the
///   file itself cannot be trusted, so they still refuse the whole catalog.
///   Serving *part* of a file that is wrong about its own structure would be
///   guessing at the operator's intent.
/// - **Economic** faults — a candidate priced above its owning tier's sell
///   rate, at ANY prompt size either side reprices at — condemn exactly one
///   tier and nothing else. They are returned here (tier id → the rendered
///   [`TierConfigError::NegativeMargin`]) instead of erroring, so the caller
///   can withhold that tier and keep serving the rest. The rule is unchanged;
///   only its blast radius is.
///
/// A candidate whose thresholds differ from its tier's is **not** a fault of
/// either kind. A rung is not obliged to reprice where its tier does — the
/// documented edge ladder puts a $0 local rung under a tiered tier, and $0
/// never reprices — so the margin rule probes both schedules at every prompt
/// size either declares rather than demanding they agree. See
/// [`validate_candidate_margin`].
///
/// Duplicate concrete ids are checked across *every* tier, healthy or not, and
/// stay fatal. They are inherently cross-tier: a repeated id makes `resolve`'s
/// answer depend on map order, so the file is genuinely ambiguous about what a
/// customer buys. Scoping the check to surviving tiers would make a structural
/// property depend on economics — a duplicate hidden behind a below-cost tier
/// would detonate at the moment an operator *fixed* that tier's pricing,
/// turning a repair into an outage. File-wide uniqueness is also the invariant
/// that lets [`TierCatalog::unavailable_for`] and
/// [`TierCatalog::model_listing`] treat a concrete id as belonging to exactly
/// one tier.
///
/// If *every* tier is condemned the catalog errors after all: there is no
/// product left, and an empty catalog that answers 404 for every model in the
/// file is a worse lie than being visibly down.
fn validate_tier_catalog(
    catalog: &TierCatalog,
) -> Result<BTreeMap<String, String>, TierConfigError> {
    if catalog.schema_version != 1 {
        return Err(TierConfigError::UnsupportedSchema(catalog.schema_version));
    }

    // Shape first, for every pin the file declares — including one no tier
    // happens to use. A malformed pin is a malformed claim, and leaving an
    // unused one unchecked would let it rot until the day a candidate points at
    // it, which is the worst possible moment to discover the date is a typo.
    for (provider, pin) in &catalog.retention {
        validate_retention_pin(&format!("provider {provider}"), pin)?;
    }

    let mut concrete_ids = BTreeSet::new();
    let mut withheld: BTreeMap<String, TierConfigError> = BTreeMap::new();

    for (tier_id, definition) in &catalog.tiers {
        if let Some(override_pin) = &definition.retention {
            validate_retention_pin(&format!("tier {tier_id}"), override_pin)?;
        }
        // A tier id is one of two shapes. A reserved `zero/*` id names a routing
        // alias (the namespace kept for future intent/routed tiers). A
        // vendor-named pin is keyed by the `{vendor}/{model}` id of one of its
        // own candidates, so the public id equals the concrete model id and
        // OpenRouter clients address it unchanged. Anything else is neither.
        let reserved_alias = tier_id.starts_with("zero/") && tier_id.len() > "zero/".len();
        let vendor_pin = definition
            .candidates
            .iter()
            .any(|candidate| candidate.id == *tier_id);
        if !reserved_alias && !vendor_pin {
            return Err(TierConfigError::InvalidTierId(tier_id.clone()));
        }
        reject_priority_suffix_collision(tier_id)?;
        if definition.candidates.is_empty() {
            return Err(TierConfigError::EmptyTier {
                tier: tier_id.clone(),
            });
        }
        validate_rate_schedule(tier_id, &definition.rates)?;

        for candidate in &definition.candidates {
            if candidate.id.trim().is_empty()
                || candidate.id.starts_with("zero/")
                || candidate.provider.trim().is_empty()
                || candidate.model.trim().is_empty()
            {
                return Err(TierConfigError::InvalidCandidate {
                    tier: tier_id.clone(),
                });
            }
            reject_priority_suffix_collision(&candidate.id)?;
            if !is_supported_provider(&candidate.provider) {
                return Err(TierConfigError::UnsupportedProvider {
                    tier: tier_id.clone(),
                    provider: candidate.provider.clone(),
                });
            }
            if !concrete_ids.insert(candidate.id.clone()) {
                return Err(TierConfigError::DuplicateModelId(candidate.id.clone()));
            }
            // STRUCTURAL, not economic: this refuses the whole file rather than
            // withholding the one tier. A withheld tier would also keep the
            // unlabelled lane off `/v1/models`, so the invariant would survive
            // either way — but the catalog would then quietly serve a partial
            // lineup because someone forgot a block, and the operator's claim is
            // that EVERY lane is labelled. A file that cannot make that claim is
            // not a file to serve half of.
            if definition.retention.is_none()
                && !catalog.retention.contains_key(&candidate.provider)
            {
                return Err(TierConfigError::UnlabelledLane {
                    tier: tier_id.clone(),
                    candidate: candidate.id.clone(),
                    provider: candidate.provider.clone(),
                });
            }
            validate_rate_schedule(tier_id, &candidate.rates)?;
            validate_metadata(tier_id, candidate)?;
            validate_zero_price(tier_id, candidate)?;
            // Margin FIRST, so a table that is both mispriced and internally
            // inverted reports the mispricing — the more specific complaint.
            // Both are economic: they withhold this tier and leave the rest
            // serving.
            if let Err(error) = validate_candidate_margin(tier_id, &definition.rates, candidate)
                .and_then(|()| validate_cache_is_a_discount(tier_id, &candidate.rates))
                .and_then(|()| validate_cache_is_a_discount(tier_id, &definition.rates))
            {
                // The first violating candidate becomes the tier's reason, but
                // the walk continues: a later candidate in the same tier can
                // still carry a structural fault, and that must condemn the
                // file no matter how the tier's economics come out.
                withheld.entry(tier_id.clone()).or_insert(error);
            }
        }
    }

    if withheld.len() == catalog.tiers.len() {
        return match withheld.into_values().next() {
            // Nothing servable is left. Fail the load with a real margin error
            // rather than hand back a catalog that 404s its own contents.
            Some(error) => Err(error),
            // A catalog with no tiers at all withholds nothing.
            None => Ok(BTreeMap::new()),
        };
    }

    Ok(withheld
        .into_iter()
        .map(|(tier, error)| (tier, error.to_string()))
        .collect())
}

/// Reject a tier or candidate id whose final `:`-delimited segment is a
/// priority keyword.
///
/// The model-suffix carrier (`zero/balanced:cost`, design doc: "Model-suffix
/// carrier") strips a trailing `:keyword` only after resolving the untouched
/// string fails, so an id that itself ends in `:cost` would still resolve —
/// but a request for `that-id:cost` meaning "that id, cost priority" would
/// resolve to the literal id instead and silently drop the customer's
/// priority. Resolve-first keeps a hypothetical colliding id *serving*; this
/// rule keeps the collision from ever being introduced. Today no shipped id
/// contains a colon at all, but that is a data-file convention — Bedrock
/// ARN-style ids (`arn:aws:bedrock:...`) are one plausible future counter-
/// example, and they pass this rule because their final segment is not a
/// priority keyword.
///
/// Structural, like [`TierConfigError::InvalidTierId`]: it condemns the file,
/// not just the tier, because a colliding id is an authoring error with a
/// one-line fix, never an economics verdict to route around.
fn reject_priority_suffix_collision(id: &str) -> Result<(), TierConfigError> {
    let colliding = id
        .rsplit_once(':')
        .is_some_and(|(_, keyword)| Priority::from_keyword(keyword).is_some());
    if colliding {
        return Err(TierConfigError::PrioritySuffixCollision(id.to_owned()));
    }
    Ok(())
}

/// Reject a candidate that costs more than its owning tier sells for.
///
/// The tier tables encode a deliberate pricing model: a tier's flagship
/// candidate is priced *at* the tier sell rate (0% markup) and margin comes
/// from routing inside the tier to cheaper candidates. Every candidate bills
/// at `sell_rates` regardless of which one serves (see [`TierCatalog::resolve`]
/// and [`TierCatalog::model_listing`]), so a candidate whose basis exceeds the
/// tier rate on any dimension loses money on *every* request it serves — a
/// silent margin leak that no amount of downstream routing policy can repair.
/// Catching it here turns that leak into a refusal to serve.
///
/// The rule is absolute; its *consequence* is scoped. `validate_tier_catalog`
/// treats this error as a verdict on one tier, which is withheld while every
/// other tier keeps serving — a mispriced tier is a pricing bug in that tier,
/// not evidence that the rest of the file is wrong.
///
/// Two edges the rule must respect:
///
/// - `basis == sell` is **legal**. That is the intended flagship shape, so
///   only a strictly greater basis is a violation.
/// - A dimension either side leaves unset is skipped, not read as zero. An
///   absent rate is "unknown here", and treating it as free (or as a
///   violation) would both be wrong.
///
/// # Why the cached dimension is compared on EFFECTIVE rates
///
/// The second edge above used to be applied to the cached dimension as
/// DECLARED, and that was a hole, because an absent cached rate is not
/// unknown to the biller: [`crate::openai::usage_cost`] prices it at the
/// INPUT rate. Skipping the comparison whenever either side omitted it
/// therefore skipped a comparison that settlement would go on to make real,
/// and two negative-margin shapes loaded happily:
///
/// - **Candidate declares a cached rate, the tier does not.** Basis 5.00
///   against a tier whose cached traffic sells at its 3.00 input rate: the
///   customer pays 3.00 for cached tokens that cost 5.00, on every request
///   that reports any.
/// - **The tier declares one, the candidate does not.** A tier selling cached
///   input at a 0.60 discount, served by a candidate with a 3.00 input rate
///   and no cached rate of its own — so the discount is real for the customer
///   and imaginary for ZeroRouter.
///
/// Both are exactly the silent margin leak this function exists to catch, and
/// both are invisible to a declared-value comparison. Reading each side
/// through [`ModelRates::effective_cached_input_per_mtok`] — the same fallback
/// `usage_cost` applies — closes it. "Unknown" still skips: the fallback
/// resolves to `None` only when the input rate is absent too, which
/// `validate_rates` has already refused before this runs.
/// # Why the rule is evaluated at prompt SIZES rather than band by band
///
/// A conditional rate table reprices the whole request past a threshold, so a
/// candidate and its tier each hold several rate tables rather than one. The
/// margin rule is a claim about what a request costs against what it sells
/// for, and a request lands in exactly one band on each side — so the rule has
/// to hold at every prompt size, or it holds only for short requests. A
/// candidate whose 272k basis is 0.40 under a tier still selling 0.20 up there
/// loses money on every long request while its base row looks perfect.
///
/// The obvious implementation — zip the two `conditional` lists positionally —
/// needs the two sides to declare the SAME thresholds, and demanding that is
/// what this function used to do (via a structural check that refused the
/// whole file on a mismatch). That was wrong, and expensively so. A tier's
/// rungs are not required to reprice where the tier does: the documented edge
/// ladder (`docs/edge-quickstart.md`, `zero/burst`) puts a $0 local rung beside
/// a hosted rung, and a $0 rung has no threshold because it never reprices.
/// Requiring one refused the entire catalog on an edge box — a total outage
/// produced by a configuration that is economically impeccable, since $0 is
/// under the sell rate in every band.
///
/// So the comparison is made by PROBING both schedules at a set of prompt
/// sizes, with no alignment required:
///
/// ```text
/// for size in {0} ∪ basis thresholds ∪ sell thresholds:
///     basis.at_prompt_tokens(size) ≤ sell.at_prompt_tokens(size)
/// ```
///
/// That set is sufficient, not a sample. Both schedules are step functions of
/// prompt size, so each is constant between consecutive thresholds; the UNION
/// of their thresholds therefore partitions every prompt size into intervals on
/// which BOTH sides are constant, and the probe set is exactly one point from
/// each interval (its left end, which is a threshold, or 0 for the first). Any
/// prompt size not probed has the same (basis, sell) pair as one that was.
///
/// # What this deliberately does NOT refuse
///
/// A tier that reprices at 200,000 while its candidate reprices at 272,000
/// sells the band between them at the high rate while paying the low one. That
/// is a MARKUP, not a negative margin, so this rule is silent about it by
/// construction — it only ever forbids a basis above a sell rate, and it always
/// has. Markup on a pass-through pin is caught elsewhere and deliberately:
/// `tests/http.rs` requires every shipped pass-through pin's candidate schedule
/// to equal its tier's outright, and `admin catalog-drift` reports an
/// undisclosed markup against the upstream's real numbers.
fn validate_candidate_margin(
    tier: &str,
    sell_rates: &RateSchedule,
    candidate: &TierCandidate,
) -> Result<(), TierConfigError> {
    // 0 is the base band; every threshold either side declares is a point
    // where one of them changes price. `BTreeSet` dedupes a threshold the two
    // share and keeps the probes ascending, so the first violation reported is
    // the one at the smallest prompt size.
    let mut probes: BTreeSet<u64> = BTreeSet::from([0]);
    probes.extend(candidate.rates.thresholds());
    probes.extend(sell_rates.thresholds());

    let bands = probes.into_iter().map(|probe| {
        let at = if probe == 0 {
            String::new()
        } else {
            format!(" above {probe} prompt tokens")
        };
        (
            at,
            candidate.rates.at_prompt_tokens(probe),
            sell_rates.at_prompt_tokens(probe),
        )
    });

    for (at, basis_rates, sell_rates) in bands {
        for (dimension, basis, sell) in [
            (
                "input_per_mtok",
                basis_rates.input_per_mtok,
                sell_rates.input_per_mtok,
            ),
            (
                "output_per_mtok",
                basis_rates.output_per_mtok,
                sell_rates.output_per_mtok,
            ),
            (
                "cached_input_per_mtok",
                basis_rates.effective_cached_input_per_mtok(),
                sell_rates.effective_cached_input_per_mtok(),
            ),
        ] {
            let (Some(basis), Some(sell)) = (basis, sell) else {
                continue;
            };
            if basis > sell {
                return Err(TierConfigError::NegativeMargin {
                    tier: tier.to_owned(),
                    candidate: candidate.id.clone(),
                    dimension,
                    basis,
                    sell,
                    at,
                });
            }
        }
    }
    Ok(())
}

/// Reject a $0 candidate whose provider has not declared that it settles free,
/// on the *structural* side of the split described on [`validate_tier_catalog`].
///
/// A $0 basis claims "serving this candidate costs ZeroRouter nothing". On an
/// upstream that bills — a cloud vendor, or a hosted ZeroRouter taking metered
/// burst traffic — that claim is one of two things, and the rate table cannot
/// tell them apart: a fat-fingered rate, which records no COGS against real
/// spend and reports a healthy margin until the invoice arrives, or a
/// deliberate attempt to file a paid model under the free rung. Both are
/// refused, whole-file, and the operator's fix is one line either way.
///
/// This is the second half of the design's free-lane rule — *"no paid model may
/// be reachable through the free lane"* — landed a stage early and on purpose:
/// stage 3's metering skip keys on [`TierCandidate::is_free`], and that skip is
/// far easier to reason about when a $0 price alone can never produce it.
///
/// Note what the rule does and does not achieve. It cannot verify that a
/// free-declaring upstream really is free; nothing can, short of the invoice.
/// What it enforces is that reaching the free lane takes two deliberate
/// statements in two files — the provider's `"settlement": "free"` and this
/// candidate's zero price — so it is never reached by accident.
///
/// It is whole-file rather than tier-scoped for the same reason an unsupported
/// provider is: the file is wrong about the world, not merely mispriced, and
/// withholding one tier is precisely how it would go unnoticed.
///
/// # Why the REQUIRED dimensions alone are enough to refuse
///
/// This used to ask [`TierCandidate::rates_are_zero`] — "is this candidate
/// free?" — and that let a shape through that it should never have:
/// `{input: 0.00, output: 0.00, cached_input: 5.00}` on a cloud provider. It
/// is not free (the declared cached rate is real money), so the free-lane
/// question answers no and the rule stayed silent; but it still asserts that
/// the input and output tokens EVERY request generates cost ZeroRouter
/// nothing on an upstream that bills for them. That is the fat-fingered-rate
/// signature this function exists to catch, and letting it load records ~0
/// COGS against real spend and reports a healthy margin until the invoice
/// arrives.
///
/// So the question asked here is the weaker, more suspicious one —
/// [`ModelRates::required_rates_are_zero`] — while [`TierCandidate::is_free`]
/// keeps asking the stronger one. The two are deliberately different: what
/// makes a claim REFUSABLE is not the same as what makes a route FREE, and
/// conflating them is how the hole got in.
///
/// # Why ANY table in the schedule is enough to refuse
///
/// The question is asked of every rate table a schedule holds
/// ([`RateSchedule::any_required_rates_are_zero`]), not of the base one. A
/// conditional table reading `{input: 0.00, output: 0.00}` past 272,000 tokens
/// makes the identical claim about the identical traffic — every request in
/// that band generates input and output tokens — and on a metered upstream it
/// records no COGS against real spend for exactly the requests that cost the
/// most. On a flat schedule this is the base table's answer and nothing else,
/// so the rule is unchanged for every catalog written before conditional rates
/// existed.
fn validate_zero_price(tier: &str, candidate: &TierCandidate) -> Result<(), TierConfigError> {
    if candidate.rates.any_required_rates_are_zero() && !provider_settles_free(&candidate.provider)
    {
        return Err(TierConfigError::ZeroPriceWithoutFreeSettlement {
            tier: tier.to_owned(),
            candidate: candidate.id.clone(),
            provider: candidate.provider.clone(),
        });
    }
    Ok(())
}

/// Reject a retention pin that is present but unusable as evidence.
///
/// Every check here is about the *claim being checkable*, not about whether it
/// is true — nothing in this process can know that. A blank description labels
/// a lane with nothing; an unfetchable `source_url` or a malformed digest
/// silently removes that provider from `admin retention-drift`'s re-verification
/// loop, so the label would keep asserting itself with no mechanism left to
/// notice the page had changed. Those are the failures that look fine forever.
fn validate_retention_pin(subject: &str, pin: &RetentionPin) -> Result<(), TierConfigError> {
    for (field, value) in [
        ("description", &pin.description),
        ("source_url", &pin.source_url),
        ("verified", &pin.verified),
        ("source_sha256", &pin.source_sha256),
    ] {
        if value.trim().is_empty() {
            return Err(TierConfigError::IncompleteRetentionPin {
                subject: subject.to_owned(),
                field,
            });
        }
    }
    if chrono::NaiveDate::parse_from_str(pin.verified.trim(), "%Y-%m-%d").is_err() {
        return Err(TierConfigError::InvalidRetentionDate {
            subject: subject.to_owned(),
            verified: pin.verified.clone(),
        });
    }
    let url = pin.source_url.trim();
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(TierConfigError::InvalidRetentionSourceUrl {
            subject: subject.to_owned(),
            url: pin.source_url.clone(),
        });
    }
    let digest = pin.source_sha256.trim();
    if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(TierConfigError::InvalidRetentionDigest {
            subject: subject.to_owned(),
            digest: pin.source_sha256.clone(),
        });
    }
    Ok(())
}

/// Reject metadata that is present but says nothing, on the *structural* side
/// of the split described on [`validate_tier_catalog`].
///
/// Absence is always legal — an undeclared field means "unknown", which is a
/// true statement about a model no one has described. What this refuses is a
/// field that was written down and still carries no information: a zero
/// context window, a zero max output, an empty or blank modality list. Each of
/// those is an authoring accident that a consumer cannot tell apart from a
/// deliberate claim, and each has a one-character fix — delete the line — so
/// there is no reason to route around it. Refusing the file, like an
/// unsupported provider does, keeps the difference between "unknown" and
/// "zero" meaning something.
fn validate_metadata(tier: &str, candidate: &TierCandidate) -> Result<(), TierConfigError> {
    for (field, limit) in [
        ("context_window", candidate.metadata.context_window),
        ("max_output_tokens", candidate.metadata.max_output_tokens),
    ] {
        if limit == Some(0) {
            return Err(TierConfigError::ZeroModelLimit {
                tier: tier.to_owned(),
                candidate: candidate.id.clone(),
                field,
            });
        }
    }
    let modalities_say_nothing =
        candidate
            .metadata
            .input_modalities
            .as_ref()
            .is_some_and(|modalities| {
                modalities.is_empty() || modalities.iter().any(|m| m.trim().is_empty())
            });
    if modalities_say_nothing {
        return Err(TierConfigError::InvalidModalities {
            tier: tier.to_owned(),
            candidate: candidate.id.clone(),
        });
    }
    Ok(())
}

/// Reject rates the biller cannot represent, on the *structural* side of the
/// split described on [`validate_tier_catalog`].
///
/// A rate that no `Decimal` can hold is not an economic claim that happens to be
/// unprofitable — it is a number the file asserts is a price and that the code
/// cannot read as money at all. Three things follow, and each of them is a
/// reason this refuses the file rather than withholding one tier the way
/// [`validate_candidate_margin`] does:
///
/// - The margin rule itself becomes meaningless. `validate_candidate_margin`
///   compares raw `f64`s; if either side is unrepresentable, the verdict it
///   produces is about numbers the biller will never see, so there is no sound
///   basis on which to decide *which* tier to withhold.
/// - It is not tier-local evidence. A rate of `1e100` is a typo or a unit error,
///   and a file that contains one has no claim on being trusted about the rates
///   it got right — exactly the reasoning that already makes a malformed tier id
///   or an unsupported provider fatal.
/// - Withholding would hide it. A withheld tier is a running product minus one
///   model; an operator who fat-fingered a rate would see one tier quietly
///   vanish and keep serving the rest, which is precisely the silent-margin
///   failure this check exists to prevent.
///
/// So this returns `Err` and `validate_tier_catalog` propagates it, refusing the
/// load. `load_tier_catalog` runs per request, so the refusal surfaces as
/// `TierCatalogUnavailable` on every request until the file is fixed — loud, and
/// with nothing mispriced served in the meantime.
fn validate_rates(tier: &str, rates: ModelRates) -> Result<(), TierConfigError> {
    validate_rate(tier, "input_per_mtok", rates.input_per_mtok, true)?;
    validate_rate(tier, "output_per_mtok", rates.output_per_mtok, true)?;
    validate_rate(
        tier,
        "cached_input_per_mtok",
        rates.cached_input_per_mtok,
        false,
    )
}

/// Reject a schedule that prices a cache read above a fresh read.
///
/// A cached-input rate is a discount on the input rate — that is what the
/// dimension means on every upstream this catalog carries, and it is why
/// [`crate::openai::usage_cost`] falls an absent cached rate back to the input
/// rate rather than to zero. A table that inverts them is a transposition, and
/// no vendor sells one.
///
/// # Why this is enforced rather than merely expected
///
/// It is the precondition that makes a worst-case reservation SUFFICIENT.
/// Admission has no cache information — the reservation prices its entire
/// prompt bound at the input rate ([`crate::openai::usage_cost`] with no
/// `prompt_tokens_details`) — while settlement splits the measured prompt into
/// a cached part and an uncached remainder and prices them separately. So for
/// a reservation to cover every possible outcome, the cached rate must not
/// exceed the input rate; otherwise a request that turns out to be almost
/// entirely cache hits settles ABOVE what was held for it, and no amount of
/// worst-casing over the BANDS repairs it, because the gap is inside a single
/// band.
///
/// Every schedule shipped today satisfies this, and the reservation invariant
/// was checked against them one by one. This turns that arithmetic from an
/// observation about today's ten rate tables into a property of every rate
/// table that can load.
///
/// # Why ECONOMIC rather than structural
///
/// It withholds the one tier rather than refusing the file. A withheld tier
/// serves nothing, so it cannot under-reserve anything — the invariant is kept
/// by the tier not running, which is the smallest blast radius that keeps it.
/// Refusing the whole catalog would take a working product down over one
/// mistyped rate, the same over-severity that made a threshold-alignment rule
/// unshippable (see [`validate_candidate_margin`]), and on an edge box that is
/// an outage rather than a correction.
///
/// It is also checked AFTER [`validate_candidate_margin`], so a table that
/// violates both keeps reporting the margin — the more specific complaint
/// about the relationship between two rate tables, rather than this one about
/// a single table's internal shape.
fn validate_cache_is_a_discount(
    tier: &str,
    schedule: &RateSchedule,
) -> Result<(), TierConfigError> {
    // Every band, because a band is a rate table like any other and settlement
    // will split a cached prompt inside whichever one applies.
    for rates in std::iter::once(schedule.base()).chain(
        schedule
            .conditional()
            .iter()
            .map(|conditional| conditional.rates),
    ) {
        // Compared as DECLARED, not effective: the fallback makes an absent
        // cached rate equal to the input rate, which trivially satisfies this
        // and is the honest reading of "not priced separately".
        let (Some(input), Some(cached)) = (rates.input_per_mtok, rates.cached_input_per_mtok)
        else {
            continue;
        };
        if cached > input {
            return Err(TierConfigError::CachedRateAboveInputRate {
                tier: tier.to_owned(),
                input,
                cached,
            });
        }
    }
    Ok(())
}

/// [`validate_rates`] over every table a schedule holds, plus the thresholds
/// that separate them.
///
/// Each conditional table is held to the SAME rules as the base one, including
/// that input and output are required. An omitted dimension is not inherited
/// from the table below: a conditional table REPLACES the base one wholesale
/// (see [`RateSchedule`]), so `usage_cost` would price the missing dimension
/// at zero and the customer would be charged nothing for it on precisely the
/// largest requests. Requiring it means the file has to state the whole price
/// at every size, which is what the vendor quotes anyway. The cached dimension
/// stays optional and keeps falling back to that same table's input rate —
/// the one convention `usage_cost` already applies.
///
/// The thresholds must be strictly ascending and above zero. Zero is refused
/// because a table conditioned on nothing is a second base table and the file
/// would state two answers to the same question; repeats and out-of-order
/// entries are refused so the file's reading order is its pricing order.
/// [`RateSchedule::at_prompt_tokens`] does not depend on either property, so
/// this is about the file staying legible to the human who has to check it
/// against a vendor's page.
fn validate_rate_schedule(tier: &str, schedule: &RateSchedule) -> Result<(), TierConfigError> {
    validate_rates(tier, schedule.base())?;
    let thresholds: Vec<u64> = schedule.thresholds().collect();
    let ascending = thresholds
        .first()
        .is_none_or(|first| *first > 0 && thresholds.windows(2).all(|pair| pair[0] < pair[1]));
    if !ascending {
        return Err(TierConfigError::InvalidConditionalThresholds {
            tier: tier.to_owned(),
            thresholds,
        });
    }
    for conditional in schedule.conditional() {
        validate_rates(tier, conditional.rates)?;
    }
    Ok(())
}

fn validate_rate(
    tier: &str,
    dimension: &'static str,
    rate: Option<f64>,
    required: bool,
) -> Result<(), TierConfigError> {
    if (required && rate.is_none()) || rate.is_some_and(|value| !value.is_finite() || value < 0.0) {
        return Err(TierConfigError::InvalidRate {
            tier: tier.to_owned(),
            dimension,
        });
    }
    // The representability gate is [`billable_rate`] itself rather than a
    // predicate written to match it, so the catalog accepts exactly the rates
    // `usage_cost` can price — no second definition to drift.
    if let Some(rate) = rate.filter(|value| billable_rate(*value).is_none()) {
        return Err(TierConfigError::UnbillableRate {
            tier: tier.to_owned(),
            dimension,
            rate,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Conditional rates in the file: what parses, and what the loader refuses.
    // -----------------------------------------------------------------------

    /// A catalog holding the pin under test — selling at `sell`, costing
    /// `basis`, each written as the body of a `rates` table so a test can put
    /// whatever conditional blocks it likes on either side — beside one
    /// permanently healthy pin.
    ///
    /// The healthy neighbour is not decoration. A catalog with nothing servable
    /// left errors outright rather than withholding (`validate_tier_catalog`),
    /// so without it every economic fault here would surface as a whole-file
    /// refusal and the tests could not tell the two blast radii apart — which
    /// is the distinction half of them exist to pin.
    fn conditional_catalog(sell: &str, basis: &str) -> Result<TierCatalog, TierConfigError> {
        let source = format!(
            r#"
schema_version = 1
[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.google]
posture = "standard"
description = "fixture"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
[retention."local-llama"]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local-llama"
verified = "2026-08-20"
source_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
[retention.local]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local"
verified = "2026-08-20"
source_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
[tiers."openai/pin"]
[tiers."openai/pin".rates]
{sell}
[[tiers."openai/pin".candidates]]
id = "openai/pin"
provider = "openai"
model = "pin"
[tiers."openai/pin".candidates.rates]
{basis}

[tiers."openai/healthy"]
[tiers."openai/healthy".rates]
input_per_mtok = 1.0
output_per_mtok = 2.0
[[tiers."openai/healthy".candidates]]
id = "openai/healthy"
provider = "openai"
model = "healthy"
[tiers."openai/healthy".candidates.rates]
input_per_mtok = 1.0
output_per_mtok = 2.0
"#
        );
        let catalog: TierCatalog =
            toml::from_str(&source).map_err(|source| TierConfigError::Parse {
                path: PathBuf::from("<test>"),
                source,
            })?;
        validate_tier_catalog(&catalog).map(|withheld| {
            let mut catalog = catalog;
            catalog.withhold(withheld);
            catalog
        })
    }

    /// The rate table `openai/gpt-5.6-luna` ships with, sell and basis alike.
    const LUNA_RATES: &str = r#"
input_per_mtok = 0.20
cached_input_per_mtok = 0.02
output_per_mtok = 1.20
[[tiers."openai/pin".rates.conditional]]
min_prompt_tokens = 272000
input_per_mtok = 0.40
cached_input_per_mtok = 0.04
output_per_mtok = 1.80
"#;

    /// The same, spelled for the candidate's own `rates` table.
    const LUNA_BASIS: &str = r#"
input_per_mtok = 0.20
cached_input_per_mtok = 0.02
output_per_mtok = 1.20
[[tiers."openai/pin".candidates.rates.conditional]]
min_prompt_tokens = 272000
input_per_mtok = 0.40
cached_input_per_mtok = 0.04
output_per_mtok = 1.80
"#;

    #[test]
    fn a_conditional_table_parses_into_the_band_it_describes() {
        let catalog =
            conditional_catalog(LUNA_RATES, LUNA_BASIS).expect("a matched pin should load");
        let route = catalog.resolve("openai/pin").expect("the pin resolves");
        assert_eq!(
            route.sell_rates.at_prompt_tokens(271_999).input_per_mtok,
            Some(0.20)
        );
        assert_eq!(
            route.sell_rates.at_prompt_tokens(272_000).input_per_mtok,
            Some(0.40)
        );
        assert_eq!(route.sell_rates.worst_case().output_per_mtok, Some(1.80));
        assert_eq!(
            route.candidates[0].rates.at_prompt_tokens(272_000),
            route.sell_rates.at_prompt_tokens(272_000),
            "a pass-through pin costs what it sells for in the high band too"
        );
    }

    #[test]
    fn a_rates_table_with_no_conditional_block_stays_a_flat_schedule() {
        // The backwards-compatibility claim at the file level: the shape every
        // rate table in this catalog has today must keep meaning exactly what
        // it meant, which is "one price at every size".
        let catalog = conditional_catalog(
            "input_per_mtok = 1.0\noutput_per_mtok = 2.0",
            "input_per_mtok = 1.0\noutput_per_mtok = 2.0",
        )
        .expect("a flat pin should load");
        let route = catalog.resolve("openai/pin").expect("the pin resolves");
        assert!(route.sell_rates.is_flat());
        assert!(route.candidates[0].rates.is_flat());
        assert_eq!(route.sell_rates.conditional(), &[]);
    }

    #[test]
    fn a_flat_rung_under_a_tiered_tier_loads() {
        // The shape of the documented edge ladder (`docs/edge-quickstart.md`,
        // `zero/burst`): a cheap rung that charges one price at every size,
        // beside a rung that mirrors the tier's own repricing. Give the tier
        // and its expensive rung `openai/gpt-5.6-luna`'s real band and the
        // cheap rung STILL HAS NONE — it does not reprice, so it has no
        // threshold to declare.
        //
        // This must LOAD. Requiring both sides to declare the same thresholds
        // refuses the whole catalog here, and on an edge box that is a total
        // outage produced by an economically harmless configuration: the cheap
        // rung is under the sell rate in every band, which is the only question
        // the margin rule actually asks. The real ladder's cheap rung is priced
        // at $0 — the extreme of the same shape, and pinned end to end against
        // a real free-settling provider in `tests/local_candidates.rs`.
        let source = r#"
schema_version = 1
[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.google]
posture = "standard"
description = "fixture"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
[retention."local-llama"]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local-llama"
verified = "2026-08-20"
source_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
[retention.local]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local"
verified = "2026-08-20"
source_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
[tiers."zero/burst"]
[tiers."zero/burst".rates]
input_per_mtok = 0.20
cached_input_per_mtok = 0.02
output_per_mtok = 1.20
[[tiers."zero/burst".rates.conditional]]
min_prompt_tokens = 272000
input_per_mtok = 0.40
cached_input_per_mtok = 0.04
output_per_mtok = 1.80

[[tiers."zero/burst".candidates]]
id = "anthropic/burst-cheap"
provider = "anthropic"
model = "cheap"
[tiers."zero/burst".candidates.rates]
input_per_mtok = 0.05
cached_input_per_mtok = 0.005
output_per_mtok = 0.30

[[tiers."zero/burst".candidates]]
id = "openai/burst-hosted"
provider = "openai"
model = "gpt-5.6-luna"
[tiers."zero/burst".candidates.rates]
input_per_mtok = 0.20
cached_input_per_mtok = 0.02
output_per_mtok = 1.20
[[tiers."zero/burst".candidates.rates.conditional]]
min_prompt_tokens = 272000
input_per_mtok = 0.40
cached_input_per_mtok = 0.04
output_per_mtok = 1.80
"#;
        let catalog: TierCatalog = toml::from_str(source).expect("the ladder should parse");
        let withheld =
            validate_tier_catalog(&catalog).expect("a flat rung beside a tiered rung is legal");
        assert!(
            withheld.is_empty(),
            "the ladder must serve, not be withheld: {withheld:?}"
        );
    }

    #[test]
    fn a_flat_rung_dearer_than_the_tiers_base_band_is_still_withheld() {
        // The union check must not become permissive in the process of
        // dropping alignment. A rung with no bands of its own is compared
        // against whatever the tier charges at EVERY probe point, so one that
        // is cheap above the boundary and too dear below it is still caught —
        // at the base band, where the violation actually is.
        let source = r#"
schema_version = 1
[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.google]
posture = "standard"
description = "fixture"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
[retention."local-llama"]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local-llama"
verified = "2026-08-20"
source_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
[retention.local]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local"
verified = "2026-08-20"
source_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
[tiers."zero/burst"]
[tiers."zero/burst".rates]
input_per_mtok = 0.20
output_per_mtok = 1.20
[[tiers."zero/burst".rates.conditional]]
min_prompt_tokens = 272000
input_per_mtok = 9.00
output_per_mtok = 9.00

[[tiers."zero/burst".candidates]]
id = "anthropic/burst-cheap"
provider = "anthropic"
model = "cheap"
[tiers."zero/burst".candidates.rates]
input_per_mtok = 0.50
output_per_mtok = 1.20

[tiers."openai/healthy"]
[tiers."openai/healthy".rates]
input_per_mtok = 1.0
output_per_mtok = 2.0
[[tiers."openai/healthy".candidates]]
id = "openai/healthy"
provider = "openai"
model = "healthy"
[tiers."openai/healthy".candidates.rates]
input_per_mtok = 1.0
output_per_mtok = 2.0
"#;
        let catalog: TierCatalog = toml::from_str(source).expect("the ladder should parse");
        let withheld = validate_tier_catalog(&catalog)
            .expect("an economic fault withholds one tier, it does not refuse the file");
        assert_eq!(
            withheld.keys().collect::<Vec<_>>(),
            vec!["zero/burst"],
            "exactly the mispriced tier is withheld and the rest keep serving"
        );
        assert!(
            withheld["zero/burst"].contains("cost basis 0.5 exceeds tier sell rate 0.2"),
            "{}",
            withheld["zero/burst"]
        );
    }

    #[test]
    fn a_candidate_dearer_than_its_tier_in_a_conditional_band_alone_is_withheld() {
        // The margin rule, applied where the old one could not see. Both sides
        // agree perfectly at the base rate — this file looks correct to a
        // check that only reads the base table — and the candidate costs more
        // than it sells for on every request past 272,000 tokens.
        let leaky_basis = LUNA_BASIS.replace("input_per_mtok = 0.40", "input_per_mtok = 0.50");
        let catalog = conditional_catalog(LUNA_RATES, &leaky_basis)
            .expect("one mispriced tier is withheld, not fatal");
        assert!(
            catalog.resolve("openai/pin").is_none(),
            "a tier that loses money above the boundary must not serve"
        );
        let reason = &catalog
            .unavailable_for("openai/pin")
            .expect("the withheld tier explains itself")
            .reason;
        assert!(
            reason.contains("input_per_mtok")
                && reason.contains("0.5")
                && reason.contains("above 272000 prompt tokens"),
            "the refusal must name the band an operator has to go and fix: {reason}"
        );
    }

    #[test]
    fn a_margin_violation_in_the_base_band_still_reads_as_it_always_did() {
        // The pre-existing message shape is load-bearing — `tests/http.rs`
        // matches on it — so a base-band violation must not acquire a band
        // suffix it never had.
        let catalog = conditional_catalog(
            "input_per_mtok = 1.0\noutput_per_mtok = 2.0",
            "input_per_mtok = 3.0\noutput_per_mtok = 2.0",
        )
        .expect("one mispriced tier is withheld, not fatal");
        let reason = &catalog
            .unavailable_for("openai/pin")
            .expect("the withheld tier explains itself")
            .reason;
        assert!(
            reason.ends_with("cost basis 3 exceeds tier sell rate 1"),
            "a base-band refusal reads exactly as before: {reason}"
        );
    }

    #[test]
    fn a_basis_that_reprices_earlier_than_its_tier_loses_money_and_is_withheld() {
        // Thresholds that disagree are not refused — a rung is not obliged to
        // reprice where its tier does — but the LOSING direction is still
        // caught, and it is caught at the prompt size where the loss starts.
        // Basis reprices at 200,000 while the tier holds its low rate until
        // 272,000: every request in between costs 0.40 and sells for 0.20.
        let basis_at_200k =
            LUNA_BASIS.replace("min_prompt_tokens = 272000", "min_prompt_tokens = 200000");
        let catalog = conditional_catalog(LUNA_RATES, &basis_at_200k)
            .expect("an economic fault withholds one tier, it does not refuse the file");
        let reason = &catalog
            .unavailable_for("openai/pin")
            .expect("the withheld tier explains itself")
            .reason;
        assert!(
            reason.contains("above 200000 prompt tokens")
                && reason.contains("cost basis 0.4 exceeds tier sell rate 0.2"),
            "the refusal must point at the prompt size where the loss begins: {reason}"
        );
        // ...and the healthy neighbour is untouched.
        assert!(catalog.resolve("openai/healthy").is_some());
    }

    #[test]
    fn a_sell_band_that_discounts_below_a_flat_basis_is_withheld() {
        // The probe set must include the SELL side's thresholds, not just the
        // basis side's, and this is the shape that proves it. A conditional
        // band is not obliged to raise the price: a tier may discount long
        // requests. Here the tier drops to 0.10 above 272,000 tokens while its
        // rung costs a flat 0.20, so every long request loses money — and the
        // only prompt size that reveals it is a threshold NEITHER the basis
        // declares nor the base band covers.
        let source = r#"
schema_version = 1
[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.google]
posture = "standard"
description = "fixture"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
[retention."local-llama"]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local-llama"
verified = "2026-08-20"
source_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
[retention.local]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local"
verified = "2026-08-20"
source_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
[tiers."openai/discounted"]
[tiers."openai/discounted".rates]
input_per_mtok = 0.40
output_per_mtok = 2.00
[[tiers."openai/discounted".rates.conditional]]
min_prompt_tokens = 272000
input_per_mtok = 0.10
output_per_mtok = 2.00
[[tiers."openai/discounted".candidates]]
id = "openai/discounted"
provider = "openai"
model = "discounted"
[tiers."openai/discounted".candidates.rates]
input_per_mtok = 0.20
output_per_mtok = 2.00

[tiers."openai/healthy"]
[tiers."openai/healthy".rates]
input_per_mtok = 1.0
output_per_mtok = 2.0
[[tiers."openai/healthy".candidates]]
id = "openai/healthy"
provider = "openai"
model = "healthy"
[tiers."openai/healthy".candidates.rates]
input_per_mtok = 1.0
output_per_mtok = 2.0
"#;
        let catalog: TierCatalog = toml::from_str(source).expect("the catalog should parse");
        let withheld = validate_tier_catalog(&catalog)
            .expect("an economic fault withholds one tier, it does not refuse the file");
        assert_eq!(
            withheld.keys().collect::<Vec<_>>(),
            vec!["openai/discounted"],
            "a discount the rung cannot match must withhold that tier"
        );
        assert!(
            withheld["openai/discounted"].contains("above 272000 prompt tokens")
                && withheld["openai/discounted"]
                    .contains("cost basis 0.2 exceeds tier sell rate 0.1"),
            "{}",
            withheld["openai/discounted"]
        );
    }

    #[test]
    fn a_tier_that_reprices_earlier_than_its_basis_is_a_markup_this_rule_ignores() {
        // The mirror image, and it must LOAD. Between 200,000 and 272,000 the
        // customer pays 0.40 for what costs 0.20 — a markup, not a negative
        // margin, and this rule has only ever forbidden a basis ABOVE a sell
        // rate. Refusing it here would also refuse the legitimate edge ladder,
        // which is the same shape. Markup on a pass-through pin is caught by
        // the shipped-catalog test in `tests/http.rs` and reported against the
        // real upstream by `admin catalog-drift`.
        let sell_at_200k =
            LUNA_RATES.replace("min_prompt_tokens = 272000", "min_prompt_tokens = 200000");
        let catalog = conditional_catalog(&sell_at_200k, LUNA_BASIS)
            .expect("a markup is not a margin violation");
        assert!(catalog.resolve("openai/pin").is_some());
    }

    #[test]
    fn a_tiered_basis_under_a_flat_tier_is_withheld_at_the_band_that_loses() {
        // The shape an operator reaches by updating the candidate and
        // forgetting the tier: the basis reprices to 0.40 above 272,000 while
        // the tier still sells at 0.20 there. That is a real loss on every
        // long request, so the tier is withheld — and the rest of the file
        // keeps serving, because this is economics, not structure.
        let catalog = conditional_catalog(
            "input_per_mtok = 0.20\ncached_input_per_mtok = 0.02\noutput_per_mtok = 1.20",
            LUNA_BASIS,
        )
        .expect("an economic fault withholds one tier, it does not refuse the file");
        let reason = &catalog
            .unavailable_for("openai/pin")
            .expect("the withheld tier explains itself")
            .reason;
        assert!(reason.contains("above 272000 prompt tokens"), "{reason}");
        assert!(catalog.resolve("openai/healthy").is_some());
    }

    #[test]
    fn a_conditional_threshold_of_zero_or_out_of_order_refuses_the_file() {
        // Zero conditions on nothing — it is a second base table, and the file
        // would state two answers to one question. Repeated and descending
        // thresholds make the file's reading order disagree with its pricing
        // order, which is how a human checking it against a vendor's page gets
        // the wrong answer.
        for broken in [
            "min_prompt_tokens = 0",
            "min_prompt_tokens = 272000\ninput_per_mtok = 0.40\noutput_per_mtok = 1.80\n\
             [[tiers.\"openai/pin\".rates.conditional]]\nmin_prompt_tokens = 100000",
            "min_prompt_tokens = 272000\ninput_per_mtok = 0.40\noutput_per_mtok = 1.80\n\
             [[tiers.\"openai/pin\".rates.conditional]]\nmin_prompt_tokens = 272000",
        ] {
            let sell = format!(
                "input_per_mtok = 0.20\noutput_per_mtok = 1.20\n\
                 [[tiers.\"openai/pin\".rates.conditional]]\n{broken}\n\
                 input_per_mtok = 0.40\noutput_per_mtok = 1.80\n"
            );
            let error = conditional_catalog(&sell, "input_per_mtok = 0.20\noutput_per_mtok = 1.20")
                .expect_err("a malformed threshold list must refuse the file");
            assert!(
                matches!(error, TierConfigError::InvalidConditionalThresholds { .. }),
                "{broken} produced {error:?}"
            );
        }
    }

    #[test]
    fn a_cached_rate_dearer_than_its_own_input_rate_withholds_the_tier() {
        // Cached input is a DISCOUNT on input — that is what "cached" means on
        // every upstream in this catalog, and `usage_cost` falls an absent
        // cached rate back to the input rate precisely because the two are the
        // same kind of thing. A table pricing cache reads ABOVE fresh reads is
        // either a transposition or a misunderstanding, and it is never what a
        // vendor charges.
        //
        // It is refused rather than merely surprising because a reservation's
        // sufficiency rests on it. Admission prices its whole prompt bound at
        // the input rate (a reservation carries no cached detail), while
        // settlement splits the measured prompt into cached and uncached
        // parts. Reserved covers settled for every possible cache hit rate
        // exactly when the cached rate does not exceed the input rate — so
        // this check is what turns "reserved >= settled" from an observation
        // about today's ten schedules into a property the loader enforces.
        //
        // Both sides and both kinds of table: the tier's own sell schedule and
        // a candidate's basis, base row and conditional band alike.
        for (sell, basis) in [
            // The tier's base table.
            (
                "input_per_mtok = 0.20\ncached_input_per_mtok = 0.30\noutput_per_mtok = 1.20",
                "input_per_mtok = 0.20\noutput_per_mtok = 1.20",
            ),
            // The tier's conditional band, which is a rate table like any
            // other. The basis mirrors it exactly so the MARGIN rule is
            // satisfied and this check is the only complaint left.
            (
                "input_per_mtok = 0.20\ncached_input_per_mtok = 0.02\noutput_per_mtok = 1.20\n\
                 [[tiers.\"openai/pin\".rates.conditional]]\nmin_prompt_tokens = 272000\n\
                 input_per_mtok = 0.40\ncached_input_per_mtok = 0.50\noutput_per_mtok = 1.80\n",
                "input_per_mtok = 0.20\ncached_input_per_mtok = 0.02\noutput_per_mtok = 1.20\n\
                 [[tiers.\"openai/pin\".candidates.rates.conditional]]\n\
                 min_prompt_tokens = 272000\n\
                 input_per_mtok = 0.40\ncached_input_per_mtok = 0.04\noutput_per_mtok = 1.80\n",
            ),
            // A candidate's own band, under a tier that is priced sanely.
            (
                "input_per_mtok = 9.00\ncached_input_per_mtok = 9.00\noutput_per_mtok = 9.00",
                "input_per_mtok = 0.20\noutput_per_mtok = 1.20\n\
                 [[tiers.\"openai/pin\".candidates.rates.conditional]]\n\
                 min_prompt_tokens = 272000\n\
                 input_per_mtok = 0.40\ncached_input_per_mtok = 0.90\noutput_per_mtok = 1.80\n",
            ),
        ] {
            let catalog = conditional_catalog(sell, basis)
                .expect("an economic fault withholds one tier, it does not refuse the file");
            let reason = &catalog
                .unavailable_for("openai/pin")
                .expect("a cached rate above its own input rate must withhold the tier")
                .reason;
            assert!(
                reason.contains("a cache read is a DISCOUNT on a fresh read"),
                "{reason}"
            );
            // The blast radius is one tier: the rest of the file keeps serving.
            assert!(catalog.resolve("openai/healthy").is_some());
        }
    }

    #[test]
    fn a_cached_rate_equal_to_its_input_rate_still_loads() {
        // The boundary of the rule above: an upstream with no cache discount
        // at all prices cache reads at the fresh rate, which is legal and
        // common on local servers. Only a STRICTLY greater cached rate is the
        // transposition this refuses.
        conditional_catalog(
            "input_per_mtok = 0.20\ncached_input_per_mtok = 0.20\noutput_per_mtok = 1.20",
            "input_per_mtok = 0.20\ncached_input_per_mtok = 0.20\noutput_per_mtok = 1.20",
        )
        .expect("cached == input is a cache with no discount, not an error");
    }

    #[test]
    fn a_conditional_band_missing_a_required_rate_refuses_the_file() {
        // A band REPLACES the base table rather than patching it, so an
        // omitted input or output rate would price that dimension at zero for
        // every request past the boundary — the silent-free-dimension failure,
        // reached from a new direction.
        for missing in ["input_per_mtok = 0.40", "output_per_mtok = 1.80"] {
            let sell = format!(
                "input_per_mtok = 0.20\noutput_per_mtok = 1.20\n\
                 [[tiers.\"openai/pin\".rates.conditional]]\nmin_prompt_tokens = 272000\n{missing}\n"
            );
            let error = conditional_catalog(&sell, "input_per_mtok = 0.20\noutput_per_mtok = 1.20")
                .expect_err("an incomplete band must refuse the file");
            assert!(
                matches!(error, TierConfigError::InvalidRate { .. }),
                "a band declaring only `{missing}` produced {error:?}"
            );
        }
    }

    #[test]
    fn a_conditional_band_priced_at_zero_on_a_metered_provider_refuses_the_file() {
        // `validate_zero_price`'s claim, made by a band instead of a base
        // table: it asserts that the input and output tokens every long
        // request generates cost ZeroRouter nothing on an upstream that bills
        // for them. Same fat-fingered-rate signature, same refusal.
        let basis = "input_per_mtok = 0.20\noutput_per_mtok = 1.20\n\
             [[tiers.\"openai/pin\".candidates.rates.conditional]]\nmin_prompt_tokens = 272000\n\
             input_per_mtok = 0.0\noutput_per_mtok = 0.0\n";
        let sell = "input_per_mtok = 0.20\noutput_per_mtok = 1.20\n\
             [[tiers.\"openai/pin\".rates.conditional]]\nmin_prompt_tokens = 272000\n\
             input_per_mtok = 0.40\noutput_per_mtok = 1.80\n";
        let error = conditional_catalog(sell, basis)
            .expect_err("a $0 band on a metered upstream must refuse the file");
        assert!(
            matches!(
                error,
                TierConfigError::ZeroPriceWithoutFreeSettlement { .. }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_misspelled_key_inside_a_conditional_band_refuses_the_file() {
        // The hazard that makes this block stricter than the table around it:
        // a misspelled OPTIONAL dimension would otherwise deserialize to
        // `None` and quietly price cached tokens at the band's input rate
        // instead of the discounted one the operator meant to write.
        let sell = "input_per_mtok = 0.20\noutput_per_mtok = 1.20\n\
             [[tiers.\"openai/pin\".rates.conditional]]\nmin_prompt_tokens = 272000\n\
             input_per_mtok = 0.40\noutput_per_mtok = 1.80\ncached_input_per_mtoks = 0.04\n";
        let error = conditional_catalog(sell, "input_per_mtok = 0.20\noutput_per_mtok = 1.20")
            .expect_err("an unknown key in a band must refuse the file");
        assert!(matches!(error, TierConfigError::Parse { .. }), "{error:?}");
    }

    #[test]
    fn concrete_model_resolves_to_one_candidate() {
        let catalog: TierCatalog = toml::from_str(
            r#"
schema_version = 1
[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.google]
posture = "standard"
description = "fixture"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
[retention."local-llama"]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local-llama"
verified = "2026-08-20"
source_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
[retention.local]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local"
verified = "2026-08-20"
source_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
[tiers."zero/test"]
[tiers."zero/test".rates]
input_per_mtok = 1
output_per_mtok = 2
[[tiers."zero/test".candidates]]
id = "provider/model"
provider = "openai"
model = "upstream-model"
[tiers."zero/test".candidates.rates]
input_per_mtok = 1
output_per_mtok = 2
"#,
        )
        .expect("catalog should parse");
        validate_tier_catalog(&catalog).expect("catalog should validate");

        let route = catalog
            .resolve("provider/model")
            .expect("concrete model should resolve");
        assert_eq!(route.candidates.len(), 1);
        assert_eq!(route.candidates[0].model, "upstream-model");
    }

    #[test]
    fn concrete_model_bills_at_the_owning_tier_sell_rate() {
        // The tier sell rate differs from the candidate cost basis; pinning a
        // concrete candidate must meter at the tier sell rate, never the
        // cheaper cost basis (margin-leak regression guard).
        let catalog: TierCatalog = toml::from_str(
            r#"
schema_version = 1
[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.google]
posture = "standard"
description = "fixture"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
[retention."local-llama"]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local-llama"
verified = "2026-08-20"
source_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
[retention.local]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local"
verified = "2026-08-20"
source_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
[tiers."zero/test"]
[tiers."zero/test".rates]
input_per_mtok = 10
output_per_mtok = 20
[[tiers."zero/test".candidates]]
id = "provider/model"
provider = "openai"
model = "upstream-model"
[tiers."zero/test".candidates.rates]
input_per_mtok = 1
output_per_mtok = 2
"#,
        )
        .expect("catalog should parse");
        validate_tier_catalog(&catalog).expect("catalog should validate");

        let tier = catalog.resolve("zero/test").expect("tier should resolve");
        let concrete = catalog
            .resolve("provider/model")
            .expect("concrete model should resolve");
        assert_eq!(
            concrete.sell_rates, tier.sell_rates,
            "a pinned candidate must bill at the tier sell rate"
        );
    }

    /// A one-tier, one-candidate catalog with the given `[rates]` bodies.
    fn catalog_with(sell: &str, basis: &str) -> TierCatalog {
        toml::from_str(&format!(
            r#"
schema_version = 1
[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.google]
posture = "standard"
description = "fixture"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
[retention."local-llama"]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local-llama"
verified = "2026-08-20"
source_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
[retention.local]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local"
verified = "2026-08-20"
source_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
[tiers."zero/test"]
[tiers."zero/test".rates]
{sell}
[[tiers."zero/test".candidates]]
id = "provider/model"
provider = "openai"
model = "upstream-model"
[tiers."zero/test".candidates.rates]
{basis}
"#
        ))
        .expect("catalog should parse")
    }

    #[test]
    fn a_candidate_priced_above_its_tier_is_rejected_on_every_dimension() {
        // The three rungs the shipped table used to carry, reduced to their
        // shapes. Every candidate in a tier bills at the one tier sell rate,
        // so a basis above it on *any* dimension loses money on every request
        // that candidate serves — and the error has to say which dimension and
        // both numbers, or the operator cannot fix the table from the log line.
        for (label, sell, basis, message) in [
            (
                // Opus in zero/high-end: dearer on input.
                "input",
                "input_per_mtok = 2.00\noutput_per_mtok = 10.00\ncached_input_per_mtok = 0.20",
                "input_per_mtok = 5.00\noutput_per_mtok = 10.00\ncached_input_per_mtok = 0.20",
                "input_per_mtok cost basis 5 exceeds tier sell rate 2",
            ),
            (
                // Haiku in zero/balanced: *cheaper* on input, dearer on output.
                // The dimension that leaks is not always the headline rate.
                "output",
                "input_per_mtok = 1.74\noutput_per_mtok = 3.48\ncached_input_per_mtok = 0.14",
                "input_per_mtok = 1.00\noutput_per_mtok = 5.00\ncached_input_per_mtok = 0.10",
                "output_per_mtok cost basis 5 exceeds tier sell rate 3.48",
            ),
            (
                "cached input",
                "input_per_mtok = 2.00\noutput_per_mtok = 10.00\ncached_input_per_mtok = 0.20",
                "input_per_mtok = 2.00\noutput_per_mtok = 10.00\ncached_input_per_mtok = 0.50",
                "cached_input_per_mtok cost basis 0.5 exceeds tier sell rate 0.2",
            ),
        ] {
            let catalog = catalog_with(sell, basis);
            let error = validate_tier_catalog(&catalog)
                .expect_err(&format!("a below-cost {label} basis must be rejected"));
            assert!(
                matches!(error, TierConfigError::NegativeMargin { .. }),
                "{label}: unexpected error {error:?}"
            );
            assert_eq!(
                error.to_string(),
                format!(
                    "candidate provider/model in tier zero/test costs more than the tier sells: \
                     {message}"
                )
            );
        }
    }

    #[test]
    fn a_basis_equal_to_the_tier_sell_rate_is_legal() {
        // The pricing model itself: the flagship candidate sits *at* the sell
        // rate (0% markup) and margin comes from routing to cheaper candidates
        // in the same tier. Rejecting equality would reject every tier we ship.
        let catalog = catalog_with(
            "input_per_mtok = 2.00\noutput_per_mtok = 10.00\ncached_input_per_mtok = 0.20",
            "input_per_mtok = 2.00\noutput_per_mtok = 10.00\ncached_input_per_mtok = 0.20",
        );
        validate_tier_catalog(&catalog).expect("a 0%-markup flagship must stay legal");
    }

    #[test]
    fn an_absent_cached_basis_is_compared_at_the_rate_it_will_be_billed_at() {
        // DELIBERATELY REVERSED. This test used to assert the opposite, on the
        // reasoning that "an unset dimension is unknown, not free and not a
        // violation — reading it as the input rate would fail a table that is
        // fine". The table is not fine, and the premise was the bug: `usage_cost`
        // reads an absent cached rate AS the input rate, so this shape sells
        // cached tokens at 0.06 and buys them at 0.30. A fivefold loss on every
        // request that reports a cache hit is exactly the silent margin leak
        // `validate_candidate_margin` exists to refuse, and the old rule stayed
        // quiet about it because it compared what was WRITTEN rather than what
        // would be BILLED.
        let leaking = catalog_with(
            "input_per_mtok = 0.30\noutput_per_mtok = 1.20\ncached_input_per_mtok = 0.06",
            "input_per_mtok = 0.30\noutput_per_mtok = 1.20",
        );
        let error = validate_tier_catalog(&leaking)
            .expect_err("a cached discount the candidate cannot honour must be refused");
        assert!(
            matches!(
                error,
                TierConfigError::NegativeMargin {
                    dimension: "cached_input_per_mtok",
                    ..
                }
            ),
            "unexpected error {error:?}"
        );

        // What the old test was RIGHT to protect, and what survives: a rate
        // table that predates cached pricing, where NEITHER side declares the
        // dimension. Both fall back to their own input rate, an at-cost
        // flagship stays exactly at cost, and nothing is refused. "Unknown" is
        // still not a violation — it is only no longer a way to dodge the
        // comparison while the biller goes ahead and makes it.
        let honest = catalog_with(
            "input_per_mtok = 0.30\noutput_per_mtok = 1.20",
            "input_per_mtok = 0.30\noutput_per_mtok = 1.20",
        );
        validate_tier_catalog(&honest)
            .expect("an absent cached dimension on both sides is at cost, not over it");
    }

    #[test]
    fn a_rate_the_biller_cannot_represent_refuses_the_catalog() {
        // `1e100` is a perfectly ordinary f64 and a perfectly ordinary typo. It
        // used to pass validation as "finite and non-negative", convert to
        // `Decimal::ZERO` in `usage_cost`, and price its whole dimension free —
        // visible to nobody but the margin. Same failure from the other end for
        // a nonzero rate below `Decimal`'s smallest step, and for a rate large
        // enough that `tokens * rate` would overflow mid-request.
        for (label, sell, basis, dimension) in [
            (
                "sell rate out of Decimal's range",
                "input_per_mtok = 1e100\noutput_per_mtok = 10.00",
                "input_per_mtok = 1.00\noutput_per_mtok = 1.00",
                "input_per_mtok",
            ),
            (
                "candidate basis out of Decimal's range",
                "input_per_mtok = 2.00\noutput_per_mtok = 10.00",
                "input_per_mtok = 1.00\noutput_per_mtok = 1e100",
                "output_per_mtok",
            ),
            (
                "nonzero sell rate that rounds to zero",
                "input_per_mtok = 2.00\noutput_per_mtok = 10.00\ncached_input_per_mtok = 1e-30",
                "input_per_mtok = 1.00\noutput_per_mtok = 1.00",
                "cached_input_per_mtok",
            ),
            (
                "sell rate that would overflow the multiplication",
                "input_per_mtok = 2.00\noutput_per_mtok = 1e27",
                "input_per_mtok = 1.00\noutput_per_mtok = 1.00",
                "output_per_mtok",
            ),
        ] {
            let catalog = catalog_with(sell, basis);
            let error = validate_tier_catalog(&catalog)
                .expect_err(&format!("{label} must refuse the catalog"));
            assert!(
                matches!(
                    error,
                    TierConfigError::UnbillableRate { dimension: found, .. } if found == dimension
                ),
                "{label}: unexpected error {error:?}"
            );
        }
    }

    #[test]
    fn a_rate_at_the_billing_ceiling_still_loads() {
        // The bound is arithmetic headroom, not a business ceiling: it has to
        // leave every rate an operator would plausibly write alone, including
        // the ceiling itself.
        let at_ceiling =
            format!("input_per_mtok = {MAX_RATE_PER_MTOK}\noutput_per_mtok = {MAX_RATE_PER_MTOK}");
        validate_tier_catalog(&catalog_with(&at_ceiling, &at_ceiling))
            .expect("a rate at the ceiling must still load");
    }

    /// A one-tier catalog selling at 2.00/10.00 with `candidates` spliced in
    /// verbatim, so a metadata test can vary the thing it is about without
    /// restating the pricing that has nothing to do with it.
    fn catalog_with_candidates(candidates: &str) -> TierCatalog {
        toml::from_str(&format!(
            r#"
schema_version = 1
[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.google]
posture = "standard"
description = "fixture"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
[retention."local-llama"]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local-llama"
verified = "2026-08-20"
source_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
[retention.local]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local"
verified = "2026-08-20"
source_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
[tiers."zero/test"]
[tiers."zero/test".rates]
input_per_mtok = 2.00
output_per_mtok = 10.00
{candidates}
"#
        ))
        .expect("catalog should parse")
    }

    /// One candidate under `zero/test`, priced well inside the tier, carrying
    /// `metadata` as its `[...metadata]` body. An empty `metadata` emits no
    /// table at all — the shape of a file written before metadata existed.
    fn candidate(id: &str, metadata: &str) -> String {
        let metadata = if metadata.trim().is_empty() {
            String::new()
        } else {
            format!("[tiers.\"zero/test\".candidates.metadata]\n{metadata}\n")
        };
        format!(
            r#"
[[tiers."zero/test".candidates]]
id = "openai/{id}"
provider = "openai"
model = "upstream/{id}"
{metadata}
[tiers."zero/test".candidates.rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
"#
        )
    }

    #[test]
    fn a_catalog_that_declares_no_metadata_lists_exactly_as_it_did_before() {
        // The backward-compatibility contract in one assertion. A file written
        // before this table existed still parses and still validates, and
        // every metadata field comes out unknown — not zero, not a plausible
        // default, not an empty list that would read as "text only".
        let catalog = catalog_with_candidates(&candidate("plain", ""));
        validate_tier_catalog(&catalog).expect("metadata must be optional");

        // Every provider credentialed: these assert the listing's SHAPE
        // (rates, metadata, posture), not which lanes a deployment can
        // reach. The credential filter has its own tests.
        let listing = catalog.model_listing(&|_| true);
        for id in ["zero/test", "openai/plain"] {
            assert_eq!(
                listing[id].metadata,
                ModelMetadata::default(),
                "{id} should make no metadata claim at all"
            );
        }
    }

    #[test]
    fn a_tier_advertises_the_narrowest_thing_its_candidates_can_do() {
        // A tier id is a promise about wherever the walk lands, so it takes the
        // smaller window, the smaller output, the shared modalities, and tool
        // calling only if both rungs have it. Once a 200k rung exists the
        // flagship's 1M window is emphatically not the tier's window — that
        // number would be wrong exactly when failover happens.
        let catalog = catalog_with_candidates(&format!(
            "{}{}",
            candidate(
                "wide",
                "context_window = 1000000\nmax_output_tokens = 128000\n\
                 input_modalities = [\"text\", \"image\", \"pdf\"]\ntool_call = true",
            ),
            candidate(
                "narrow",
                "context_window = 200000\nmax_output_tokens = 64000\n\
                 input_modalities = [\"text\", \"pdf\"]\ntool_call = false",
            ),
        ));
        validate_tier_catalog(&catalog).expect("catalog should validate");
        // Every provider credentialed: these assert the listing's SHAPE
        // (rates, metadata, posture), not which lanes a deployment can
        // reach. The credential filter has its own tests.
        let listing = catalog.model_listing(&|_| true);

        assert_eq!(
            listing["zero/test"].metadata,
            ModelMetadata {
                context_window: Some(200_000),
                max_output_tokens: Some(64_000),
                input_modalities: Some(vec!["text".to_owned(), "pdf".to_owned()]),
                tool_call: Some(false),
            }
        );

        // Each pinned candidate keeps its own facts. Pricing runs the other way
        // — a pinned candidate bills at its owning tier's rate — and the
        // asymmetry is deliberate: a price belongs to the tier a request is
        // billed through, a window belongs to the model that serves it.
        assert_eq!(
            listing["openai/wide"].metadata,
            ModelMetadata {
                context_window: Some(1_000_000),
                max_output_tokens: Some(128_000),
                input_modalities: Some(vec![
                    "text".to_owned(),
                    "image".to_owned(),
                    "pdf".to_owned(),
                ]),
                tool_call: Some(true),
            },
            "a pinned candidate advertises itself, not its tier"
        );
        assert_eq!(
            listing["openai/narrow"].metadata.context_window,
            Some(200_000)
        );
    }

    #[test]
    fn one_undeclared_candidate_makes_the_whole_tier_field_unknown() {
        // Narrowing over only the candidates that *did* declare would publish
        // a bound the tier cannot keep: the rung nobody described might be the
        // 32k one. Unknown is the only honest tier-level answer — and it is
        // not contagious, so the candidate that did declare still advertises.
        let catalog = catalog_with_candidates(&format!(
            "{}{}",
            candidate(
                "declared",
                "context_window = 1000000\nmax_output_tokens = 128000\n\
                 input_modalities = [\"text\", \"image\"]\ntool_call = true",
            ),
            candidate("silent", ""),
        ));
        validate_tier_catalog(&catalog).expect("catalog should validate");
        // Every provider credentialed: these assert the listing's SHAPE
        // (rates, metadata, posture), not which lanes a deployment can
        // reach. The credential filter has its own tests.
        let listing = catalog.model_listing(&|_| true);

        assert_eq!(
            listing["zero/test"].metadata,
            ModelMetadata::default(),
            "one silent rung makes every tier-level field unknown"
        );
        assert_eq!(
            listing["openai/declared"].metadata.context_window,
            Some(1_000_000),
            "the rung that did declare still advertises its own window"
        );
    }

    #[test]
    fn a_declared_limit_of_zero_refuses_the_file() {
        // Absence is always legal, so there is never a reason to write a limit
        // that carries no information. Zero is not a small window, it is a
        // typo, and a consumer cannot tell it from a checked claim.
        for (label, body, field) in [
            ("context window", "context_window = 0", "context_window"),
            ("max output", "max_output_tokens = 0", "max_output_tokens"),
        ] {
            let catalog = catalog_with_candidates(&candidate("zero", body));
            let error = validate_tier_catalog(&catalog)
                .expect_err(&format!("a zero {label} must refuse the catalog"));
            assert!(
                matches!(
                    error,
                    TierConfigError::ZeroModelLimit { field: found, .. } if found == field
                ),
                "{label}: unexpected error {error:?}"
            );
        }
    }

    #[test]
    fn an_empty_or_blank_modality_list_refuses_the_file() {
        // Same rule for the list: declaring `[]` claims the model accepts no
        // input at all, and a blank entry is a stray comma. Say nothing by
        // saying nothing.
        for (label, body) in [
            ("an empty list", "input_modalities = []"),
            ("a blank entry", "input_modalities = [\"text\", \"  \"]"),
        ] {
            let catalog = catalog_with_candidates(&candidate("blank", body));
            let error = validate_tier_catalog(&catalog)
                .expect_err(&format!("{label} must refuse the catalog"));
            assert!(
                matches!(error, TierConfigError::InvalidModalities { .. }),
                "{label}: unexpected error {error:?}"
            );
        }
    }

    /// Two tiers: `zero/healthy` is priced sanely; `zero/below-cost` sells at
    /// 2/10 but carries a candidate whose input basis is 3. `extra` is spliced
    /// into the below-cost tier so a test can add a second candidate to it.
    fn mixed_catalog(extra: &str) -> TierCatalog {
        toml::from_str(&format!(
            r#"
schema_version = 1
[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.google]
posture = "standard"
description = "fixture"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
[retention."local-llama"]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local-llama"
verified = "2026-08-20"
source_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
[retention.local]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local"
verified = "2026-08-20"
source_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
[tiers."zero/healthy"]
[tiers."zero/healthy".rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
[[tiers."zero/healthy".candidates]]
id = "openai/cheap"
provider = "openai"
model = "upstream/cheap"
[tiers."zero/healthy".candidates.rates]
input_per_mtok = 0.50
output_per_mtok = 1.00

[tiers."zero/below-cost"]
[tiers."zero/below-cost".rates]
input_per_mtok = 2.00
output_per_mtok = 10.00
[[tiers."zero/below-cost".candidates]]
id = "anthropic/dear"
provider = "anthropic"
model = "upstream/dear"
[tiers."zero/below-cost".candidates.rates]
input_per_mtok = 3.00
output_per_mtok = 10.00
{extra}
"#
        ))
        .expect("catalog should parse")
    }

    #[test]
    fn a_below_cost_tier_is_withheld_instead_of_condemning_the_file() {
        // The proportionality rule itself: one tier priced below its own cost
        // basis is a verdict on that tier, so the load succeeds and reports
        // exactly which tier cannot be sold — and why, in the operator's words.
        let catalog = mixed_catalog("");
        let withheld = validate_tier_catalog(&catalog)
            .expect("one mispriced tier must not condemn the whole catalog");

        assert_eq!(
            withheld.keys().map(String::as_str).collect::<Vec<_>>(),
            ["zero/below-cost"]
        );
        assert_eq!(
            withheld["zero/below-cost"],
            "candidate anthropic/dear in tier zero/below-cost costs more than the tier sells: \
             input_per_mtok cost basis 3 exceeds tier sell rate 2"
        );
    }

    #[test]
    fn a_withheld_tier_stops_routing_and_stops_being_advertised() {
        // After the verdict is applied the tier is gone from everything that
        // sells: it does not resolve, neither does a concrete candidate pinned
        // inside it, and neither appears in `/v1/models`. The tier is still
        // *addressable* though — `unavailable_for` answers for the tier id and
        // for its pinned candidates alike, which is what lets a request say
        // "misconfigured" instead of "no such model".
        let mut catalog = mixed_catalog("");
        let withheld = validate_tier_catalog(&catalog).expect("catalog should load");
        catalog.withhold(withheld);

        assert!(catalog.resolve("zero/healthy").is_some());
        assert!(catalog.resolve("openai/cheap").is_some());
        assert!(catalog.resolve("zero/below-cost").is_none());
        assert!(catalog.resolve("anthropic/dear").is_none());

        for requested in ["zero/below-cost", "anthropic/dear"] {
            let unavailable = catalog
                .unavailable_for(requested)
                .unwrap_or_else(|| panic!("{requested} should report its withheld tier"));
            assert_eq!(unavailable.tier, "zero/below-cost");
            assert!(
                unavailable
                    .reason
                    .contains("costs more than the tier sells")
            );
        }
        assert!(catalog.unavailable_for("zero/healthy").is_none());
        assert!(catalog.unavailable_for("nonsense/model").is_none());

        // Every provider credentialed: these assert the listing's SHAPE
        // (rates, metadata, posture), not which lanes a deployment can
        // reach. The credential filter has its own tests.
        let listing = catalog.model_listing(&|_| true);
        assert_eq!(
            listing.keys().map(String::as_str).collect::<Vec<_>>(),
            ["openai/cheap", "zero/healthy"]
        );
    }

    #[test]
    fn a_structural_fault_beside_a_margin_fault_still_condemns_the_file() {
        // The margin verdict must not short-circuit the rest of the tier. A
        // below-cost first candidate followed by a structurally invalid second
        // one is a *file* fault: an unsupported provider means the table is
        // wrong about the world, not merely mispriced.
        let catalog = mixed_catalog(
            r#"
[[tiers."zero/below-cost".candidates]]
id = "unknown/model"
provider = "unknown"
model = "upstream/unknown"
[tiers."zero/below-cost".candidates.rates]
input_per_mtok = 0.10
output_per_mtok = 0.20
"#,
        );

        assert!(matches!(
            validate_tier_catalog(&catalog),
            Err(TierConfigError::UnsupportedProvider { .. })
        ));
    }

    #[test]
    fn an_unbillable_rate_condemns_the_file_even_inside_a_withheld_tier() {
        // The exact side of the withhold/refuse split this rule sits on. The
        // tier carrying the unbillable rate is *already* condemned on economics
        // and would be withheld while the rest of the file kept serving. It must
        // not be: a rate the biller cannot represent says the file's numbers
        // cannot be read as money at all, which is the same class as a malformed
        // tier id or an unsupported provider, and being absorbed into a withheld
        // tier is precisely how it would go unnoticed.
        let catalog = mixed_catalog(
            r#"
[[tiers."zero/below-cost".candidates]]
id = "anthropic/unbillable"
provider = "anthropic"
model = "upstream/unbillable"
[tiers."zero/below-cost".candidates.rates]
input_per_mtok = 1e100
output_per_mtok = 10.00
"#,
        );

        let error = validate_tier_catalog(&catalog)
            .expect_err("an unbillable rate must refuse the file, not vanish into a withheld tier");
        assert!(
            matches!(error, TierConfigError::UnbillableRate { .. }),
            "unexpected error {error:?}"
        );
    }

    #[test]
    fn a_duplicate_concrete_id_stays_fatal_even_behind_a_below_cost_tier() {
        // Duplicate ids are cross-tier by nature, so they are checked across
        // every tier, healthy or not, and stay whole-file fatal. Scoping the
        // check to surviving tiers would hide this duplicate until the operator
        // *fixed* the below-cost pricing, turning that repair into the outage.
        let catalog = mixed_catalog(
            r#"
[[tiers."zero/below-cost".candidates]]
id = "openai/cheap"
provider = "openai"
model = "upstream/cheap"
[tiers."zero/below-cost".candidates.rates]
input_per_mtok = 0.10
output_per_mtok = 0.20
"#,
        );

        let error = validate_tier_catalog(&catalog)
            .expect_err("a repeated concrete id must refuse the whole catalog");
        assert!(
            matches!(error, TierConfigError::DuplicateModelId(ref id) if id == "openai/cheap"),
            "unexpected error {error:?}"
        );
    }

    #[test]
    fn an_id_ending_in_a_priority_keyword_after_a_colon_is_refused() {
        // The model-suffix carrier strips `:cost|:balanced|:success` only
        // after resolution fails, so a literal id ending in a priority
        // keyword would keep resolving — while silently swallowing a
        // customer's suffix. The collision is refused at load, whole-file,
        // for tier ids and candidate ids alike.
        for (label, id_line) in [
            ("tier id", "zero/fast:cost"),
            ("tier id", "zero/fast:success"),
            ("tier id", "zero/fast:balanced"),
        ] {
            let toml = format!(
                r#"
schema_version = 1
[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.google]
posture = "standard"
description = "fixture"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
[retention."local-llama"]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local-llama"
verified = "2026-08-20"
source_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
[retention.local]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local"
verified = "2026-08-20"
source_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
[tiers."{id_line}"]
[tiers."{id_line}".rates]
input_per_mtok = 1
output_per_mtok = 2
[[tiers."{id_line}".candidates]]
id = "openai/fast"
provider = "openai"
model = "upstream/fast"
[tiers."{id_line}".candidates.rates]
input_per_mtok = 1
output_per_mtok = 2
"#
            );
            let catalog: TierCatalog = toml::from_str(&toml).expect("catalog should parse");
            let error = validate_tier_catalog(&catalog)
                .expect_err("a colliding tier id must refuse the catalog");
            assert!(
                matches!(error, TierConfigError::PrioritySuffixCollision(ref id) if id == id_line),
                "unexpected error for {label}: {error:?}"
            );
        }

        let catalog: TierCatalog = toml::from_str(
            r#"
schema_version = 1
[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.google]
posture = "standard"
description = "fixture"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
[retention."local-llama"]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local-llama"
verified = "2026-08-20"
source_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
[retention.local]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local"
verified = "2026-08-20"
source_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
[tiers."zero/test"]
[tiers."zero/test".rates]
input_per_mtok = 1
output_per_mtok = 2
[[tiers."zero/test".candidates]]
id = "openai/fast:cost"
provider = "openai"
model = "upstream/fast"
[tiers."zero/test".candidates.rates]
input_per_mtok = 1
output_per_mtok = 2
"#,
        )
        .expect("catalog should parse");
        let error = validate_tier_catalog(&catalog)
            .expect_err("a colliding candidate id must refuse the catalog");
        assert!(
            matches!(error, TierConfigError::PrioritySuffixCollision(ref id) if id == "openai/fast:cost"),
            "unexpected error {error:?}"
        );
    }

    #[test]
    fn colons_that_do_not_end_in_a_priority_keyword_stay_loadable() {
        // The rule bans the collision, not the character: an ARN-style id
        // whose final segment is not a priority keyword must keep loading,
        // because the carrier's resolve-first algorithm never misreads it.
        let catalog: TierCatalog = toml::from_str(
            r#"
schema_version = 1
[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.google]
posture = "standard"
description = "fixture"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
[retention."local-llama"]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local-llama"
verified = "2026-08-20"
source_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
[retention.local]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local"
verified = "2026-08-20"
source_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
[tiers."zero/test"]
[tiers."zero/test".rates]
input_per_mtok = 1
output_per_mtok = 2
[[tiers."zero/test".candidates]]
id = "arn:aws:bedrock:us-east-1"
provider = "openai"
model = "upstream/fast"
[tiers."zero/test".candidates.rates]
input_per_mtok = 1
output_per_mtok = 2
"#,
        )
        .expect("catalog should parse");
        validate_tier_catalog(&catalog).expect("a non-colliding colon id should validate");
        assert!(
            catalog.resolve("arn:aws:bedrock:us-east-1").is_some(),
            "the colon id must stay resolvable"
        );
    }

    #[test]
    fn a_catalog_with_no_servable_tier_left_is_refused_outright() {
        // Total degradation is not degradation, it is an outage with extra
        // steps: an empty catalog would 404 every model the file names. Fail
        // the load instead, with the margin error that explains why.
        let catalog: TierCatalog = toml::from_str(
            r#"
schema_version = 1
[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.google]
posture = "standard"
description = "fixture"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
[retention."local-llama"]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local-llama"
verified = "2026-08-20"
source_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
[retention.local]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local"
verified = "2026-08-20"
source_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
[tiers."zero/one"]
[tiers."zero/one".rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
[[tiers."zero/one".candidates]]
id = "openai/one"
provider = "openai"
model = "upstream/one"
[tiers."zero/one".candidates.rates]
input_per_mtok = 9.00
output_per_mtok = 2.00

[tiers."zero/two"]
[tiers."zero/two".rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
[[tiers."zero/two".candidates]]
id = "anthropic/two"
provider = "anthropic"
model = "upstream/two"
[tiers."zero/two".candidates.rates]
input_per_mtok = 1.00
output_per_mtok = 8.00
"#,
        )
        .expect("catalog should parse");

        let error = validate_tier_catalog(&catalog)
            .expect_err("a catalog with nothing left to sell must fail to load");
        assert!(
            matches!(error, TierConfigError::NegativeMargin { .. }),
            "unexpected error {error:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Edge mode, stage 2: the $0 rung's configuration surface
    // (`docs/design/edge-mode-local-rung.md`). The ACCEPTING half of these
    // rules needs an operator provider inventory installed, which is
    // process-global, so it is exercised end to end in
    // `tests/local_candidates.rs`; what is unit-testable here is every way the
    // file is refused, plus the two pure predicates.
    // -----------------------------------------------------------------------

    fn priced(input: Option<f64>, cached: Option<f64>, output: Option<f64>) -> TierCandidate {
        TierCandidate {
            id: "local/model".to_owned(),
            provider: "local-llama".to_owned(),
            model: "qwen3-8b".to_owned(),
            rates: RateSchedule::flat(ModelRates {
                input_per_mtok: input,
                cached_input_per_mtok: cached,
                output_per_mtok: output,
            }),
            metadata: ModelMetadata::default(),
        }
    }

    #[test]
    fn a_zero_price_is_the_tier_file_half_of_free_and_never_free_on_its_own() {
        // The price half: what `tiers.toml` can say by itself.
        assert!(
            priced(Some(0.0), None, Some(0.0)).rates_are_zero(),
            "an omitted cached rate is the local shape: nothing to state"
        );
        assert!(priced(Some(0.0), Some(0.0), Some(0.0)).rates_are_zero());

        // Anything actually priced is not zero, on any dimension. The first
        // case is the one worth being explicit about: a rung that is free to
        // read and charged to write is a PAID rung, and treating it as free
        // would put a real cost basis in front of the cheapest cloud rung.
        assert!(!priced(Some(0.0), Some(0.1), Some(0.0)).rates_are_zero());
        assert!(!priced(Some(0.0), None, Some(2.0)).rates_are_zero());
        assert!(!priced(Some(1.0), None, Some(0.0)).rates_are_zero());

        // And the half a rate table can never supply on its own. No provider
        // in this binary's inventory declares free settlement, so nothing here
        // is free however it is priced — a zero in a rate table is not a claim
        // that anyone believes, it is a number that might be a typo. The
        // accepting case needs a real installed inventory and lives in
        // `tests/local_candidates.rs`.
        for rates in [
            priced(Some(0.0), None, Some(0.0)),
            priced(Some(0.0), Some(0.0), Some(0.0)),
        ] {
            assert!(
                !rates.is_free(),
                "a $0 price on a provider that has not declared free settlement is not free"
            );
        }
    }

    #[test]
    fn a_zero_priced_candidate_on_a_metered_provider_refuses_the_file() {
        // The free-lane rule's structural half, landed a stage early: a $0
        // basis on an upstream that has not declared it settles free is either
        // a fat-fingered rate that records no COGS against real spend, or a
        // paid model filed under the free rung. Both refuse the whole file — a
        // withheld tier is exactly how this would go unnoticed. (The
        // declaration-side half, where a metered provider on the LOCAL wire is
        // refused too, needs an installed inventory and lives in
        // `tests/local_candidates.rs`.)
        let catalog = catalog_with(
            "input_per_mtok = 2.00\noutput_per_mtok = 10.00",
            "input_per_mtok = 0.00\noutput_per_mtok = 0.00",
        );
        let error = validate_tier_catalog(&catalog)
            .expect_err("a $0 basis on a metered provider must refuse the catalog");
        assert!(
            matches!(
                error,
                TierConfigError::ZeroPriceWithoutFreeSettlement { ref provider, .. }
                    if provider == "openai"
            ),
            "unexpected error {error:?}"
        );

        // A single free dimension is not a $0 rung and is left exactly as it
        // was — this stage narrows nothing it was not asked to narrow.
        validate_tier_catalog(&catalog_with(
            "input_per_mtok = 2.00\noutput_per_mtok = 10.00",
            "input_per_mtok = 0.00\noutput_per_mtok = 1.00",
        ))
        .expect("a partly-free basis keeps loading");
    }

    #[test]
    fn a_negative_rate_refuses_the_file() {
        // Pins a rule that predates this stage and now guards the $0 surface
        // too: "cheaper than free" is not a discount, it is a number that
        // would pay the customer to send traffic.
        for (label, sell, basis) in [
            (
                "sell",
                "input_per_mtok = -1.00\noutput_per_mtok = 10.00",
                "input_per_mtok = 0.10\noutput_per_mtok = 0.20",
            ),
            (
                "basis",
                "input_per_mtok = 2.00\noutput_per_mtok = 10.00",
                "input_per_mtok = -0.01\noutput_per_mtok = 0.20",
            ),
        ] {
            let error = validate_tier_catalog(&catalog_with(sell, basis))
                .expect_err(&format!("a negative {label} rate must refuse the catalog"));
            assert!(
                matches!(error, TierConfigError::InvalidRate { .. }),
                "{label}: unexpected error {error:?}"
            );
        }
    }

    /// A request that fits any plausible model: small prompt, no tools, text.
    fn modest_needs() -> RequestNeeds {
        RequestNeeds {
            prompt_bound: 1_000,
            tools: false,
            modalities: ["text".to_owned()].into_iter().collect(),
        }
    }

    #[test]
    fn an_undeclared_capability_never_makes_a_candidate_ineligible() {
        // The rule that keeps this mechanical rather than judgmental: silence
        // is not evidence. A candidate nobody has described serves everything,
        // exactly as it did before the metadata table existed — including
        // requests carrying tools and a large prompt.
        let unknown = ModelMetadata::default();
        assert!(unknown.can_serve(&modest_needs()));
        assert!(
            unknown.can_serve(&RequestNeeds {
                prompt_bound: 10_000_000,
                tools: true,
                modalities: ["text".to_owned(), "image".to_owned()]
                    .into_iter()
                    .collect(),
            })
        );
    }

    #[test]
    fn a_declared_limit_excludes_only_the_requests_it_actually_bounds() {
        let declared = ModelMetadata {
            context_window: Some(32_000),
            max_output_tokens: Some(4_096),
            input_modalities: Some(vec!["text".to_owned()]),
            tool_call: Some(false),
        };

        assert!(declared.can_serve(&modest_needs()));
        // The boundary is exclusive: a prompt bound EQUAL to the declared
        // window still fits, because the window is what the model takes.
        assert!(declared.can_serve(&RequestNeeds {
            prompt_bound: 32_000,
            ..modest_needs()
        }));
        assert!(!declared.can_serve(&RequestNeeds {
            prompt_bound: 32_001,
            ..modest_needs()
        }));
        assert!(!declared.can_serve(&RequestNeeds {
            tools: true,
            ..modest_needs()
        }));
        assert!(
            !declared.can_serve(&RequestNeeds {
                modalities: ["text".to_owned(), "image".to_owned()]
                    .into_iter()
                    .collect(),
                ..modest_needs()
            }),
            "a declared modality list that lacks what the request carries excludes it"
        );

        // A declared output ceiling is deliberately NOT an eligibility input:
        // every request carries an output limit, so gating on it would exclude
        // a small local model from requests that would have generated forty
        // tokens.
        assert!(declared.can_serve(&modest_needs()));
    }

    #[test]
    fn empty_tier_is_rejected() {
        let catalog: TierCatalog = toml::from_str(
            r#"
schema_version = 1
[retention.anthropic]
posture = "standard"
description = "fixture"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[retention.openai]
posture = "standard"
description = "fixture"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.google]
posture = "standard"
description = "fixture"
source_url = "https://example.test/google"
verified = "2026-08-20"
source_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
[retention."local-llama"]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local-llama"
verified = "2026-08-20"
source_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
[retention.local]
posture = "standard"
description = "fixture"
source_url = "https://example.test/local"
verified = "2026-08-20"
source_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
[tiers."zero/pending"]
candidates = []
[tiers."zero/pending".rates]
input_per_mtok = 1
output_per_mtok = 2
"#,
        )
        .expect("catalog should parse");

        assert!(matches!(
            validate_tier_catalog(&catalog),
            Err(TierConfigError::EmptyTier { .. })
        ));
    }

    // ------------------------------------------------------------------
    // Retention posture
    // ------------------------------------------------------------------

    /// A catalog whose `[retention]` section is exactly what the caller passes,
    /// serving one `anthropic` pin. Everything else is held constant so a test
    /// varies only the posture declaration it is about.
    fn catalog_with_retention(retention: &str) -> Result<TierCatalog, TierConfigError> {
        let catalog: TierCatalog = toml::from_str(&format!(
            r#"
schema_version = 1
{retention}
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
"#
        ))
        .expect("catalog should parse");
        validate_tier_catalog(&catalog)?;
        Ok(catalog)
    }

    const GOOD_PIN: &str = r#"
[retention.anthropic]
posture = "standard"
description = "retains for 30 days"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#;

    /// THE INVARIANT: an unlabelled lane is impossible, not defaulted.
    ///
    /// This is the test that fails when a provider's retention pin is deleted,
    /// and the reason the check is STRUCTURAL rather than economic — the file
    /// is refused outright, so there is no half-labelled catalog to serve.
    #[test]
    fn a_lane_whose_provider_declares_no_posture_refuses_the_file() {
        let error = catalog_with_retention("").expect_err("an unlabelled lane must not load");
        assert!(
            matches!(
                &error,
                TierConfigError::UnlabelledLane { provider, candidate, .. }
                    if provider == "anthropic" && candidate == "anthropic/pin"
            ),
            "expected an UnlabelledLane naming the provider, got {error:?}"
        );
        // And the message must name the block an operator has to add, because
        // this refusal is the only guidance they get.
        let rendered = error.to_string();
        assert!(
            rendered.contains("[retention.anthropic]"),
            "the refusal must name the missing block: {rendered}"
        );
    }

    /// A pin for a DIFFERENT provider does not label this lane. Guards against
    /// a resolution rule that checks "any pin exists" rather than "a pin for
    /// this candidate's provider exists".
    #[test]
    fn a_pin_for_another_provider_does_not_label_this_lane() {
        let error = catalog_with_retention(
            r#"
[retention.openai]
posture = "standard"
description = "retains"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
"#,
        )
        .expect_err("a pin for another provider labels nothing");
        assert!(matches!(
            error,
            TierConfigError::UnlabelledLane { ref provider, .. } if provider == "anthropic"
        ));
    }

    #[test]
    fn a_labelled_lane_loads_and_publishes_its_providers_posture() {
        let catalog = catalog_with_retention(GOOD_PIN).expect("a labelled lane loads");
        // Every provider credentialed: these assert the listing's SHAPE
        // (rates, metadata, posture), not which lanes a deployment can
        // reach. The credential filter has its own tests.
        let listing = catalog.model_listing(&|_| true);
        let row = &listing["anthropic/pin"];
        assert_eq!(row.retention.posture, RetentionPosture::Standard);
        assert_eq!(row.retention.description, "retains for 30 days");
        assert_eq!(row.retention.verified, "2026-08-20");
    }

    /// Each evidence field is load-bearing, so each must be refused when blank.
    /// A pin missing its URL or digest keeps asserting a claim with no
    /// re-verification loop behind it — the failure that looks fine forever.
    #[test]
    fn a_pin_with_a_blank_evidence_field_refuses_the_file() {
        for (field, blanked) in [
            ("description", GOOD_PIN.replace("retains for 30 days", "  ")),
            (
                "source_url",
                GOOD_PIN.replace("https://example.test/anthropic", ""),
            ),
            ("verified", GOOD_PIN.replace("2026-08-20", "")),
            ("source_sha256", GOOD_PIN.replace(&"a".repeat(64), "")),
        ] {
            let error =
                catalog_with_retention(&blanked).expect_err("a blank {field} must refuse the file");
            assert!(
                matches!(
                    error,
                    TierConfigError::IncompleteRetentionPin { .. }
                        | TierConfigError::InvalidRetentionSourceUrl { .. }
                ),
                "blanking {field} produced {error:?}"
            );
        }
    }

    #[test]
    fn a_pin_with_a_malformed_date_url_or_digest_refuses_the_file() {
        let cases = [
            // A date nobody can compare against is a date that cannot go stale.
            ("20th August 2026", "verified"),
        ];
        for (bad, field) in cases {
            let error = catalog_with_retention(&GOOD_PIN.replace("2026-08-20", bad))
                .expect_err("a malformed {field} must refuse the file");
            assert!(
                matches!(error, TierConfigError::InvalidRetentionDate { .. }),
                "{field}={bad} produced {error:?}"
            );
        }

        let error = catalog_with_retention(
            &GOOD_PIN.replace("https://example.test/anthropic", "example.test/anthropic"),
        )
        .expect_err("a non-http source_url must refuse the file");
        assert!(matches!(
            error,
            TierConfigError::InvalidRetentionSourceUrl { .. }
        ));

        let error = catalog_with_retention(&GOOD_PIN.replace(&"a".repeat(64), "not-a-digest"))
            .expect_err("a malformed digest must refuse the file");
        assert!(matches!(
            error,
            TierConfigError::InvalidRetentionDigest { .. }
        ));
    }

    /// A partial pin must not deserialize into a pin with blanks. Every field
    /// is required at the serde layer, so half a claim is not a claim.
    #[test]
    fn a_pin_missing_a_field_entirely_fails_to_parse() {
        let partial = r#"
[retention.anthropic]
posture = "standard"
description = "retains"
"#;
        let parsed: Result<TierCatalog, _> = toml::from_str(&format!(
            r#"
schema_version = 1
{partial}
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
"#
        ));
        assert!(parsed.is_err(), "a pin missing source_url must not parse");
    }

    /// A tier override replaces the provider pin for that tier alone.
    #[test]
    fn a_tier_override_replaces_its_providers_posture() {
        let catalog: TierCatalog = toml::from_str(&format!(
            r#"
schema_version = 1
{GOOD_PIN}
[tiers."anthropic/pin"]
[tiers."anthropic/pin".rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
[tiers."anthropic/pin".retention]
posture = "zero"
description = "negotiated ZDR endpoint"
source_url = "https://example.test/zdr-agreement"
verified = "2026-08-20"
source_sha256 = "1111111111111111111111111111111111111111111111111111111111111111"
[[tiers."anthropic/pin".candidates]]
id = "anthropic/pin"
provider = "anthropic"
model = "pin"
[tiers."anthropic/pin".candidates.rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
"#
        ))
        .expect("catalog should parse");
        validate_tier_catalog(&catalog).expect("an overridden tier loads");

        // Every provider credentialed: these assert the listing's SHAPE
        // (rates, metadata, posture), not which lanes a deployment can
        // reach. The credential filter has its own tests.
        let listing = catalog.model_listing(&|_| true);
        assert_eq!(
            listing["anthropic/pin"].retention.posture,
            RetentionPosture::Zero,
            "the tier override must win over the provider pin"
        );
        assert_eq!(
            listing["anthropic/pin"].retention.description,
            "negotiated ZDR endpoint"
        );
    }

    /// The narrowing rule: a routing alias that can reach a retaining rung
    /// retains, whichever order its candidates appear in.
    #[test]
    fn an_alias_advertises_the_weakest_posture_it_can_route_to() {
        assert_eq!(
            RetentionPosture::Zero.weaker(RetentionPosture::Standard),
            RetentionPosture::Standard
        );
        assert_eq!(
            RetentionPosture::Standard.weaker(RetentionPosture::Zero),
            RetentionPosture::Standard
        );
        assert_eq!(
            RetentionPosture::Zero.weaker(RetentionPosture::Zero),
            RetentionPosture::Zero
        );

        // And through a real two-rung alias: one zero rung, one retaining rung.
        let catalog: TierCatalog = toml::from_str(
            r#"
schema_version = 1
[retention.openai]
posture = "zero"
description = "retains nothing"
source_url = "https://example.test/openai"
verified = "2026-08-20"
source_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
[retention.anthropic]
posture = "standard"
description = "retains for 30 days"
source_url = "https://example.test/anthropic"
verified = "2026-08-20"
source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[tiers."zero/mixed"]
[tiers."zero/mixed".rates]
input_per_mtok = 5.00
output_per_mtok = 10.00
[[tiers."zero/mixed".candidates]]
id = "openai/private"
provider = "openai"
model = "private"
[tiers."zero/mixed".candidates.rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
[[tiers."zero/mixed".candidates]]
id = "anthropic/retaining"
provider = "anthropic"
model = "retaining"
[tiers."zero/mixed".candidates.rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
"#,
        )
        .expect("catalog should parse");
        validate_tier_catalog(&catalog).expect("a mixed alias loads");
        // Every provider credentialed: these assert the listing's SHAPE
        // (rates, metadata, posture), not which lanes a deployment can
        // reach. The credential filter has its own tests.
        let listing = catalog.model_listing(&|_| true);

        assert_eq!(
            listing["zero/mixed"].retention.posture,
            RetentionPosture::Standard,
            "an alias that can reach a retaining rung must not advertise zero"
        );
        // The concrete rows still carry their OWN provider's posture: a request
        // pinning `openai/private` reaches the zero-retention account whatever
        // the alias above it must advertise.
        assert_eq!(
            listing["openai/private"].retention.posture,
            RetentionPosture::Zero
        );
        assert_eq!(
            listing["anthropic/retaining"].retention.posture,
            RetentionPosture::Standard
        );
    }

    /// The ordering rank is written out rather than derived, so it gets a test
    /// of its own: this is what `ModelList::from_listing` sorts on.
    #[test]
    fn zero_ranks_before_standard() {
        assert!(
            RetentionPosture::Zero.ordering_rank() < RetentionPosture::Standard.ordering_rank(),
            "zero-retention lanes must sort before retaining ones"
        );
    }
}
