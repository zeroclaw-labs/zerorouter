//! Cross-request provider health (design: "Provider-health state", stage 2b).
//!
//! An in-process registry keyed `(provider, upstream_model)` — the rung as an
//! upstream endpoint, not as a catalog entry, so two tiers naming the same
//! upstream share one verdict while two models on one provider stay separate
//! (`tests/two_on_one_provider_tiers.toml` pins the latter). Each rung holds:
//!
//! - `error_ewma` (α = 0.3): the availability error rate. Raised by outcomes
//!   that say the upstream did not answer (`upstream_error`, `timeout`,
//!   `stream_error`), decayed by a completion (`ok`).
//! - `cooldown_until`: set 60 seconds ahead by a `rate_limited` outcome.
//!
//! A rung is **demoted** while `error_ewma > 0.5` or while cooling. The walk
//! reads that through [`ProviderHealth::should_skip`] at the top of its
//! candidate loop and records a `health_skipped` row instead of dispatching —
//! demotion-as-skip is the shape migration 0004 already documents for that
//! outcome ("not dispatched: health demotion skipped this rung"); ordering
//! modes that sink a demoted rung to the back instead arrive with the knob
//! (rollout stage 3a).
//!
//! # What updates what, and why
//!
//! The registry answers one question — is this upstream endpoint worth a
//! dispatch right now? — so only outcomes that speak to availability move it:
//!
//! - `ok` is direct evidence the rung serves: it decays the EWMA **and clears
//!   any cooldown**. The clear matters on a single-candidate route, where the
//!   walk still dispatches into a cooldown (see the skip guard in
//!   `crate::api`) and a success must not leave the rung marked cooling for
//!   every other tier that shares it.
//! - `rate_limited` sets the cooldown and deliberately does NOT raise the
//!   EWMA. A 429 is pressure, not brokenness: it is time-bounded and
//!   self-heals when the window passes. Folding it into the EWMA would let a
//!   chronically busy rung ratchet past the demotion threshold, where —
//!   because a demoted rung is skipped and a skipped rung is never observed —
//!   no evidence could ever bring it back before a restart.
//! - `upstream_error`, `timeout`, `stream_error` raise the EWMA. A shared-
//!   deadline expiry can land a `timeout` on a rung that inherited a spent
//!   clock, which slightly over-counts; that is accepted rather than special-
//!   cased, because under-counting real hangs is the worse failure.
//! - `validation_failed` is neutral: the upstream completed and the governing
//!   check rejected the content — an answer-quality fact for the stage-5a
//!   estimator, not an availability one. `aborted` is neutral: the router
//!   ended the attempt, not the upstream. The two skip outcomes were never
//!   dispatched, so they carry no evidence at all.
//!
//! # Deliberately in-process
//!
//! Single-instance, lost on restart — the design defers persisted or shared
//! health explicitly. Today's deploy is one task, and the failure mode of
//! cold health state is exactly today's (pre-health) behavior. The durable,
//! restart-surviving record of every attempt outcome is `request_attempts`;
//! this registry is only the walk's working memory of it.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use tokio::time::Instant;

use crate::{config::TierCandidate, db::AttemptRecord};

/// EWMA smoothing factor: one observation carries 0.3 of the verdict.
pub const ERROR_EWMA_ALPHA: f64 = 0.3;

/// Demotion threshold on `error_ewma`, exclusive. From a cold rung it takes
/// two consecutive availability failures to cross (0.3, then 0.51), and one
/// completion to fall back under (0.51 → 0.357).
pub const DEMOTION_THRESHOLD: f64 = 0.5;

/// How long one 429 cools a rung.
pub const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(60);

/// The registry handle. Clones share one map; the router holds one per
/// process and every walk observes into it through [`crate::health::WalkLedger`].
#[derive(Clone, Default)]
pub struct ProviderHealth {
    rungs: Arc<Mutex<HashMap<(String, String), RungHealth>>>,
}

#[derive(Clone, Copy, Default)]
struct RungHealth {
    error_ewma: f64,
    cooldown_until: Option<Instant>,
}

impl RungHealth {
    fn demoted(self, now: Instant) -> bool {
        self.error_ewma > DEMOTION_THRESHOLD || self.cooldown_until.is_some_and(|until| now < until)
    }
}

