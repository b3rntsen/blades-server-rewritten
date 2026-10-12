-- The head-to-head boards, one row per ALT (owner decision 2026-10-10; tracker #373).
--
-- `2026-10-10-000000-0000_alts_own_arena_history` made every alt its own arena
-- character: match history, frozen standings and awards carry the alt that
-- played. The h2h rating tables were left keyed by `characters.id`, which every
-- alt of a user shares, so one alt's human-vs-human results were rated, counted
-- and NAMED as whichever alt happened to be live. Measured on NBS-2: spacemunk's
-- win over Sephoris was listed as newmunk's, and Ivan's two wins over Sephoris
-- were filed under Ísdís.
--
-- `alt_key` is the alt that played: the match row's `source_alt_uuid`, or for a
-- row recorded before alts were tracked the alt live now, or — for a character
-- that has never had an alt id — the character id itself. It is NOT NULL so it
-- can be part of the primary key.
--
-- Both tables are derived data: the reconcile at startup (and every
-- `h2h_reconcile_minutes`) deletes and rebuilds them from `arena_match_results`,
-- so the backfill below only has to be valid, not exact.

ALTER TABLE arena_h2h_ratings        ADD COLUMN IF NOT EXISTS alt_key UUID;
ALTER TABLE arena_h2h_season_ratings ADD COLUMN IF NOT EXISTS alt_key UUID;

UPDATE arena_h2h_ratings        SET alt_key = character_id WHERE alt_key IS NULL;
UPDATE arena_h2h_season_ratings SET alt_key = character_id WHERE alt_key IS NULL;

ALTER TABLE arena_h2h_ratings        ALTER COLUMN alt_key SET NOT NULL;
ALTER TABLE arena_h2h_season_ratings ALTER COLUMN alt_key SET NOT NULL;

ALTER TABLE arena_h2h_ratings DROP CONSTRAINT IF EXISTS arena_h2h_ratings_pkey;
ALTER TABLE arena_h2h_ratings ADD PRIMARY KEY (character_id, alt_key);

ALTER TABLE arena_h2h_season_ratings DROP CONSTRAINT IF EXISTS arena_h2h_season_ratings_pkey;
ALTER TABLE arena_h2h_season_ratings ADD PRIMARY KEY (season_id, character_id, alt_key);
