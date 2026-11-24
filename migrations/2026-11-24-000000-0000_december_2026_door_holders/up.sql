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
-- One row per (account, day). The first request that binds wins: the server
-- writes it with INSERT ... ON CONFLICT DO NOTHING, so two requests racing at
-- midnight cannot both hold the door. `alt_uuid` is the holding alt
-- (`characters.source_alt_uuid`, the nil uuid for a row from before alts were
-- tracked). `rehearsal` keeps a test character's pre-December rehearsal from
-- holding the real day's door.
--
-- Idempotent.
CREATE TABLE IF NOT EXISTS december_2026_door_holders (
    user_id      UUID     NOT NULL,
    day          SMALLINT NOT NULL,
    rehearsal    BOOLEAN  NOT NULL DEFAULT FALSE,
    alt_uuid     UUID     NOT NULL,
    character_id UUID     NOT NULL,
    bound_at     BIGINT   NOT NULL,
    PRIMARY KEY (user_id, day, rehearsal)
);
