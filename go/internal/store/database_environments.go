// Port of mj-store/src/database/environments.rs.

// Physical ownership outlives logical sessions and their parent
// relationships.

package store

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"

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

func readEnvironment(connection Connection, id string) (*EnvironmentRecord, error) {
	panic("unported: mj-store/src/database/environments.rs read_environment")
}

func SessionEnvironment(sessionID string) (*EnvironmentRecord, error) {
	panic("unported: mj-store/src/database/environments.rs session_environment")
}

const environmentOwnerQuery = `SELECT b.session_id, e.owner_session_id FROM environment_bindings b
     JOIN environments e ON e.environment_id = b.environment_id`

func SessionEnvironmentOwner(sessionID string) (*string, error) {
	panic("unported: mj-store/src/database/environments.rs session_environment_owner")
}

func sessionEnvironmentOwnerWith(connection Connection, sessionID string) (*string, error) {
	var owner string
	err := connection.QueryRowContext(context.Background(),
		environmentOwnerQuery+" WHERE b.session_id = ?1", sessionID).Scan(new(string), &owner)
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
	panic("unported: mj-store/src/database/environments.rs environment_workers")
}

func readEnvironmentWorkers(connection Connection, environmentID string) ([]EnvironmentWorker, error) {
	panic("unported: mj-store/src/database/environments.rs read_environment_workers")
}

func storeWorker(
	transaction *sql.Tx,
	environmentID, sessionID, targetTemplateID string,
	runtime *core.TargetRuntimeSettings,
	target core.TargetLocator,
) (string, error) {
	panic("unported: mj-store/src/database/environments.rs store_worker")
}

// captureSessionEnvironment is called in the same transaction that publishes
// a placement. A binding is presentation; the environment and its worker
// leases never cascade with it.
func captureSessionEnvironment(transaction *sql.Tx, session *core.SessionRecord, parent *string) error {
	panic("unported: mj-store/src/database/environments.rs capture_session_environment")
}

func rebindBorrower(transaction *sql.Tx, sessionID, boundID string) (string, error) {
	panic("unported: mj-store/src/database/environments.rs rebind_borrower")
}

// BeginEnvironmentRetirement closes admission and selects the writers in one
// serialized decision.
func BeginEnvironmentRetirement(environmentID string) (EnvironmentRecord, []EnvironmentWorker, error) {
	panic("unported: mj-store/src/database/environments.rs begin_environment_retirement")
}

func FinishEnvironmentRetirement(environmentID string) error {
	panic("unported: mj-store/src/database/environments.rs finish_environment_retirement")
}

// FinishEnvironmentRuntimeRemoval: a runtime can be replaced while its
// workspace remains owned by this same environment. Only the owner, after
// draining every writer and removing the old runtime, may release those
// leases and reopen launch admission.
func FinishEnvironmentRuntimeRemoval(environmentID string) error {
	panic("unported: mj-store/src/database/environments.rs finish_environment_runtime_removal")
}

// backfillEnvironments: migration uses the existing target decoder,
// including targets whose harness is no longer supported. Physical ownership
// must not depend on that parser.
func backfillEnvironments(transaction *sql.Tx) error {
	panic("unported: mj-store/src/database/environments.rs backfill_environments")
}

// WorkerEnvironments is all physical obligations for a worker, even after its
// session was deleted.
func WorkerEnvironments(sessionID string) ([]EnvironmentRecord, error) {
	panic("unported: mj-store/src/database/environments.rs worker_environments")
}

// AdmitEnvironmentWorker: launch admission and retirement use the same
// serialized writer. The caller retains its owner admission until launch
// finishes; retirement drains it.
func AdmitEnvironmentWorker(sessionID string, target core.TargetLocator) error {
	panic("unported: mj-store/src/database/environments.rs admit_environment_worker")
}

func CheckoutEnvironments(worktree *core.ManagedWorktree) ([]EnvironmentRecord, error) {
	panic("unported: mj-store/src/database/environments.rs checkout_environments")
}

// EnvironmentReports is each (child session, report directory) the
// environment retains.
func EnvironmentReports(environmentID string) ([][2]string, error) {
	panic("unported: mj-store/src/database/environments.rs environment_reports")
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
	panic("unported: mj-store/src/database/environments.rs begin_session_destroy")
}

// historicalPlacement reads the physical fields from an old saved
// SessionRecord, a JSON value in github.com/BrokkAi/acp-go/jsonvalue form.
// Harness parsing must not erase an independently persisted Move source
// during upgrade.
func historicalPlacement(value any) (EnvironmentPlacement, error) {
	panic("unported: mj-store/src/database/environments.rs historical_placement")
}

func registerHistoricalEnvironment(transaction *sql.Tx, owner string, placement EnvironmentPlacement) (string, error) {
	panic("unported: mj-store/src/database/environments.rs register_historical_environment")
}

func backfillMoveEnvironments(transaction *sql.Tx) error {
	panic("unported: mj-store/src/database/environments.rs backfill_move_environments")
}

// ReserveOwnedCheckout: a stopped owned checkout is reserved before Resume
// materializes or mutates it. Its old reclaimed environment stays a
// tombstone; the new reservation owns construction and can be reclaimed even
// if no worker ever starts.
func ReserveOwnedCheckout(session *core.SessionRecord) (EnvironmentRecord, error) {
	panic("unported: mj-store/src/database/environments.rs reserve_owned_checkout")
}

// projectRepositories reads a stored project's repository destinations from
// a jsonvalue-form value; nil when any is missing or unsafe.
func projectRepositories(value any) *[]string {
	panic("unported: mj-store/src/database/environments.rs project_repositories")
}

func PreparedMoveEnvironment(operationID string) (*EnvironmentRecord, error) {
	panic("unported: mj-store/src/database/environments.rs prepared_move_environment")
}

// recordPreparedMoveEnvironment: legacy prepared destinations reserve
// cleanup independently of the Move row. Initialize once; only the physical
// owner advances its cleanup identity.
func recordPreparedMoveEnvironment(connection Connection, operation *core.MoveOperation) error {
	panic("unported: mj-store/src/database/environments.rs record_prepared_move_environment")
}

func RecordPreparedEnvironmentInstance(environmentID, instanceID string) error {
	panic("unported: mj-store/src/database/environments.rs record_prepared_environment_instance")
}
