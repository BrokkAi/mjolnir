-- Historical revision 1, used only by upgrade regression tests.
BEGIN IMMEDIATE;
CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY CHECK(version > 0),
    applied_at TEXT NOT NULL
) STRICT;
CREATE TABLE session_contexts (
    session_id TEXT PRIMARY KEY,
    bundle_id TEXT NOT NULL,
    created_at TEXT NOT NULL
) STRICT;
CREATE TABLE sessions (
    session_id TEXT PRIMARY KEY REFERENCES session_contexts(session_id),
    title TEXT NOT NULL CHECK(length(trim(title)) > 0),
    harness_kind TEXT NOT NULL CHECK(harness_kind IN ('codex','claude','kimi')),
    last_profile TEXT NOT NULL,
    target_template_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN (
        'provisioning','running','disconnected','checkpointing','closing','destroying',
        'archived','lost','error','destroyed-with-data-loss'
    )),
    native_session_id TEXT,
    acp_session_title TEXT CHECK(acp_session_title IS NULL OR length(trim(acp_session_title)) > 0),
    session_title_override TEXT CHECK(session_title_override IS NULL OR length(trim(session_title_override)) > 0),
    updated_at TEXT NOT NULL,
    last_viewed_event_sequence INTEGER NOT NULL DEFAULT 0 CHECK(last_viewed_event_sequence >= 0),
    last_error TEXT
) STRICT;
CREATE TABLE session_targets (
    session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('local-bare','local-podman','apple-container','aws-ec2','ssh-bare','ssh-podman')),
    host TEXT,
    resource_id TEXT,
    address TEXT,
    workspace BLOB,
    worker_id TEXT,
    CHECK(
        (kind = 'local-bare' AND workspace IS NOT NULL
         AND host IS NULL AND resource_id IS NULL AND address IS NULL AND worker_id IS NULL)
     OR (kind IN ('local-podman','apple-container') AND resource_id IS NOT NULL
         AND host IS NULL AND address IS NULL AND workspace IS NULL AND worker_id IS NULL)
     OR (kind = 'aws-ec2' AND resource_id IS NOT NULL
         AND host IS NULL AND workspace IS NULL AND worker_id IS NULL)
     OR (kind = 'ssh-bare' AND host IS NOT NULL AND workspace IS NOT NULL
         AND resource_id IS NULL AND address IS NULL)
     OR (kind = 'ssh-podman' AND host IS NOT NULL AND resource_id IS NOT NULL
         AND address IS NULL AND workspace IS NULL AND worker_id IS NULL)
    )
) STRICT;
CREATE TABLE session_mounts (
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    source BLOB NOT NULL,
    destination BLOB NOT NULL,
    PRIMARY KEY(session_id, ordinal),
    UNIQUE(session_id, destination)
) STRICT;
CREATE TABLE session_checkpoints (
    session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
    archive_path BLOB NOT NULL,
    sha256 TEXT NOT NULL CHECK(length(sha256) = 64 AND sha256 NOT GLOB '*[^0-9a-f]*'),
    created_at TEXT NOT NULL,
    event_sequence INTEGER NOT NULL CHECK(event_sequence >= 0)
) STRICT;
CREATE TABLE mount_history (
    host TEXT NOT NULL CHECK(length(trim(host)) > 0),
    source BLOB NOT NULL,
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    PRIMARY KEY(host, ordinal),
    UNIQUE(host, source)
) STRICT;
CREATE TABLE prompt_history (
    history_id INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES session_contexts(session_id),
    event_sequence INTEGER NOT NULL CHECK(event_sequence >= 0),
    submitted_at TEXT NOT NULL,
    text TEXT NOT NULL CHECK(length(trim(text)) > 0),
    UNIQUE(session_id, event_sequence)
) STRICT;
CREATE INDEX prompt_history_session_recent
    ON prompt_history(session_id, history_id DESC);
CREATE INDEX session_contexts_bundle
    ON session_contexts(bundle_id, session_id);
CREATE INDEX prompt_history_recent
    ON prompt_history(history_id DESC);
INSERT INTO schema_migrations(version, applied_at)
    VALUES (1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
PRAGMA user_version = 1;
COMMIT;
