-- Durable login sessions: persist the in-memory SessionStore (server/src/session.rs)
-- so an arena-server rebuild/restart no longer invalidates every connected client's
-- blades_v1 token (which caused comm errors + rms 401s until each client relaunched).
-- One row per issued session; reconstructed lazily on a cold lookup. request_count
-- (reset to 1) and matchmaking_ws (re-established on rms reconnect) are intentionally
-- NOT persisted. Idempotent (IF NOT EXISTS) so it is safe to apply by hand on the
-- existing prod DB (the arena-migrate one-shot skips all migrations once `users` exists).
CREATE TABLE IF NOT EXISTS sessions (
    session_id     UUID PRIMARY KEY,
    user_id        UUID NOT NULL,
    secret_user_id UUID NOT NULL,
    extra_secret   UUID NOT NULL,
    expires_at     TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_expires_at_idx ON sessions (expires_at);
