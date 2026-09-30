-- Frozen project-catalog revision 67 from d9953118, before the merge.
BEGIN IMMEDIATE;
            ALTER TABLE sessions ADD COLUMN project_json TEXT CHECK(project_json IS NULL OR json_valid(project_json));
            CREATE TABLE IF NOT EXISTS project_catalog (
                bundle_id TEXT PRIMARY KEY,
                project_key TEXT NOT NULL UNIQUE,
                snapshot_json TEXT NOT NULL CHECK(json_valid(snapshot_json)),
                hidden INTEGER NOT NULL DEFAULT 0 CHECK(hidden IN (0,1))
            ) STRICT;
            CREATE TABLE IF NOT EXISTS project_aliases (
                bundle_id TEXT PRIMARY KEY,
                canonical_id TEXT NOT NULL REFERENCES project_catalog(bundle_id),
                snapshot_json TEXT NOT NULL CHECK(json_valid(snapshot_json)),
                config_pending INTEGER NOT NULL DEFAULT 0 CHECK(config_pending IN (0,1))
            ) STRICT;
            CREATE TABLE IF NOT EXISTS project_session_aliases (
                session_id TEXT NOT NULL REFERENCES session_contexts(session_id) ON DELETE CASCADE,
                bundle_id TEXT NOT NULL, PRIMARY KEY(session_id,bundle_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS project_locations (
                host TEXT NOT NULL,
                directory BLOB NOT NULL,
                checkout_root BLOB NOT NULL,
                repository_root BLOB NOT NULL,
                identity_json TEXT NOT NULL CHECK(json_valid(identity_json)),
                seen_at TEXT NOT NULL,
                PRIMARY KEY(host, directory)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS project_seed_homes (
                harness TEXT NOT NULL,
                home BLOB NOT NULL,
                PRIMARY KEY(harness, home)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS project_seed_failures (
             harness TEXT NOT NULL, home BLOB NOT NULL, directory BLOB NOT NULL, error TEXT NOT NULL, source_file INTEGER NOT NULL CHECK(source_file IN (0,1)),
             PRIMARY KEY(harness,home,directory)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS project_discovery_changes (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                directory BLOB,
                managed_worktree TEXT,
                target_template_id TEXT NOT NULL
            ) STRICT;
            CREATE TABLE IF NOT EXISTS project_discovery_progress (
                singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                sequence INTEGER NOT NULL DEFAULT 0
            ) STRICT;
            CREATE TABLE IF NOT EXISTS project_discovery_failures (
                sequence INTEGER PRIMARY KEY REFERENCES project_discovery_changes(sequence) ON DELETE CASCADE,
                error TEXT NOT NULL
            ) STRICT;
            INSERT OR IGNORE INTO project_discovery_progress(singleton) VALUES(1);
            INSERT INTO project_discovery_changes(session_id, directory, managed_worktree, target_template_id)
                SELECT session_id, project_directory, managed_worktree, target_template_id FROM sessions
                WHERE project_directory IS NOT NULL AND NOT EXISTS (
                    SELECT 1 FROM project_discovery_changes d WHERE d.session_id=sessions.session_id
                );
            CREATE TRIGGER IF NOT EXISTS project_discovery_insert AFTER INSERT ON sessions
                WHEN NEW.project_directory IS NOT NULL BEGIN
                    INSERT INTO project_discovery_changes(session_id,directory,managed_worktree,target_template_id)
                    VALUES(NEW.session_id,NEW.project_directory,NEW.managed_worktree,NEW.target_template_id);
                END;
            CREATE TRIGGER IF NOT EXISTS project_discovery_update AFTER UPDATE OF project_directory,managed_worktree,target_template_id ON sessions
                WHEN NEW.project_directory IS NOT NULL AND
                    (NEW.project_directory IS NOT OLD.project_directory
                    OR NEW.managed_worktree IS NOT OLD.managed_worktree
                    OR NEW.target_template_id IS NOT OLD.target_template_id) BEGIN
                    INSERT INTO project_discovery_changes(session_id,directory,managed_worktree,target_template_id)
                    VALUES(NEW.session_id,NEW.project_directory,NEW.managed_worktree,NEW.target_template_id);
                END;
            UPDATE schema_compatibility SET minimum_compatible_version=67 WHERE singleton=1;
            INSERT INTO schema_migrations(version,applied_at) VALUES(67,strftime('%Y-%m-%dT%H:%M:%fZ','now'));
            PRAGMA user_version=67;
            COMMIT;
