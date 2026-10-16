-- Operator TEST characters (owner decision 2026-10-16).
--
-- A trusted tester gets an alt whose level an operator can set at will, to tune
-- level-based loot. Such a character's numbers are not earned, so it must never
-- rank: not on the in-game leaderboard, the website's Top 100 boards, the h2h
-- ratings, the season standings and awards, or a guild's score.
--
-- WHY A TABLE, KEYED BY ALT: every ranking is per ALT (2026-10-10 / 2026-10-11).
-- A user's alts take turns occupying ONE `characters` row, so a flag on that row
-- would be wrong as soon as the tester switched back to his real character, and a
-- flag inside `server_state` would have to be dug out of each newest snapshot. The
-- alt id is already the key every ranking query groups or joins on, so excluding a
-- test alt is one indexed `NOT EXISTS` against this primary key.
--
-- The name also carries " (Test)" so the character is visibly a test one in game
-- and on the website; that is the label, this table is the rule.
--
-- No unmark on purpose: a test alt's cups and wins were not earned, and unmarking
-- would put them on the boards.

CREATE TABLE IF NOT EXISTS test_characters (
    alt_uuid   UUID PRIMARY KEY,
    user_id    UUID NOT NULL,
    marked_by  TEXT NOT NULL,
    note       TEXT,
    marked_at  BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint
);

-- Every operator action on a test character, kept. Append-only by convention:
-- nothing in the server updates or deletes these rows.
CREATE TABLE IF NOT EXISTS test_character_audit (
    -- A sequence, not a uuid: the trail reads in the order it was written, even
    -- for two actions in the same second.
    id           BIGSERIAL PRIMARY KEY,
    alt_uuid     UUID NOT NULL,
    user_id      UUID NOT NULL,
    character_id UUID,
    action       TEXT NOT NULL,
    actor        TEXT NOT NULL,
    detail       JSONB NOT NULL DEFAULT '{}'::jsonb,
    at           BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint
);

CREATE INDEX IF NOT EXISTS test_character_audit_by_alt
    ON test_character_audit (alt_uuid, id DESC);
