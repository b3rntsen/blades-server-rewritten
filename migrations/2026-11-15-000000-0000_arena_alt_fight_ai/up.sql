-- Per-alt "Fight AI" preference (owner, 2026-11-15): "make a flag on each alt's
-- profile: match AI true, false. People can then choose to queue up, and if
-- anyone pops in, they get human fights, no AI fights."
--
-- Keyed (user_id, alt_key) where alt_key is the alt as the h2h boards key it:
-- `characters.source_alt_uuid`, or the character id for a character that has
-- never had an alt id. NOT a column on `characters`: that row is swapped by
-- `switch-alt`, so a flag there would follow whichever alt is live rather than
-- the alt it was set on. A row here stays put while its alt is parked.
--
-- No row = TRUE (today's behaviour). Only the website writes it, through the
-- token-gated dev route.
CREATE TABLE IF NOT EXISTS arena_alt_preferences (
    user_id    UUID        NOT NULL,
    alt_key    UUID        NOT NULL,
    fight_ai   BOOLEAN     NOT NULL DEFAULT TRUE,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, alt_key)
);
