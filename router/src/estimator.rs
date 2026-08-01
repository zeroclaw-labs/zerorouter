//! The cost estimator's read path (design doc: "Cost estimator"; rollout
//! Stage 3b).
//!
//! An in-process cache of output-token percentiles per estimator cell, where
//! a cell is either `(task_signature, candidate)` — the selection grain — or
//! `(task_signature)` alone — the candidate-agnostic grain the response
//! `estimate` block shows and Stage 4's reservation sizing will read. The
//! percentiles themselves are one SQL aggregate over settled `usage_events`
//! rows ([`crate::db::output_token_percentiles`]); no ML, ever, in this
//! design.
//!
//! **Never on the request path.** Requests only ever read this cache; a miss
//! or a stale entry answers [`CellRead::Cold`] — exactly today's behavior —
//! and enqueues the cell for the background refresher
//! (`RouterState::spawn_estimator_refresher`), which batches the percentile
//! queries off-path. Restart = cold until warmed: the failure mode of the
//! whole estimator is the status quo, the same deliberate contract as
//! [`crate::health::ProviderHealth`], its neighbor on `RouterServices`.

use std::{
    collections::{HashMap, HashSet},
    sync::{Mutex, PoisonError, RwLock},
    time::Duration,
};

use tokio::time::Instant;

use crate::openai::TaskSignature;

/// Warm gate: a cell needs at least this many settled 200-rows in the
/// trailing window before its percentiles are usable (design doc: "n ≥ 50").
pub const WARM_MIN_ROWS: i64 = 50;

/// A cached cell older than this answers cold and is re-enqueued — the
/// design's ">5 min stale" rule.
pub const CELL_TTL: Duration = Duration::from_secs(5 * 60);

/// The background refresher's batching cadence.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Cells refreshed per batch. Generous against [`MAX_PENDING`]: one pass
/// drains everything a full pending set can hold.
pub const REFRESH_BATCH: usize = 1_024;

/// Bound on the cell map. Cells are per (user-scoped signature, candidate),
/// so the population grows with users; at this cap the map refuses new
/// inserts after evicting what is stale, and a refused cell simply stays
/// cold — safe, because cold is the status quo.
const MAX_CELLS: usize = 65_536;

/// Bound on the refresh queue. A full queue drops the enqueue; the next
/// request that misses re-offers the cell.
const MAX_PENDING: usize = 1_024;

/// One estimator cell's identity. `candidate: None` is the
/// candidate-agnostic per-signature cell.
///
/// The scheme is part of the key because signature values are not comparable
/// across schemes (migration 0007): a scheme-1 and a scheme-2 row for the
/// same request shape are different segments, and an estimator that grouped
/// them would train on a mixture.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CellKey {
    pub signature: String,
    pub scheme: i16,
    pub candidate: Option<String>,
}

impl CellKey {
    /// The candidate-agnostic cell for a request's segment.
    #[must_use]
    pub fn for_signature(signature: &TaskSignature) -> Self {
        Self {
            signature: signature.hex.clone(),
            scheme: signature.scheme,
            candidate: None,
        }
    }

    /// The selection-grain cell for one candidate of a request's segment.
    #[must_use]
    pub fn for_candidate(signature: &TaskSignature, candidate_id: &str) -> Self {
        Self {
            signature: signature.hex.clone(),
            scheme: signature.scheme,
            candidate: Some(candidate_id.to_owned()),
        }
    }
}

/// The output-token percentiles of one cell's trailing window, plus the row
/// count that produced them. `rows` is kept even below the warm gate so a
/// refreshed-but-thin cell is distinguishable from a never-measured one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputPercentiles {
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub rows: i64,
}

impl OutputPercentiles {
    /// Whether this measurement clears the warm gate.
    #[must_use]
    pub fn is_warm(&self) -> bool {
        self.rows >= WARM_MIN_ROWS
    }
}

/// What one cache read answers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellRead {
    /// Fresh and past the warm gate: the percentiles are usable.
    Warm(OutputPercentiles),
    /// Everything else — missing, stale, or measured thin. The caller uses
    /// the cold estimate (byte bound + `max_tokens`), which is exactly
    /// today's behavior. Missing and stale cells were enqueued for refresh
    /// as a side effect; a fresh-but-thin cell was not (its next
    /// re-measure arrives when its TTL lapses, not every request).
    Cold,
}

