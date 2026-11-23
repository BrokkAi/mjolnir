// Port of mj-store/src/database/environments.rs.

// Physical ownership outlives logical sessions and their parent
// relationships.

package store

import (
	"cmp"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"reflect"
	"slices"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type EnvironmentPhase string

const (
	EnvironmentPhaseActive    EnvironmentPhase = "active"
	EnvironmentPhaseRetiring  EnvironmentPhase = "retiring"
	EnvironmentPhaseReclaimed EnvironmentPhase = "reclaimed"
)

func environmentPhaseFromStored(value string) (EnvironmentPhase, error) {
	switch phase := EnvironmentPhase(value); phase {
	case EnvironmentPhaseActive, EnvironmentPhaseRetiring, EnvironmentPhaseReclaimed:
		return phase, nil
	}
	return "", fmt.Errorf("unknown environment phase %q", value)
}

// EnvironmentPlacement is resource metadata only: no prompt history, agent
// state or logical lifecycle.
//
// Its JSON denies unknown members and never carries Target (serde skip).
// ProjectDirectory and ContainerWorkspace keep the database path encoding
// (optional_path): a JSON array of the path's stored bytes, or null, and both
// members are required.
type EnvironmentPlacement struct {
	Target             core.TargetLocator
	TargetTemplateID   string
	Runtime            *core.TargetRuntimeSettings
	ManagedWorktree    *core.ManagedWorktree
	ProjectDirectory   *string
	ContainerWorkspace *string
	BuildCache         *core.SessionBuildCache
	// Repositories is nil when unknown; a non-nil empty slice is a project
	// with no repositories.
	Repositories        *[]string
	BundleID            string
	PreparedDestination *EnvironmentPreparedDestination
}

// environmentPlacementJSON is EnvironmentPlacement's serialized form, in
// field order.
type environmentPlacementJSON struct {
	TargetTemplateID    string                          `json:"target_template_id"`
	Runtime             *core.TargetRuntimeSettings     `json:"runtime"`
	ManagedWorktree     *core.ManagedWorktree           `json:"managed_worktree"`
	ProjectDirectory    any                             `json:"project_directory"`
	ContainerWorkspace  any                             `json:"container_workspace"`
	BuildCache          *core.SessionBuildCache         `json:"build_cache"`
	Repositories        *[]string                       `json:"repositories"`
	BundleID            string                          `json:"bundle_id"`
	PreparedDestination *EnvironmentPreparedDestination `json:"prepared_destination,omitempty"`
}

func (p EnvironmentPlacement) MarshalJSON() ([]byte, error) {
	return databaseMarshalJSON(environmentPlacementJSON{
		TargetTemplateID:    p.TargetTemplateID,
		Runtime:             p.Runtime,
		ManagedWorktree:     p.ManagedWorktree,
		ProjectDirectory:    optionalPathSerialize(p.ProjectDirectory),
		ContainerWorkspace:  optionalPathSerialize(p.ContainerWorkspace),
		BuildCache:          p.BuildCache,
		Repositories:        p.Repositories,
		BundleID:            p.BundleID,
		PreparedDestination: p.PreparedDestination,
	})
}

func (p *EnvironmentPlacement) UnmarshalJSON(data []byte) error {
	if err := databaseCheckFields(data, true,
		[]string{"target_template_id", "runtime", "managed_worktree", "project_directory",
			"container_workspace", "build_cache", "repositories", "bundle_id", "prepared_destination"},
		[]string{"target_template_id", "project_directory", "container_workspace", "bundle_id"}); err != nil {
		return err
	}
	var wire struct {
		environmentPlacementJSON
		ProjectDirectory   *[]uint8 `json:"project_directory"`
		ContainerWorkspace *[]uint8 `json:"container_workspace"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*p = EnvironmentPlacement{
		TargetTemplateID:    wire.TargetTemplateID,
		Runtime:             wire.Runtime,
		ManagedWorktree:     wire.ManagedWorktree,
		ProjectDirectory:    optionalPathDeserialize(wire.ProjectDirectory),
		ContainerWorkspace:  optionalPathDeserialize(wire.ContainerWorkspace),
		BuildCache:          wire.BuildCache,
		Repositories:        wire.Repositories,
		BundleID:            wire.BundleID,
		PreparedDestination: wire.PreparedDestination,
	}
	return nil
}

type EnvironmentPreparedDestination struct {
	OperationID string                       `json:"operation_id"`
	Destination core.PreparedMoveDestination `json:"destination"`
}

// UnmarshalJSON requires both members and denies unknown ones, as serde does.
func (d *EnvironmentPreparedDestination) UnmarshalJSON(data []byte) error {
	fields := []string{"operation_id", "destination"}
	if err := databaseCheckFields(data, true, fields, fields); err != nil {
		return err
	}
	type plain EnvironmentPreparedDestination
	return json.Unmarshal(data, (*plain)(d))
}

func EnvironmentPlacementFromSession(session *core.SessionRecord) EnvironmentPlacement {
	placement := EnvironmentPlacement{
		Target:             session.Target,
		TargetTemplateID:   session.TargetTemplateID,
		ProjectDirectory:   session.ProjectDirectory,
		ContainerWorkspace: session.ContainerWorkspace,
		BundleID:           session.BundleID,
	}
	if session.BuildCache != nil {
		cache := *session.BuildCache
		placement.BuildCache = &cache
	}
	if session.TargetRuntime != nil {
		runtime := *session.TargetRuntime
		placement.Runtime = &runtime
	}
	if session.ManagedWorktree != nil {
		worktree := *session.ManagedWorktree
		placement.ManagedWorktree = &worktree
	}
	if session.Project != nil {
		repositories := make([]string, 0, len(session.Project.Bundle.Repositories))
		for _, repository := range session.Project.Bundle.Repositories {
			repositories = append(repositories, repository.Destination)
		}
		placement.Repositories = &repositories
	}
	return placement
}

type EnvironmentRecord struct {
	ID             string
	OwnerSessionID string
	Phase          EnvironmentPhase
	Placement      EnvironmentPlacement
}

type EnvironmentWorker struct {
	leaseID          string
	SessionID        string
	Target           core.TargetLocator
	TargetTemplateID string
	Runtime          *core.TargetRuntimeSettings
}

// environmentRowValues scans one row's first n columns as rusqlite's Row
// holds them: a NULL column is nil.
func environmentRowValues(scan func(dest ...any) error, n int) ([]any, error) {
	values := make([]any, n)
	pointers := make([]any, n)
	for i := range values {
		pointers[i] = &values[i]
	}
	if err := scan(pointers...); err != nil {
		return nil, err
	}
	return values, nil
}

func readEnvironment(connection Queryer, id string) (*EnvironmentRecord, error) {
	var owner, phase, text string
	var lease *string
	err := connection.QueryRow("SELECT owner_session_id,phase,placement_json,target_lease_id FROM environments WHERE environment_id=?1", id).
		Scan(&owner, &phase, &text, &lease)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var placement EnvironmentPlacement
	if err := json.Unmarshal([]byte(text), &placement); err != nil {
		return nil, fmt.Errorf("read environment placement: %w", err)
	}
	if lease != nil {
		row, err := environmentRowValues(connection.QueryRow("SELECT kind,host,resource_id,address,workspace,worker_id,workspace_storage,borrowed_from FROM environment_workers WHERE lease_id=?1", *lease).Scan, 8)
		if err != nil {
			return nil, err
		}
		if placement.Target, err = decodeTarget(row, 0); err != nil {
			return nil, err
		}
	}
	stored, err := environmentPhaseFromStored(phase)
	if err != nil {
		return nil, err
	}
	return &EnvironmentRecord{ID: id, OwnerSessionID: owner, Phase: stored, Placement: placement}, nil
}

// environmentByID reads the environment id names, nil when id is nil
// (Rust's id.map(read_environment).transpose().map(Option::flatten)).
func environmentByID(connection Queryer, id *string) (*EnvironmentRecord, error) {
	if id == nil {
		return nil, nil
	}
	return readEnvironment(connection, *id)
}

// environmentOptionalString is a query_row(...).optional() of one nullable
// text column: nil for no row or NULL.
func environmentOptionalString(connection Queryer, query string, args ...any) (*string, error) {
	var value *string
	err := connection.QueryRow(query, args...).Scan(&value)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	return value, err
}

func SessionEnvironment(sessionID string) (*EnvironmentRecord, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	id, err := environmentOptionalString(connection,
		"SELECT environment_id FROM environment_bindings WHERE session_id=?1", sessionID)
	if err != nil {
		return nil, err
	}
	return environmentByID(connection, id)
}

const environmentOwnerQuery = `SELECT b.session_id, e.owner_session_id FROM environment_bindings b
     JOIN environments e ON e.environment_id = b.environment_id`

func SessionEnvironmentOwner(sessionID string) (*string, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return sessionEnvironmentOwnerWith(connection, sessionID)
}

func sessionEnvironmentOwnerWith(connection Queryer, sessionID string) (*string, error) {
	var owner string
	err := connection.QueryRow(environmentOwnerQuery+" WHERE b.session_id = ?1", sessionID).Scan(new(string), &owner)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return &owner, nil
}

func SessionOwnsEnvironment(sessionID string) (bool, error) {
	owner, err := SessionEnvironmentOwner(sessionID)
	if err != nil {
		return false, err
	}
	return owner != nil && *owner == sessionID, nil
}

func EnvironmentWorkers(environmentID string) ([]EnvironmentWorker, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return readEnvironmentWorkers(connection, environmentID)
}

// environmentColumnText is a TEXT column of a row scanned by
// environmentRowValues.
func environmentColumnText(row []any, index int, name string) (string, error) {
	text, ok := row[index].(string)
	if !ok {
		return "", fmt.Errorf("Invalid column type %T at index: %d, name: %s", row[index], index, name)
	}
	return text, nil
}

func readEnvironmentWorkers(connection Queryer, environmentID string) ([]EnvironmentWorker, error) {
	rows, err := connection.Query("SELECT worker_session_id,kind,host,resource_id,address,workspace,worker_id,workspace_storage,borrowed_from,target_template_id,runtime_json,lease_id FROM environment_workers WHERE environment_id=?1 ORDER BY worker_session_id,lease_id", environmentID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	workers := []EnvironmentWorker{}
	for rows.Next() {
		row, err := environmentRowValues(rows.Scan, 12)
		if err != nil {
			return nil, err
		}
		sessionID, err := environmentColumnText(row, 0, "worker_session_id")
		if err != nil {
			return nil, err
		}
		target, err := decodeTarget(row, 1)
		if err != nil {
			return nil, err
		}
		templateID, err := environmentColumnText(row, 9, "target_template_id")
		if err != nil {
			return nil, err
		}
		leaseID, err := environmentColumnText(row, 11, "lease_id")
		if err != nil {
			return nil, err
		}
		worker := EnvironmentWorker{
			leaseID:          leaseID,
			SessionID:        sessionID,
			Target:           target,
			TargetTemplateID: templateID,
		}
		if row[10] != nil {
			runtime, err := environmentColumnText(row, 10, "runtime_json")
			if err != nil {
				return nil, err
			}
			var settings core.TargetRuntimeSettings
			if err := json.Unmarshal([]byte(runtime), &settings); err != nil {
				return nil, fmt.Errorf("read environment worker access: %w", err)
			}
			worker.Runtime = &settings
		}
		workers = append(workers, worker)
	}
	return workers, rows.Err()
}

func storeWorker(
	transaction *sql.Tx,
	environmentID, sessionID, targetTemplateID string,
	runtime *core.TargetRuntimeSettings,
	target core.TargetLocator,
) (string, error) {
	workers, err := readEnvironmentWorkers(transaction, environmentID)
	if err != nil {
		return "", err
	}
	// Rust evaluates the fallback identifier before looking at the
	// existing lease (unwrap_or, not unwrap_or_else).
	lease, err := core.NewSessionID()
	if err != nil {
		return "", err
	}
	for _, worker := range workers {
		if worker.SessionID == sessionID && worker.Target.SameResource(target) {
			lease = worker.leaseID
			break
		}
	}
	fields, err := storedTargetValues(target)
	if err != nil {
		return "", err
	}
	var runtimeJSON *string
	if runtime != nil {
		encoded, err := databaseMarshalJSON(runtime)
		if err != nil {
			return "", err
		}
		text := string(encoded)
		runtimeJSON = &text
	}
	if _, err := transaction.Exec("INSERT INTO environment_workers(lease_id,environment_id,worker_session_id,target_template_id,runtime_json,kind,host,resource_id,address,workspace,worker_id,workspace_storage,borrowed_from) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13) ON CONFLICT(lease_id) DO UPDATE SET address=excluded.address,workspace_storage=excluded.workspace_storage,runtime_json=coalesce(environment_workers.runtime_json,excluded.runtime_json)",
		lease, environmentID, sessionID, targetTemplateID, runtimeJSON, fields.Kind, fields.Host, fields.Resource,
		fields.Address, fields.Workspace, fields.WorkerID, fields.WorkspaceStorage, fields.BorrowedFrom); err != nil {
		return "", err
	}
	environment, err := readEnvironment(transaction, environmentID)
	if err != nil {
		return "", err
	}
	if environment != nil && environment.OwnerSessionID == sessionID {
		if _, err := transaction.Exec("UPDATE environments SET target_lease_id=?2 WHERE environment_id=?1", environmentID, lease); err != nil {
			return "", err
		}
	}
	return lease, nil
}

// insertEnvironment records a new active environment owned by owner.
func insertEnvironment(connection Queryer, id, owner string, placement *EnvironmentPlacement) error {
	encoded, err := databaseMarshalJSON(placement)
	if err != nil {
		return err
	}
	_, err = connection.Exec("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES (?1,?2,'active',?3)",
		id, owner, string(encoded))
	return err
}

// managedWorktreeEqual is ManagedWorktree's derived equality. A kind left
// empty is a worktree, as Rust's default.
func managedWorktreeEqual(a, b *core.ManagedWorktree) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	left, right := *a, *b
	if left.Kind == "" {
		left.Kind = core.ManagedCheckoutKindWorktree
	}
	if right.Kind == "" {
		right.Kind = core.ManagedCheckoutKindWorktree
	}
	return reflect.DeepEqual(left, right)
}

// captureSessionEnvironment is called in the same transaction that publishes
// a placement. A binding is presentation; the environment and its worker
// leases never cascade with it.
func captureSessionEnvironment(transaction *sql.Tx, session *core.SessionRecord, parent *string) error {
	var boundID, boundOwner string
	err := transaction.QueryRow(`SELECT b.environment_id, e.owner_session_id
             FROM environment_bindings b JOIN environments e USING (environment_id)
             WHERE b.session_id=?1`, session.ID).Scan(&boundID, &boundOwner)
	bound := err == nil
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return err
	}
	var id string
	if bound {
		if boundOwner == session.ID || session.Target == nil {
			id = boundID
		} else if id, err = rebindBorrower(transaction, session.ID, boundID); err != nil {
			return err
		}
	} else {
		if parent == nil {
			if parent, err = environmentOptionalString(transaction,
				"SELECT parent_session_id FROM subagent_sessions WHERE child_session_id=?1", session.ID); err != nil {
				return err
			}
		}
		var inherited *string
		if parent != nil {
			if inherited, err = environmentOptionalString(transaction,
				"SELECT environment_id FROM environment_bindings WHERE session_id=?1", *parent); err != nil {
				return err
			}
		}
		if inherited != nil {
			id = *inherited
		} else {
			if parent != nil {
				return errors.New("parent environment must be registered before its worker")
			}
			if id, err = core.NewSessionID(); err != nil {
				return err
			}
			placement := EnvironmentPlacementFromSession(session)
			if err := insertEnvironment(transaction, id, session.ID, &placement); err != nil {
				return err
			}
		}
		environment, err := readEnvironment(transaction, id)
		if err != nil {
			return err
		}
		if environment == nil || environment.Phase != EnvironmentPhaseActive {
			return errors.New("environment is retiring; child registration is closed")
		}
		if _, err := transaction.Exec("INSERT INTO environment_bindings(session_id,environment_id) VALUES (?1,?2)", session.ID, id); err != nil {
			return err
		}
	}
	environment, err := readEnvironment(transaction, id)
	if err != nil {
		return err
	}
	if environment == nil {
		return errors.New("session environment disappeared")
	}
	next := EnvironmentPlacementFromSession(session)
	var storageChanges bool
	before, after := environment.Placement.ManagedWorktree, next.ManagedWorktree
	switch {
	case before != nil && after != nil:
		storageChanges = before.WorktreeRoot != after.WorktreeRoot || !before.Target.SameLocation(after.Target)
	case before != nil:
		storageChanges = true
	case after != nil:
		storageChanges = environment.Placement.Target != nil
	}
	newTarget := session.Target != nil && environment.Placement.Target != nil &&
		!environment.Placement.Target.SameResource(session.Target)
	resumeReclaimed := environment.Phase == EnvironmentPhaseReclaimed && session.State == core.SessionStateProvisioning
	if environment.OwnerSessionID == session.ID && (resumeReclaimed || storageChanges || newTarget) {
		// This transaction publishes a new physical placement. Keep the old
		// owner and leases for Move retirement and interrupted cleanup.
		if id, err = core.NewSessionID(); err != nil {
			return err
		}
		placement := EnvironmentPlacementFromSession(session)
		if err := insertEnvironment(transaction, id, session.ID, &placement); err != nil {
			return err
		}
		if _, err := transaction.Exec("UPDATE environment_bindings SET environment_id=?2 WHERE session_id=?1", session.ID, id); err != nil {
			return err
		}
		if environment, err = readEnvironment(transaction, id); err != nil {
			return err
		}
		if environment == nil {
			return errors.New("new environment disappeared")
		}
	}
	if environment.Phase == EnvironmentPhaseReclaimed {
		return nil
	}
	if environment.OwnerSessionID == session.ID && (session.Target != nil || environment.Placement.Target == nil) {
		placement := EnvironmentPlacementFromSession(session)
		encoded, err := databaseMarshalJSON(&placement)
		if err != nil {
			return err
		}
		if _, err := transaction.Exec("UPDATE environments SET placement_json=?2 WHERE environment_id=?1", id, string(encoded)); err != nil {
			return err
		}
	}
	if session.Target != nil {
		workers, err := readEnvironmentWorkers(transaction, id)
		if err != nil {
			return err
		}
		existing := false
		for _, worker := range workers {
			if worker.SessionID == session.ID && worker.Target.SameResource(session.Target) {
				existing = true
				break
			}
		}
		if !existing && environment.Phase != EnvironmentPhaseActive {
			return errors.New("environment is retiring; new worker admission is closed")
		}
		if _, err := storeWorker(transaction, id, session.ID, session.TargetTemplateID, session.TargetRuntime, session.Target); err != nil {
			return err
		}
	}
	return nil
}

func rebindBorrower(transaction *sql.Tx, sessionID, boundID string) (string, error) {
	parent, err := environmentOptionalString(transaction,
		"SELECT parent_session_id FROM subagent_sessions WHERE child_session_id=?1", sessionID)
	if err != nil {
		return "", err
	}
	if parent == nil {
		return boundID, nil
	}
	var current string
	if err := transaction.QueryRow("SELECT environment_id FROM environment_bindings WHERE session_id=?1", *parent).Scan(&current); err != nil {
		return "", err
	}
	if current != boundID {
		environment, err := readEnvironment(transaction, current)
		if err != nil {
			return "", err
		}
		if environment == nil {
			return "", errors.New("parent environment disappeared")
		}
		if environment.Phase != EnvironmentPhaseActive {
			return "", errors.New("parent environment is retiring; worker admission is closed")
		}
		if _, err := transaction.Exec("UPDATE environment_bindings SET environment_id=?2 WHERE session_id=?1", sessionID, current); err != nil {
			return "", err
		}
	}
	return current, nil
}

// environmentParents is every recorded sub-agent's parent, by child.
func environmentParents(connection Queryer) (map[string]string, error) {
	rows, err := connection.Query("SELECT child_session_id,parent_session_id FROM subagent_sessions")
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	parents := map[string]string{}
	for rows.Next() {
		var child, parent string
		if err := rows.Scan(&child, &parent); err != nil {
			return nil, err
		}
		parents[child] = parent
	}
	return parents, rows.Err()
}

// BeginEnvironmentRetirement closes admission and selects the writers in one
// serialized decision.
func BeginEnvironmentRetirement(environmentID string) (EnvironmentRecord, []EnvironmentWorker, error) {
	type retirement struct {
		environment EnvironmentRecord
		workers     []EnvironmentWorker
	}
	result, err := submitDatabaseWrite("retire environment", func(connection *Connection) (retirement, error) {
		transaction, err := connection.Begin()
		if err != nil {
			return retirement{}, err
		}
		defer transaction.Rollback()
		environment, err := readEnvironment(transaction, environmentID)
		if err != nil {
			return retirement{}, err
		}
		if environment == nil {
			return retirement{}, errors.New("unknown environment")
		}
		if environment.Phase == EnvironmentPhaseReclaimed {
			return retirement{environment: *environment, workers: []EnvironmentWorker{}}, nil
		}
		if _, err := transaction.Exec("UPDATE environments SET phase='retiring' WHERE environment_id=?1", environmentID); err != nil {
			return retirement{}, err
		}
		workers, err := readEnvironmentWorkers(transaction, environmentID)
		if err != nil {
			return retirement{}, err
		}
		parents, err := environmentParents(transaction)
		if err != nil {
			return retirement{}, err
		}
		depths := map[string]int{}
		for _, worker := range workers {
			current := worker.SessionID
			seen := map[string]struct{}{}
			depth := 0
			for current != environment.OwnerSessionID {
				if _, repeated := seen[current]; repeated {
					return retirement{}, fmt.Errorf("cyclic sub-agent ancestry during environment retirement: %s", worker.SessionID)
				}
				seen[current] = struct{}{}
				depth++
				parent, ok := parents[current]
				if !ok {
					// Historical rows outlive relations. Attach an orphan
					// directly under this root, keeping known descendants
					// below it.
					break
				}
				current = parent
			}
			depths[worker.SessionID] = depth
		}
		// Reviewers can nest under workers; Suspend/ForceDestroy acquire
		// parent then child. Taking a lower-ID descendant first could
		// deadlock against its parent's close.
		slices.SortStableFunc(workers, func(a, b EnvironmentWorker) int {
			if order := cmp.Compare(depths[a.SessionID], depths[b.SessionID]); order != 0 {
				return order
			}
			if order := strings.Compare(a.SessionID, b.SessionID); order != 0 {
				return order
			}
			return strings.Compare(a.leaseID, b.leaseID)
		})
		if err := transaction.Commit(); err != nil {
			return retirement{}, err
		}
		environment.Phase = EnvironmentPhaseRetiring
		return retirement{environment: *environment, workers: workers}, nil
	})
	return result.environment, result.workers, err
}

func FinishEnvironmentRetirement(environmentID string) error {
	_, err := submitDatabaseWrite("finish environment retirement", func(connection *Connection) (struct{}, error) {
		transaction, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer transaction.Rollback()
		result, err := transaction.Exec("UPDATE environments SET phase='reclaimed' WHERE environment_id=?1 AND phase='retiring'", environmentID)
		if err != nil {
			return struct{}{}, err
		}
		changed, err := result.RowsAffected()
		if err != nil {
			return struct{}{}, err
		}
		if changed != 1 {
			return struct{}{}, errors.New("environment retirement no longer owns this environment")
		}
		// Keep reclaimed descriptors as tombstones while a logical record
		// can still retry settlement. They are not admitted worker leases.
		if _, err := transaction.Exec("DELETE FROM environment_reports WHERE environment_id=?1", environmentID); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, transaction.Commit()
	})
	return err
}

// FinishEnvironmentRuntimeRemoval: a runtime can be replaced while its
// workspace remains owned by this same environment. Only the owner, after
// draining every writer and removing the old runtime, may release those
// leases and reopen launch admission.
func FinishEnvironmentRuntimeRemoval(environmentID string) error {
	_, err := submitDatabaseWrite("finish environment runtime removal", func(connection *Connection) (struct{}, error) {
		transaction, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer transaction.Rollback()
		environment, err := readEnvironment(transaction, environmentID)
		if err != nil {
			return struct{}{}, err
		}
		if environment == nil {
			return struct{}{}, errors.New("environment disappeared")
		}
		if environment.Phase != EnvironmentPhaseRetiring {
			return struct{}{}, errors.New("environment no longer owns runtime removal")
		}
		environment.Placement.Target = nil
		if _, err := transaction.Exec("DELETE FROM environment_workers WHERE environment_id=?1", environmentID); err != nil {
			return struct{}{}, err
		}
		encoded, err := databaseMarshalJSON(&environment.Placement)
		if err != nil {
			return struct{}{}, err
		}
		if _, err := transaction.Exec("UPDATE environments SET phase='active',placement_json=?2 WHERE environment_id=?1", environmentID, string(encoded)); err != nil {
			return struct{}{}, err
		}
		if _, err := transaction.Exec("UPDATE sessions SET state='error',last_error='The shared environment was stopped for recovery.' WHERE session_id IN (SELECT session_id FROM environment_bindings WHERE environment_id=?1) AND session_id<>?2 AND state IN ('running','disconnected')",
			environmentID, environment.OwnerSessionID); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, transaction.Commit()
	})
	return err
}

// backfillEnvironments: migration uses the existing target decoder,
// including targets whose harness is no longer supported. Physical ownership
// must not depend on that parser.
func backfillEnvironments(transaction *sql.Tx) error {
	targets, err := readTargets(transaction, nil)
	if err != nil {
		return err
	}
	parents, err := environmentParents(transaction)
	if err != nil {
		return err
	}
	records, err := environmentSessionPlacements(transaction, targets)
	if err != nil {
		return err
	}
	ids := slices.Sorted(maps.Keys(records))
	owners := map[string]string{}
	for _, id := range ids {
		owner := id
		visited := map[string]struct{}{}
		for {
			parent, ok := parents[owner]
			if !ok {
				break
			}
			if _, repeated := visited[owner]; repeated {
				return errors.New("cyclic environment ownership in stored sub-agents")
			}
			visited[owner] = struct{}{}
			owner = parent
		}
		if _, ok := records[owner]; !ok {
			return fmt.Errorf("environment owner %s has no recorded placement", owner)
		}
		owners[id] = owner
	}
	distinctOwners := map[string]struct{}{}
	for _, owner := range owners {
		distinctOwners[owner] = struct{}{}
	}
	environmentIDs := map[string]string{}
	for _, owner := range slices.Sorted(maps.Keys(distinctOwners)) {
		id, err := core.NewSessionID()
		if err != nil {
			return err
		}
		placement := records[owner]
		if err := insertEnvironment(transaction, id, owner, &placement); err != nil {
			return err
		}
		environmentIDs[owner] = id
	}
	for _, worker := range ids {
		placement := records[worker]
		id := environmentIDs[owners[worker]]
		if _, err := transaction.Exec("INSERT INTO environment_bindings(session_id,environment_id) VALUES (?1,?2)", worker, id); err != nil {
			return err
		}
		if placement.Target != nil {
			if _, err := storeWorker(transaction, id, worker, placement.TargetTemplateID, placement.Runtime, placement.Target); err != nil {
				return err
			}
		}
	}
	_, err = transaction.Exec("INSERT OR IGNORE INTO environment_reports SELECT b.environment_id,h.child_session_id,h.report_dir FROM subagent_handbacks h JOIN environment_bindings b ON b.session_id=h.child_session_id WHERE h.report_dir IS NOT NULL")
	return err
}

// environmentSessionPlacements is each stored session's physical placement,
// read the way backfillEnvironments needs it.
func environmentSessionPlacements(transaction *sql.Tx, targets map[string]core.TargetLocator) (map[string]EnvironmentPlacement, error) {
	rows, err := transaction.Query("SELECT s.session_id,s.target_template_id,s.target_runtime_json,s.managed_worktree,s.project_directory,s.container_workspace,s.build_cache_json,s.project_json,c.bundle_id FROM sessions s JOIN session_contexts c USING(session_id)")
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	records := map[string]EnvironmentPlacement{}
	for rows.Next() {
		var id, templateID, bundleID string
		var runtime, worktree, containerWorkspace, cache, project *string
		var projectDirectory any
		if err := rows.Scan(&id, &templateID, &runtime, &worktree, &projectDirectory, &containerWorkspace, &cache, &project, &bundleID); err != nil {
			return nil, err
		}
		placement := EnvironmentPlacement{TargetTemplateID: templateID, BundleID: bundleID}
		if target, ok := targets[id]; ok {
			placement.Target = target
		}
		if runtime != nil {
			var settings core.TargetRuntimeSettings
			if err := json.Unmarshal([]byte(*runtime), &settings); err != nil {
				return nil, err
			}
			placement.Runtime = &settings
		}
		if worktree != nil {
			var managed core.ManagedWorktree
			if err := json.Unmarshal([]byte(*worktree), &managed); err != nil {
				return nil, err
			}
			placement.ManagedWorktree = &managed
		}
		blob, present, err := blobOrNull(projectDirectory)
		if err != nil {
			return nil, err
		}
		if present {
			path := blobToPath(blob)
			placement.ProjectDirectory = &path
		}
		placement.ContainerWorkspace = containerWorkspace
		if cache != nil {
			var buildCache core.SessionBuildCache
			if err := json.Unmarshal([]byte(*cache), &buildCache); err != nil {
				return nil, err
			}
			placement.BuildCache = &buildCache
		}
		if project != nil {
			value, err := jsonvalue.Parse([]byte(*project))
			if err != nil {
				return nil, err
			}
			placement.Repositories = projectRepositories(value)
		}
		records[id] = placement
	}
	return records, rows.Err()
}

// environmentRecordsByID reads every environment ids names, in order; each
// must exist.
func environmentRecordsByID(connection Queryer, ids []string) ([]EnvironmentRecord, error) {
	environments := make([]EnvironmentRecord, 0, len(ids))
	for _, id := range ids {
		environment, err := readEnvironment(connection, id)
		if err != nil {
			return nil, err
		}
		if environment == nil {
			return nil, errors.New("environment disappeared")
		}
		environments = append(environments, *environment)
	}
	return environments, nil
}

// WorkerEnvironments is all physical obligations for a worker, even after its
// session was deleted.
func WorkerEnvironments(sessionID string) ([]EnvironmentRecord, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	ids, err := committedStrings(connection, "SELECT environment_id FROM environments WHERE owner_session_id=?1 OR environment_id IN (SELECT environment_id FROM environment_workers WHERE worker_session_id=?1) ORDER BY rowid", sessionID)
	if err != nil {
		return nil, err
	}
	return environmentRecordsByID(connection, ids)
}

// AdmitEnvironmentWorker: launch admission and retirement use the same
// serialized writer. The caller retains its owner admission until launch
// finishes; retirement drains it.
func AdmitEnvironmentWorker(sessionID string, target core.TargetLocator) error {
	_, err := submitDatabaseWrite("admit environment worker", func(connection *Connection) (struct{}, error) {
		id, err := environmentOptionalString(connection, "SELECT b.environment_id FROM environment_bindings b JOIN environments e USING(environment_id) WHERE b.session_id=?1 AND e.phase='active'", sessionID)
		if err != nil {
			return struct{}{}, err
		}
		if id == nil {
			return struct{}{}, errors.New("environment is retiring or worker has no committed placement; worker launch refused")
		}
		workers, err := readEnvironmentWorkers(connection, *id)
		if err != nil {
			return struct{}{}, err
		}
		for _, worker := range workers {
			if worker.SessionID == sessionID && reflect.DeepEqual(worker.Target, target) {
				return struct{}{}, nil
			}
		}
		return struct{}{}, errors.New("worker has no admitted environment placement")
	})
	return err
}

func CheckoutEnvironments(worktree *core.ManagedWorktree) ([]EnvironmentRecord, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	ids, err := committedStrings(connection, "SELECT environment_id FROM environments ORDER BY rowid")
	if err != nil {
		return nil, err
	}
	environments := []EnvironmentRecord{}
	for _, id := range ids {
		environment, err := readEnvironment(connection, id)
		if err != nil {
			return nil, err
		}
		if environment == nil {
			return nil, errors.New("environment disappeared")
		}
		if environment.Placement.ManagedWorktree != nil && managedWorktreeEqual(environment.Placement.ManagedWorktree, worktree) {
			environments = append(environments, *environment)
		}
	}
	return environments, nil
}

// EnvironmentReports is each (child session, report directory) the
// environment retains.
func EnvironmentReports(environmentID string) ([][2]string, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := connection.Query("SELECT child_session_id,report_dir FROM environment_reports WHERE environment_id=?1 ORDER BY child_session_id,report_dir", environmentID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	reports := [][2]string{}
	for rows.Next() {
		var report [2]string
		if err := rows.Scan(&report[0], &report[1]); err != nil {
			return nil, err
		}
		reports = append(reports, report)
	}
	return reports, rows.Err()
}

// optionalPathSerialize keeps the existing database path encoding instead of
// forcing user paths into JSON strings. On Unix this preserves arbitrary
// bytes; Windows uses UTF-16. The bytes are a JSON array of numbers, as serde
// writes a Vec<u8>.
func optionalPathSerialize(path *string) any {
	if path == nil {
		return nil
	}
	blob := pathToBlob(*path)
	numbers := make([]uint16, len(blob))
	for i, b := range blob {
		numbers[i] = uint16(b)
	}
	return numbers
}

// optionalPathDeserialize reads optionalPathSerialize's form.
func optionalPathDeserialize(blob *[]uint8) *string {
	if blob == nil {
		return nil
	}
	path := blobToPath(*blob)
	return &path
}

// BeginSessionDestroy closes registration and selects the logical children
// in one writer decision. Physical evidence is untouched; a crash still
// leaves a retryable record.
func BeginSessionDestroy(sessionID string) ([]string, error) {
	return submitDatabaseWrite("begin session destroy", func(connection *Connection) ([]string, error) {
		transaction, err := connection.Begin()
		if err != nil {
			return nil, err
		}
		defer transaction.Rollback()
		if _, err := transaction.Exec("UPDATE sessions SET state='destroying' WHERE session_id=?1", sessionID); err != nil {
			return nil, err
		}
		if _, err := transaction.Exec("UPDATE environments SET phase='retiring' WHERE owner_session_id=?1 AND phase='active'", sessionID); err != nil {
			return nil, err
		}
		children, err := committedStrings(transaction, "SELECT child_session_id FROM subagent_sessions WHERE parent_session_id=?1 ORDER BY child_session_id", sessionID)
		if err != nil {
			return nil, err
		}
		if children == nil {
			children = []string{}
		}
		return children, transaction.Commit()
	})
}

// jsonValueGet is serde_json's Value::get with a key: a member of an object,
// absent for anything else.
func jsonValueGet(value any, key string) (any, bool) {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil, false
	}
	return object.Get(key)
}

// jsonValueDecode is serde_json::from_value into target.
func jsonValueDecode(value any, target any) error {
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		return err
	}
	return json.Unmarshal(encoded, target)
}

// jsonValueKind is serde's description of a JSON value in an invalid-type
// error.
func jsonValueKind(value any) string {
	switch value := value.(type) {
	case nil:
		return "null"
	case bool:
		return fmt.Sprintf("boolean `%t`", value)
	case int64:
		return fmt.Sprintf("integer `%d`", value)
	case uint64:
		return fmt.Sprintf("integer `%d`", value)
	case float64:
		return fmt.Sprintf("floating point `%v`", value)
	case string:
		return fmt.Sprintf("string %q", value)
	case []any:
		return "sequence"
	}
	return "map"
}

// historicalPlacement reads the physical fields from an old saved
// SessionRecord, a JSON value in github.com/BrokkAi/acp-go/jsonvalue form.
// Harness parsing must not erase an independently persisted Move source
// during upgrade.
func historicalPlacement(value any) (EnvironmentPlacement, error) {
	metadata := jsonvalue.Object{}
	for _, key := range []string{"target_template_id", "managed_worktree", "build_cache", "bundle_id"} {
		member, ok := jsonValueGet(value, key)
		if !ok {
			if key == "target_template_id" || key == "bundle_id" {
				member = ""
			} else {
				member = nil
			}
		}
		metadata.Set(key, member)
	}
	project, _ := jsonValueGet(value, "project")
	if repositories := projectRepositories(project); repositories != nil {
		list := make([]any, len(*repositories))
		for i, repository := range *repositories {
			list[i] = repository
		}
		metadata.Set("repositories", list)
	} else {
		metadata.Set("repositories", nil)
	}
	runtime, _ := jsonValueGet(value, "target_runtime")
	metadata.Set("runtime", runtime)
	for _, key := range []string{"project_directory", "container_workspace"} {
		member, _ := jsonValueGet(value, key)
		switch path := member.(type) {
		case nil:
			metadata.Set(key, nil)
		case string:
			blob := pathToBlob(path)
			numbers := make([]any, len(blob))
			for i, b := range blob {
				numbers[i] = int64(b)
			}
			metadata.Set(key, numbers)
		default:
			return EnvironmentPlacement{}, fmt.Errorf("invalid type: %s, expected path string", jsonValueKind(member))
		}
	}
	var placement EnvironmentPlacement
	if err := jsonValueDecode(metadata, &placement); err != nil {
		return EnvironmentPlacement{}, err
	}
	if target, ok := jsonValueGet(value, "target"); ok && target != nil {
		encoded, err := jsonvalue.Marshal(target)
		if err != nil {
			return EnvironmentPlacement{}, err
		}
		if placement.Target, err = core.UnmarshalTargetLocator(encoded); err != nil {
			return EnvironmentPlacement{}, err
		}
	}
	return placement, nil
}

func registerHistoricalEnvironment(transaction *sql.Tx, owner string, placement EnvironmentPlacement) (string, error) {
	ids, err := committedStrings(transaction, "SELECT environment_id FROM environments WHERE owner_session_id=?1 ORDER BY rowid", owner)
	if err != nil {
		return "", err
	}
	for _, id := range ids {
		candidate, err := readEnvironment(transaction, id)
		if err != nil {
			return "", err
		}
		if candidate == nil {
			return "", errors.New("historical environment disappeared")
		}
		var sameTarget bool
		if left, right := candidate.Placement.Target, placement.Target; left != nil && right != nil {
			sameTarget = left.SameResource(right)
		} else {
			sameTarget = left == nil && right == nil
		}
		var sameCheckout bool
		if left, right := candidate.Placement.ManagedWorktree, placement.ManagedWorktree; left != nil && right != nil {
			sameCheckout = left.WorktreeRoot == right.WorktreeRoot && left.Target.SameLocation(right.Target)
		} else {
			sameCheckout = left == nil && right == nil
		}
		if sameTarget && sameCheckout {
			return id, nil
		}
	}
	id, err := core.NewSessionID()
	if err != nil {
		return "", err
	}
	if err := insertEnvironment(transaction, id, owner, &placement); err != nil {
		return "", err
	}
	if placement.Target != nil {
		if _, err := storeWorker(transaction, id, owner, placement.TargetTemplateID, placement.Runtime, placement.Target); err != nil {
			return "", err
		}
	}
	return id, nil
}

// environmentTextRows collects rows of text columns, as Rust collects a
// query_map before reading anything else on the connection.
func environmentTextRows(connection Queryer, columns int, query string, args ...any) ([][]string, error) {
	rows, err := connection.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var collected [][]string
	for rows.Next() {
		values := make([]string, columns)
		pointers := make([]any, columns)
		for i := range values {
			pointers[i] = &values[i]
		}
		if err := rows.Scan(pointers...); err != nil {
			return nil, err
		}
		collected = append(collected, values)
	}
	return collected, rows.Err()
}

func backfillMoveEnvironments(transaction *sql.Tx) error {
	retained, err := environmentTextRows(transaction, 3, "SELECT operation_id,session_id,source_json FROM retained_move_sources")
	if err != nil {
		return err
	}
	for _, source := range retained {
		operation, owner, text := source[0], source[1], source[2]
		value, err := jsonvalue.Parse([]byte(text))
		if err != nil {
			return err
		}
		placement, err := historicalPlacement(value)
		if err != nil {
			return err
		}
		id, err := registerHistoricalEnvironment(transaction, owner, placement)
		if err != nil {
			return err
		}
		if _, err := transaction.Exec("UPDATE retained_move_sources SET environment_id=?2 WHERE operation_id=?1", operation, id); err != nil {
			return err
		}
	}
	moves, err := environmentTextRows(transaction, 2, "SELECT session_id,operation_json FROM session_moves")
	if err != nil {
		return err
	}
	for _, move := range moves {
		owner, text := move[0], move[1]
		value, err := jsonvalue.Parse([]byte(text))
		if err != nil {
			return err
		}
		if prepared, ok := jsonValueGet(value, "prepared_destination"); ok && prepared != nil {
			var destination core.PreparedMoveDestination
			if err := jsonValueDecode(prepared, &destination); err != nil {
				return err
			}
			if adopted, ok := destination.State.(core.PreparedDestinationStateAdopted); ok {
				runtime := destination.Runtime
				if _, err := registerHistoricalEnvironment(transaction, owner, EnvironmentPlacement{
					Target:  adopted.Target,
					Runtime: &runtime,
				}); err != nil {
					return err
				}
			} else if destination.OwnsResource() {
				member, _ := jsonValueGet(value, "operation_id")
				operationID, ok := member.(string)
				if !ok {
					return errors.New("prepared Move has no operation ID")
				}
				placement := EnvironmentPlacement{PreparedDestination: &EnvironmentPreparedDestination{
					OperationID: operationID,
					Destination: destination,
				}}
				id, err := core.NewSessionID()
				if err != nil {
					return err
				}
				if err := insertEnvironment(transaction, id, owner, &placement); err != nil {
					return err
				}
			}
		}
		// serde_json's pointer finds a member even when it is null; only an
		// absent one falls back to the recovery snapshot.
		var source any
		found := false
		if transfer, ok := jsonValueGet(value, "workspace_transfer"); ok {
			source, found = jsonValueGet(transfer, "source")
		}
		if !found {
			if recovery, ok := jsonValueGet(value, "recovery_session"); ok && recovery != nil {
				source, found = recovery, true
			}
		}
		var id string
		if found {
			placement, err := historicalPlacement(source)
			if err != nil {
				return err
			}
			if id, err = registerHistoricalEnvironment(transaction, owner, placement); err != nil {
				return err
			}
		} else if err := transaction.QueryRow("SELECT environment_id FROM environment_bindings WHERE session_id=?1", owner).Scan(&id); err != nil {
			return err
		}
		if _, err := transaction.Exec("UPDATE session_moves SET source_environment_id=?2 WHERE session_id=?1", owner, id); err != nil {
			return err
		}
	}
	return nil
}

// ReserveOwnedCheckout: a stopped owned checkout is reserved before Resume
// materializes or mutates it. Its old reclaimed environment stays a
// tombstone; the new reservation owns construction and can be reclaimed even
// if no worker ever starts.
func ReserveOwnedCheckout(session *core.SessionRecord) (EnvironmentRecord, error) {
	return submitDatabaseWrite("reserve resumed checkout", func(connection *Connection) (EnvironmentRecord, error) {
		transaction, err := connection.Begin()
		if err != nil {
			return EnvironmentRecord{}, err
		}
		defer transaction.Rollback()
		var currentID string
		if err := transaction.QueryRow("SELECT environment_id FROM environment_bindings WHERE session_id=?1", session.ID).Scan(&currentID); err != nil {
			return EnvironmentRecord{}, err
		}
		current, err := readEnvironment(transaction, currentID)
		if err != nil {
			return EnvironmentRecord{}, err
		}
		if current == nil {
			return EnvironmentRecord{}, errors.New("checkout owner disappeared")
		}
		if current.OwnerSessionID != session.ID {
			return EnvironmentRecord{}, errors.New("cannot reserve a borrowed checkout")
		}
		if current.Phase == EnvironmentPhaseRetiring {
			return EnvironmentRecord{}, errors.New("shared workspace cleanup is incomplete; retry its cleanup before reusing it")
		}
		id := currentID
		if current.Phase == EnvironmentPhaseReclaimed {
			if id, err = core.NewSessionID(); err != nil {
				return EnvironmentRecord{}, err
			}
			placement := EnvironmentPlacementFromSession(session)
			placement.Target = nil
			if err := insertEnvironment(transaction, id, session.ID, &placement); err != nil {
				return EnvironmentRecord{}, err
			}
			if _, err := transaction.Exec("UPDATE environment_bindings SET environment_id=?2 WHERE session_id=?1", session.ID, id); err != nil {
				return EnvironmentRecord{}, err
			}
		}
		environment, err := readEnvironment(transaction, id)
		if err != nil {
			return EnvironmentRecord{}, err
		}
		if environment == nil {
			return EnvironmentRecord{}, errors.New("checkout reservation disappeared")
		}
		return *environment, transaction.Commit()
	})
}

// projectRepositories reads a stored project's repository destinations from
// a jsonvalue-form value; nil when any is missing or unsafe.
func projectRepositories(value any) *[]string {
	bundle, ok := jsonValueGet(value, "bundle")
	if !ok {
		return nil
	}
	member, ok := jsonValueGet(bundle, "repositories")
	if !ok {
		return nil
	}
	list, ok := member.([]any)
	if !ok {
		return nil
	}
	repositories := make([]string, 0, len(list))
	for _, repository := range list {
		destination, ok := jsonValueGet(repository, "destination")
		if !ok {
			return nil
		}
		path, ok := destination.(string)
		if !ok {
			return nil
		}
		if core.ValidateRelativeDestination(path) != nil {
			return nil
		}
		repositories = append(repositories, path)
	}
	return &repositories
}

func PreparedMoveEnvironment(operationID string) (*EnvironmentRecord, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	id, err := environmentOptionalString(connection, "SELECT environment_id FROM environments WHERE json_extract(placement_json,'$.prepared_destination.operation_id')=?1", operationID)
	if err != nil {
		return nil, err
	}
	return environmentByID(connection, id)
}

// recordPreparedMoveEnvironment: legacy prepared destinations reserve
// cleanup independently of the Move row. Initialize once; only the physical
// owner advances its cleanup identity.
func recordPreparedMoveEnvironment(connection Queryer, operation *core.MoveOperation) error {
	destination := operation.PreparedDestination
	if destination != nil {
		if adopted, ok := destination.State.(core.PreparedDestinationStateAdopted); ok {
			var id string
			if err := connection.QueryRow("SELECT environment_id FROM environment_bindings WHERE session_id=?1", operation.Selection.SessionID).Scan(&id); err != nil {
				return err
			}
			owner, err := readEnvironment(connection, id)
			if err != nil {
				return err
			}
			if owner == nil {
				return errors.New("adopted destination owner disappeared")
			}
			if owner.Placement.Target == nil || !owner.Placement.Target.SameResource(adopted.Target) {
				return errors.New("adopted destination has no committed worker placement")
			}
			// Adoption transfers cleanup to the worker environment in this
			// same publication transaction; the preparation must never
			// terminate it.
			_, err = connection.Exec("UPDATE environments SET phase='reclaimed' WHERE json_extract(placement_json,'$.prepared_destination.operation_id')=?1", operation.OperationID)
			return err
		}
	}
	if destination == nil || !destination.OwnsResource() {
		return nil
	}
	var exists bool
	if err := connection.QueryRow("SELECT EXISTS(SELECT 1 FROM environments WHERE json_extract(placement_json,'$.prepared_destination.operation_id')=?1)", operation.OperationID).Scan(&exists); err != nil {
		return err
	}
	if !exists {
		id, err := core.NewSessionID()
		if err != nil {
			return err
		}
		placement := EnvironmentPlacement{PreparedDestination: &EnvironmentPreparedDestination{
			OperationID: operation.OperationID,
			Destination: *destination,
		}}
		if err := insertEnvironment(connection, id, operation.Selection.SessionID, &placement); err != nil {
			return err
		}
	}
	return nil
}

func RecordPreparedEnvironmentInstance(environmentID, instanceID string) error {
	_, err := submitDatabaseWrite("record prepared environment instance", func(connection *Connection) (struct{}, error) {
		transaction, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer transaction.Rollback()
		environment, err := readEnvironment(transaction, environmentID)
		if err != nil {
			return struct{}{}, err
		}
		if environment == nil {
			return struct{}{}, errors.New("prepared environment disappeared")
		}
		if environment.Phase != EnvironmentPhaseRetiring {
			return struct{}{}, errors.New("prepared environment is no longer retiring")
		}
		prepared := environment.Placement.PreparedDestination
		if prepared == nil {
			return struct{}{}, errors.New("environment is not a prepared destination")
		}
		instance := instanceID
		prepared.Destination.State = core.PreparedDestinationStateCleanupPending{InstanceID: &instance}
		encoded, err := databaseMarshalJSON(&environment.Placement)
		if err != nil {
			return struct{}{}, err
		}
		if _, err := transaction.Exec("UPDATE environments SET placement_json=?2 WHERE environment_id=?1", environmentID, string(encoded)); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, transaction.Commit()
	})
	return err
}
