Event dungeons need a database migration for event_dungeons table, make sure you have it:

CREATE TABLE event_dungeons (
    id UUID PRIMARY KEY,
    character_id UUID NOT NULL REFERENCES characters(id),
    event_id UUID NOT NULL,
    dungeon_id UUID NOT NULL,
    dungeon_state JSONB,
    initial_state JSONB,
    generated_data JSONB NOT NULL,
    entered_at TIMESTAMP NOT NULL,
    expires_at TIMESTAMP,
    entry_count INTEGER NOT NULL DEFAULT 1,
    max_entries INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX idx_event_dungeons_character ON event_dungeons(character_id);
CREATE INDEX idx_event_dungeons_event ON event_dungeons(event_id);
CREATE INDEX idx_event_dungeons_dungeon ON event_dungeons(dungeon_id);

CREATE TABLE event_completions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    character_id UUID NOT NULL REFERENCES characters(id),
    event_id UUID NOT NULL,
    completion_count INTEGER NOT NULL DEFAULT 0,
    last_completed_at TIMESTAMP NOT NULL DEFAULT NOW(),
    UNIQUE(character_id, event_id)
);