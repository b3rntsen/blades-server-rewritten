-- A character name belongs to ONE character (owner decision 2026-11-15).
--
-- WHY: the arena Top 100 showed four "Adventurer"s and two "Markus Valerius".
-- Retail told same-named characters apart with a hidden 4-digit tag; our boards
-- show the bare name, so two characters with one name are indistinguishable.
--
-- WHAT IS A CHARACTER HERE: an alt. A user's alts take turns in ONE `characters`
-- row and the rest are parked in `character_versions`, so "every character" is
-- the live row (keyed `COALESCE(source_alt_uuid, id)`, as every ranking keys it)
-- plus, for each other alt, its NEWEST version. A name moves between the two
-- tables on every switch-alt; that must never look like a second claim.
--
-- HOW: a registry, `character_names`, one row per (user, alt) holding that alt's
-- current name and its normalised form, with a UNIQUE index on the normalised
-- name. It is not written by the application directly but RECOMPUTED for a user
-- by `character_names_sync` from what the two source tables hold, in a
-- DEFERRED constraint trigger — i.e. at COMMIT. Inside a switch-alt the name is
-- briefly in both tables or in neither; by commit it is in exactly one place, so
-- the check only ever sees the end state. Every path that writes a name (import,
-- new-alt, game create, starter, website rename, restore, switch, operator SQL)
-- is covered without each having to remember to.
--
-- NORMALISATION: NFC, trim, runs of whitespace collapsed to one space, then
-- lower(). NFC because the game's name whitelist is precomposed letters ("å" is
-- one code point there, never "a" + combining ring) and both rename paths
-- already store NFC; NFKC would additionally fold "ĳ" (on the whitelist) into
-- "ij", which is a different spelling, not the same name. lower() is the
-- database's (en_US.utf8 on prod: Ø/ø, Д/д, Ĳ/ĳ all fold). Cross-script
-- look-alikes (Latin "a" vs Cyrillic "а") are NOT folded.
--
-- NOT REGISTERED (cannot conflict): blank names; the starter placeholder
-- "Adventurer" / "Adventurer N" (the server replaces it with a generated name at
-- creation, and the backfill replaces old ones — kept out of the index so a path
-- that still writes the placeholder can never break character creation); AI
-- copies (" (AI)"); operator test characters (" (Test)", or listed in
-- `test_characters`).
--
-- EXISTING DUPLICATES are tolerated, not renamed, by this migration: per
-- duplicate group the oldest character holds the name and the others are
-- `grandfathered` (outside the unique index). A grandfathered alt keeps working
-- through switches and imports; it stops being grandfathered when it is renamed.
-- The rename itself is the separate, owner-approved backfill
-- (POST …/dev/v1/character-names/backfill), which numbers them.

CREATE OR REPLACE FUNCTION character_name_norm(n text) RETURNS text
LANGUAGE sql IMMUTABLE AS $$
  SELECT lower(regexp_replace(normalize(btrim(coalesce(n, '')), NFC), '\s+', ' ', 'g'))
$$;

CREATE OR REPLACE FUNCTION character_name_exempt(n text) RETURNS boolean
LANGUAGE sql IMMUTABLE AS $$
  SELECT character_name_norm(n) = ''
      OR character_name_norm(n) ~ '^adventurer( [0-9]+)?$'
      OR character_name_norm(n) LIKE '%(ai)'
      OR character_name_norm(n) LIKE '% (test)'
$$;

CREATE TABLE IF NOT EXISTS character_names (
    user_id       UUID NOT NULL,
    alt_key       UUID NOT NULL,
    name          TEXT NOT NULL,
    norm_name     TEXT NOT NULL,
    grandfathered BOOLEAN NOT NULL DEFAULT false,
    updated_at    BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint,
    PRIMARY KEY (user_id, alt_key)
);
CREATE UNIQUE INDEX IF NOT EXISTS character_names_unique_name
    ON character_names (norm_name) WHERE NOT grandfathered;
CREATE INDEX IF NOT EXISTS character_names_by_name ON character_names (norm_name);

