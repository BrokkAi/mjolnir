-- The controller store at schema revision 33, the revision Mjolnir 2.8.0
-- shipped. schema.rs creates a new store from this file in one transaction and
-- applies later revisions as numbered migrations.

CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY CHECK(version > 0),
    applied_at TEXT NOT NULL
) STRICT;

CREATE TABLE session_contexts (
    session_id TEXT PRIMARY KEY,
    bundle_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    workspace_id TEXT NOT NULL DEFAULT 'default'
) STRICT;

CREATE TABLE session_mounts (
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    source BLOB NOT NULL,
    destination BLOB NOT NULL,
    read_only INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(session_id, ordinal),
    UNIQUE(session_id, destination)
) STRICT;

CREATE TABLE session_checkpoints (
    session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
    archive_path BLOB NOT NULL,
    sha256 TEXT NOT NULL CHECK(length(sha256) = 64 AND sha256 NOT GLOB '*[^0-9a-f]*'),
    created_at TEXT NOT NULL,
    event_frontier INTEGER NOT NULL CHECK(event_frontier >= 0)
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
    event_ordinal INTEGER NOT NULL CHECK(event_ordinal >= 0),
    submitted_at TEXT NOT NULL,
    text TEXT NOT NULL CHECK(length(trim(text)) > 0),
    UNIQUE(session_id, event_ordinal)
) STRICT;

CREATE TABLE materialized_sessions (
    session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
    applied_event_ordinal INTEGER NOT NULL DEFAULT 0 CHECK(applied_event_ordinal >= 0),
    applied_event_digest TEXT NOT NULL
    DEFAULT '0000000000000000000000000000000000000000000000000000000000000000'
    CHECK(length(applied_event_digest) = 64
        AND applied_event_digest NOT GLOB '*[^0-9a-f]*'),
    last_activity_at_ms INTEGER,
    execution_state TEXT NOT NULL DEFAULT 'idle'
    CHECK(execution_state IN ('idle','running','closing','closed')),
    running_started_at_ms INTEGER,
    session_title TEXT CHECK(session_title IS NULL OR length(trim(session_title)) > 0),
    configuration_json TEXT NOT NULL DEFAULT '{}',
    pending_elicitations_json TEXT NOT NULL DEFAULT '[]',
    active_turn_json TEXT
        CHECK(active_turn_json IS NULL OR json_valid(active_turn_json)),
    last_turn_outcome_json TEXT
        CHECK(last_turn_outcome_json IS NULL OR json_valid(last_turn_outcome_json)),
    CHECK(
        (execution_state = 'running' AND running_started_at_ms IS NOT NULL)
        OR (execution_state != 'running' AND running_started_at_ms IS NULL)
    )
) STRICT;

CREATE TABLE materialized_transcript_items (
    session_id TEXT NOT NULL REFERENCES materialized_sessions(session_id) ON DELETE CASCADE,
    stable_id TEXT NOT NULL CHECK(length(trim(stable_id)) > 0),
    position INTEGER NOT NULL CHECK(position > 0),
    latest_content_event_ordinal INTEGER
    CHECK(latest_content_event_ordinal IS NULL
        OR latest_content_event_ordinal >= position),
    created_at_ms INTEGER NOT NULL,
    last_changed_at_ms INTEGER NOT NULL CHECK(last_changed_at_ms >= created_at_ms),
    body_json TEXT NOT NULL,
    PRIMARY KEY(session_id, stable_id)
) STRICT;

