-- One-off arena events (tracker #526, HauDrauf's Arena Happy Hour); see
-- server/src/arena/arena_event.rs.
--
-- arena_event_fights logs every fight counted toward an event, once per
-- (event, account, gameSessionId), so the same fight reaching the writer twice
-- counts once. arena_event_counts is the per-account running count the rewards
-- are decided on. Gifts the fights earn are rows of arena_fight_gifts with a
-- negative milestone. Idempotent, like the other arena migrations.

CREATE TABLE IF NOT EXISTS arena_event_fights (
    event_id TEXT NOT NULL,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    game_session_id UUID NOT NULL,
    character_id UUID NOT NULL,
    -- The opponent's account: at most MAX_COUNTED_FIGHTS_PER_OPPONENT (3)
    -- counted fights per (event, account, opponent account).
    opponent_user_id UUID NOT NULL,
    recorded_at BIGINT NOT NULL,
    PRIMARY KEY (event_id, user_id, game_session_id)
);

CREATE INDEX IF NOT EXISTS arena_event_fights_opponent
    ON arena_event_fights (event_id, user_id, opponent_user_id);

CREATE TABLE IF NOT EXISTS arena_event_counts (
    event_id TEXT NOT NULL,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    fights BIGINT NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (event_id, user_id)
);
