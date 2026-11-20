// Port of mj-core/src/move_workspace.rs.

// Move's file selection and temporary workspace-transfer contracts.
//
// These are not checkpoint payloads. File bodies never travel in these values.

package core

import (
	"encoding/json"
	"fmt"
)

const LargeTransferBytes uint64 = 1_000_000_000
const SubstantialRootBytes uint64 = 100_000_000

// WorkspaceCommand is worker utility input. Large content is written to
// disk, never returned here. A sealed interface over WorkspaceCommandInspect,
// WorkspaceCommandCapture, WorkspaceCommandRestore and WorkspaceCommandVerify;
// serialized internally tagged by "action" with unknown fields denied.
type WorkspaceCommand interface {
	isWorkspaceCommand()
}

type WorkspaceCommandInspect struct {
	Repositories []WorkspaceRepository `json:"repositories"`
}

type WorkspaceCommandCapture struct {
	Repositories []WorkspaceRepository `json:"repositories"`
	Selection    WorkspaceSelection    `json:"selection"`
	Destination  string                `json:"destination"`
}

type WorkspaceCommandRestore struct {
	Source       string                `json:"source"`
	Repositories []WorkspaceRepository `json:"repositories"`
}

type WorkspaceCommandVerify struct {
	Source string `json:"source"`
}

func (WorkspaceCommandInspect) isWorkspaceCommand() {}
func (WorkspaceCommandCapture) isWorkspaceCommand() {}
func (WorkspaceCommandRestore) isWorkspaceCommand() {}
func (WorkspaceCommandVerify) isWorkspaceCommand()  {}

func (c WorkspaceCommandInspect) MarshalJSON() ([]byte, error) {
	type wire WorkspaceCommandInspect
	return json.Marshal(struct {
		Action string `json:"action"`
		wire
	}{"inspect", wire(c)})
}

func (c WorkspaceCommandCapture) MarshalJSON() ([]byte, error) {
	type wire WorkspaceCommandCapture
	return json.Marshal(struct {
		Action string `json:"action"`
		wire
	}{"capture", wire(c)})
}

func (c WorkspaceCommandRestore) MarshalJSON() ([]byte, error) {
	type wire WorkspaceCommandRestore
	return json.Marshal(struct {
		Action string `json:"action"`
		wire
	}{"restore", wire(c)})
}

func (c WorkspaceCommandVerify) MarshalJSON() ([]byte, error) {
	type wire WorkspaceCommandVerify
	return json.Marshal(struct {
		Action string `json:"action"`
		wire
	}{"verify", wire(c)})
}

// unmarshalWorkspaceCommand decodes the "action"-tagged form, refusing
// unknown fields as serde's deny_unknown_fields does.
func unmarshalWorkspaceCommand(data []byte) (WorkspaceCommand, error) {
	var tag struct {
		Action *string `json:"action"`
	}
	if err := json.Unmarshal(data, &tag); err != nil {
		return nil, err
	}
	if tag.Action == nil {
		return nil, fmt.Errorf("missing field `action`")
	}
	switch *tag.Action {
	case "inspect":
		type wire WorkspaceCommandInspect
		var command struct {
			Action string `json:"action"`
			wire
		}
		err := unmarshalDenyUnknown(data, &command)
		return WorkspaceCommandInspect(command.wire), err
	case "capture":
		type wire WorkspaceCommandCapture
		var command struct {
			Action string `json:"action"`
			wire
		}
		err := unmarshalDenyUnknown(data, &command)
		return WorkspaceCommandCapture(command.wire), err
	case "restore":
		type wire WorkspaceCommandRestore
		var command struct {
			Action string `json:"action"`
			wire
		}
		err := unmarshalDenyUnknown(data, &command)
		return WorkspaceCommandRestore(command.wire), err
	case "verify":
		type wire WorkspaceCommandVerify
		var command struct {
			Action string `json:"action"`
			wire
		}
		err := unmarshalDenyUnknown(data, &command)
		return WorkspaceCommandVerify(command.wire), err
	}
	return nil, fmt.Errorf("unknown variant `%s`, expected one of `inspect`, `capture`, `restore`, `verify`", *tag.Action)
}

type WorkspaceRepository struct {
	ID   string `json:"id"`
	Root string `json:"root"`
	// Capture's authoritative handoff base. Capture runs only in the
	// controller's exact-build Move helper, never an older session worker.
	BaseCommit *string `json:"base_commit,omitempty"`
}

func (r *WorkspaceRepository) UnmarshalJSON(data []byte) error {
	type wire WorkspaceRepository
	return unmarshalDenyUnknown(data, (*wire)(r))
}

type TransferFile struct {
	Path   string `json:"path"`
	Bytes  uint64 `json:"bytes"`
	Sha256 string `json:"sha256"`
}