CREATE TABLE materialized_queued_prompts (
    session_id TEXT NOT NULL REFERENCES materialized_sessions(session_id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    command_id TEXT NOT NULL CHECK(length(trim(command_id)) > 0),
    content_json TEXT NOT NULL,
    queued_at_ms INTEGER NOT NULL,
    kind_json TEXT NOT NULL DEFAULT '"prompt"',
    accepted_ordinal INTEGER
        CHECK(accepted_ordinal IS NULL OR accepted_ordinal > 0),
    PRIMARY KEY(session_id, ordinal),
    UNIQUE(session_id, command_id)
) STRICT;

CREATE TABLE profile_config_cache (
    profile TEXT NOT NULL,
    model TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY(profile, model)
);

CREATE TABLE api_config_results (
    session_id TEXT NOT NULL REFERENCES materialized_sessions(session_id) ON DELETE CASCADE,
    command_id TEXT NOT NULL,
    error TEXT,
    PRIMARY KEY(session_id, command_id)
);

CREATE TABLE session_turn_usage (
    session_id TEXT NOT NULL REFERENCES materialized_sessions(session_id) ON DELETE CASCADE,
    command_id TEXT NOT NULL,
    completed_ordinal INTEGER NOT NULL,
    turn_start_position INTEGER,
    body TEXT NOT NULL,
    PRIMARY KEY(session_id, command_id)
);

CREATE TABLE session_provider_cost (
    session_id TEXT PRIMARY KEY REFERENCES materialized_sessions(session_id) ON DELETE CASCADE,
    body TEXT NOT NULL
);

CREATE TABLE workspaces (
    workspace_id TEXT PRIMARY KEY CHECK(length(trim(workspace_id)) > 0),
    name TEXT NOT NULL CHECK(length(trim(name)) BETWEEN 1 AND 64),
    name_key TEXT NOT NULL UNIQUE CHECK(length(trim(name_key)) BETWEEN 1 AND 64),
    created_at TEXT NOT NULL,
    last_opened_at TEXT NOT NULL
) STRICT;

CREATE TABLE client_read_frontiers (
    client_id TEXT NOT NULL CHECK(length(trim(client_id)) > 0),
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
    session_id TEXT NOT NULL REFERENCES session_contexts(session_id) ON DELETE CASCADE,
    through_event_ordinal INTEGER NOT NULL DEFAULT 0
    CHECK(through_event_ordinal >= 0),
    updated_at TEXT NOT NULL,
    PRIMARY KEY(client_id, workspace_id, session_id)
) STRICT;

CREATE TABLE detached_drafts (
    draft_id TEXT PRIMARY KEY CHECK(length(trim(draft_id)) > 0),
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id),
    session_id TEXT REFERENCES session_contexts(session_id),
    source TEXT NOT NULL CHECK(length(trim(source)) > 0),
    owner_pid INTEGER CHECK(owner_pid IS NULL OR owner_pid > 0),
    saved_at TEXT NOT NULL,
    text TEXT NOT NULL CHECK(length(text) > 0),
    recovered_at TEXT
) STRICT;

CREATE TABLE client_session_state (
    client_id TEXT NOT NULL CHECK(length(trim(client_id)) > 0),
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
    session_id TEXT NOT NULL REFERENCES session_contexts(session_id) ON DELETE CASCADE,
    draft TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL,
    PRIMARY KEY(client_id, workspace_id, session_id)
) STRICT;

CREATE TABLE host_container_sizes (
    host TEXT PRIMARY KEY CHECK(length(trim(host)) > 0),
    cpus INTEGER NOT NULL CHECK(cpus > 0),
    memory_bytes INTEGER NOT NULL CHECK(memory_bytes > 0)
) STRICT;

CREATE TABLE second_opinion_defaults (
    workspace_id TEXT NOT NULL CHECK(length(trim(workspace_id)) > 0),
    profile_id TEXT NOT NULL CHECK(length(trim(profile_id)) > 0),
    model TEXT NOT NULL,
    effort TEXT NOT NULL,
    PRIMARY KEY (workspace_id, profile_id, model)
) STRICT;

CREATE TABLE second_opinion_reviews (
    session_id TEXT PRIMARY KEY
    REFERENCES sessions(session_id) ON DELETE CASCADE,
    workflow TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation >= 0),
    context_baseline INTEGER NOT NULL CHECK(context_baseline >= 0),
    native_lost INTEGER NOT NULL CHECK(native_lost IN (0, 1)),
    reviewer_transcript TEXT NOT NULL DEFAULT '[]'
) STRICT;

