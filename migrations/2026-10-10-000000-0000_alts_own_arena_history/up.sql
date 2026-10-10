-- Every alt is its own character (owner decision 2026-10-10).
--
-- A user has ONE live `characters` row that every alt takes turns occupying, so
-- anything keyed only by `characters.id` was shared by all of a user's alts. For
-- the arena that meant: one alt's matches counted towards another's season
-- standing, the season reward was recorded against the shared id, and whichever
-- alt happened to be live when it was claimed received it (tracker #348: newmunk
-- finished top-50, spacemunk got Victor's Blade).
--
-- Match history, frozen standings and season awards therefore carry the alt they
-- belong to. NULL means "recorded before alts were tracked" and is read as the
-- alt currently live — exactly what every reader did before this column existed,
-- so nothing changes for the 550+ characters that have never had a second alt.

ALTER TABLE arena_match_results    ADD COLUMN IF NOT EXISTS source_alt_uuid UUID;
ALTER TABLE arena_season_standings ADD COLUMN IF NOT EXISTS source_alt_uuid UUID;
ALTER TABLE arena_season_awards    ADD COLUMN IF NOT EXISTS source_alt_uuid UUID;

-- Two alts of one user can each place in the same season, under the same
-- character id. The old keys allowed one standing and one award per kind per
-- character id; they become one per ALT. COALESCE because a unique index treats
-- NULLs as distinct, and the legacy (NULL) row must stay unique too.
ALTER TABLE arena_season_standings DROP CONSTRAINT IF EXISTS arena_season_standings_pkey;
CREATE UNIQUE INDEX IF NOT EXISTS arena_season_standings_one_per_alt
    ON arena_season_standings (
        season_id, character_id,
        COALESCE(source_alt_uuid, '00000000-0000-0000-0000-000000000000'::uuid));

DROP INDEX IF EXISTS arena_season_awards_one_per_kind;
CREATE UNIQUE INDEX IF NOT EXISTS arena_season_awards_one_per_alt_kind
    ON arena_season_awards (
        season_id, character_id,
        COALESCE(source_alt_uuid, '00000000-0000-0000-0000-000000000000'::uuid),
        kind);

-- Backfill: which alt played each match already recorded.
--
-- From the snapshot timeline. Every alt switch, import and restore snapshots the
-- row it replaces into `character_versions`, labelled with the alt that was live
-- (`SNAPSHOT_CHARACTER_SQL`). So the alt that played a match is the label on the
-- first snapshot taken AFTER it; a match with no later snapshot was played by the
-- alt live now. Exact for rows that carried their alt id; a row imported before
-- alt tracking was labelled with the alt arriving at that import, so matches
-- before such a snapshot go to that alt — the best the data can say. Only characters that have ever been snapshotted are touched —
-- the rest have only ever had one occupant and stay NULL, read as "the live alt".
-- A snapshot labelled NULL (pre-alt-tracking) leaves the match NULL as well.
-- The backfill (and every later "versions of this character" lookup) needs it:
-- without it each correlated subquery below scans the whole table.
CREATE INDEX IF NOT EXISTS character_versions_by_character
    ON character_versions (character_id, saved_at);

UPDATE arena_match_results r
   SET source_alt_uuid = COALESCE(
         (SELECT v.source_alt_uuid
            FROM character_versions v
           WHERE v.character_id = r.character_id
             AND v.saved_at >= EXTRACT(EPOCH FROM r.recorded_at)::bigint
           ORDER BY v.saved_at ASC, v.id ASC
           LIMIT 1),
         CASE WHEN NOT EXISTS (
                SELECT 1 FROM character_versions v
                 WHERE v.character_id = r.character_id
                   AND v.saved_at >= EXTRACT(EPOCH FROM r.recorded_at)::bigint)
              THEN (SELECT c.source_alt_uuid FROM characters c WHERE c.id = r.character_id)
         END)
 WHERE r.source_alt_uuid IS NULL
   AND EXISTS (SELECT 1 FROM character_versions v WHERE v.character_id = r.character_id);

CREATE INDEX IF NOT EXISTS arena_match_results_character_alt_idx
    ON arena_match_results (character_id, source_alt_uuid, recorded_at DESC);
