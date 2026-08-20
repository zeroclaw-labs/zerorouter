-- 0024_autopay_tax_lifecycle.sql
--
-- The rest of the autopay tax lifecycle: remember the recorded tax
-- TRANSACTION, and remember reversing it.
--
-- Migration 0021 froze what was PRICED (tax_amount_usd, tax_calculation_id)
-- but kept nothing about what was REPORTED: the tax transaction recorded from
-- that calculation after a credited settle was fire-and-forget, its id read
-- once from the response and dropped. That id is not recoverable later — the
-- Tax API has create_from_calculation, create_reversal, and retrieve-by-id,
-- and nothing that finds a transaction by its reference — so dropping it cost
-- two things:
--
--   1. A recording failure could only be replayed by an operator reading the
--      calculation id out of an ERROR log. Nothing durable said which settled
--      charges were still missing from the filing report.
--   2. A refunded or disputed charge could not have its tax reversed
--      automatically, because create_reversal requires `original_transaction`
--      — the transaction ID — and nowhere was it stored.
--
-- Four nullable columns close both. NULL keeps the 0021 meaning of "not yet":
--
--   tax_transaction_id            the recorded tax transaction (tax_...).
--   tax_recorded_at               when recording was CONFIRMED. This, not the
--                                 id, is the "stop retrying" marker: it can be
--                                 stamped with a NULL id in the one edge where
--                                 a recording is known to exist but its id was
--                                 lost (see below).
--   tax_reversal_transaction_id   the reversal transaction, once recorded.
--   tax_reversed_at               when the reversal was confirmed. Stamped
--                                 only after Stripe accepted the reversal.
--
-- The autopay sweep gains two passes that drive rows toward these being
-- filled: succeeded rows with a calculation and no tax_recorded_at are
-- re-recorded (the reference is the PaymentIntent id, unique across all
-- transactions, so the endpoint itself deduplicates a retry); recorded rows
-- whose credit the ledger shows reversed (a 'refund' row naming the intent)
-- get a full tax reversal. Rows with tax_recorded_at set but no transaction
-- id cannot be reversed automatically and are surfaced loudly instead.
--
-- BACKFILL: rows that settled before this migration ran had their tax
-- transaction recorded (or its failure logged) by the fire-and-forget path,
-- and their ids are gone either way. Stamping tax_recorded_at on them keeps
-- the new sweep from re-POSTing those references forever: the retry would be
-- refused as a duplicate every 30 minutes and the log would never go quiet.
-- Their reversal path degrades to the operator surface, which is exactly
-- where it was before this migration existed. The backfill deliberately
-- covers only rows already terminal at migration time ('succeeded' with a
-- calculation); everything later gets the real lifecycle.
--
-- REGISTRATION REQUIRED: db.rs migration vec gains Migration::new(24, ...).

ALTER TABLE stripe_autopay_intents
    ADD COLUMN tax_transaction_id TEXT,
    ADD COLUMN tax_recorded_at TIMESTAMPTZ,
    ADD COLUMN tax_reversal_transaction_id TEXT,
    ADD COLUMN tax_reversed_at TIMESTAMPTZ;

UPDATE stripe_autopay_intents
SET tax_recorded_at = updated_at
WHERE status = 'succeeded'
  AND tax_calculation_id IS NOT NULL;
