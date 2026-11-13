-- Community hunt counters (owner decision d48, tracker #498): a "Spirit of the
-- Hunt" style event, step 1 only — count town jobs completed inside an event
-- window. NO rewards: nothing reads these rows to pay anyone.
--
-- Off by default. A completion is recorded only while a row in
-- `community_events` has starts_at <= now() < ends_at, so with this table empty
-- (as the migration leaves it) the server records nothing. To run an event:
--   INSERT INTO community_events (id, name, starts_at, ends_at)
--   VALUES ('hunt-2026-12', 'Spirit of the Hunt', '2026-12-01T12:00Z', '2026-12-15T12:00Z');
CREATE TABLE IF NOT EXISTS community_events (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    starts_at  TIMESTAMPTZ NOT NULL,
    ends_at    TIMESTAMPTZ NOT NULL,
    CHECK (ends_at > starts_at)
);

-- One row per completed job per event. `location_type` is the job board's own
-- kit for the job's `jobSetup.dungeonTemplateId` — cave, forest, dungeon (stone),
-- ayleid, arena (a Duel) — or `unknown` for a job row stored before the server
-- kept its template. No FK to characters: a character row can be replaced by an
-- alt switch, and its count must not vanish with it.
CREATE TABLE IF NOT EXISTS community_hunt_completions (
    event_id        TEXT NOT NULL REFERENCES community_events(id),
    character_id    UUID NOT NULL,
    quest_id        UUID NOT NULL,
    user_id         UUID NOT NULL,
    source_alt_uuid UUID,
    location_type   TEXT NOT NULL,
    completed_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (event_id, character_id, quest_id)
);
CREATE INDEX IF NOT EXISTS community_hunt_completions_event_user
    ON community_hunt_completions (event_id, user_id);
