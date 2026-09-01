-- Record the cache-WRITE portion of a prompt, so a settled row explains its
-- own price.
--
-- # What was wrong
--
-- `usage_events` has carried `input_tokens` and `cached_input_tokens` since
-- 0001, on the contract that cached is a SUBSET of input and the remainder is
-- fresh. Two rates, two buckets, and `cost_usd` was reproducible from the row.
--
-- Anthropic reports three buckets, not two: tokens read from its prompt cache,
-- tokens WRITTEN into it under a `cache_control` breakpoint, and tokens that
-- touched neither. The Anthropic wire has always folded the write bucket into
-- the input total, where it became indistinguishable from a fresh read — so
-- ZeroRouter billed writes at 1x while Anthropic invoiced 1.25x for them. The
-- wire sets three breakpoints of its own on every request, so that was not an
-- edge case: it was every Claude request the router has ever served.
--
-- With `cache_write_per_mtok` in the catalog the premium is now metered. That
-- makes the two-column row unable to explain itself — a Claude row's `cost_usd`
-- stops being derivable from `input_tokens`, `cached_input_tokens` and the rate
-- snapshot, because a third bucket at a third rate is hidden inside the first.
-- This column is that bucket.
--
-- # The reading
--
-- `cache_write_input_tokens` is a subset of `input_tokens` and is DISJOINT from
-- `cached_input_tokens`. A token is read from the cache, written to it, or
-- neither, and the fresh remainder is
-- `input_tokens - cached_input_tokens - cache_write_input_tokens`.
--
-- NULL means the write count was never captured: the row predates this
-- migration, or the upstream reports no such dimension (every wire but the two
-- that speak the Anthropic Messages dialect). It is NOT "zero writes" for a
-- historical row, and a census must date-bound itself accordingly. There is no
-- backfill — `usage_events` is append-only and the fact was never recorded.
--
-- Rows from before this migration are not wrong about what was CHARGED; they
-- are complete records of a cheaper price. Do not attempt to restate them.
--
-- # request_attempts gets the same column, for the same reason
--
-- Per-attempt COGS is priced from the attempt's own token counts
-- (`AttemptTokens::priceable`). An attempt that lost the write bucket would
-- report a cost basis 25% under the invoice on precisely the lanes this
-- dimension exists for, and `attempts_cost_basis_usd` would silently disagree
-- with the served row beside it.

ALTER TABLE usage_events
    ADD COLUMN cache_write_input_tokens BIGINT;

ALTER TABLE request_attempts
    ADD COLUMN cache_write_input_tokens BIGINT;

COMMENT ON COLUMN usage_events.cache_write_input_tokens IS
    'Prompt tokens written into the upstream cache under a cache_control breakpoint, billed at cache_write_per_mtok. A subset of input_tokens, disjoint from cached_input_tokens; fresh = input - cached - cache_write. NULL = not captured (predates migration 0029, or the wire reports no such dimension), never "no writes"';

COMMENT ON COLUMN request_attempts.cache_write_input_tokens IS
    'The same bucket for one attempt, so its cost_basis_usd prices the 1.25x write premium rather than a fresh read. NULL = not measured, which also makes attempts_cost_basis_complete false';
