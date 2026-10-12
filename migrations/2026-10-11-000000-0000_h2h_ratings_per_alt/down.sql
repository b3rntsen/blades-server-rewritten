-- Back to one row per character id. Two alts of one character may each hold a
-- row now, which the old key forbids; the tables are derived data that the
-- reconcile rebuilds at startup, so they are emptied rather than merged.
DELETE FROM arena_h2h_season_ratings;
DELETE FROM arena_h2h_ratings;

ALTER TABLE arena_h2h_season_ratings DROP CONSTRAINT IF EXISTS arena_h2h_season_ratings_pkey;
ALTER TABLE arena_h2h_season_ratings DROP COLUMN IF EXISTS alt_key;
ALTER TABLE arena_h2h_season_ratings ADD PRIMARY KEY (season_id, character_id);

ALTER TABLE arena_h2h_ratings DROP CONSTRAINT IF EXISTS arena_h2h_ratings_pkey;
ALTER TABLE arena_h2h_ratings DROP COLUMN IF EXISTS alt_key;
ALTER TABLE arena_h2h_ratings ADD PRIMARY KEY (character_id);
