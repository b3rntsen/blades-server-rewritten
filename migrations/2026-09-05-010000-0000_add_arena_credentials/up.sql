-- Account credentials, so ONE generic APK works for everybody.
--
-- Identity today comes from the WireGuard IP the mitm stamps into
-- `X-Newblades-Device-Ip` (the client sends `deviceId: null`), so removing the
-- VPN removes the player's identity with it. Retail's own login endpoint —
-- `auth/bnet/login`, `{username, password, deviceId, platform}` — is already in
-- the client, so a username and password set on the profile lets a player sign
-- in through the game's own screen and land on their own character.
--
-- APPLY BY HAND on the box: the migrate one-shot skips everything once `users`
-- exists. Idempotent and safe to re-run.

CREATE TABLE IF NOT EXISTS arena_credentials (
    -- Lowercased at write time. `Ruukoto` and `ruukoto` are one account: retail's
    -- login is a display name, and letting case create two accounts is a support
    -- burden nobody would thank us for.
    username      TEXT PRIMARY KEY,
    user_id       UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- `pbkdf2$<iterations>$<salt hex>$<hash hex>`, self-describing so the cost
    -- can be raised later without locking existing players out.
    password_hash TEXT NOT NULL,
    created_at    BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint,
    updated_at    BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint
);

-- One login per account. Without this a user could accumulate several usernames
-- and it would stop being obvious which one is theirs.
CREATE UNIQUE INDEX IF NOT EXISTS arena_credentials_one_per_user
    ON arena_credentials (user_id);
