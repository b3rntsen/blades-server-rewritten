-- Durable matchmaking history (#NB-3). Replaces the in-memory RecentMatches
-- ring buffer in arena/matchmaker.rs, which was wiped on every arena-server
-- restart. One row per matchmaking ticket: inserted 'searching' when the
-- matchmaker receives it, updated 'matched' when it resolves (solo or pair).
-- Idempotent (CREATE … IF NOT EXISTS) so the migration is safe to re-apply.
CREATE TABLE IF NOT EXISTS arena_matches (
    ticket_id       UUID PRIMARY KEY,
    user_id         UUID NOT NULL REFERENCES users(id),
    status          TEXT NOT NULL,            -- 'searching' | 'matched'
    game_session_id UUID,
    paired          BOOLEAN NOT NULL DEFAULT FALSE,
    recorded_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at     TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS arena_matches_recorded_at_idx ON arena_matches (recorded_at DESC);
CREATE INDEX IF NOT EXISTS arena_matches_user_id_idx ON arena_matches (user_id);
