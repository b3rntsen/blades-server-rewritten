-- Forward-only in practice: dropping this table puts every child account's copy
-- of a parent alt into the rankings and the name registry. Unlock every device
-- first (POST …/dev/v1/characters/unlock-alt) and roll back the companion
-- 2026-11-24-000001 migration before running this.
DROP TABLE IF EXISTS dev_alt_lock_accounts;
