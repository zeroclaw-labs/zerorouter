-- Backstop the reserve->settle overdraft invariant at the database. Settlement
-- clamps the debit to what admission reserved, so the balance can never be
-- driven negative in credits mode; this CHECK turns any future regression into
-- a failed transaction (surfaced as a settlement error) instead of a silent
-- overdraft.
ALTER TABLE users
    ADD CONSTRAINT users_credit_balance_is_nonnegative
        CHECK (credit_balance_usd >= 0);
