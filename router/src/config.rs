use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Deserializer};
use thiserror::Error;
use zeroclaw_providers::pricing::ModelRates;

use crate::providers::is_supported_provider;

pub const DEFAULT_TIER_CONFIG_PATH: &str = "config/tiers.toml";
pub const TIER_CONFIG_PATH_ENV: &str = "ZEROROUTER_TIERS_PATH";

#[derive(Clone, Debug, Deserialize)]
pub struct TierCatalog {
    pub schema_version: u32,
    pub tiers: BTreeMap<String, TierDefinition>,
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
    #[error("tier {tier} contains an invalid candidate")]
    InvalidCandidate { tier: String },
    #[error("tier {tier} must define at least one candidate")]
    EmptyTier { tier: String },
    #[error("unsupported provider {provider} in tier {tier}")]
    UnsupportedProvider { tier: String, provider: String },
    #[error("duplicate concrete model id {0}")]
    DuplicateModelId(String),
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

    /// The public `/v1/models` catalog: every tier id and every candidate id,
    /// each paired with who serves it and what it bills at. A candidate's
    /// `sell_rates` is always its *owning* tier's rate, never its own cost
    /// basis — mirroring `resolve`'s pinned-candidate rule above, so a model
    /// listing can never advertise a price cheaper than what a request for
    /// that id is actually metered at.
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
    let catalog: TierCatalog =
        toml::from_str(&source).map_err(|source| TierConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    validate_tier_catalog(&catalog)?;
    Ok(catalog)
}

fn validate_tier_catalog(catalog: &TierCatalog) -> Result<(), TierConfigError> {
    if catalog.schema_version != 1 {
        return Err(TierConfigError::UnsupportedSchema(catalog.schema_version));
    }

    let mut concrete_ids = BTreeSet::new();

    for (tier_id, definition) in &catalog.tiers {
        if !tier_id.starts_with("zero/") || tier_id.len() == "zero/".len() {
            return Err(TierConfigError::InvalidTierId(tier_id.clone()));
        }
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
            validate_candidate_margin(tier_id, definition.rates, candidate)?;
        }
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
/// Catching it here turns that leak into a startup failure.
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
