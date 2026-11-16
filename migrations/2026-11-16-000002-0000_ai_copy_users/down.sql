-- Restore the 2026-11-16-000000 definition (no account exclusion).
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
DROP TABLE IF EXISTS arena_ai_copy_users;
