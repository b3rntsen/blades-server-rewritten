-- Guild exchange ("gift") mechanic: a member requests an item; guildmates donate it;
-- the requester redeems the donated total. Idempotent (IF NOT EXISTS).
CREATE TABLE IF NOT EXISTS guild_exchanges (
    id                    TEXT PRIMARY KEY,
    guild_id              TEXT NOT NULL,
    requester_user_id     UUID NOT NULL,
    requester_character_id UUID NOT NULL,
    item_template_id      UUID NOT NULL,
    requested_amount      BIGINT NOT NULL DEFAULT 10,
    max_donation_amount   BIGINT NOT NULL DEFAULT 5,
    donations             JSONB NOT NULL DEFAULT '[]'::jsonb,
    donation_sum          BIGINT NOT NULL DEFAULT 0,
    creation_time         BIGINT NOT NULL DEFAULT 0,
    redeemed              BOOLEAN NOT NULL DEFAULT false
);
CREATE INDEX IF NOT EXISTS idx_guild_exchanges_guild ON guild_exchanges(guild_id);
