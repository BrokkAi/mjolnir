CREATE TABLE IF NOT EXISTS alert_baseline (identity TEXT PRIMARY KEY NOT NULL);
INSERT OR IGNORE INTO alert_baseline SELECT identity FROM push_entries;
CREATE TABLE IF NOT EXISTS alert_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    identity TEXT NOT NULL UNIQUE REFERENCES push_entries(identity),
    destination TEXT NOT NULL,
    session_title TEXT NOT NULL,
    text TEXT NOT NULL,
    kind TEXT NOT NULL
);
