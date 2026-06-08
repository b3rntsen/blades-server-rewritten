-- Device -> user bindings: the per-player "claim link". A device's anon
-- `auth/anon` deviceId is recorded here on every login; once a player claims it
-- (binds it to their Transfer'd character's users.id via /api/dev/v1/bind-device),
-- that device's anon login resolves to their user. Idempotent (IF NOT EXISTS) so
-- it is safe to apply by hand on an existing prod DB (the arena-migrate one-shot
-- skips all migrations once `users` exists).
CREATE TABLE IF NOT EXISTS device_bindings (
    device_id TEXT PRIMARY KEY,
    user_id   UUID REFERENCES users(id),
    platform  TEXT,
    last_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
    bound_at  TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS device_bindings_last_seen_idx ON device_bindings (last_seen DESC);
