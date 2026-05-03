CREATE TABLE matchmaking (
    id UUID PRIMARY KEY REFERENCES users(id),
    other_id UUID REFERENCES users(id),
    match_info JSONB,
    ack_info JSONB
);
