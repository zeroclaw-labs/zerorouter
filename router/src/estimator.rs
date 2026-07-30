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
use uuid::Uuid;

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

/// Reservation-sizing headroom over the segment's p99 (design doc: "Cost
/// estimator" / Risks §1 — "p99 × 1.25").
pub const RESERVATION_HEADROOM: f64 = 1.25;

/// The structural floor: a learned reservation never drops below this
/// fraction of the requested `max_tokens`. Caps per-row clamp loss at
/// `0.75 × requested_max × sell output rate` no matter how poisoned the
/// percentile, and makes dilution self-defeating — filler traffic carrying
/// a huge `max_tokens` still reserves a quarter of it, burning the
/// attacker's own velocity and balance headroom.
pub const RESERVATION_FLOOR_FRACTION: f64 = 0.25;

/// Heavy-tail gate: a segment whose p99/p50 exceeds this never leaves cold
/// sizing — a fat right tail is exactly where a percentile reservation
/// under-covers.
pub const TAIL_GATE_RATIO: f64 = 8.0;

/// How long a revert keeps a segment (or user) on cold sizing. Trailing
/// windows keep re-firing the trigger while the loss evidence is inside
/// them, so the effective cold period is at least this and at most the
/// window plus this.
pub const REVERT_COOLDOWN: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Auto-revert thresholds (design doc: "Auto-revert triggers on dollars,
/// not rates"). Judgment-set numbers to be re-fit against Stage-4 shadow
/// telemetry; the dollar-denominated mechanism is the commitment. The
/// clamp-hit RATE stays secondary because a rate is dilutable with filler
/// traffic while per-row dollar losses are not.
pub const SEGMENT_LOSS_LIMIT_7D_USD: f64 = 10.0;
pub const ROW_LOSS_LIMIT_USD: f64 = 1.0;
pub const SEGMENT_HIT_RATE_LIMIT: f64 = 0.005;
pub const USER_LOSS_LIMIT_30D_USD: f64 = 50.0;

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

/// The learned output-token reservation for one request, or `None` when the
/// segment's shape disqualifies it:
/// `min(requested_max, max(p99 × 1.25, 0.25 × requested_max))`.
///
/// The tail gate runs here — p99/p50 > 8 (or a degenerate p50 ≤ 0) answers
/// `None` — so no caller can size from a heavy-tailed segment by forgetting
/// a check. The result never exceeds `requested_max_tokens`, which keeps the
/// learned bound inside the byte bound it replaces: admission can only get
/// SMALLER under this function, never larger.
#[must_use]
pub fn learned_output_bound(
    percentiles: OutputPercentiles,
    requested_max_tokens: u32,
) -> Option<u32> {
    if percentiles.p50 <= 0.0 || percentiles.p99 / percentiles.p50 > TAIL_GATE_RATIO {
        return None;
    }
    let requested = f64::from(requested_max_tokens);
    let sized = (percentiles.p99 * RESERVATION_HEADROOM).ceil();
    let floor = (requested * RESERVATION_FLOOR_FRACTION).ceil();
    let bound = sized.max(floor).min(requested);
    // The min against `requested` bounds this inside u32 range.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(bound as u32)
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
    /// Segments (by signature) whose learned sizing is reverted until the
    /// marked instant. In-process like the cells, deliberately: the loss
    /// evidence lives in settled rows, and the refresher's next evaluation
    /// re-fires any revert a restart forgot — the state is a cache of a
    /// verdict the database can always re-derive.
    reverted_segments: Mutex<HashMap<(String, i16), Instant>>,
    /// Users whose EVERY segment is reverted (trailing-30d aggregate).
    /// Segments are user-scoped, so re-slicing traffic cannot escape this.
    reverted_users: Mutex<HashMap<Uuid, Instant>>,
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

    /// Put one segment on cold sizing for [`REVERT_COOLDOWN`].
    pub fn revert_segment(&self, signature: &str, scheme: i16) {
        let until = Instant::now() + REVERT_COOLDOWN;
        self.lock_reverted_segments()
            .insert((signature.to_owned(), scheme), until);
    }

    /// Put every segment of one user on cold sizing for [`REVERT_COOLDOWN`].
    pub fn revert_user(&self, user_id: Uuid) {
        let until = Instant::now() + REVERT_COOLDOWN;
        self.lock_reverted_users().insert(user_id, until);
    }

    /// Whether this segment's learned sizing is currently reverted. Expired
    /// marks are dropped on read, so the maps cannot accrete forever.
    #[must_use]
    pub fn segment_reverted(&self, signature: &TaskSignature) -> bool {
        let now = Instant::now();
        let mut reverted = self.lock_reverted_segments();
        match reverted.get(&(signature.hex.clone(), signature.scheme)) {
            Some(until) if *until > now => true,
            Some(_) => {
                reverted.remove(&(signature.hex.clone(), signature.scheme));
                false
            }
            None => false,
        }
    }

    /// Whether every segment of this user is currently reverted.
    #[must_use]
    pub fn user_reverted(&self, user_id: Uuid) -> bool {
        let now = Instant::now();
        let mut reverted = self.lock_reverted_users();
        match reverted.get(&user_id) {
            Some(until) if *until > now => true,
            Some(_) => {
                reverted.remove(&user_id);
                false
            }
            None => false,
        }
    }

    fn lock_reverted_segments(&self) -> std::sync::MutexGuard<'_, HashMap<(String, i16), Instant>> {
        self.reverted_segments
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_reverted_users(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Instant>> {
        self.reverted_users
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

#[cfg(test)]
mod sizing_tests {
    use super::*;

    fn shaped(p50: f64, p99: f64) -> OutputPercentiles {
        OutputPercentiles {
            p50,
            p90: p99,
            p99,
            rows: WARM_MIN_ROWS,
        }
    }

    #[test]
    fn the_floor_binds_when_the_segment_is_terse() {
        // p99 × 1.25 = 250 sits under 0.25 × 4096 = 1024.
        assert_eq!(
            learned_output_bound(shaped(180.0, 200.0), 4_096),
            Some(1_024)
        );
    }

    #[test]
    fn the_percentile_binds_between_floor_and_cap() {
        // p99 × 1.25 = 2500, between the 1024 floor and the 4096 cap.
        assert_eq!(
            learned_output_bound(shaped(1_900.0, 2_000.0), 4_096),
            Some(2_500)
        );
    }

    #[test]
    fn the_requested_max_caps_the_learned_bound() {
        // p99 × 1.25 = 6250 exceeds the request's own 4096: the learned
        // bound can only ever shrink admission, never grow it.
        assert_eq!(
            learned_output_bound(shaped(4_900.0, 5_000.0), 4_096),
            Some(4_096)
        );
    }

    #[test]
    fn heavy_tails_and_degenerate_medians_never_size() {
        // p99/p50 = 10 > 8: exactly where a percentile reservation
        // under-covers.
        assert_eq!(learned_output_bound(shaped(100.0, 1_000.0), 4_096), None);
        // A zero median cannot form a ratio and cannot be trusted either.
        assert_eq!(learned_output_bound(shaped(0.0, 100.0), 4_096), None);
    }

    #[tokio::test]
    async fn reverts_hold_for_their_cooldown_and_expire() {
        tokio::time::pause();
        let estimator = EstimatorState::default();
        let signature = TaskSignature {
            hex: "00112233aabbccdd".to_owned(),
            scheme: 2,
            tool_names_sha256: String::new(),
        };
        let user = Uuid::new_v4();
        assert!(!estimator.segment_reverted(&signature));
        assert!(!estimator.user_reverted(user));

        estimator.revert_segment(&signature.hex, signature.scheme);
        estimator.revert_user(user);
        assert!(estimator.segment_reverted(&signature));
        assert!(estimator.user_reverted(user));

        tokio::time::advance(REVERT_COOLDOWN + Duration::from_secs(1)).await;
        assert!(
            !estimator.segment_reverted(&signature),
            "an expired revert clears on read"
        );
        assert!(!estimator.user_reverted(user));
    }
}