func (f *TransferFile) UnmarshalJSON(data []byte) error {
	type wire TransferFile
	return unmarshalDenyUnknown(data, (*wire)(f))
}

type TransferredRepository struct {
	ID           string          `json:"id"`
	Head         string          `json:"head"`
	Branch       *string         `json:"branch"`
	Refs         [][2]string     `json:"refs"`
	SymbolicRefs [][2]string     `json:"symbolic_refs"`
	StashLog     *string         `json:"stash_log"`
	Status       string          `json:"status"`
	Untracked    []WorkspacePath `json:"untracked"`
}

func (r *TransferredRepository) UnmarshalJSON(data []byte) error {
	type wire TransferredRepository
	return unmarshalDenyUnknown(data, (*wire)(r))
}

type WorkspaceManifest struct {
	Version      uint32                  `json:"version"`
	Repositories []TransferredRepository `json:"repositories"`
	Files        []TransferFile          `json:"files"`
}

func (m *WorkspaceManifest) UnmarshalJSON(data []byte) error {
	type wire WorkspaceManifest
	return unmarshalDenyUnknown(data, (*wire)(m))
}

// WorkspaceTransfer is durable ownership of a temporary transfer, distinct
// from checkpoint history.
type WorkspaceTransfer struct {
	Assessment      WorkspaceAssessment    `json:"assessment"`
	Source          *SessionRecord         `json:"source"`
	SourceStage     string                 `json:"source_stage"`
	ControllerStage string                 `json:"controller_stage"`
	Phase           WorkspaceTransferPhase `json:"phase"`
}

func (t *WorkspaceTransfer) UnmarshalJSON(data []byte) error {
	type wire WorkspaceTransfer
	return unmarshalDenyUnknown(data, (*wire)(t))
}

type WorkspaceTransferPhase string

const (
	WorkspaceTransferPhasePlanned       WorkspaceTransferPhase = "planned"
	WorkspaceTransferPhaseCaptured      WorkspaceTransferPhase = "captured"
	WorkspaceTransferPhaseDownloaded    WorkspaceTransferPhase = "downloaded"
	WorkspaceTransferPhaseSourceStopped WorkspaceTransferPhase = "source_stopped"
	WorkspaceTransferPhaseRestored      WorkspaceTransferPhase = "restored"
	WorkspaceTransferPhaseReady         WorkspaceTransferPhase = "ready"
)

func (p *WorkspaceTransferPhase) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch phase := WorkspaceTransferPhase(text); phase {
	case WorkspaceTransferPhasePlanned, WorkspaceTransferPhaseCaptured, WorkspaceTransferPhaseDownloaded,
		WorkspaceTransferPhaseSourceStopped, WorkspaceTransferPhaseRestored, WorkspaceTransferPhaseReady:
		*p = phase
		return nil
	}
	return fmt.Errorf("unknown variant `%s`, expected one of `planned`, `captured`, `downloaded`, `source_stopped`, `restored`, `ready`", text)
}

type WorkspaceSelection struct {
	Exclusions               []WorkspacePath `json:"exclusions"`
	AcknowledgeLargeTransfer bool            `json:"acknowledge_large_transfer"`
}

func (s *WorkspaceSelection) UnmarshalJSON(data []byte) error {
	type wire WorkspaceSelection
	return unmarshalDenyUnknown(data, (*wire)(s))
}

type WorkspacePath struct {
	Repository string `json:"repository"`
	Path       string `json:"path"`
}

func (p *WorkspacePath) UnmarshalJSON(data []byte) error {
	type wire WorkspacePath
	return unmarshalDenyUnknown(data, (*wire)(p))
}

// Compare is the Rust derived Ord: repository, then path by component.
func (p WorkspacePath) Compare(other WorkspacePath) int {
	panic("unported: mj-core/src/move_workspace.rs WorkspacePath Ord")
}

// LargeTransferConsentRequired: the selection is valid but the caller has
// not agreed to a large transfer. A distinct type lets callers offer the
// flags that give that consent, and only for this refusal.
type LargeTransferConsentRequired string

func (e LargeTransferConsentRequired) Error() string {
	return fmt.Sprintf("Move includes %s; review Choose files and acknowledge the large transfer", string(e))
}

func (p *WorkspacePath) Validate() error {
	panic("unported: mj-core/src/move_workspace.rs WorkspacePath::validate")
}

func (p *WorkspacePath) Contains(other *WorkspacePath) bool {
	panic("unported: mj-core/src/move_workspace.rs WorkspacePath::contains")
}

type WorkspaceFile struct {
	Location WorkspacePath `json:"location"`
	Bytes    uint64        `json:"bytes"`
}

func (f *WorkspaceFile) UnmarshalJSON(data []byte) error {
	type wire WorkspaceFile
	return unmarshalDenyUnknown(data, (*wire)(f))
}

