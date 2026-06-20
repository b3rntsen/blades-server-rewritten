-- Per-user town. The fork served one static default_town.json to every
-- character; this column carries each transferred character's OWN captured town
-- (populated at import-character time) so `get_town` can return it instead of the
-- barren default. Nullable: fresh/un-transferred characters keep the default.
-- Idempotent (IF NOT EXISTS) so it is safe to apply BY HAND on the existing prod
-- DB — the arena-migrate one-shot skips all migrations once `users` exists.
ALTER TABLE characters ADD COLUMN IF NOT EXISTS town JSONB;
