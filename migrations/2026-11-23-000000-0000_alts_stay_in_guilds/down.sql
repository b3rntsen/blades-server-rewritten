-- Not a true inverse: seats moved here are not written back into versions.
-- Run only on a database that never served this change.
ALTER TABLE guild_messages DROP COLUMN IF EXISTS target_alt_key;
ALTER TABLE guild_messages DROP COLUMN IF EXISTS alt_key;
DROP TABLE IF EXISTS guild_alt_seats;
