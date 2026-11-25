-- Developer device locks: one alt pinned to one device (owner ask, 2026-11-24).
--
-- "I'd like for developers to be able to lock an alt to a certain device. This
-- way, once logged in, I can play on my ipad, but I can test on pixel 8pro and
-- iphone 7 with other alts."
--
-- WHY A CHILD ACCOUNT. A user has ONE live `characters` row
-- (`uq_characters_user_id`) and ~27 call sites resolve the character from
-- `user_id` alone, so two alts of one account cannot be live at once (see the
-- 2026-09-21 alt-selection migration). Rather than drop that constraint, a lock
-- MOVES the alt into a child account of its own and points the device's
-- `device_bindings` row at the child. The child has its own single live row, so
-- the parent keeps playing its live alt on every other device concurrently.
-- Unlocking files the child's row back into the parent's `character_versions`
-- under the same alt (newest version wins, so progress on the test device is
-- kept) and points the device back at the parent.
--
-- ONE ROW PER (parent, device), KEPT FOREVER. `locked_alt_uuid` NULL means the
-- child is dormant: its live row was archived to the parent and nothing plays
-- it. A later lock of the same device reuses the child rather than minting
-- another account. The row is never deleted because it is also the RULE that
-- keeps a child out of rankings and the name registry: the child's live row is
-- a copy of a parent alt, and listing it would show that alt twice.
--
-- APPLY BY HAND on the box: the migrate one-shot skips once `users` exists.
-- Idempotent; forward-only.

CREATE TABLE IF NOT EXISTS dev_alt_lock_accounts (
    -- No foreign keys: ranking test fixtures carry this table without `users`,
    -- and a child is only ever created in the same transaction as its user row.
    child_user_id   UUID PRIMARY KEY,
    parent_user_id  UUID NOT NULL,
    -- The child's one live row. Set by the first lock and stable after: a
    -- dormant child keeps its row. The rankings exclude by it.
    child_character_id UUID,
    device_id       TEXT NOT NULL,
    locked_alt_uuid UUID,
    locked_at       BIGINT,
    created_at      BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint,
    CONSTRAINT dev_alt_lock_accounts_parent_device UNIQUE (parent_user_id, device_id)
);

-- An alt is locked to at most one device, and a device holds at most one lock:
-- the alt must never be live twice.
CREATE UNIQUE INDEX IF NOT EXISTS dev_alt_lock_accounts_one_per_alt
    ON dev_alt_lock_accounts (parent_user_id, locked_alt_uuid)
    WHERE locked_alt_uuid IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS dev_alt_lock_accounts_one_per_device
    ON dev_alt_lock_accounts (device_id)
    WHERE locked_alt_uuid IS NOT NULL;