struct CachedCell {
    fetched_at: Instant,
    /// `None` when the window held zero rows; thin measurements keep their
    /// numbers so `rows` is inspectable, but only [`OutputPercentiles::is_warm`]
    /// cells ever answer [`CellRead::Warm`].
    measured: Option<OutputPercentiles>,
    /// The state's age offset when this cell was fetched. Aging must apply
    /// only to cells that existed when `age_cells` was called — a cell
    /// re-measured afterwards starts fresh — so freshness reads the DELTA
    /// between the current offset and this snapshot, not the whole offset.
    #[cfg(feature = "testing")]
    offset_at_fetch: Duration,
}

/// The estimator cache handle. Clones share state; `RouterServices` holds
/// one per process beside the auth cache and the health registry — and like
/// the auth cache it is a cache, not a source of truth.
#[derive(Default)]
pub struct EstimatorState {
    cells: RwLock<HashMap<CellKey, CachedCell>>,
    pending: Mutex<HashSet<CellKey>>,
    /// Test-only extra age added to every cell's elapsed time. An offset
    /// rather than backdated `fetched_at`s because `Instant` subtraction
    /// panics when the result would precede the host's monotonic epoch —
    /// which "six minutes ago" does on a freshly booted CI machine.
    #[cfg(feature = "testing")]
    age_offset: Mutex<Duration>,
}

impl EstimatorState {
    /// Read one cell. Cache-only — this is the request path's entire contact
    /// surface with the estimator.
    pub fn lookup(&self, key: &CellKey) -> CellRead {
        let now = Instant::now();
        {
            let cells = self.read_cells();
            if let Some(cell) = cells.get(key)
                && self.aged(cell, now) <= CELL_TTL
            {
                return match cell.measured {
                    Some(measured) if measured.is_warm() => CellRead::Warm(measured),
                    _ => CellRead::Cold,
                };
            }
        }
        // Missing or stale: cold now, refreshed soon.
        self.enqueue(key.clone());
        CellRead::Cold
    }

    /// Offer a cell to the refresh queue. Deduplicated; a full queue drops
    /// the offer (the next miss re-offers).
    pub fn enqueue(&self, key: CellKey) {
        let mut pending = self.lock_pending();
        if pending.len() < MAX_PENDING {
            pending.insert(key);
        }
    }

    /// Take up to `limit` queued cells for one refresh pass.
    pub fn drain_pending(&self, limit: usize) -> Vec<CellKey> {
        let mut pending = self.lock_pending();
        let keys: Vec<CellKey> = pending.iter().take(limit).cloned().collect();
        for key in &keys {
            pending.remove(key);
        }
        keys
    }

    /// Record one refresh result. `measured` is `None` when the window held
    /// zero rows. At the cell cap, stale cells are evicted first; if the map
    /// is still full the result is dropped and the cell stays cold — safe,
    /// because cold is the status quo.
    pub fn apply(&self, key: CellKey, measured: Option<OutputPercentiles>) {
        let now = Instant::now();
        let mut cells = self.write_cells();
        if cells.len() >= MAX_CELLS && !cells.contains_key(&key) {
            let mut evictable = Vec::new();
            for (cell_key, cell) in cells.iter() {
                if self.aged(cell, now) > CELL_TTL {
                    evictable.push(cell_key.clone());
                }
            }
            for cell_key in evictable {
                cells.remove(&cell_key);
            }
            if cells.len() >= MAX_CELLS {
                return;
            }
        }
        cells.insert(
            key,
            CachedCell {
                fetched_at: now,
                measured,
                #[cfg(feature = "testing")]
                offset_at_fetch: self.current_offset(),
            },
        );
    }

    /// Age every cached cell, so a test can cross [`CELL_TTL`] without
    /// touching the clock the rest of the router runs on. Additive on an
    /// offset — never a backdated `Instant`, which would panic on hosts
    /// whose uptime is shorter than the requested age.
    #[cfg(feature = "testing")]
    pub fn age_cells(&self, by: Duration) {
        let mut offset = self
            .age_offset
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *offset = offset.saturating_add(by);
    }

