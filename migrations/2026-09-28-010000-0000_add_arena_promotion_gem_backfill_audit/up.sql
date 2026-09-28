-- Audit rows for the one-off arena-promotion Gem backfill.
--
-- The grant idempotency marker lives in characters.server_state under
-- arenaPromotionLootGrants, shared with the live match-end promotion path. This
-- table records which of those markers were written by the backfill and how many
-- Gems the wallet received. The migration is idempotent because production's
-- migrate one-shot is commonly applied by hand on an existing database.

CREATE TABLE IF NOT EXISTS arena_promotion_gem_backfill_audit (
    id UUID PRIMARY KEY,
    character_id UUID NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    arena INTEGER NOT NULL,
    threshold BIGINT NOT NULL,
    gems BIGINT NOT NULL,
    character_name TEXT NOT NULL,
    old_gems BIGINT NOT NULL,
    new_gems BIGINT NOT NULL,
    live_grant_start_secs BIGINT NOT NULL,
    reason TEXT NOT NULL DEFAULT 'arena promotion gem backfill',
    granted_at BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint,
    UNIQUE (character_id, threshold)
);
