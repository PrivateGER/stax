CREATE TABLE IF NOT EXISTS library_state (
    singleton INTEGER NOT NULL PRIMARY KEY CHECK (singleton = 1),
    revision  INTEGER NOT NULL
);

INSERT INTO library_state (singleton, revision)
VALUES (1, 0)
ON CONFLICT(singleton) DO NOTHING;
