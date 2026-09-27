-- Per-season human-vs-human Elo boards.
--
-- `arena_h2h_ratings` remains the all-time board and is never reset. This table
-- starts each arena season from `h2h_rating.start`, preserves ended seasons
-- verbatim, and lets matches outside every season count only toward all-time.
CREATE TABLE IF NOT EXISTS arena_h2h_season_ratings (
    season_id       UUID NOT NULL REFERENCES arena_seasons(id) ON DELETE CASCADE,
    character_id    UUID NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    rating          INTEGER NOT NULL,
    wins            INTEGER NOT NULL DEFAULT 0,
    losses          INTEGER NOT NULL DEFAULT 0,
    ties            INTEGER NOT NULL DEFAULT 0,
    matches         INTEGER NOT NULL DEFAULT 0,
    last_match_at   TIMESTAMPTZ,
    PRIMARY KEY (season_id, character_id)
);

CREATE INDEX IF NOT EXISTS arena_h2h_season_ratings_top_idx
    ON arena_h2h_season_ratings (season_id, rating DESC, matches DESC);

CREATE INDEX IF NOT EXISTS arena_seasons_bounds_idx
    ON arena_seasons (starts_at, ends_at);
