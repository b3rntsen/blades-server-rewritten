-- December 2026 gift calendar (tracker #499): which character of an account
-- holds each day's door.
--
-- WHY: HauDrauf, 2026-11-24 (message 3924): "Die Geschenke gibt es nur für den
-- Charakter der sich an dem Tag als erstes einloggt, sonst ist es unfair." One
-- door a day per ACCOUNT, to the first character the server sees that day. Alts
-- share one `characters` row and swap its `server_state`, so the per-character
-- claim count alone would let every alt open the door; this table is what the
-- alts have in common.
--
-- THE ACCOUNT is the WEBSITE (Discord) account where the arena can tell
-- (HauDrauf, message 3937: nobody should be rewarded for a second account).
-- A website account's arena user id is uuidv5("discord:<id>") (web
-- `arenaUserId`), so a v5 user IS its website account. A device-only arena user
-- (random v4 id) counts as the website account its devices were claimed by: the
-- v5 user that `device_bindings` binds its `users.data.gp_deviceids` to, when
-- that is exactly one. Otherwise it is its own account. Two different Discord
-- logins cannot be told apart and stay two accounts.
--
-- One row per (account, day). The first request that binds wins: the server
-- writes it with INSERT ... ON CONFLICT DO NOTHING, so two requests racing at
-- midnight cannot both hold the door. `alt_uuid` is the holding alt
-- (`characters.source_alt_uuid`, the nil uuid for a row from before alts were
-- tracked). `rehearsal` keeps a test character's pre-December rehearsal from
-- holding the real day's door.
--
-- Idempotent.
CREATE TABLE IF NOT EXISTS december_2026_door_holders (
    account_id   UUID     NOT NULL, -- website account's arena user id, else the arena user id
    day          SMALLINT NOT NULL,
    rehearsal    BOOLEAN  NOT NULL DEFAULT FALSE,
    user_id      UUID     NOT NULL, -- the arena user that bound it
    alt_uuid     UUID     NOT NULL,
    character_id UUID     NOT NULL,
    bound_at     BIGINT   NOT NULL,
    PRIMARY KEY (account_id, day, rehearsal)
);
