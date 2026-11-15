-- Fix for 2026-11-16-000000 (prod, 2026-11-15 22:20): the backfill's apply
-- rolled back with "character name Taheen is already taken".
--
-- `character_names_sync` refused any non-grandfathered name that another row
-- also carried — including a name this alt ALREADY HELD. Taheen's holder had
-- another alt renamed in the same run, so its sync ran while the duplicate's row
-- still read "taheen" (grandfathered, not yet re-synced), and the holder's own
-- unchanged claim was refused. The check now applies only to a NEW claim: an alt
-- that already holds the name, non-grandfathered, keeps it. That is safe because
-- the unique index guarantees no other non-grandfathered row can carry it, and
-- grandfathered rows are tolerated by design.

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
    IF NOT w.gf
       AND NOT EXISTS (SELECT 1 FROM character_names o
                        WHERE o.user_id = uid AND o.alt_key = w.alt
                          AND o.norm_name = w.norm AND NOT o.grandfathered)
       AND EXISTS (SELECT 1 FROM character_names o
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

