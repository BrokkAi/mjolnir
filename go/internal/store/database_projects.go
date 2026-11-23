// Port of mj-store/src/database/projects.rs.

// Durable catalog decisions share the controller's serialized database
// writer.

package store

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type ProjectDiscoveryChange struct {
	Sequence  int64
	SessionID string
	Directory string
	TargetID  string
}

func ReadProjectCatalog() (core.ProjectCatalogView, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return core.ProjectCatalogView{}, err
	}
	defer connection.Close()
	return readProjectCatalogFrom(connection)
}

func readProjectCatalogFrom(conn Queryer) (core.ProjectCatalogView, error) {
	var view core.ProjectCatalogView
	type catalogRow struct{ bundleID, json string }
	projects, err := valuesQueryMap(conn, "SELECT bundle_id,snapshot_json FROM project_catalog WHERE hidden=0 ORDER BY bundle_id", nil,
		func(rows *sql.Rows) (catalogRow, error) {
			var row catalogRow
			err := rows.Scan(&row.bundleID, &row.json)
			return row, err
		})
	if err != nil {
		return core.ProjectCatalogView{}, err
	}
	for _, row := range projects {
		var project core.ProjectBundleSnapshot
		if err := json.Unmarshal([]byte(row.json), &project); err != nil {
			return core.ProjectCatalogView{}, err
		}
		view.Projects = append(view.Projects, core.SavedProject{BundleID: row.bundleID, Name: project.Name(), Project: project})
	}
	type locationRow struct {
		host                                    string
		directory, checkoutRoot, repositoryRoot []byte
		identity, seenAt                        string
	}
	locations, err := valuesQueryMap(conn, `SELECT host,directory,checkout_root,repository_root,identity_json,seen_at
         FROM project_locations ORDER BY seen_at DESC,host,directory`, nil, func(rows *sql.Rows) (locationRow, error) {
		var row locationRow
		err := rows.Scan(&row.host, &row.directory, &row.checkoutRoot, &row.repositoryRoot, &row.identity, &row.seenAt)
		return row, err
	})
	if err != nil {
		return core.ProjectCatalogView{}, err
	}
	for _, row := range locations {
		identity, err := core.UnmarshalRepositoryIdentity([]byte(row.identity))
		if err != nil {
			return core.ProjectCatalogView{}, err
		}
		view.Locations = append(view.Locations, core.ProjectLocation{
			Host:           row.host,
			Directory:      blobToPath(row.directory),
			CheckoutRoot:   blobToPath(row.checkoutRoot),
			RepositoryRoot: blobToPath(row.repositoryRoot),
			Identity:       identity,
			SeenAt:         row.seenAt,
		})
	}
	return view, nil
}

// projectsRowsAffected is rusqlite's execute count.
func projectsRowsAffected(result sql.Result, err error) (int64, error) {
	if err != nil {
		return 0, err
	}
	return result.RowsAffected()
}

// projectsWrite runs a catalog write in the writer under an upgrade
// activity guard that lasts as long as the write, as Rust's `_commit` guard
// does.
func projectsWrite[T any](label string, activity func() (*core.UpgradeWork, error), write func() (T, error)) (T, error) {
	return submitDatabaseWrite(label, func(*Connection) (T, error) {
		var zero T
		commit, err := activity()
		if err != nil {
			return zero, err
		}
		defer commit.Release()
		return write()
	})
}

func StoreProjectLocation(location *core.ProjectLocation) error {
	if strings.TrimSpace(location.Host) == "" ||
		!core.IsAbsoluteOnHostOrTarget(location.Directory) ||
		!core.IsAbsoluteOnHostOrTarget(location.CheckoutRoot) ||
		!core.IsAbsoluteOnHostOrTarget(location.RepositoryRoot) {
		return fmt.Errorf("project location on %s must contain absolute directory, checkout, and repository paths", core.RustDebugString(location.Host))
	}
	stored := *location
	_, err := projectsWrite("store_project_location", func() (*core.UpgradeWork, error) {
		return core.UpgradeActivityUnlessDraining("project location catalog")
	}, func() (struct{}, error) {
		connection, err := open(DatabasePath())
		if err != nil {
			return struct{}{}, err
		}
		defer connection.Close()
		identity, err := databaseMarshalJSON(stored.Identity)
		if err != nil {
			return struct{}{}, err
		}
		_, err = connection.Exec(`INSERT INTO project_locations(host,directory,checkout_root,repository_root,identity_json,seen_at)
            VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(host,directory) DO UPDATE SET
            checkout_root=excluded.checkout_root,repository_root=excluded.repository_root,
            identity_json=excluded.identity_json,seen_at=max(project_locations.seen_at,excluded.seen_at)`,
			stored.Host, pathToBlob(stored.Directory), pathToBlob(stored.CheckoutRoot), pathToBlob(stored.RepositoryRoot), string(identity), stored.SeenAt)
		return struct{}{}, err
	})
	return err
}

