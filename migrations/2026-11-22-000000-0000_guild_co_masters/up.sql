-- Co-guild masters: a WEBSITE role, not an in-game rank (2026-11-22).
--
-- WHY: owner request — "assign a co-guild master who on the web has rights to
-- kick and approve new members". Retail has no rank that could carry it:
-- `GuildData._guildRanksData` (data/guild_data.json) grants MASTER, ELDER and
-- MEMBER no power at all, the client reads its buttons from that shipped asset,
-- and 0 of 1422 captured member records hold MASTER or ELDER. Writing MASTER
-- into `guild_members.rank` would change nothing in the game except a title no
-- retail client was ever observed rendering. So the in-game rank stays as it
-- is and the role lives beside it, read only by `guild_manage.rs`.
--
-- Keyed by the PUBLIC arena user id (the website account), like
-- `guild_members`. A row has power only while that account holds a LIVE seat
-- in the guild; kick/leave/ban and the guild's deletion remove it.
--
-- Idempotent, like every guild migration.
CREATE TABLE IF NOT EXISTS guild_co_masters (
    guild_id     TEXT   NOT NULL,
    user_id      UUID   NOT NULL,
    appointed_by UUID,              -- NULL = a site operator
    appointed_at BIGINT NOT NULL,
    PRIMARY KEY (guild_id, user_id)
);

-- Who did what to whom, when — every website guild-management write, in the
-- transaction that performs it. Names are copied in: the row must still read
-- after the character is renamed or gone.
CREATE TABLE IF NOT EXISTS guild_manage_log (
    id                  BIGSERIAL PRIMARY KEY,
    guild_id            TEXT   NOT NULL,
    at                  BIGINT NOT NULL,
    action              TEXT   NOT NULL,  -- KICK | APPROVE | DENY | APPOINT_CO_MASTER | REMOVE_CO_MASTER
    actor_role          TEXT   NOT NULL,  -- GRANDMASTER | CO_MASTER | OPERATOR
    actor_user_id       UUID,             -- NULL for an operator
    actor_name          TEXT   NOT NULL DEFAULT '',
    target_user_id      UUID   NOT NULL,
    target_character_id UUID,
    target_name         TEXT   NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_guild_manage_log_guild ON guild_manage_log(guild_id, id DESC);
