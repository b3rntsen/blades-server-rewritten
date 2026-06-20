-- Per-character server-managed feature state (gift claims, daily-reward collection,
-- craft jobs, abyss run, generated challenge sets, …) that the captured character
-- JSON does not model. Never sent to the client; backs server bookkeeping so the
-- town/RPG economy stays coherent (e.g. the daily reward can't be re-collected).
-- Idempotent + defaulted so it is safe to apply BY HAND on the existing prod DB
-- (the arena-migrate one-shot skips all migrations once `users` exists), and so
-- existing character rows backfill to an empty state.
ALTER TABLE characters ADD COLUMN IF NOT EXISTS server_state JSONB NOT NULL DEFAULT '{}'::jsonb;
