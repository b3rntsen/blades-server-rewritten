-- Collecting a half-filled guild request no longer closes it (tracker #434).
--
-- Retail kept a request open until it was full: a collect paid whatever had
-- been donated SINCE THE LAST COLLECT, and the request stayed on the board for
-- the rest. Captured, request 05b4dd6b at 5/10: collect pays 5, the board
-- still lists it at 5/10, a second donor brings it to 10/10, the next collect
-- pays 5 (not 10) and only then does it disappear. We closed it on the first
-- collect, so the second half could never be donated.
--
-- `redeemed_amount` is how much of `donation_sum` has been paid out;
-- `redeemed` now means CLOSED (full and collected, or replaced by a newer
-- request from the same character).
--
-- Rows closed under the old rule were paid in full on close, so they start
-- with redeemed_amount = donation_sum — otherwise the next collect would pay
-- every past donation a second time. Guarded so a re-run cannot re-mark rows.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name = 'guild_exchanges'
           AND column_name = 'redeemed_amount'
    ) THEN
        ALTER TABLE guild_exchanges
            ADD COLUMN redeemed_amount BIGINT NOT NULL DEFAULT 0;
        UPDATE guild_exchanges SET redeemed_amount = donation_sum WHERE redeemed;
    END IF;
END
$$;
