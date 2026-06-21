DROP INDEX IF EXISTS device_bindings_source_wg_ip_idx;
ALTER TABLE device_bindings DROP COLUMN IF EXISTS source_wg_ip;
