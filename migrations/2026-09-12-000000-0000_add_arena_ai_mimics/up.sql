-- Characters explicitly approved for use as server-driven Arena opponents.
--
-- The old ARENA_BOT_USER_IDS setting names whole users. That cannot express
-- "use this player's mage, but let the same player queue with their fighter",
-- because the characters table deliberately has one playable character per
-- user. The capture web app imports an isolated copy of each selected alt and
-- records that copy here.
CREATE TABLE arena_ai_mimics (
    character_id UUID PRIMARY KEY REFERENCES characters(id) ON DELETE CASCADE,
    enabled_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Until the first checkbox is used, production keeps using the legacy env
-- roster. The first management write flips this singleton to true; from then
-- on the table above is authoritative, including when it is deliberately empty.
CREATE TABLE arena_ai_mimic_control (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    managed BOOLEAN NOT NULL DEFAULT FALSE
);

INSERT INTO arena_ai_mimic_control (singleton, managed)
VALUES (TRUE, FALSE);