// StoreCatalogProject records a configured project. Existing choices win
// after first reconciliation. Callers order new entries by reference count
// then ID so a fresh catalog chooses the least disruptive ID.
func StoreCatalogProject(bundleID string, project *core.ProjectBundleSnapshot, configured bool) (string, error) {
	return projectsWrite("store_catalog_project", func() (*core.UpgradeWork, error) {
		return core.UpgradeActivityUnlessDraining("configured project catalog")
	}, func() (string, error) {
		return storeCatalogProjectAt(DatabasePath(), bundleID, project, configured)
	})
}

func storeCatalogProjectAt(path, bundleID string, project *core.ProjectBundleSnapshot, configured bool) (string, error) {
	connection, err := open(path)
	if err != nil {
		return "", err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return "", err
	}
	defer tx.Rollback()
	key, err := project.Key()
	if err != nil {
		return "", err
	}
	encoded, err := databaseMarshalJSON(project)
	if err != nil {
		return "", err
	}
	snapshot := string(encoded)
	// Reintroducing a removed alias as a different project must not redirect
	// newly created sessions to the old project. Existing workers already
	// have session-scoped aliases and retain their original context.
	var previousAlias string
	found, err := valuesOptional(tx.QueryRow("SELECT snapshot_json FROM project_aliases WHERE bundle_id=?1", bundleID).Scan(&previousAlias))
	if err != nil {
		return "", err
	}
	if found {
		var previous core.ProjectBundleSnapshot
		if err := json.Unmarshal([]byte(previousAlias), &previous); err != nil {
			return "", err
		}
		previousKey, err := previous.Key()
		if err != nil {
			return "", err
		}
		if previousKey != key {
			if _, err := tx.Exec("DELETE FROM project_aliases WHERE bundle_id=?1", bundleID); err != nil {
				return "", err
			}
		}
	}
	if err := retireChangedProject(tx, bundleID, key); err != nil {
		return "", err
	}
	canonical := bundleID
	if _, err := valuesOptional(tx.QueryRow("SELECT bundle_id FROM project_catalog WHERE project_key=?1", key).Scan(&canonical)); err != nil {
		return "", err
	}
	if _, err := tx.Exec(`INSERT INTO project_catalog(bundle_id,project_key,snapshot_json) VALUES(?1,?2,?3)
        ON CONFLICT(bundle_id) DO UPDATE SET snapshot_json=excluded.snapshot_json,hidden=0
        WHERE project_catalog.project_key=excluded.project_key AND project_catalog.bundle_id=?4`,
		canonical, key, snapshot, bundleID); err != nil {
		return "", err
	}
	var storedKey string
	if err := tx.QueryRow("SELECT project_key FROM project_catalog WHERE bundle_id=?1", canonical).Scan(&storedKey); err != nil {
		return "", err
	}
	if storedKey != key {
		return "", fmt.Errorf("project %s changed repository membership; save it with a new project ID", core.RustDebugString(canonical))
	}
	if canonical != bundleID {
		if _, err := tx.Exec(`INSERT INTO project_aliases(bundle_id,canonical_id,snapshot_json,config_pending) VALUES(?1,?2,?3,?4)
            ON CONFLICT(bundle_id) DO UPDATE SET canonical_id=excluded.canonical_id,
            snapshot_json=excluded.snapshot_json,config_pending=max(project_aliases.config_pending,excluded.config_pending)`,
			bundleID, canonical, snapshot, configured); err != nil {
			return "", err
		}
	}
	if configured {
		if _, err := tx.Exec("UPDATE project_catalog SET hidden=0 WHERE bundle_id=?1", canonical); err != nil {
			return "", err
		}
	}
	// Capture the old accepted definition before rebinding its context.
	// Active workers retain their own replica and repository IDs across this
	// update.
	if _, err := tx.Exec(`UPDATE sessions SET project_json=?2 WHERE project_json IS NULL AND project_directory IS NULL
        AND session_id IN (SELECT session_id FROM session_contexts WHERE bundle_id=?1)`, bundleID, snapshot); err != nil {
		return "", err
	}
	if canonical != bundleID {
		if _, err := tx.Exec("INSERT OR IGNORE INTO project_session_aliases(session_id,bundle_id) SELECT session_id,bundle_id FROM session_contexts WHERE bundle_id=?1", bundleID); err != nil {
			return "", err
		}
		if _, err := tx.Exec("UPDATE session_contexts SET bundle_id=?2 WHERE bundle_id=?1", bundleID, canonical); err != nil {
			return "", err
		}
	}
	if err := tx.Commit(); err != nil {
		return "", err
	}
	return canonical, nil
}

