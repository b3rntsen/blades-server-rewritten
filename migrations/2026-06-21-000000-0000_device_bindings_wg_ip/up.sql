-- Add a `source_wg_ip` column to device_bindings so that devices whose binding
-- was created under a stable deviceId hash can still be found when they later
-- reconnect with deviceId: null (and the server falls back to the WG peer IP).
--
-- On every anon_log_in the server now also writes the observed WG peer IP into
-- `source_wg_ip` (overwriting the previous value — the WG peer IP is stable per
-- client config, so successive logins from the same device will write the same
-- value). When a WG-IP-keyed lookup finds no user_id, the server tries a
-- secondary lookup: "find the most-recently-seen binding whose source_wg_ip ==
-- this IP AND user_id IS NOT NULL". This bridges the gap when a device was bound
-- under its stable deviceId hash but now logs in with deviceId: null.
--
-- Idempotent (IF NOT EXISTS / no-op on existing rows). Safe to apply on a live
-- prod DB (the arena-migrate one-shot skips all migrations once `users` exists —
-- apply by hand: ALTER TABLE device_bindings ADD COLUMN IF NOT EXISTS …).
ALTER TABLE device_bindings
    ADD COLUMN IF NOT EXISTS source_wg_ip TEXT;

CREATE INDEX IF NOT EXISTS device_bindings_source_wg_ip_idx
    ON device_bindings (source_wg_ip)
    WHERE source_wg_ip IS NOT NULL;
