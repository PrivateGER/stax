CREATE TABLE IF NOT EXISTS rooms (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    media_title TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS playback_sessions (
    room_id TEXT PRIMARY KEY NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    status TEXT NOT NULL,
    anchor_position_seconds REAL NOT NULL,
    clock_updated_at TEXT NOT NULL,
    playback_rate REAL NOT NULL,
    updated_at TEXT NOT NULL
);
