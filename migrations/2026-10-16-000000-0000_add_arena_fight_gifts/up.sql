-- The arena weapon gift: every 25th finished arena fight pays a Divine Stalhrim
-- weapon. An owner-approved deviation from retail (tracker #390/#391, approved
-- 2026-10-16); see server/src/arena/arena_fight_gift.rs.
--
-- The count is per ACCOUNT (users.id), not per character or alt: a user's alts
-- share one counter, so switching alts cannot farm it. AI fights count. It starts
-- at zero at deploy; no history is backfilled, because a backfill would mint a
-- burst of gifts at once for every veteran.
--
-- Each gift is its own row and its id IS the globalGiftId the client claims, so
-- one gift is one item, one banner and one claim. `source_alt_uuid` is the alt
-- that fought the milestone fight; only that alt is offered the gift, as with
-- season awards (owner decision 2026-10-10). Idempotent because production's
-- migrate one-shot may be applied by hand on an existing database.

CREATE TABLE IF NOT EXISTS arena_account_fight_counts (
    user_id UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    fights BIGINT NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint
);

CREATE TABLE IF NOT EXISTS arena_fight_gifts (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    character_id UUID NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    source_alt_uuid UUID,
    milestone BIGINT NOT NULL,
    item JSONB NOT NULL,
    created_at BIGINT NOT NULL,
    granted_at BIGINT,
    UNIQUE (user_id, milestone)
);

CREATE INDEX IF NOT EXISTS arena_fight_gifts_pending
    ON arena_fight_gifts (character_id) WHERE granted_at IS NULL;
