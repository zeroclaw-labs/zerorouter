use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock, PoisonError},
};

use crate::provider::ModelRates;
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
    #[serde(deserialize_with = "deserialize_model_rates")]
    pub rates: ModelRates,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TierCandidate {
    pub id: String,
    pub provider: String,
    pub model: String,
    #[serde(deserialize_with = "deserialize_model_rates")]
    pub rates: ModelRates,
    /// What this model can take and produce. Declared here, beside the rates,
    /// because it is the same kind of claim: static, human-owned, and true
    /// only because someone checked. A candidate that declares none is
    /// `ModelMetadata::default()` — every field unknown — which is exactly how
    /// the file read before this table existed.
    #[serde(default)]
    pub metadata: ModelMetadata,
}

impl TierCandidate {
    /// Whether this candidate's PRICE is zero — the tier-file half of "free".
    ///
    /// Zero means the two REQUIRED dimensions are declared and exactly zero,
    /// and the optional cached-input dimension is either absent or zero. The
    /// asymmetry follows the file's existing convention rather than inventing
    /// one: `input_per_mtok` and `output_per_mtok` are mandatory (an absent one
    /// is a load error), while an absent `cached_input_per_mtok` means "unknown
    /// here" — the bedrock shape, kept because an upstream that reports no
    /// cached tokens has no cached rate to state. A local server has no cache
    /// pricing either, so requiring the operator to write `= 0` for it would
    /// make the commonest honest config silently *not* free, which is a worse
    /// failure than the one strictness buys. A DECLARED nonzero cached rate is
    /// still money, and still disqualifies.
    ///
    /// On its own this says nothing about whether anyone is billed — a rate
    /// table can be wrong, and a zero in it is as likely to be a typo as a
    /// claim. [`Self::is_free`] is the question with an answer.
    #[must_use]
    pub fn rates_are_zero(&self) -> bool {
        self.rates.input_per_mtok == Some(0.0)
            && self.rates.output_per_mtok == Some(0.0)
            && self
                .rates
                .cached_input_per_mtok
                .is_none_or(|rate| rate == 0.0)
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

#[derive(Clone, Debug)]
pub struct ResolvedRoute {
    pub requested_model: String,
    pub candidates: Vec<TierCandidate>,
    pub sell_rates: ModelRates,
}

#[derive(Debug, Deserialize)]
struct RawModelRates {
    input_per_mtok: Option<f64>,
    output_per_mtok: Option<f64>,
    cached_input_per_mtok: Option<f64>,
}

fn deserialize_model_rates<'de, D>(deserializer: D) -> Result<ModelRates, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = RawModelRates::deserialize(deserializer)?;
    Ok(ModelRates {
        input_per_mtok: raw.input_per_mtok,
        output_per_mtok: raw.output_per_mtok,
        cached_input_per_mtok: raw.cached_input_per_mtok,
    })
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
         {dimension} cost basis {basis} exceeds tier sell rate {sell}"
    )]
    NegativeMargin {
        tier: String,
        candidate: String,
        dimension: &'static str,
        basis: f64,
        sell: f64,
    },
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
                sell_rates: tier.rates,
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
                    // a model cannot undercut the tier price.
                    sell_rates: tier.rates,
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
    /// Metadata does *not* follow the sell-rate rule, and the asymmetry is
    /// deliberate. A price is a property of the tier a request is billed
    /// through, so a pinned candidate inherits its tier's. A context window is
    /// a property of the model itself: a request for
    /// `anthropic/claude-sonnet-5` reaches sonnet and gets sonnet's window
    /// whatever tier the id happens to sit under. So a candidate row carries
    /// its own metadata and a tier row carries
    /// [`ModelMetadata::narrowed`] across everything it can route to.
    #[must_use]
    pub fn model_listing(&self) -> BTreeMap<String, ModelListing> {
        let mut models = BTreeMap::new();
        for (tier_id, definition) in &self.tiers {
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
                models.insert(
                    tier_id.clone(),
                    ModelListing {
                        owned_by: "zerorouter".to_owned(),
                        sell_rates: definition.rates,
                        metadata: ModelMetadata::narrowed(&definition.candidates),
                    },
                );
            }
            for candidate in &definition.candidates {
                models
                    .entry(candidate.id.clone())
                    .or_insert_with(|| ModelListing {
                        owned_by: candidate.provider.clone(),
                        sell_rates: definition.rates,
                        metadata: candidate.metadata.clone(),
                    });
            }
        }
        models
    }
}

