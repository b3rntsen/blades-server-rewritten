-- AI copies are recognised by ACCOUNT, not by name (2026-11-16).
--
-- WHY: `character_name_exempt` left AI copies out of the unique-names registry
-- by their " (AI)" suffix — but the website's AI copies ("AI mimics": one
-- isolated arena account per copied alt) are STORED under the bare name; the
-- matchmaker only adds " (AI)" when it serves them. So the registry and the
-- 2026-11-15 backfill treated 20 copies as players and numbered them
-- ("Ravena" -> "Ravena 2"), while a player could take "Ravena AI".
--
-- WHAT: `arena_ai_copy_users` lists those accounts. It is filled by the
-- server when the website enables a mimic (`ai-mimic`) or imports one
-- (`aiCopy`), and for existing copies by the owner-run repair
-- (POST …/dev/v1/character-names/ai-fix), because only the website can derive
-- which accounts it minted. `arena_ai_mimics` cannot serve: it lists ENABLED
-- opponents only (a disabled copy drops out) and also a player's own real
-- account opted in through the legacy roster.

CREATE TABLE IF NOT EXISTS arena_ai_copy_users (
    user_id   UUID PRIMARY KEY,
    marked_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The registry leaves AI-copy accounts out (in addition to the name rule).
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
     AND NOT EXISTS (SELECT 1 FROM arena_ai_copy_users u WHERE u.user_id = a.user_id)
$$;
