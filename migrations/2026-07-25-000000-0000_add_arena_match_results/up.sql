-- Phase 5.4 — durable match-end economy.
--
-- The victory card (op49) used to be wire-only: trophies, XP, gold and the chest
-- meter were computed, sent, and forgotten. They now persist. The player-visible
-- state itself lives in JSONB columns that already exist
-- (characters.character / .wallet / .inventory), so NO `ALTER TABLE characters`
-- is needed — `pvp_trophies`, `pvp_winning_streak`, `pvp_chest_meter`,
-- `matchmaking_pvp_trophies`, `highest_arena_reached`,
-- `highest_level_arena_reached` and `number_pvp_match_played` are all fields of
-- the serialized CompleteCharacter.
--
-- What IS new is this audit table: one row per player per finished match, so the
-- ladder is reconstructible and the leaderboard's "matches won" is a real count
-- rather than a guess.
--
-- NOTE FOR DEPLOY: the migrate one-shot skips everything once `users` exists, so
-- this must be applied BY HAND on the box before the new binary ships. The
-- statements are idempotent (IF NOT EXISTS) and safe to re-run.
CREATE TABLE IF NOT EXISTS arena_match_results (
    id                          UUID PRIMARY KEY,
    character_id                UUID NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    opponent_character_id       UUID,
    game_session_id             UUID,
    win                         BOOLEAN NOT NULL,
    rounds_won                  INTEGER NOT NULL DEFAULT 0,
    rounds_lost                 INTEGER NOT NULL DEFAULT 0,
    gold                        BIGINT  NOT NULL DEFAULT 0,
    character_xp                BIGINT  NOT NULL DEFAULT 0,
    trophy_delta                BIGINT  NOT NULL DEFAULT 0,
    trophies_after              BIGINT  NOT NULL DEFAULT 0,
    matchmaking_trophies_after  BIGINT  NOT NULL DEFAULT 0,
    arena                       INTEGER NOT NULL DEFAULT 1,
    arena_level                 INTEGER NOT NULL DEFAULT 1,
    chest_meter                 BIGINT  NOT NULL DEFAULT 0,
    recorded_at                 TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS arena_match_results_character_idx
    ON arena_match_results (character_id, recorded_at DESC);
CREATE INDEX IF NOT EXISTS arena_match_results_recorded_at_idx
    ON arena_match_results (recorded_at DESC);
-- The leaderboard ranks on this; without it every page scans every character.
CREATE INDEX IF NOT EXISTS arena_match_results_win_idx
    ON arena_match_results (character_id) WHERE win;

-- Leaderboard ordering key: pvpTrophies out of the character JSONB. The
-- expression must match leaderboards.rs's RANKED_CTE exactly (including the
-- COALESCE) or the planner will not use it and every page sorts the whole table.
-- `character` is a reserved word, hence the quoting.
CREATE INDEX IF NOT EXISTS characters_pvp_trophies_idx
    ON characters ((COALESCE(("character" ->> 'pvpTrophies')::bigint, 0)) DESC);
