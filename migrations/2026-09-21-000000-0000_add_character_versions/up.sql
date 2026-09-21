-- Every character this server is about to overwrite, kept.
--
-- WHY: `import_character` replaces the user's character row in place. That is
-- right for a transfer — the point is to put the captured character on the
-- server — but it means any play that happened here since the last transfer is
-- gone the moment somebody transfers again. RonnieRaider lost four days that
-- way: he played from 14 September, two imports on the 17th and 18th rolled his
-- character back to the capture, and the vendor purchases and other progress in
-- between simply stopped existing. Nothing warned him and nothing recorded it.
--
-- A preservation project must not be the thing that deletes a character. So
-- before an import writes, the row it is about to replace is copied here.
--
-- WHAT THIS IS NOT: it is not per-device character selection. A user still has
-- one live character row; this makes the previous ones recoverable rather than
-- letting several be live at once. Playing different alts on different phones
-- needs the multi-character model and is deliberately not attempted here.
--
-- `source_alt_uuid` is the identity that survives a transfer: the arena
-- `characters.id` is minted fresh per import, so it cannot answer "which alt is
-- this a version of". The web app knows the alt it is transferring and sends it.
-- NULL means an older caller that does not; those versions are still kept and
-- still restorable, they just cannot be grouped by alt.
--
-- WHY SEPARATE FROM schema.rs: `server/src/schema.rs` is diesel's COMPILE-TIME
-- description of the database; adding `diesel::table!` blocks creates nothing.
-- The migrate one-shot skips everything once `users` exists, so APPLY THIS BY
-- HAND on the box before shipping the binary.
--
-- Times are BIGINT unix seconds, this schema's convention.

CREATE TABLE IF NOT EXISTS character_versions (
    id              UUID PRIMARY KEY,
    -- The arena character row this snapshot was taken from. Not a foreign key:
    -- the row may later be replaced or removed, and the snapshot must outlive it.
    character_id    UUID NOT NULL,
    user_id         UUID NOT NULL,
    -- Which alt this is a version of, when the caller knows. See above.
    source_alt_uuid UUID,
    -- Denormalised for the picker, so listing versions does not mean parsing
    -- every blob.
    name            TEXT NOT NULL DEFAULT '',
    level           INTEGER NOT NULL DEFAULT 0,
    -- The row, verbatim. Same columns as `characters`, so a restore is a copy
    -- rather than a translation.
    character       JSONB NOT NULL,
    data            JSONB NOT NULL,
    inventory       JSONB NOT NULL,
    wallet          JSONB NOT NULL,
    town            JSONB,
    server_state    JSONB NOT NULL,
    -- 'import' (about to be overwritten by a transfer), 'manual' (asked for),
    -- 'restore' (snapshot taken before restoring another version, so a restore
    -- is itself undoable).
    reason          TEXT NOT NULL DEFAULT 'import',
    saved_at        BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint
);

-- The two questions asked of this table: "what versions does this user have"
-- and "what is the newest version of this alt".
CREATE INDEX IF NOT EXISTS character_versions_by_user
    ON character_versions (user_id, saved_at DESC);
CREATE INDEX IF NOT EXISTS character_versions_by_alt
    ON character_versions (source_alt_uuid, saved_at DESC)
    WHERE source_alt_uuid IS NOT NULL;