// retireChangedProject moves the old shape of an edited TOML ID aside. An
// edited TOML ID can describe a new shape. Existing sessions retain the old
// shape and a scoped alias, so their workers cannot write new-project
// history.
func retireChangedProject(tx *Transaction, id, newKey string) error {
	var key, snapshot string
	found, err := valuesOptional(tx.QueryRow("SELECT project_key,snapshot_json FROM project_catalog WHERE bundle_id=?1", id).Scan(&key, &snapshot))
	if err != nil {
		return err
	}
	if !found || key == newKey {
		return nil
	}
	base := id + "-history"
	retired := base
	for suffix := uint32(2); ; suffix++ {
		var exists bool
		if err := tx.QueryRow("SELECT EXISTS(SELECT 1 FROM project_catalog WHERE bundle_id=?1 UNION SELECT 1 FROM project_aliases WHERE bundle_id=?1)", retired).Scan(&exists); err != nil {
			return err
		}
		if !exists {
			break
		}
		retired = fmt.Sprintf("%s-%d", base, suffix)
	}
	statements := []struct {
		query string
		args  []any
	}{
		{"INSERT INTO project_catalog(bundle_id,project_key,snapshot_json,hidden) VALUES(?1,?2,?3,1)", []any{retired, "retiring:" + retired, snapshot}},
		{"UPDATE sessions SET project_json=?2 WHERE project_json IS NULL AND project_directory IS NULL AND session_id IN (SELECT session_id FROM session_contexts WHERE bundle_id=?1)", []any{id, snapshot}},
		{"INSERT OR IGNORE INTO project_session_aliases(session_id,bundle_id) SELECT session_id,bundle_id FROM session_contexts WHERE bundle_id=?1", []any{id}},
		{"UPDATE session_contexts SET bundle_id=?2 WHERE bundle_id=?1", []any{id, retired}},
		{"UPDATE project_aliases SET canonical_id=?2 WHERE canonical_id=?1", []any{id, retired}},
		{"DELETE FROM project_catalog WHERE bundle_id=?1", []any{id}},
		{"UPDATE project_catalog SET project_key=?2 WHERE bundle_id=?1", []any{retired, key}},
	}
	for _, statement := range statements {
		if _, err := tx.Exec(statement.query, statement.args...); err != nil {
			return err
		}
	}
	return nil
}

// ProjectAlias is one pending project alias (Rust's (String, String,
// ProjectBundleSnapshot)): the alias id, its canonical id and its snapshot.
type ProjectAlias struct {
	BundleID    string
	CanonicalID string
	Project     core.ProjectBundleSnapshot
}

func PendingProjectAliases() ([]ProjectAlias, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	type aliasRow struct{ id, canonical, json string }
	rows, err := valuesQueryMap(connection, "SELECT bundle_id,canonical_id,snapshot_json FROM project_aliases WHERE config_pending=1 ORDER BY bundle_id", nil,
		func(rows *sql.Rows) (aliasRow, error) {
			var row aliasRow
			err := rows.Scan(&row.id, &row.canonical, &row.json)
			return row, err
		})
	if err != nil {
		return nil, err
	}
	aliases := make([]ProjectAlias, 0, len(rows))
	for _, row := range rows {
		alias := ProjectAlias{BundleID: row.id, CanonicalID: row.canonical}
		if err := json.Unmarshal([]byte(row.json), &alias.Project); err != nil {
			return nil, err
		}
		aliases = append(aliases, alias)
	}
	return aliases, nil
}

