-- d54: the trophy rule can be switched (`arena_ranking_config.config.cups_rule`)
-- and the season recalculated under it. `trophy_delta` stays "what this match
-- paid under the rule in force"; this column keeps what the CLASSIC rule paid,
-- so a recalculation back to classic is lossless.
--
-- NULL = the row was never rewritten and `trophy_delta` IS the classic value
-- (every row recorded before this migration). Live results write it always.
ALTER TABLE arena_match_results ADD COLUMN IF NOT EXISTS classic_trophy_delta BIGINT;