impl ProviderHealth {
    /// Fold one attempt outcome into the rung it dispatched to (or, for a
    /// skip, deliberately do nothing). Called from the single
    /// [`WalkLedger::push`] funnel so no recording site can forget it.
    pub fn observe(&self, attempt: &AttemptRecord) {
        enum Update {
            Success,
            Failure,
            RateLimited,
        }
        // The migration-0004 outcome vocabulary, partitioned by what it says
        // about availability (module docs). A label this match does not know
        // is treated as no evidence rather than guessed at.
        let update = match attempt.outcome.as_str() {
            "ok" => Update::Success,
            "rate_limited" => Update::RateLimited,
            "upstream_error" | "timeout" | "stream_error" => Update::Failure,
            _ => return,
        };
        let mut rungs = self.lock();
        let rung = rungs
            .entry((
                attempt.upstream_provider.clone(),
                attempt.upstream_model.clone(),
            ))
            .or_default();
        match update {
            Update::Success => {
                rung.error_ewma *= 1.0 - ERROR_EWMA_ALPHA;
                rung.cooldown_until = None;
            }
            Update::Failure => {
                rung.error_ewma = ERROR_EWMA_ALPHA + (1.0 - ERROR_EWMA_ALPHA) * rung.error_ewma;
            }
            Update::RateLimited => {
                rung.cooldown_until = Some(Instant::now() + RATE_LIMIT_COOLDOWN);
            }
        }
    }

