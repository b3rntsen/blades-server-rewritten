DROP INDEX IF EXISTS character_versions_by_character;
DROP INDEX IF EXISTS arena_match_results_character_alt_idx;
DROP INDEX IF EXISTS arena_season_awards_one_per_alt_kind;
DROP INDEX IF EXISTS arena_season_standings_one_per_alt;
ALTER TABLE arena_season_awards    DROP COLUMN IF EXISTS source_alt_uuid;
ALTER TABLE arena_season_standings DROP COLUMN IF EXISTS source_alt_uuid;
ALTER TABLE arena_match_results    DROP COLUMN IF EXISTS source_alt_uuid;
-- Fails if two alts of one character id now hold a row each for one season —
-- which is the state this migration exists to allow. Resolve those first.
CREATE UNIQUE INDEX IF NOT EXISTS arena_season_awards_one_per_kind
    ON arena_season_awards (season_id, character_id, kind);
ALTER TABLE arena_season_standings ADD PRIMARY KEY (season_id, character_id);
