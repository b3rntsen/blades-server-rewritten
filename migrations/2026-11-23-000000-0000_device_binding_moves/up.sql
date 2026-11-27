-- A password sign-in on a device re-binds that device (owner decision d124,
-- tracker #607), and every re-binding is written down.
--
-- WHY: `device_bindings` decides which account a phone lands on at every cold
-- start. Until now nothing could take a binding from a real account, so a
-- phone claimed by the wrong member (#607: from the website's device list)
-- stayed theirs until someone repaired the row by hand — the owner's own
-- password login "worked" for one session and the next launch went back.
-- Holding the device AND knowing the account's password is now enough to move
-- that one row. A rule that can move a binding between two real accounts needs
-- a record that outlives the row it changed, hence this table.
--
-- `device_binding_moves` — one row per change of holder made by a password
-- sign-in, written in the SAME transaction as the change (no audit row, no
-- move). Deliberately no foreign keys: the record must still read after either
-- account is gone. `prior_*` carry what the binding row held before, which is
-- everything needed to put it back:
--
--   UPDATE device_bindings d
--      SET user_id = m.from_user_id, bound_at = m.prior_bound_at,
--          active_alt_uuid = m.prior_active_alt_uuid
--     FROM device_binding_moves m
--    WHERE m.id = <id> AND d.device_id = m.device_id AND d.user_id = m.to_user_id;
--
-- Read it with:
--   SELECT moved_at, left(device_id, 8) AS device, from_kind, from_user_id,
--          to_user_id, path FROM device_binding_moves ORDER BY id DESC LIMIT 50;
--
-- `sessions.device_id` — the device key a session was established from. The
-- move takes the device from HERE, never from the body of the password
-- request, so a sign-in cannot name a device other than the one its own
-- session was opened with. Nullable: older sessions, and clients that present
-- no usable device identity, simply cannot move a binding.
--
-- Idempotent, like every migration here.
CREATE TABLE IF NOT EXISTS device_binding_moves (
    id                    BIGSERIAL   PRIMARY KEY,
    moved_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    device_id             TEXT        NOT NULL,
    from_user_id          UUID,                 -- NULL = the device was unclaimed
    -- 'unclaimed' | 'throwaway' (no login, no real character) | 'account'
    from_kind             TEXT        NOT NULL,
    to_user_id            UUID        NOT NULL, -- the account whose password was proven
    session_user_id       UUID        NOT NULL, -- the account the client was signed in as
    path                  TEXT        NOT NULL, -- 'bnet/login' | 'bnet/link' | 'bnet/link/force'
    prior_bound_at        TIMESTAMPTZ,
    prior_active_alt_uuid UUID,
    source_wg_ip          TEXT
);
CREATE INDEX IF NOT EXISTS device_binding_moves_device_idx
    ON device_binding_moves (device_id, id DESC);
CREATE INDEX IF NOT EXISTS device_binding_moves_from_idx
    ON device_binding_moves (from_user_id) WHERE from_user_id IS NOT NULL;

ALTER TABLE sessions ADD COLUMN IF NOT EXISTS device_id TEXT;
