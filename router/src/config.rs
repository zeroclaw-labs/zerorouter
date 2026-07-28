use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock, PoisonError},
};

use serde::{Deserialize, Deserializer};
use thiserror::Error;
use zeroclaw_providers::pricing::ModelRates;

use crate::{
    openai::{MAX_RATE_PER_MTOK, billable_rate},
    priority::Priority,
    providers::is_supported_provider,
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
    #[error("tier id must begin with 'zero/': {0}")]
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

    /// The public `/v1/models` catalog: every *servable* tier id and every
    /// candidate id under it, each paired with who serves it and what it bills
    /// at. A candidate's `sell_rates` is always its *owning* tier's rate, never
    /// its own cost basis — mirroring `resolve`'s pinned-candidate rule above,
    /// so a model listing can never advertise a price cheaper than what a
    /// request for that id is actually metered at.
    ///
    /// Withheld tiers are absent from `tiers` and so are their candidates:
    /// the catalog never offers a model that a request for it would refuse.
    #[must_use]
    pub fn model_listing(&self) -> BTreeMap<String, ModelListing> {
        let mut models = BTreeMap::new();
        for (tier_id, definition) in &self.tiers {
            models.insert(
                tier_id.clone(),
                ModelListing {
                    owned_by: "zerorouter".to_owned(),
                    sell_rates: definition.rates,
                },
            );
            for candidate in &definition.candidates {
                models
                    .entry(candidate.id.clone())
                    .or_insert_with(|| ModelListing {
                        owned_by: candidate.provider.clone(),
                        sell_rates: definition.rates,
                    });
            }
        }
        models
    }
}

/// One row of the public catalog: the provider that serves this id, and the
/// sell rate a request for it is billed at.
#[derive(Clone, Debug)]
pub struct ModelListing {
    pub owned_by: String,
    pub sell_rates: ModelRates,
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
        if !tier_id.starts_with("zero/") || tier_id.len() == "zero/".len() {
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
provider = "fireworks"
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
provider = "fireworks"
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
provider = "fireworks"
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
id = "fireworks/cheap"
provider = "fireworks"
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
        assert!(catalog.resolve("fireworks/cheap").is_some());
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
            ["fireworks/cheap", "zero/healthy"]
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
id = "fireworks/cheap"
provider = "fireworks"
model = "upstream/cheap"
[tiers."zero/below-cost".candidates.rates]
input_per_mtok = 0.10
output_per_mtok = 0.20
"#,
        );

        let error = validate_tier_catalog(&catalog)
            .expect_err("a repeated concrete id must refuse the whole catalog");
        assert!(
            matches!(error, TierConfigError::DuplicateModelId(ref id) if id == "fireworks/cheap"),
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
id = "fireworks/fast"
provider = "fireworks"
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
id = "fireworks/fast:cost"
provider = "fireworks"
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
            matches!(error, TierConfigError::PrioritySuffixCollision(ref id) if id == "fireworks/fast:cost"),
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
provider = "fireworks"
model = "upstream/fast"
[tiers."zero/test".candidates.rates]
input_per_mtok = 1
output_per_mtok = 2
"#,
        )
        .expect("catalog should parse");
        validate_tier_catalog(&catalog)
            .expect("a non-colliding colon id should validate");
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
id = "fireworks/one"
provider = "fireworks"
model = "upstream/one"
[tiers."zero/one".candidates.rates]
input_per_mtok = 9.00
output_per_mtok = 2.00

[tiers."zero/two"]
[tiers."zero/two".rates]
input_per_mtok = 1.00
output_per_mtok = 2.00
[[tiers."zero/two".candidates]]
id = "together/two"
provider = "together"
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
