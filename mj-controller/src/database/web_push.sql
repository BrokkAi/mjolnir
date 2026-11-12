CREATE TABLE IF NOT EXISTS push_devices(endpoint TEXT PRIMARY KEY, subscription TEXT NOT NULL, retry_at INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS push_observation(singleton INTEGER PRIMARY KEY CHECK(singleton=1), away INTEGER NOT NULL, revision INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS push_episodes(session TEXT PRIMARY KEY, identity TEXT NOT NULL, payload TEXT NOT NULL, signature TEXT NOT NULL, eligible INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS push_episode_counters(session TEXT PRIMARY KEY, sequence INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS push_deliveries(endpoint TEXT NOT NULL REFERENCES push_devices(endpoint) ON DELETE CASCADE,
 episode TEXT NOT NULL, payload TEXT NOT NULL, accepted INTEGER NOT NULL DEFAULT 0,
 retry_at INTEGER NOT NULL DEFAULT 0, failures INTEGER NOT NULL DEFAULT 0, error TEXT, summary_episode TEXT,
 PRIMARY KEY(endpoint,episode));
-- Notification cleanup must survive subscription expiry and outbox retirement.
CREATE TABLE IF NOT EXISTS push_summary_coverage(url TEXT PRIMARY KEY, sessions TEXT NOT NULL);
