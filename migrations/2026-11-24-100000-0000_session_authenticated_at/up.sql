-- Security review M7: a token refresh used to mint a fresh session from any live
-- one, so a sign-in could be stretched forever. Each session now carries the time
-- of the sign-in it descends from; refreshes copy it and stop at 30 days.
-- Existing rows start their clock now (they all expire within 24 h anyway).
-- Idempotent, like the sessions migration it extends.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS authenticated_at TIMESTAMPTZ NOT NULL DEFAULT now();
