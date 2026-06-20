-- Guild subsystem: guilds + members + a message board. Guild ids are 24-hex Mongo
-- ObjectId strings (TEXT), matching retail. Idempotent so it is safe to apply BY HAND
-- on the existing prod DB (the arena-migrate one-shot skips migrations once `users`
-- exists). The exchange/"gift" mechanic is intentionally not modelled here yet (see
-- docs/non-arena-feature-gaps.md).
CREATE TABLE IF NOT EXISTS guilds (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    tag_id            TEXT NOT NULL,
    guild_type        TEXT NOT NULL DEFAULT 'OPEN',
    short_description TEXT NOT NULL DEFAULT '',
    long_description  TEXT NOT NULL DEFAULT '',
    badge_icon_index  INTEGER NOT NULL DEFAULT 0,
    region_index      INTEGER NOT NULL DEFAULT 0,
    trophies          BIGINT NOT NULL DEFAULT 0,
    created_at        BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS guild_members (
    guild_id     TEXT NOT NULL,
    user_id      UUID NOT NULL,
    character_id UUID NOT NULL,
    rank         TEXT NOT NULL DEFAULT 'MEMBER',
    join_date    BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (guild_id, user_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_guild_members_user ON guild_members(user_id);

CREATE TABLE IF NOT EXISTS guild_messages (
    message_id         TEXT PRIMARY KEY,
    guild_id           TEXT NOT NULL,
    user_id            UUID NOT NULL,
    character_id       UUID NOT NULL,
    message_type       TEXT NOT NULL,
    type_specific_data JSONB NOT NULL DEFAULT '{}'::jsonb,
    creation_time      BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_guild_messages_guild ON guild_messages(guild_id, creation_time);