CREATE TABLE turn_review_state (
    session_id TEXT PRIMARY KEY
    REFERENCES sessions(session_id) ON DELETE CASCADE,
    baselines TEXT NOT NULL,
    reviewed_through_ordinal INTEGER NOT NULL
    CHECK(reviewed_through_ordinal >= 0),
    prior_review TEXT,
    active TEXT,
    pending_forward TEXT
) STRICT;

CREATE TABLE session_targets (
    session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('local-bare','local-podman','local-docker','apple-container','aws-ec2','ssh-bare','ssh-podman','ssh-docker')),
    host TEXT,
    resource_id TEXT,
    address TEXT,
    workspace BLOB,
    worker_id TEXT,
    workspace_storage TEXT,
    CHECK(
        (kind = 'local-bare' AND workspace IS NOT NULL
            AND host IS NULL AND resource_id IS NULL AND address IS NULL AND worker_id IS NULL)
        OR (kind IN ('local-podman','local-docker','apple-container') AND resource_id IS NOT NULL
            AND host IS NULL AND address IS NULL AND workspace IS NULL AND worker_id IS NULL)
        OR (kind = 'aws-ec2' AND resource_id IS NOT NULL
            AND host IS NULL AND workspace IS NULL AND worker_id IS NULL)
        OR (kind = 'ssh-bare' AND host IS NOT NULL AND workspace IS NOT NULL
            AND resource_id IS NULL AND address IS NULL)
        OR (kind IN ('ssh-podman','ssh-docker') AND host IS NOT NULL AND resource_id IS NOT NULL
            AND address IS NULL AND workspace IS NULL AND worker_id IS NULL)
    )
) STRICT;

CREATE TABLE workspace_pane_sizes (
    workspace_id TEXT PRIMARY KEY REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
    sessions TEXT NOT NULL CHECK(sessions IN ('minimized', 'standard', 'maximized')),
    targets TEXT NOT NULL CHECK(targets IN ('minimized', 'standard', 'maximized')),
    quota TEXT NOT NULL CHECK(quota IN ('minimized', 'standard', 'maximized')),
    CHECK((sessions = 'maximized') + (targets = 'maximized') + (quota = 'maximized') <= 1)
) STRICT;

CREATE TABLE session_moves (
    session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
    operation_id TEXT NOT NULL UNIQUE,
    operation_json TEXT NOT NULL CHECK(json_valid(operation_json))
) STRICT;

CREATE TABLE api_idempotency (
    key TEXT PRIMARY KEY CHECK(length(trim(key)) BETWEEN 1 AND 128),
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    created_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE schema_compatibility (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    minimum_compatible_version INTEGER NOT NULL CHECK(minimum_compatible_version >= 30)
) STRICT;

CREATE TABLE subagent_sessions (
    child_session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
    parent_session_id TEXT NOT NULL REFERENCES sessions(session_id),
    request_key TEXT NOT NULL,
    record_json TEXT NOT NULL CHECK(json_valid(record_json)),
    CHECK(child_session_id <> parent_session_id),
    UNIQUE(parent_session_id, request_key)
) STRICT;

CREATE TABLE sessions (
    session_id TEXT PRIMARY KEY REFERENCES session_contexts(session_id),
    title TEXT NOT NULL CHECK(length(trim(title)) > 0),
    harness_kind TEXT NOT NULL CHECK(harness_kind IN ('codex','claude','kimi','grok','deepseek','muse','zcode')),
    last_profile TEXT NOT NULL,
    target_template_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN (
            'provisioning','running','disconnected','checkpointing','closing','destroying',
            'stopped','lost','error','destroyed-with-data-loss'
        )),
    native_session_id TEXT,
    acp_session_title TEXT CHECK(acp_session_title IS NULL OR length(trim(acp_session_title)) > 0),
    session_title_override TEXT CHECK(session_title_override IS NULL OR length(trim(session_title_override)) > 0),
    updated_at TEXT NOT NULL,
    viewed_through_event_ordinal INTEGER NOT NULL DEFAULT 0
    CHECK(viewed_through_event_ordinal >= 0),
    last_error TEXT,
    resource_allocation TEXT,
    last_checkpoint_error TEXT,
    project_directory BLOB,
    managed_worktree TEXT,
    draft_input TEXT NOT NULL DEFAULT '',
    container_cpus TEXT,
    container_memory TEXT,
    archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0, 1)),
    create_managed_worktree INTEGER
        CHECK(create_managed_worktree IN (0, 1)),
    mjolnir_subagents INTEGER
        CHECK(mjolnir_subagents IN (0, 1))
) STRICT;

