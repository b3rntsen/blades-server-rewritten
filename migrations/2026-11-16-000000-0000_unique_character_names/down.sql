DROP TRIGGER IF EXISTS character_names_characters_ins_del ON characters;
DROP TRIGGER IF EXISTS character_names_characters_upd ON characters;
DROP TRIGGER IF EXISTS character_names_versions_ins_del ON character_versions;
DROP TRIGGER IF EXISTS character_names_versions_upd ON character_versions;
DROP FUNCTION IF EXISTS character_names_on_write();
DROP FUNCTION IF EXISTS character_names_sync(uuid);
DROP FUNCTION IF EXISTS character_name_all_alts(uuid);
DROP TABLE IF EXISTS character_names;
DROP FUNCTION IF EXISTS character_name_exempt(text);
DROP FUNCTION IF EXISTS character_name_norm(text);
-- character_name_backfill_undo is kept: it is the only record of what the
-- backfill renamed.