-- Every registrable alt of every user, with what the oldest-wins rule orders by.
-- "Oldest" has no creation timestamp to go on (`characters` has none), so it is
-- the account's first device binding, then the alt's first kept version (an alt
-- never parked is the account's original), then the higher level.
CREATE OR REPLACE FUNCTION character_name_all_alts(only_user uuid DEFAULT NULL)
RETURNS TABLE (user_id uuid, alt_key uuid, name text, norm_name text, level int,
               account_first_seen timestamptz, alt_first_saved bigint)
LANGUAGE sql STABLE AS $$
  WITH live AS (
    SELECT c.user_id, COALESCE(c.source_alt_uuid, c.id) AS alt_key,
           c."character"->>'name' AS name,
           COALESCE((c."character"->>'level')::int, 0) AS level
      FROM characters c
     WHERE only_user IS NULL OR c.user_id = only_user
  ), parked AS (
    SELECT DISTINCT ON (v.user_id, v.source_alt_uuid)
           v.user_id, v.source_alt_uuid AS alt_key, v.name, v.level
      FROM character_versions v
     WHERE v.source_alt_uuid IS NOT NULL
       AND (only_user IS NULL OR v.user_id = only_user)
     ORDER BY v.user_id, v.source_alt_uuid, v.saved_at DESC, v.id DESC
  ), alts AS (
    SELECT * FROM live
    UNION ALL
    SELECT p.* FROM parked p
     WHERE NOT EXISTS (SELECT 1 FROM live l
                        WHERE l.user_id = p.user_id AND l.alt_key = p.alt_key)
  )
  SELECT a.user_id, a.alt_key, a.name, character_name_norm(a.name), a.level,
         (SELECT min(LEAST(d.bound_at, d.last_seen)) FROM device_bindings d
           WHERE d.user_id = a.user_id),
         (SELECT min(v.saved_at) FROM character_versions v
           WHERE v.user_id = a.user_id AND v.source_alt_uuid = a.alt_key)
    FROM alts a
   WHERE NOT character_name_exempt(a.name)
     AND NOT EXISTS (SELECT 1 FROM test_characters t WHERE t.alt_uuid = a.alt_key)
$$;

-- Bring one user's registry rows in line with what the source tables hold.
CREATE OR REPLACE FUNCTION character_names_sync(uid uuid) RETURNS void
LANGUAGE plpgsql AS $$
DECLARE
  want jsonb;
  w record;
BEGIN
  IF uid IS NULL THEN
    RETURN;
  END IF;
  -- Desired rows, with the grandfathered flag decided against the registry as
  -- it stands: an alt keeping its name keeps its flag; a name re-appearing under
  -- another key of the SAME user (an alt id learnt late) inherits it.
  SELECT coalesce(jsonb_agg(jsonb_build_object(
           'alt', s.alt_key, 'name', s.name, 'norm', s.norm_name, 'gf', s.gf)), '[]'::jsonb)
    INTO want
    FROM (
      SELECT a.alt_key, a.name, a.norm_name,
             COALESCE(
               (SELECT o.grandfathered FROM character_names o
                 WHERE o.user_id = uid AND o.alt_key = a.alt_key
                   AND o.norm_name = a.norm_name),
               EXISTS (SELECT 1 FROM character_names o
                        WHERE o.user_id = uid AND o.norm_name = a.norm_name
                          AND o.grandfathered)) AS gf
        FROM character_name_all_alts(uid) a
    ) s;

  DELETE FROM character_names o
   WHERE o.user_id = uid
     AND NOT EXISTS (SELECT 1 FROM jsonb_to_recordset(want) AS x(alt uuid, norm text)
                      WHERE x.alt = o.alt_key AND x.norm = o.norm_name);

  FOR w IN SELECT * FROM jsonb_to_recordset(want) AS x(alt uuid, name text, norm text, gf boolean)
  LOOP
    -- The unique index ignores grandfathered rows, so a NEW claim on a name a
    -- grandfathered alt still carries is refused here.
    IF NOT w.gf AND EXISTS (SELECT 1 FROM character_names o
                             WHERE o.norm_name = w.norm
                               AND NOT (o.user_id = uid AND o.alt_key = w.alt)) THEN
      RAISE EXCEPTION 'character name % is already taken', w.name
        USING ERRCODE = 'unique_violation', CONSTRAINT = 'character_names_unique_name';
    END IF;
    INSERT INTO character_names AS o (user_id, alt_key, name, norm_name, grandfathered)
    VALUES (uid, w.alt, w.name, w.norm, w.gf)
    ON CONFLICT (user_id, alt_key) DO UPDATE
       SET name = EXCLUDED.name, norm_name = EXCLUDED.norm_name,
           grandfathered = EXCLUDED.grandfathered,
           updated_at = EXTRACT(EPOCH FROM now())::bigint
     WHERE (o.name, o.norm_name, o.grandfathered)
           IS DISTINCT FROM (EXCLUDED.name, EXCLUDED.norm_name, EXCLUDED.grandfathered);
  END LOOP;
END $$;

CREATE OR REPLACE FUNCTION character_names_on_write() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'DELETE' THEN
    PERFORM character_names_sync(OLD.user_id);
    RETURN NULL;
  END IF;
  PERFORM character_names_sync(NEW.user_id);
  IF TG_OP = 'UPDATE' AND OLD.user_id IS DISTINCT FROM NEW.user_id THEN
    PERFORM character_names_sync(OLD.user_id);
  END IF;
  RETURN NULL;
END $$;

-- Populate, oldest-wins. Runs before the triggers exist.
INSERT INTO character_names (user_id, alt_key, name, norm_name, grandfathered)
SELECT r.user_id, r.alt_key, r.name, r.norm_name, r.rnk > 1
  FROM (
    SELECT a.*, row_number() OVER (
             PARTITION BY a.norm_name
             ORDER BY a.account_first_seen ASC NULLS LAST,
                      a.alt_first_saved ASC NULLS FIRST,
                      a.level DESC, a.user_id, a.alt_key) AS rnk
      FROM character_name_all_alts() a
  ) r
ON CONFLICT (user_id, alt_key) DO NOTHING;

-- Only writes that can change a name, an alt id or an owner re-check. The game
-- updates `characters` on nearly every request; a WHEN clause keeps those free.
DROP TRIGGER IF EXISTS character_names_characters_ins_del ON characters;
CREATE CONSTRAINT TRIGGER character_names_characters_ins_del
    AFTER INSERT OR DELETE ON characters
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION character_names_on_write();

DROP TRIGGER IF EXISTS character_names_characters_upd ON characters;
CREATE CONSTRAINT TRIGGER character_names_characters_upd
    AFTER UPDATE ON characters
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (OLD."character"->>'name' IS DISTINCT FROM NEW."character"->>'name'
          OR OLD.source_alt_uuid IS DISTINCT FROM NEW.source_alt_uuid
          OR OLD.user_id IS DISTINCT FROM NEW.user_id)
    EXECUTE FUNCTION character_names_on_write();

DROP TRIGGER IF EXISTS character_names_versions_ins_del ON character_versions;
CREATE CONSTRAINT TRIGGER character_names_versions_ins_del
    AFTER INSERT OR DELETE ON character_versions
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION character_names_on_write();

DROP TRIGGER IF EXISTS character_names_versions_upd ON character_versions;
CREATE CONSTRAINT TRIGGER character_names_versions_upd
    AFTER UPDATE ON character_versions
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (OLD.name IS DISTINCT FROM NEW.name
          OR OLD.source_alt_uuid IS DISTINCT FROM NEW.source_alt_uuid
          OR OLD.user_id IS DISTINCT FROM NEW.user_id
          OR OLD.saved_at IS DISTINCT FROM NEW.saved_at)
    EXECUTE FUNCTION character_names_on_write();

-- What the backfill renamed, so it can be undone row for row.
CREATE TABLE IF NOT EXISTS character_name_backfill_undo (
    id          BIGSERIAL PRIMARY KEY,
    run_id      UUID NOT NULL,
    user_id     UUID NOT NULL,
    alt_key     UUID NOT NULL,
    old_name    TEXT NOT NULL,
    new_name    TEXT NOT NULL,
    reason      TEXT NOT NULL,
    undone_at   BIGINT,
    renamed_at  BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint
);
CREATE INDEX IF NOT EXISTS character_name_backfill_undo_by_run
    ON character_name_backfill_undo (run_id, id);
