-- Make the streaming usage gap countable, and record where a finish reason
-- came from.
--
-- # The gap that could not be counted
--
-- A chat-completions stream can settle with no usage for two very different
-- reasons, and `wire.rs` has distinguished them since edge mode's stage 3:
--
--   * `include_usage_ignored` — the stream framed itself correctly (`[DONE]`
--     arrived) and simply never sent the optional usage chunk. Several local
--     servers do this on every request. Ordinary.
--   * `done_missing` — the socket closed after a real `finish_reason` without
--     the sentinel AND without usage. That is what a truncating proxy in front
--     of an upstream that DOES report usage looks like. Not ordinary.
--
-- Both bill nothing, which is exactly why they must stay apart: folding the
-- second into the first lets a fleet-wide middlebox quietly erase revenue while
-- looking like a known, tolerated limitation.
--
-- Until now the distinction existed only as a tracing field. The metered lane
-- could at least be audited after the fact, by joining a served attempt against
-- an all-zero settled row (see `StreamDelivery::settled_usage`). The FREE lane
-- cannot: it writes no `request_attempts` rows, so a free-lane gap produced a
-- $0 row with nothing to join against and dropped out of the audit entirely.
-- That was tolerable while the amount at stake was definitionally zero — a hole
-- in a dashboard, not in a bill — but it also meant the one lane where local
-- upstreams actually run was the one lane whose gaps were invisible. The label
-- now lands on the row itself, on both lanes, so counting a middlebox does not
-- require reconstructing it from an absence.
--
-- NULL means one of: usage was reported, or this row is not a chat-completions
-- stream, or it predates this migration. It is emphatically NOT "no gap" for
-- historical rows, and a census must date-bound itself accordingly. There is no
-- backfill: `usage_events` is append-only, and the fact was never captured.
--
-- # finish_reason_source finally has a second value
--
-- Migration 0004 added `finish_reason_source` and wrote 'synthetic' into every
-- row, with the comment: "'synthetic' now; 'upstream' once StreamEvent /
-- ChatResponse carry the real stop reason". They now do — all three wires read
-- the upstream's own stop reason (chat completions' `finish_reason`,
-- Anthropic's `stop_reason`, the Responses API's status + incomplete_details)
-- and normalize it to the OpenAI vocabulary. So rows begin carrying 'upstream',
-- which 0004's CHECK constraint already permits; this migration adds no
-- constraint of its own for it and only restates the column's meaning.
--
-- Nothing about what a customer is charged changes. No branch in the router
-- reads a finish reason: the walk's only content-driven retry inspects the
-- response text/tool calls/reasoning, and `shape_ok` is a telemetry label no
-- non-test code reads. What changes is that the label and the reason are now
-- ground truth where the upstream supplied one, and the two cohorts are
-- separable by this column instead of being silently mixed.

ALTER TABLE usage_events
    ADD COLUMN usage_gap TEXT,
    ADD CONSTRAINT usage_events_usage_gap_is_known
        CHECK (usage_gap IS NULL OR usage_gap IN ('include_usage_ignored', 'done_missing'));

COMMENT ON COLUMN usage_events.usage_gap IS
    'Why a stream settled unbilled: include_usage_ignored (ordinary) vs done_missing (a truncating middlebox looks like this). Written on both lanes; NULL = usage reported, not a chat-completions stream, or predates migration 0020';

COMMENT ON COLUMN usage_events.finish_reason IS
    'The stop reason for the served attempt. Ground truth from the upstream when finish_reason_source = upstream; router-synthesized from token arithmetic (tool_calls beats length) when synthetic';

COMMENT ON COLUMN usage_events.finish_reason_source IS
    'upstream = the provider''s own stop reason, normalized to the OpenAI vocabulary by the wire; synthetic = inferred by the router because the upstream reported none it could map. Never mix the cohorts when training on finish_reason or shape_ok';