func FinishProjectAlias(bundleID string) error {
	_, err := projectsWrite("finish_project_alias", func() (*core.UpgradeWork, error) {
		return core.UpgradeActivityUnlessDraining("project alias completion")
	}, func() (struct{}, error) {
		connection, err := open(DatabasePath())
		if err != nil {
			return struct{}{}, err
		}
		defer connection.Close()
		_, err = connection.Exec("UPDATE project_aliases SET config_pending=0 WHERE bundle_id=?1", bundleID)
		return struct{}{}, err
	})
	return err
}

func sessionProjectID(conn Queryer, sessionID, bundleID string) (string, error) {
	var scoped string
	found, err := valuesOptional(conn.QueryRow("SELECT c.bundle_id FROM session_contexts c JOIN project_session_aliases a ON a.session_id=c.session_id WHERE c.session_id=?1 AND a.bundle_id=?2", sessionID, bundleID).Scan(&scoped))
	if err != nil {
		return "", err
	}
	if found {
		return scoped, nil
	}
	return canonicalProjectID(conn, bundleID)
}

func canonicalProjectID(conn Queryer, bundleID string) (string, error) {
	canonical := bundleID
	if _, err := valuesOptional(conn.QueryRow("SELECT canonical_id FROM project_aliases WHERE bundle_id=?1", bundleID).Scan(&canonical)); err != nil {
		return "", err
	}
	return canonical, nil
}

func ProjectSeeded(harness string, home string) (bool, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return false, err
	}
	defer connection.Close()
	var seeded bool
	err = connection.QueryRow("SELECT EXISTS(SELECT 1 FROM project_seed_homes WHERE harness=?1 AND home=?2)", harness, pathToBlob(home)).Scan(&seeded)
	return seeded, err
}

func FinishProjectSeed(harness string, home string) error {
	_, err := projectsWrite("finish_project_seed", func() (*core.UpgradeWork, error) {
		return core.UpgradeActivityUnlessDraining("project seed completion")
	}, func() (struct{}, error) {
		connection, err := open(DatabasePath())
		if err != nil {
			return struct{}{}, err
		}
		defer connection.Close()
		_, err = connection.Exec("INSERT OR IGNORE INTO project_seed_homes(harness,home) VALUES(?1,?2)", harness, pathToBlob(home))
		return struct{}{}, err
	})
	return err
}

func ProjectDiscoveryChanges(retry bool) ([]ProjectDiscoveryChange, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	changes, err := valuesQueryMap(connection, `SELECT sequence,session_id,directory,target_template_id
        FROM project_discovery_changes WHERE sequence>(SELECT sequence FROM project_discovery_progress WHERE singleton=1)
        OR (?1 AND sequence IN (SELECT sequence FROM project_discovery_failures)) ORDER BY sequence LIMIT CASE WHEN ?1 THEN -1 ELSE 128 END`,
		[]any{retry}, func(rows *sql.Rows) (ProjectDiscoveryChange, error) {
			var change ProjectDiscoveryChange
			var directory []byte
			if err := rows.Scan(&change.Sequence, &change.SessionID, &directory, &change.TargetID); err != nil {
				return ProjectDiscoveryChange{}, err
			}
			change.Directory = blobToPath(directory)
			return change, nil
		})
	if err != nil {
		return nil, err
	}
	if changes == nil {
		changes = []ProjectDiscoveryChange{}
	}
	return changes, nil
}