/// One row of the public catalog: the provider that serves this id, the sell
/// rate a request for it is billed at, and what the model can take and
/// produce.
#[derive(Clone, Debug)]
pub struct ModelListing {
    pub owned_by: String,
    pub sell_rates: ModelRates,
    pub metadata: ModelMetadata,
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
///   a malformed candidate, an unsupported provider, a duplicate concrete id —
///   mean the file itself cannot be trusted, so they still refuse the whole
///   catalog. Serving *part* of a file that is wrong about its own structure
///   would be guessing at the operator's intent.
/// - **Economic** faults — a candidate priced above its owning tier's sell
///   rate — condemn exactly one tier and nothing else. They are returned here
///   (tier id → the rendered [`TierConfigError::NegativeMargin`]) instead of
///   erroring, so the caller can withhold that tier and keep serving the rest.
///   The rule is unchanged; only its blast radius is.
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

    let mut concrete_ids = BTreeSet::new();
    let mut withheld: BTreeMap<String, TierConfigError> = BTreeMap::new();

    for (tier_id, definition) in &catalog.tiers {
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
        validate_rates(tier_id, definition.rates)?;

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
            validate_rates(tier_id, candidate.rates)?;
            validate_metadata(tier_id, candidate)?;
            validate_zero_price(tier_id, candidate)?;
            if let Err(error) = validate_candidate_margin(tier_id, definition.rates, candidate) {
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
/// - A dimension either side leaves unset is skipped, not read as zero. The
///   bedrock rows omit `cached_input_per_mtok` because Bedrock does not report
///   cached tokens at all; an absent basis is "unknown here", and treating it
///   as free (or as a violation) would both be wrong.
fn validate_candidate_margin(
    tier: &str,
    sell_rates: ModelRates,
    candidate: &TierCandidate,
) -> Result<(), TierConfigError> {
    for (dimension, basis, sell) in [
        (
            "input_per_mtok",
            candidate.rates.input_per_mtok,
            sell_rates.input_per_mtok,
        ),
        (
            "output_per_mtok",
            candidate.rates.output_per_mtok,
            sell_rates.output_per_mtok,
        ),
        (
            "cached_input_per_mtok",
            candidate.rates.cached_input_per_mtok,
            sell_rates.cached_input_per_mtok,
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
            });
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
fn validate_zero_price(tier: &str, candidate: &TierCandidate) -> Result<(), TierConfigError> {
    if candidate.rates_are_zero() && !provider_settles_free(&candidate.provider) {
        return Err(TierConfigError::ZeroPriceWithoutFreeSettlement {
            tier: tier.to_owned(),
            candidate: candidate.id.clone(),
            provider: candidate.provider.clone(),
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

    #[test]
    fn concrete_model_resolves_to_one_candidate() {
        let catalog: TierCatalog = toml::from_str(
            r#"
schema_version = 1
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
    fn an_absent_cached_basis_is_not_a_margin_violation() {
        // The bedrock shape: no `cached_input_per_mtok` on the candidate, and
        // an input basis well above the tier's *cached* sell rate. An unset
        // dimension is unknown, not free and not a violation — reading it as
        // the input rate would fail a table that is fine.
        let catalog = catalog_with(
            "input_per_mtok = 0.30\noutput_per_mtok = 1.20\ncached_input_per_mtok = 0.06",
            "input_per_mtok = 0.30\noutput_per_mtok = 1.20",
        );
        validate_tier_catalog(&catalog)
            .expect("a candidate that declares no cached rate must still validate");
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

        let listing = catalog.model_listing();
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
        let listing = catalog.model_listing();

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
        let listing = catalog.model_listing();

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

        let listing = catalog.model_listing();
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
            rates: ModelRates {
                input_per_mtok: input,
                cached_input_per_mtok: cached,
                output_per_mtok: output,
            },
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
}
