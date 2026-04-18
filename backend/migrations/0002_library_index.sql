CREATE TABLE IF NOT EXISTS library_roots (
    path TEXT PRIMARY KEY NOT NULL,
    created_at TEXT NOT NULL,
    last_scanned_at TEXT,
    last_scan_error TEXT
);

CREATE TABLE IF NOT EXISTS media_items (
    id TEXT PRIMARY KEY NOT NULL,
    root_path TEXT NOT NULL REFERENCES library_roots(path) ON DELETE CASCADE,
    relative_path TEXT NOT NULL,
    file_name TEXT NOT NULL,
    extension TEXT,
    size_bytes INTEGER NOT NULL,
    modified_at TEXT NOT NULL,
    indexed_at TEXT NOT NULL,
    content_type TEXT,
    UNIQUE(root_path, relative_path)
);