func FinishProjectDiscovery(sequence int64, err *string) error {
	failure := err
	_, writeErr := projectsWrite("finish_project_discovery", func() (*core.UpgradeWork, error) {
		return core.UpgradeActivityUnlessDraining("project discovery progress")
	}, func() (struct{}, error) {
		connection, err := open(DatabasePath())
		if err != nil {
			return struct{}{}, err
		}
		defer connection.Close()
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		if failure != nil {
			_, err = tx.Exec("INSERT INTO project_discovery_failures(sequence,error) VALUES(?1,?2) ON CONFLICT(sequence) DO UPDATE SET error=excluded.error", sequence, *failure)
		} else {
			_, err = tx.Exec("DELETE FROM project_discovery_failures WHERE sequence=?1", sequence)
		}
		if err != nil {
			return struct{}{}, err
		}
		if _, err := tx.Exec("UPDATE project_discovery_progress SET sequence=max(sequence,?1) WHERE singleton=1", sequence); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return writeErr
}

// StoreDirectoryProject allocates a catalog ID and decides reuse inside the
// serialized writer.
func StoreDirectoryProject(project *core.ProjectBundleSnapshot) (string, error) {
	return projectsWrite("store_directory_project", func() (*core.UpgradeWork, error) {
		return core.UpgradeActivityUnlessDraining("directory project catalog")
	}, func() (string, error) {
		connection, err := open(DatabasePath())
		if err != nil {
			return "", err
		}
		key, err := project.Key()
		if err != nil {
			connection.Close()
			return "", err
		}
		var existing string
		found, err := valuesOptional(connection.QueryRow("SELECT bundle_id FROM project_catalog WHERE project_key=?1", key).Scan(&existing))
		if err != nil {
			connection.Close()
			return "", err
		}
		if found {
			_, err := connection.Exec("UPDATE project_catalog SET hidden=0 WHERE bundle_id=?1", existing)
			connection.Close()
			if err != nil {
				return "", err
			}
			return existing, nil
		}
		base := SetupStyleID(project.Name())
		id := base
		for suffix := uint32(2); ; suffix++ {
			var used bool
			if err := connection.QueryRow("SELECT EXISTS(SELECT 1 FROM project_catalog WHERE bundle_id=?1 UNION SELECT 1 FROM project_aliases WHERE bundle_id=?1)", id).Scan(&used); err != nil {
				connection.Close()
				return "", err
			}
			if !used {
				break
			}
			id = fmt.Sprintf("%s-%d", base, suffix)
		}
		connection.Close()
		return storeCatalogProjectAt(DatabasePath(), id, project, false)
	})
}

func BindSessionProject(sessionID, bundleID string, project *core.ProjectBundleSnapshot) error {
	_, err := projectsWrite("bind_session_project", func() (*core.UpgradeWork, error) {
		return core.UpgradeActivityUnlessDraining("session project catalog")
	}, func() (struct{}, error) {
		connection, err := open(DatabasePath())
		if err != nil {
			return struct{}{}, err
		}
		defer connection.Close()
		tx, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		encoded, err := databaseMarshalJSON(project)
		if err != nil {
			return struct{}{}, err
		}
		// Discovery backfills identity; it never changes an accepted shape.
		updated, err := projectsRowsAffected(tx.Exec("UPDATE sessions SET project_json=?2 WHERE session_id=?1 AND project_json IS NULL", sessionID, string(encoded)))
		if err != nil {
			return struct{}{}, err
		}
		if updated != 0 {
			if _, err := tx.Exec("INSERT OR IGNORE INTO project_session_aliases(session_id,bundle_id) SELECT session_id,bundle_id FROM session_contexts WHERE session_id=?1", sessionID); err != nil {
				return struct{}{}, err
			}
			if _, err := tx.Exec("UPDATE session_contexts SET bundle_id=?2 WHERE session_id=?1", sessionID, bundleID); err != nil {
				return struct{}{}, err
			}
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

func SeedFailure(harness string, home string, directory string, sourceFile bool, err *string) error {
	_, failure := projectsWrite("project_seed_failure", func() (*core.UpgradeWork, error) {
		return core.UpgradeActivityUnlessDraining("project seed retry catalog")
	}, func() (struct{}, error) {
		connection, openErr := open(DatabasePath())
		if openErr != nil {
			return struct{}{}, openErr
		}
		defer connection.Close()
		if err != nil {
			_, execErr := connection.Exec("INSERT INTO project_seed_failures(harness,home,directory,error,source_file) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(harness,home,directory) DO UPDATE SET error=excluded.error,source_file=excluded.source_file",
				harness, pathToBlob(home), pathToBlob(directory), *err, sourceFile)
			return struct{}{}, execErr
		}
		_, execErr := connection.Exec("DELETE FROM project_seed_failures WHERE harness=?1 AND home=?2 AND directory=?3",
			harness, pathToBlob(home), pathToBlob(directory))
		return struct{}{}, execErr
	})
	return failure
}

// ProjectSeedFailure is one recorded seed failure (Rust's (String, PathBuf,
// PathBuf, bool)).
type ProjectSeedFailure struct {
	Harness    string
	Home       string
	Directory  string
	SourceFile bool
}

func SeedFailures() ([]ProjectSeedFailure, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	failures, err := valuesQueryMap(connection, "SELECT harness,home,directory,source_file FROM project_seed_failures ORDER BY harness,home,directory", nil,
		func(rows *sql.Rows) (ProjectSeedFailure, error) {
			var failure ProjectSeedFailure
			var home, directory any
			if err := rows.Scan(&failure.Harness, &home, &directory, &failure.SourceFile); err != nil {
				return ProjectSeedFailure{}, err
			}
			homeBlob, ok := home.([]byte)
			if !ok {
				return ProjectSeedFailure{}, projectsBlobError(home)
			}
			directoryBlob, ok := directory.([]byte)
			if !ok {
				return ProjectSeedFailure{}, projectsBlobError(directory)
			}
			failure.Home, failure.Directory = blobToPath(homeBlob), blobToPath(directoryBlob)
			return failure, nil
		})
	if err != nil {
		return nil, err
	}
	if failures == nil {
		failures = []ProjectSeedFailure{}
	}
	return failures, nil
}

// projectsBlobError is rusqlite's error for a column that as_blob refuses.
func projectsBlobError(value any) error {
	if _, _, err := blobOrNull(value); err != nil {
		return err
	}
	return fmt.Errorf("Invalid column type Null")
}

func DiscoveryErrors() ([]string, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	errorsFound, err := valuesQueryMap(connection, "SELECT error FROM project_discovery_failures UNION SELECT error FROM project_seed_failures ORDER BY error", nil,
		func(rows *sql.Rows) (string, error) {
			var message string
			err := rows.Scan(&message)
			return message, err
		})
	if err != nil {
		return nil, err
	}
	if errorsFound == nil {
		errorsFound = []string{}
	}
	return errorsFound, nil
}

// SavedProject is the canonical id and snapshot a bundle id names; ok is
// false when the catalog has neither (Rust's Option<(String,
// ProjectBundleSnapshot)>).
func SavedProject(bundleID string) (canonical string, project core.ProjectBundleSnapshot, ok bool, err error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return "", core.ProjectBundleSnapshot{}, false, err
	}
	defer connection.Close()
	canonical, err = canonicalProjectID(connection, bundleID)
	if err != nil {
		return "", core.ProjectBundleSnapshot{}, false, err
	}
	var snapshot string
	found, err := valuesOptional(connection.QueryRow("SELECT snapshot_json FROM project_aliases WHERE bundle_id=?1 UNION ALL SELECT snapshot_json FROM project_catalog WHERE bundle_id=?1 LIMIT 1", bundleID).Scan(&snapshot))
	if err != nil || !found {
		return "", core.ProjectBundleSnapshot{}, false, err
	}
	if err := json.Unmarshal([]byte(snapshot), &project); err != nil {
		return "", core.ProjectBundleSnapshot{}, false, err
	}
	return canonical, project, true, nil
}

func HideCatalogProject(bundleID string) error {
	_, err := projectsWrite("hide_catalog_project", func() (*core.UpgradeWork, error) {
		return core.UpgradeActivity("remove project catalog entry")
	}, func() (struct{}, error) {
		connection, err := open(DatabasePath())
		if err != nil {
			return struct{}{}, err
		}
		defer connection.Close()
		_, err = connection.Exec("UPDATE project_catalog SET hidden=1 WHERE bundle_id=?1", bundleID)
		return struct{}{}, err
	})
	return err
}

// SetupStyleID is a project name reduced to the characters a bundle id may
// contain.
func SetupStyleID(value string) string {
	id := make([]rune, 0, 64)
	for _, character := range value {
		if len(id) == 64 {
			break
		}
		if character < 0x80 && (character >= '0' && character <= '9' || character >= 'a' && character <= 'z' || character >= 'A' && character <= 'Z' || character == '-' || character == '_' || character == '.') {
			id = append(id, character)
		}
	}
	if len(id) == 0 || string(id) == "." || string(id) == ".." {
		return "repository"
	}
	return string(id)
}
