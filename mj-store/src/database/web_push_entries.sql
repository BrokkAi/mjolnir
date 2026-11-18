CREATE TABLE IF NOT EXISTS push_entries (
    identity TEXT PRIMARY KEY NOT NULL,
    session TEXT,
    completed_ordinal INTEGER,
    visible INTEGER NOT NULL,
    baseline INTEGER NOT NULL,
    first_seen_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS push_visible_entries ON push_entries(identity) WHERE visible=1;
CREATE TABLE IF NOT EXISTS push_entry_deliveries (
    endpoint TEXT NOT NULL REFERENCES push_devices(endpoint) ON DELETE CASCADE,
    identity TEXT NOT NULL REFERENCES push_entries(identity),
    accepted INTEGER NOT NULL DEFAULT 0 CHECK(accepted IN (0,1,3)),
    flight_session TEXT,
    retry_at INTEGER NOT NULL DEFAULT 0,
    failures INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    PRIMARY KEY(endpoint, identity)
);
CREATE INDEX IF NOT EXISTS push_queued_entry_deliveries
    ON push_entry_deliveries(endpoint, identity) WHERE accepted=0;
CREATE INDEX IF NOT EXISTS push_entry_flights
    ON push_entry_deliveries(endpoint, flight_session) WHERE accepted=3;
CREATE TABLE IF NOT EXISTS push_entry_cutover (singleton INTEGER PRIMARY KEY CHECK(singleton=1));