    /// How many cells are queued for refresh (testing visibility).
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.lock_pending().len()
    }

    /// A cell's elapsed time as the freshness check reads it: real elapsed,
    /// plus — in testing builds — however much aging was requested since the
    /// cell was fetched. Production builds read the plain elapsed.
    fn aged(&self, cell: &CachedCell, now: Instant) -> Duration {
        let elapsed = now.duration_since(cell.fetched_at);
        #[cfg(feature = "testing")]
        {
            elapsed.saturating_add(self.current_offset().saturating_sub(cell.offset_at_fetch))
        }
        #[cfg(not(feature = "testing"))]
        {
            elapsed
        }
    }

    #[cfg(feature = "testing")]
    fn current_offset(&self) -> Duration {
        *self
            .age_offset
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn read_cells(&self) -> std::sync::RwLockReadGuard<'_, HashMap<CellKey, CachedCell>> {
        // Poisoned state is stale advice, not a reason to fail requests:
        // every write is a complete assignment (same contract as the health
        // registry's lock).
        self.cells.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_cells(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<CellKey, CachedCell>> {
        self.cells.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashSet<CellKey>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> CellKey {
        CellKey {
            signature: "00112233aabbccdd".to_owned(),
            scheme: 2,
            candidate: Some(name.to_owned()),
        }
    }

    fn warm_measure() -> OutputPercentiles {
        OutputPercentiles {
            p50: 210.0,
            p90: 640.0,
            p99: 900.0,
            rows: WARM_MIN_ROWS,
        }
    }

    #[tokio::test]
    async fn a_miss_answers_cold_and_enqueues_once() {
        let estimator = EstimatorState::default();
        assert_eq!(estimator.lookup(&key("a")), CellRead::Cold);
        assert_eq!(estimator.lookup(&key("a")), CellRead::Cold);
        assert_eq!(
            estimator.drain_pending(REFRESH_BATCH),
            vec![key("a")],
            "repeated misses deduplicate to one queued refresh"
        );
        assert!(estimator.drain_pending(REFRESH_BATCH).is_empty());
    }

    #[tokio::test]
    async fn a_warm_cell_answers_its_percentiles_until_it_stales() {
        tokio::time::pause();
        let estimator = EstimatorState::default();
        estimator.apply(key("a"), Some(warm_measure()));
        assert_eq!(estimator.lookup(&key("a")), CellRead::Warm(warm_measure()));
        assert!(
            estimator.drain_pending(REFRESH_BATCH).is_empty(),
            "a fresh warm read must not enqueue"
        );

        tokio::time::advance(CELL_TTL + Duration::from_secs(1)).await;
        assert_eq!(
            estimator.lookup(&key("a")),
            CellRead::Cold,
            "a stale cell answers cold"
        );
        assert_eq!(
            estimator.drain_pending(REFRESH_BATCH),
            vec![key("a")],
            "and is re-enqueued"
        );
    }

    #[tokio::test]
    async fn a_thin_measurement_is_cold_but_not_requeried_every_read() {
        let estimator = EstimatorState::default();
        estimator.apply(
            key("a"),
            Some(OutputPercentiles {
                rows: WARM_MIN_ROWS - 1,
                ..warm_measure()
            }),
        );
        assert_eq!(estimator.lookup(&key("a")), CellRead::Cold);
        assert!(
            estimator.drain_pending(REFRESH_BATCH).is_empty(),
            "fresh-but-thin waits for its TTL, not for the next request"
        );
    }

    #[tokio::test]
    async fn an_empty_window_is_cached_as_cold_too() {
        let estimator = EstimatorState::default();
        estimator.apply(key("a"), None);
        assert_eq!(estimator.lookup(&key("a")), CellRead::Cold);
        assert!(estimator.drain_pending(REFRESH_BATCH).is_empty());
    }

    #[tokio::test]
    async fn the_pending_queue_is_bounded_and_drops_offers_at_the_cap() {
        let estimator = EstimatorState::default();
        for index in 0..(MAX_PENDING + 10) {
            estimator.enqueue(key(&format!("candidate-{index}")));
        }
        let drained = estimator.drain_pending(usize::MAX);
        assert_eq!(drained.len(), MAX_PENDING);
    }

    #[tokio::test]
    async fn aging_cells_crosses_the_ttl_without_a_clock() {
        let estimator = EstimatorState::default();
        estimator.apply(key("a"), Some(warm_measure()));
        estimator.age_cells(CELL_TTL + Duration::from_secs(1));
        assert_eq!(estimator.lookup(&key("a")), CellRead::Cold);
        assert_eq!(estimator.drain_pending(REFRESH_BATCH), vec![key("a")]);
    }
}
