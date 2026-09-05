-- Event quests (PR #135) — the two tables its models are backed by.
--
-- WHY THIS EXISTS SEPARATELY: `server/src/schema.rs` is diesel's COMPILE-TIME
-- description of the database. Adding a `diesel::table!` block there makes the
-- code build and CI pass; it does not create anything. Without this migration
-- #135 deploys green and then 500s on the first event-quest request, because
-- `event_completions` and `event_dungeons` do not exist on the box.
--
-- NOTE FOR DEPLOY: the migrate one-shot skips everything once `users` exists, so
-- this must be applied BY HAND on the box before the new binary ships — the same
-- caveat as add_arena_match_results. All statements are idempotent and safe to
-- re-run.
--
-- Column types follow the models rather than convention: both structs use
-- `chrono::NaiveDateTime`, which diesel maps to `Timestamp` (WITHOUT time zone).
-- Using TIMESTAMPTZ here would compile and then fail at runtime on a type
-- mismatch, so these are deliberately not TIMESTAMPTZ even though
-- arena_match_results is.

CREATE TABLE IF NOT EXISTS event_completions (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    character_id       UUID NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    event_id           UUID NOT NULL,
    completion_count   INTEGER   NOT NULL DEFAULT 0,
    last_completed_at  TIMESTAMP NOT NULL DEFAULT now(),
    created_at         TIMESTAMP NOT NULL DEFAULT now()
);

-- `EventCompletion::get_or_create` looks a row up by exactly this pair. Without a
-- unique constraint two concurrent requests both miss the SELECT and both INSERT,
-- leaving a character with two completion counters for one event and whichever
-- one is read first winning.
CREATE UNIQUE INDEX IF NOT EXISTS event_completions_character_event_idx
    ON event_completions (character_id, event_id);

CREATE TABLE IF NOT EXISTS event_dungeons (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    character_id    UUID NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    event_id        UUID NOT NULL,
    dungeon_id      UUID NOT NULL,
    dungeon_state   JSONB,
    initial_state   JSONB,
    generated_data  JSONB     NOT NULL,
    entered_at      TIMESTAMP NOT NULL DEFAULT now(),
    expires_at      TIMESTAMP,
    entry_count     INTEGER   NOT NULL DEFAULT 0,
    max_entries     INTEGER   NOT NULL DEFAULT 0
);

-- The hot lookup is "this character's dungeon for this event".
CREATE INDEX IF NOT EXISTS event_dungeons_character_event_idx
    ON event_dungeons (character_id, event_id);