CREATE TABLE "hidden_native_sessions" (
    harness_kind TEXT NOT NULL CHECK(harness_kind IN ('codex','claude','kimi','grok','deepseek','muse','zcode')),
    native_session_id TEXT NOT NULL CHECK(length(trim(native_session_id)) > 0),
    hidden_at TEXT NOT NULL,
    PRIMARY KEY(harness_kind, native_session_id)
) STRICT;

CREATE TABLE api_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    recorded_at_ms INTEGER NOT NULL,
    body TEXT NOT NULL CHECK(json_valid(body))
) STRICT;

CREATE TABLE api_session_activity (
    session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
    body TEXT NOT NULL CHECK(json_valid(body))
) STRICT;

CREATE INDEX prompt_history_session_recent
ON prompt_history(session_id, history_id DESC);

CREATE INDEX session_contexts_bundle
ON session_contexts(bundle_id, session_id);

CREATE INDEX prompt_history_recent
ON prompt_history(history_id DESC);

CREATE INDEX materialized_transcript_position
ON materialized_transcript_items(session_id, position, stable_id);

CREATE INDEX session_turn_usage_order ON session_turn_usage(session_id, completed_ordinal);

CREATE INDEX session_contexts_workspace
ON session_contexts(workspace_id, session_id);

CREATE INDEX detached_drafts_workspace_recent
ON detached_drafts(workspace_id, saved_at DESC);

CREATE INDEX client_session_state_age
ON client_session_state(updated_at);

CREATE INDEX subagent_sessions_parent
ON subagent_sessions(parent_session_id, child_session_id);

CREATE INDEX api_events_session ON api_events(session_id, seq);

CREATE TRIGGER session_contexts_workspace_insert
BEFORE INSERT ON session_contexts
WHEN NOT EXISTS(
    SELECT 1 FROM workspaces WHERE workspace_id = NEW.workspace_id
)
BEGIN
    SELECT RAISE(ABORT, 'unknown workspace');
END;

CREATE TRIGGER session_contexts_workspace_update
BEFORE UPDATE OF workspace_id ON session_contexts
WHEN NOT EXISTS(
    SELECT 1 FROM workspaces WHERE workspace_id = NEW.workspace_id
)
BEGIN
    SELECT RAISE(ABORT, 'unknown workspace');
END;

CREATE TRIGGER api_session_error_updated
AFTER UPDATE OF last_error ON sessions
WHEN NEW.last_error IS NOT NULL AND NEW.last_error IS NOT OLD.last_error
BEGIN
    INSERT INTO api_events(session_id, recorded_at_ms, body)
    VALUES (NEW.session_id, CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    json_object('type', 'error', 'data', json_object('message', NEW.last_error, 'command_id', NULL)));
END;

CREATE TRIGGER api_session_error_inserted
AFTER INSERT ON sessions WHEN NEW.last_error IS NOT NULL
BEGIN
    INSERT INTO api_events(session_id, recorded_at_ms, body)
    VALUES (NEW.session_id, CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    json_object('type', 'error', 'data', json_object('message', NEW.last_error, 'command_id', NULL)));
END;

INSERT INTO workspaces(workspace_id, name, name_key, created_at, last_opened_at)
VALUES (
    'default', 'default', 'default',
    strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
    strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
);
