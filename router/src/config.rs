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
                    sell_rates: candidate.rates,
                    candidates: vec![candidate],
                })
        })
    }

    #[must_use]
    pub fn model_owners(&self) -> BTreeMap<String, String> {
        let mut models = BTreeMap::new();
        for (tier_id, definition) in &self.tiers {
            models.insert(tier_id.clone(), "zerorouter".to_owned());
            for candidate in &definition.candidates {
                models
                    .entry(candidate.id.clone())
                    .or_insert_with(|| candidate.provider.clone());
            }
        }
        models
    }
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
