-- The rematch rule for developer (TST) characters reads a test character's
-- recent opponents from the NORMAL player's rows, by `opponent_character_id`,
-- inside the single matchmaker loop. The server also builds this index at
-- startup with CONCURRENTLY (`dev_alt_lock::ensure_table`); applying this by
-- hand is optional. Idempotent.
CREATE INDEX IF NOT EXISTS arena_match_results_by_opponent
    ON arena_match_results (opponent_character_id, recorded_at DESC);
