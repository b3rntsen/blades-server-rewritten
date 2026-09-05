-- Seasons, standings and awards — the tables behind a season the owner can run
-- from the web UI instead of a Rust rebuild.
--
-- WHY: `arena_season::SEASONS` is a compile-time `[SeasonConfig; 1]`. Opening a
-- season therefore meant editing Rust, rebuilding the image and redeploying,
-- which is why the 2026-09-01 attempt never landed: nothing could run it. These
-- tables move the season itself into data so the rollover endpoint (and a UI)
-- can act on it.
--
-- WHY SEPARATE FROM THE CODE: `server/src/schema.rs` is diesel's COMPILE-TIME
-- description of the database. Adding `diesel::table!` blocks makes the build
-- pass; it creates nothing. Without this migration the season routes deploy
-- green and 500 on first use. The migrate one-shot skips everything once
-- `users` exists, so APPLY THIS BY HAND on the box before shipping the binary —
-- same caveat as add_arena_match_results and add_event_quest_tables.
--
-- Times are BIGINT unix seconds, not TIMESTAMPTZ. That is this schema's own
-- convention (`guilds.created_at -> Int8`) and it keeps `chrono` out of the
-- server crate for four columns. Do not "modernise" these to TIMESTAMPTZ
-- without changing the models: diesel would compile and then fail at runtime on
-- the type mismatch.

CREATE TABLE IF NOT EXISTS arena_seasons (
    id          UUID PRIMARY KEY,
    number      INTEGER NOT NULL,
    name        TEXT NOT NULL,
    starts_at   BIGINT NOT NULL,
    ends_at     BIGINT NOT NULL,
    -- 'scheduled' -> 'active' -> 'ended'. Text rather than an enum so adding a
    -- state later is a code change, not a migration on a live database.
    status      TEXT NOT NULL DEFAULT 'scheduled',
    -- Mirrors ScoringVariant / TrophyResetRule so a season carries its own
    -- rules rather than inheriting whatever the binary was built with.
    scoring     TEXT NOT NULL DEFAULT 'shipped',
    reset_rule  TEXT NOT NULL DEFAULT 'hard_reset',
    created_at  BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint,
    ended_at    BIGINT
);

-- Only one season may be live at a time; a second would split the ladder and
-- make "which season is this match in" unanswerable. Enforced in the database
-- because the UI is not the only caller.
CREATE UNIQUE INDEX IF NOT EXISTS arena_seasons_one_active
    ON arena_seasons ((status)) WHERE status = 'active';

-- The final ladder, frozen at season end. The live standings live on the
-- character (pvpTrophies) and are ZEROED by the rollover, so without this the
-- season's result would exist nowhere afterwards.
CREATE TABLE IF NOT EXISTS arena_season_standings (
    season_id     UUID NOT NULL REFERENCES arena_seasons(id) ON DELETE CASCADE,
    character_id  UUID NOT NULL,
    rank          INTEGER NOT NULL,
    trophies      BIGINT NOT NULL,
    matches       INTEGER NOT NULL DEFAULT 0,
    wins          INTEGER NOT NULL DEFAULT 0,
    -- TEXT, not UUID: `guilds.id` is Text in this schema. A UUID column
    -- here compiles and then fails at runtime on the join.
    guild_id      TEXT,
    recorded_at   BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint,
    PRIMARY KEY (season_id, character_id)
);
CREATE INDEX IF NOT EXISTS arena_season_standings_rank
    ON arena_season_standings (season_id, rank);

CREATE TABLE IF NOT EXISTS arena_season_guild_standings (
    season_id  UUID NOT NULL REFERENCES arena_seasons(id) ON DELETE CASCADE,
    guild_id   TEXT NOT NULL,
    rank       INTEGER NOT NULL,
    trophies   BIGINT NOT NULL,
    members    INTEGER NOT NULL DEFAULT 0,
    recorded_at BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint,
    PRIMARY KEY (season_id, guild_id)
);

-- What each player earned. Recorded at season end and granted separately, so a
-- failed grant can be retried without recomputing the ladder — and so the
-- ladder is auditable even if grants are disputed.
CREATE TABLE IF NOT EXISTS arena_season_awards (
    id            UUID PRIMARY KEY,
    season_id     UUID NOT NULL REFERENCES arena_seasons(id) ON DELETE CASCADE,
    character_id  UUID NOT NULL,
    -- 'rank' (personal ladder) or 'guild_rank' (their guild's placing).
    kind          TEXT NOT NULL,
    rank          INTEGER NOT NULL,
    tier          TEXT NOT NULL,
    payload       JSONB NOT NULL,
    granted_at    BIGINT,
    created_at    BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint
);
CREATE UNIQUE INDEX IF NOT EXISTS arena_season_awards_one_per_kind
    ON arena_season_awards (season_id, character_id, kind);
CREATE INDEX IF NOT EXISTS arena_season_awards_ungranted
    ON arena_season_awards (season_id) WHERE granted_at IS NULL;
