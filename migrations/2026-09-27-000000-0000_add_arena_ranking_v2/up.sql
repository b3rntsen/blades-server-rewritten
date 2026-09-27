-- Arena ranking v2: versioned operator config plus the human-only Elo board.
--
-- NOTE FOR DEPLOY: the migrate one-shot skips everything once `users` exists, so
-- apply this migration by hand on existing boxes before shipping the binary.
-- The statements are idempotent and safe to re-run.
CREATE TABLE IF NOT EXISTS arena_ranking_config (
    id          UUID PRIMARY KEY,
    version     INTEGER NOT NULL UNIQUE,
    config      JSONB NOT NULL,
    updated_by  TEXT,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    note        TEXT
);

CREATE INDEX IF NOT EXISTS arena_ranking_config_version_idx
    ON arena_ranking_config (version DESC);

CREATE TABLE IF NOT EXISTS arena_h2h_ratings (
    character_id    UUID PRIMARY KEY REFERENCES characters(id) ON DELETE CASCADE,
    rating          INTEGER NOT NULL,
    wins            INTEGER NOT NULL DEFAULT 0,
    losses          INTEGER NOT NULL DEFAULT 0,
    ties            INTEGER NOT NULL DEFAULT 0,
    matches         INTEGER NOT NULL DEFAULT 0,
    last_match_at   TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS arena_h2h_ratings_top_idx
    ON arena_h2h_ratings (rating DESC, matches DESC);

CREATE INDEX IF NOT EXISTS arena_match_results_h2h_rebuild_idx
    ON arena_match_results (game_session_id, recorded_at, id)
    WHERE opponent_character_id IS NOT NULL;
