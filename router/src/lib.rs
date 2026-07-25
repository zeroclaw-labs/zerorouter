pub mod admin;
pub mod api;
pub mod auth;
pub mod billing;
pub mod config;
pub mod db;
pub mod device;
pub mod error;
pub mod logging;
pub mod oidc;
pub mod openai;
pub mod portal;
pub mod providers;
pub mod session;
mod sqlx;
pub mod stripe;
#[cfg(feature = "testing")]
pub mod testing;
pub mod web;

pub use api::{RouterState, app};
pub use config::{DEFAULT_TIER_CONFIG_PATH, TIER_CONFIG_PATH_ENV, TierCatalog, load_tier_catalog};
