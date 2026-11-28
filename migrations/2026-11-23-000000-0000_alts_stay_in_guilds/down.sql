-- FORWARD-ONLY: this drops every seat of an alt that is not being played, and
-- the previous binary reads board entries by member id as non-members. Run only
-- on a database that never served this change (e.g. a test schema).
ALTER TABLE guild_messages DROP COLUMN IF EXISTS target_alt_key;
ALTER TABLE guild_messages DROP COLUMN IF EXISTS target_account;
ALTER TABLE guild_messages DROP COLUMN IF EXISTS alt_key;
ALTER TABLE guild_messages DROP COLUMN IF EXISTS alt_account;
DROP TABLE IF EXISTS guild_alt_seats;
DROP TABLE IF EXISTS alt_member_ids;