    /// Whether this candidate is demoted. Two consumers, one verdict:
    /// `api::order_candidates` sinks a demoted rung to the back of the route
    /// (demotion's first line since stage 3a), and the walk records
    /// `health_skipped` and moves on when it still reaches one (the
    /// backstop). Advisory either way: ordering never removes a rung, and
    /// the walk's own guard keeps a skip from ever leaving a request with
    /// nothing to dispatch.
    #[must_use]
    pub fn should_skip(&self, candidate: &TierCandidate) -> bool {
        let now = Instant::now();
        self.lock()
            .get(&(candidate.provider.clone(), candidate.model.clone()))
            .is_some_and(|rung| rung.demoted(now))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), RungHealth>> {
        // A poisoned registry is stale advice, not a reason to fail requests:
        // every write is a complete assignment, so whatever the panicking
        // thread left behind is still a coherent map.
        self.rungs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The walk ledger: the rows a walk will drain into its settle transaction,
/// wrapped so that recording an attempt IS observing it. This is the single
/// `attempts.push(...)` funnel the design names — a new terminal that records
/// a row cannot bypass health, because the only way to record a row is here.
pub struct WalkLedger {
    health: ProviderHealth,
    rows: Vec<AttemptRecord>,
}

impl WalkLedger {
    #[must_use]
    pub fn new(health: ProviderHealth) -> Self {
        Self {
            health,
            rows: Vec::new(),
        }
    }

    /// Record one attempt row, folding its outcome into the health registry.
    pub fn push(&mut self, row: AttemptRecord) {
        self.health.observe(&row);
        self.rows.push(row);
    }

    /// Rows recorded so far; the walk's 1-based attempt ordinal is
    /// `len() + 1`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rows recorded so far, in walk order — read by the response-block
    /// builder (`api::zerorouter_block`); draining stays with
    /// [`WalkLedger::take_rows`]/[`WalkLedger::into_rows`], so a peek cannot
    /// detach a row from its settle transaction.
    #[must_use]
    pub fn rows(&self) -> &[AttemptRecord] {
        &self.rows
    }

    /// Drain the rows for a settle site, leaving the ledger empty but usable —
    /// the `std::mem::take` shape the walk terminals already speak.
    pub fn take_rows(&mut self) -> Vec<AttemptRecord> {
        std::mem::take(&mut self.rows)
    }

    /// The rows, for a terminal that consumes the walk.
    #[must_use]
    pub fn into_rows(self) -> Vec<AttemptRecord> {
        self.rows
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use zeroclaw_providers::pricing::ModelRates;

    use super::*;
    use crate::db::AttemptTokens;

    fn candidate(provider: &str, model: &str) -> TierCandidate {
        TierCandidate {
            id: format!("{provider}/{model}"),
            provider: provider.to_owned(),
            model: model.to_owned(),
            rates: ModelRates {
                input_per_mtok: Some(1.0),
                output_per_mtok: Some(2.0),
                cached_input_per_mtok: None,
            },
        }
    }

    fn attempt(provider: &str, model: &str, outcome: &str) -> AttemptRecord {
        AttemptRecord {
            attempt_no: 1,
            started_at: Utc::now(),
            candidate_id: format!("{provider}/{model}"),
            upstream_provider: provider.to_owned(),
            upstream_model: model.to_owned(),
            outcome: outcome.to_owned(),
            served: false,
            latency_ms: 0,
            tokens: AttemptTokens::unknown(),
            tokens_estimated: false,
            cost_basis_usd: None,
            finish_reason: None,
            validator_kind: None,
        }
    }

    fn observe(health: &ProviderHealth, outcome: &str) {
        health.observe(&attempt("prov", "model", outcome));
    }

    #[test]
    fn a_rung_never_observed_is_not_demoted() {
        let health = ProviderHealth::default();
        assert!(!health.should_skip(&candidate("prov", "model")));
    }

    /// From cold, the EWMA crosses the threshold on the second consecutive
    /// availability failure (0.3, then 0.51) — one flake is not a verdict.
    #[test]
    fn two_consecutive_failures_demote_and_one_does_not() {
        for failure in ["upstream_error", "timeout", "stream_error"] {
            let health = ProviderHealth::default();
            observe(&health, failure);
            assert!(
                !health.should_skip(&candidate("prov", "model")),
                "one {failure} must not demote"
            );
            observe(&health, failure);
            assert!(
                health.should_skip(&candidate("prov", "model")),
                "two of {failure} must demote"
            );
        }
    }

    /// One completion pulls a just-demoted rung back under the threshold
    /// (0.51 → 0.357): recovery takes one piece of positive evidence, not a
    /// clean streak.
    #[test]
    fn a_success_recovers_a_demoted_rung() {
        let health = ProviderHealth::default();
        observe(&health, "upstream_error");
        observe(&health, "upstream_error");
        assert!(health.should_skip(&candidate("prov", "model")));
        observe(&health, "ok");
        assert!(!health.should_skip(&candidate("prov", "model")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_429_cools_the_rung_for_exactly_the_cooldown() {
        let health = ProviderHealth::default();
        observe(&health, "rate_limited");
        assert!(health.should_skip(&candidate("prov", "model")));
        tokio::time::advance(RATE_LIMIT_COOLDOWN - Duration::from_secs(1)).await;
        assert!(
            health.should_skip(&candidate("prov", "model")),
            "still cooling one second before expiry"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(
            !health.should_skip(&candidate("prov", "model")),
            "a cooldown expires on its own"
        );
    }

    /// 429s set the cooldown and only the cooldown: however many arrive, once
    /// the window passes the rung is dispatchable again. An EWMA fed by 429s
    /// would ratchet a busy rung past the threshold, where — skipped and
    /// therefore never observed — nothing could bring it back.
    #[tokio::test(start_paused = true)]
    async fn rate_limits_do_not_accumulate_into_demotion() {
        let health = ProviderHealth::default();
        for _ in 0..10 {
            observe(&health, "rate_limited");
        }
        tokio::time::advance(RATE_LIMIT_COOLDOWN + Duration::from_secs(1)).await;
        assert!(
            !health.should_skip(&candidate("prov", "model")),
            "any number of 429s is pressure, not brokenness"
        );
    }

    /// A completion is direct evidence the rung serves again, so it ends a
    /// cooldown early rather than leaving the rung marked cooling for every
    /// tier that shares it.
    #[tokio::test(start_paused = true)]
    async fn a_success_clears_a_cooldown() {
        let health = ProviderHealth::default();
        observe(&health, "rate_limited");
        assert!(health.should_skip(&candidate("prov", "model")));
        observe(&health, "ok");
        assert!(!health.should_skip(&candidate("prov", "model")));
    }

    /// Keyed `(provider, upstream_model)`: two models on one provider are two
    /// rungs, and two catalog entries naming the same upstream are one.
    #[test]
    fn verdicts_are_keyed_by_provider_and_model() {
        let health = ProviderHealth::default();
        health.observe(&attempt("together", "model-a", "upstream_error"));
        health.observe(&attempt("together", "model-a", "upstream_error"));
        assert!(health.should_skip(&candidate("together", "model-a")));
        assert!(
            !health.should_skip(&candidate("together", "model-b")),
            "a sibling model on the same provider keeps its own verdict"
        );
        let mut twin = candidate("together", "model-a");
        twin.id = "another-tier/same-upstream".to_owned();
        assert!(
            health.should_skip(&twin),
            "the verdict follows the upstream endpoint, not the catalog id"
        );
    }

    /// Outcomes that say nothing about availability leave the registry alone:
    /// completed-but-rejected answers, router-side aborts, and rows for
    /// attempts that were never dispatched.
    #[test]
    fn non_availability_outcomes_are_neutral() {
        let health = ProviderHealth::default();
        for _ in 0..10 {
            for outcome in [
                "validation_failed",
                "aborted",
                "health_skipped",
                "policy_skipped",
            ] {
                observe(&health, outcome);
            }
        }
        assert!(!health.should_skip(&candidate("prov", "model")));
    }

    /// Recording through the ledger IS observing: the walk cannot push a row
    /// health does not see, and draining rows for a settle site does not
    /// disturb what was learned.
    #[test]
    fn the_ledger_observes_every_row_it_records() {
        let health = ProviderHealth::default();
        let mut ledger = WalkLedger::new(health.clone());
        ledger.push(attempt("prov", "model", "upstream_error"));
        ledger.push(attempt("prov", "model", "upstream_error"));
        assert_eq!(ledger.len(), 2);
        assert!(health.should_skip(&candidate("prov", "model")));

        let rows = ledger.take_rows();
        assert_eq!(rows.len(), 2);
        assert!(ledger.is_empty());
        assert!(
            health.should_skip(&candidate("prov", "model")),
            "draining the ledger must not forget what it observed"
        );

        ledger.push(attempt("prov", "model", "ok"));
        assert_eq!(ledger.into_rows().len(), 1);
        assert!(!health.should_skip(&candidate("prov", "model")));
    }
}