type WorkspaceAssessment struct {
	Files []WorkspaceFile `json:"files"`
	Roots []FileNode      `json:"roots"`
	// A sorted set (Rust BTreeSet), ordered by WorkspacePath.Compare.
	InitiallyExpanded []WorkspacePath `json:"initially_expanded"`
	// Git history and tracked edits always travel; only untracked paths are selectable.
	RequiredBytes   uint64             `json:"required_bytes"`
	IgnoredFiles    uint64             `json:"ignored_files"`
	IgnoredBytes    uint64             `json:"ignored_bytes"`
	CredentialFiles uint64             `json:"credential_files"`
	Blockers        []string           `json:"blockers"`
	Storage         []WorkspaceStorage `json:"storage"`
}

func (a *WorkspaceAssessment) UnmarshalJSON(data []byte) error {
	type wire WorkspaceAssessment
	return unmarshalDenyUnknown(data, (*wire)(a))
}

type WorkspaceStorage struct {
	FilesystemKey string `json:"filesystem_key"`
	// A sorted set (Rust BTreeSet).
	Allocations    []string `json:"allocations"`
	Location       string   `json:"location"`
	AvailableBytes uint64   `json:"available_bytes"`
	Copies         uint64   `json:"copies"`
}

// SelectionProblem: capacity changes with the selection; consent is
// provided by Continue.
func (a *WorkspaceAssessment) SelectionProblem(selection *WorkspaceSelection) *string {
	acknowledged := *selection
	acknowledged.AcknowledgeLargeTransfer = true
	if err := acknowledged.Validate(a); err != nil {
		message := err.Error()
		return &message
	}
	return nil
}

func (s *WorkspaceSelection) Includes(path *WorkspacePath) bool {
	for i := range s.Exclusions {
		if s.Exclusions[i].Contains(path) {
			return false
		}
	}
	return true
}

func (s *WorkspaceSelection) IncludedBytes(assessment *WorkspaceAssessment) uint64 {
	panic("unported: mj-core/src/move_workspace.rs WorkspaceSelection::included_bytes")
}

func (s *WorkspaceSelection) Validate(assessment *WorkspaceAssessment) error {
	panic("unported: mj-core/src/move_workspace.rs WorkspaceSelection::validate")
}

// SetIncluded: a checked parent includes all its descendants; an unchecked
// parent excludes them. Selecting a child inside an excluded parent expands
// the exclusion into the other exact files, preserving their selection.
func (s *WorkspaceSelection) SetIncluded(assessment *WorkspaceAssessment, path *WorkspacePath, included bool) {
	panic("unported: mj-core/src/move_workspace.rs WorkspaceSelection::set_included")
}

type FileSelectionState int

const (
	FileSelectionStateIncluded FileSelectionState = iota
	FileSelectionStateExcluded
	FileSelectionStateMixed
)

type FileNode struct {
	Location WorkspacePath `json:"location"`
	Bytes    uint64        `json:"bytes"`
	Files    int           `json:"files"`
	Children []FileNode    `json:"children"`
}

func (n *FileNode) State(assessment *WorkspaceAssessment, selection *WorkspaceSelection) FileSelectionState {
	panic("unported: mj-core/src/move_workspace.rs FileNode::state")
}

func (n *FileNode) Label() string {
	suffix := ""
	if len(n.Children) > 0 {
		suffix = "/"
	}
	return fmt.Sprintf("%s/%s%s — %s", n.Location.Repository, n.Location.Path, suffix, FormatBytes(n.Bytes))
}

func FileTree(assessment *WorkspaceAssessment) []FileNode {
	panic("unported: mj-core/src/move_workspace.rs file_tree")
}

// InitialExpansion returns a sorted set (Rust BTreeSet).
func InitialExpansion(roots []FileNode) []WorkspacePath {
	panic("unported: mj-core/src/move_workspace.rs initial_expansion")
}

func FormatBytes(bytes uint64) string {
	switch {
	case bytes >= 1_000_000_000:
		return fmt.Sprintf("%.2f GB", float64(bytes)/1_000_000_000.0)
	case bytes >= 1_000_000:
		return fmt.Sprintf("%.1f MB", float64(bytes)/1_000_000.0)
	case bytes >= 1_000:
		return fmt.Sprintf("%.1f KB", float64(bytes)/1_000.0)
	}
	return fmt.Sprintf("%d B", bytes)
}

// RetainedMoveSource is a stopped source retained because the user excluded
// files from a Move.
type RetainedMoveSource struct {
	OperationID string          `json:"operation_id"`
	SessionID   string          `json:"session_id"`
	Source      SessionRecord   `json:"source"`
	Exclusions  []WorkspacePath `json:"exclusions"`
	CreatedAt   string          `json:"created_at"`
}
