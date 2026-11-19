CREATE TABLE environments (
    environment_id TEXT PRIMARY KEY,
    owner_session_id TEXT NOT NULL,
    target_lease_id TEXT REFERENCES environment_workers(lease_id) ON DELETE SET NULL,
    phase TEXT NOT NULL CHECK(phase IN ('active', 'retiring', 'reclaimed')),
    placement_json TEXT NOT NULL CHECK(json_valid(placement_json))
) STRICT;
CREATE TABLE environment_bindings (
    session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
    environment_id TEXT NOT NULL REFERENCES environments(environment_id)
) STRICT;
CREATE TABLE environment_workers (
    lease_id TEXT PRIMARY KEY,
    environment_id TEXT NOT NULL REFERENCES environments(environment_id) ON DELETE CASCADE,
    worker_session_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('local-bare','local-podman','local-docker','apple-container','aws-ec2','ssh-bare','ssh-podman','ssh-docker','ssh-apple-container')),
    host TEXT,
    resource_id TEXT,
    address TEXT,
    workspace BLOB,
    worker_id TEXT,
    workspace_storage TEXT,
    borrowed_from TEXT,
    target_template_id TEXT NOT NULL,
    runtime_json TEXT CHECK(runtime_json IS NULL OR json_valid(runtime_json)),
    CHECK((kind='local-bare' AND workspace IS NOT NULL)
        OR (kind='ssh-bare' AND host IS NOT NULL AND workspace IS NOT NULL)
        OR (kind IN ('local-podman','local-docker','apple-container','aws-ec2') AND resource_id IS NOT NULL)
        OR (kind IN ('ssh-podman','ssh-docker','ssh-apple-container') AND host IS NOT NULL AND resource_id IS NOT NULL))
) STRICT;
CREATE INDEX environment_worker_identity ON environment_workers(worker_session_id);
CREATE TABLE environment_reports (
    environment_id TEXT NOT NULL REFERENCES environments(environment_id) ON DELETE CASCADE,
    child_session_id TEXT NOT NULL,
    report_dir TEXT NOT NULL,
    PRIMARY KEY(environment_id,child_session_id,report_dir)
) STRICT;
CREATE TRIGGER environment_report_registered AFTER INSERT ON subagent_handbacks
WHEN NEW.report_dir IS NOT NULL BEGIN
    INSERT OR IGNORE INTO environment_reports
        SELECT environment_id,NEW.child_session_id,NEW.report_dir FROM environment_bindings WHERE session_id=NEW.child_session_id;
END;
CREATE TRIGGER environment_report_updated AFTER UPDATE OF report_dir ON subagent_handbacks
WHEN NEW.report_dir IS NOT NULL BEGIN
    INSERT OR IGNORE INTO environment_reports
        SELECT environment_id,NEW.child_session_id,NEW.report_dir FROM environment_bindings WHERE session_id=NEW.child_session_id;
END;

-- Reclaimed descriptors are retry tombstones, not outstanding cleanup. Their
-- last logical owner leaving removes them; uncertain environments remain.
CREATE TRIGGER reclaimed_environment_owner_deleted AFTER DELETE ON sessions BEGIN
    DELETE FROM environments WHERE owner_session_id=OLD.session_id AND phase='reclaimed'
        AND NOT EXISTS(SELECT 1 FROM environment_bindings b WHERE b.environment_id=environments.environment_id);
END;
CREATE TRIGGER reclaimed_environment_binding_deleted AFTER DELETE ON environment_bindings BEGIN
    DELETE FROM environments WHERE environment_id=OLD.environment_id AND phase='reclaimed'
        AND NOT EXISTS(SELECT 1 FROM environment_bindings b WHERE b.environment_id=OLD.environment_id)
        AND NOT EXISTS(SELECT 1 FROM sessions s WHERE s.session_id=environments.owner_session_id);
END;
ALTER TABLE session_moves ADD COLUMN source_environment_id TEXT REFERENCES environments(environment_id) ON DELETE SET NULL;
ALTER TABLE retained_move_sources ADD COLUMN environment_id TEXT REFERENCES environments(environment_id) ON DELETE SET NULL;
