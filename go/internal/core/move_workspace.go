// Port of mj-core/src/move_workspace.rs.

// Move's file selection and temporary workspace-transfer contracts.
//
// These are not checkpoint payloads. File bodies never travel in these values.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"slices"
	"strings"
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

// nonNilRepositories is a present empty list for a nil one.
func nonNilRepositories(repositories []WorkspaceRepository) []WorkspaceRepository {
	if repositories == nil {
		return []WorkspaceRepository{}
	}
	return repositories
}

func (c WorkspaceCommandInspect) MarshalJSON() ([]byte, error) {
	type wire WorkspaceCommandInspect
	c.Repositories = nonNilRepositories(c.Repositories)
	return marshalSerdeJSON(struct {
		Action string `json:"action"`
		wire
	}{"inspect", wire(c)})
}

func (c WorkspaceCommandCapture) MarshalJSON() ([]byte, error) {
	type wire WorkspaceCommandCapture
	c.Repositories = nonNilRepositories(c.Repositories)
	return marshalSerdeJSON(struct {
		Action string `json:"action"`
		wire
	}{"capture", wire(c)})
}

func (c WorkspaceCommandRestore) MarshalJSON() ([]byte, error) {
	type wire WorkspaceCommandRestore
	c.Repositories = nonNilRepositories(c.Repositories)
	return marshalSerdeJSON(struct {
		Action string `json:"action"`
		wire
	}{"restore", wire(c)})
}

func (c WorkspaceCommandVerify) MarshalJSON() ([]byte, error) {
	type wire WorkspaceCommandVerify
	return marshalSerdeJSON(struct {
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
		if err := requireFields(data, "repositories"); err != nil {
			return nil, err
		}
		type wire WorkspaceCommandInspect
		var command struct {
			Action string `json:"action"`
			wire
		}
		err := unmarshalDenyUnknown(data, &command)
		return WorkspaceCommandInspect(command.wire), err
	case "capture":
		if err := requireFields(data, "repositories", "selection", "destination"); err != nil {
			return nil, err
		}
		type wire WorkspaceCommandCapture
		var command struct {
			Action string `json:"action"`
			wire
		}
		err := unmarshalDenyUnknown(data, &command)
		return WorkspaceCommandCapture(command.wire), err
	case "restore":
		if err := requireFields(data, "source", "repositories"); err != nil {
			return nil, err
		}
		type wire WorkspaceCommandRestore
		var command struct {
			Action string `json:"action"`
			wire
		}
		err := unmarshalDenyUnknown(data, &command)
		return WorkspaceCommandRestore(command.wire), err
	case "verify":
		if err := requireFields(data, "source"); err != nil {
			return nil, err
		}
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
	if err := requireFields(data, "id", "root"); err != nil {
		return err
	}
	type wire WorkspaceRepository
	return unmarshalDenyUnknown(data, (*wire)(r))
}

type TransferFile struct {
	Path   string `json:"path"`
	Bytes  uint64 `json:"bytes"`
	Sha256 string `json:"sha256"`
}

func (f *TransferFile) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "path", "bytes", "sha256"); err != nil {
		return err
	}
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

// MarshalJSON writes empty lists as `[]`, as serde writes an empty Vec.
func (r TransferredRepository) MarshalJSON() ([]byte, error) {
	type wire TransferredRepository
	if r.Refs == nil {
		r.Refs = [][2]string{}
	}
	if r.SymbolicRefs == nil {
		r.SymbolicRefs = [][2]string{}
	}
	r.Untracked = nonNilWorkspacePaths(r.Untracked)
	return marshalSerdeJSON(wire(r))
}

func (r *TransferredRepository) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "id", "head", "refs", "symbolic_refs", "status", "untracked"); err != nil {
		return err
	}
	type wire TransferredRepository
	return unmarshalDenyUnknown(data, (*wire)(r))
}

type WorkspaceManifest struct {
	Version      uint32                  `json:"version"`
	Repositories []TransferredRepository `json:"repositories"`
	Files        []TransferFile          `json:"files"`
}

// MarshalJSON writes empty lists as `[]`, as serde writes an empty Vec.
func (m WorkspaceManifest) MarshalJSON() ([]byte, error) {
	type wire WorkspaceManifest
	if m.Repositories == nil {
		m.Repositories = []TransferredRepository{}
	}
	if m.Files == nil {
		m.Files = []TransferFile{}
	}
	return marshalSerdeJSON(wire(m))
}

func (m *WorkspaceManifest) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "version", "repositories", "files"); err != nil {
		return err
	}
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
	if err := requireFields(data, "assessment", "source", "source_stage", "controller_stage", "phase"); err != nil {
		return err
	}
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

// MarshalJSON writes no exclusions as `[]`, as serde writes an empty Vec.
func (s WorkspaceSelection) MarshalJSON() ([]byte, error) {
	type wire WorkspaceSelection
	s.Exclusions = nonNilWorkspacePaths(s.Exclusions)
	return marshalSerdeJSON(wire(s))
}

func (s *WorkspaceSelection) UnmarshalJSON(data []byte) error {
	type wire WorkspaceSelection
	return unmarshalDenyUnknown(data, (*wire)(s))
}

func nonNilWorkspacePaths(paths []WorkspacePath) []WorkspacePath {
	if paths == nil {
		return []WorkspacePath{}
	}
	return paths
}

type WorkspacePath struct {
	Repository string `json:"repository"`
	Path       string `json:"path"`
}

func (p *WorkspacePath) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "repository", "path"); err != nil {
		return err
	}
	type wire WorkspacePath
	return unmarshalDenyUnknown(data, (*wire)(p))
}

// Compare is the Rust derived Ord: repository, then path by component.
func (p WorkspacePath) Compare(other WorkspacePath) int {
	if order := strings.Compare(p.Repository, other.Repository); order != 0 {
		return order
	}
	return comparePaths(p.Path, other.Path)
}

// sortedWorkspacePaths is a Rust BTreeSet<WorkspacePath> built from paths.
func sortedWorkspacePaths(paths []WorkspacePath) []WorkspacePath {
	sorted := slices.Clone(paths)
	slices.SortFunc(sorted, WorkspacePath.Compare)
	return slices.CompactFunc(sorted, func(a, b WorkspacePath) bool { return a.Compare(b) == 0 })
}

// LargeTransferConsentRequired: the selection is valid but the caller has
// not agreed to a large transfer. A distinct type lets callers offer the
// flags that give that consent, and only for this refusal.
type LargeTransferConsentRequired string

func (e LargeTransferConsentRequired) Error() string {
	return fmt.Sprintf("Move includes %s; review Choose files and acknowledge the large transfer", string(e))
}

func (p *WorkspacePath) Validate() error {
	if p.Repository == "" {
		return errors.New("Move repository is missing")
	}
	valid := p.Path != ""
	for _, component := range pathComponents(p.Path) {
		if component.kind != pathComponentNormal {
			valid = false
		}
	}
	if !valid {
		return fmt.Errorf("Move path must be relative without parent traversal: %s", p.Path)
	}
	return nil
}

func (p *WorkspacePath) Contains(other *WorkspacePath) bool {
	return p.Repository == other.Repository && pathStartsWith(other.Path, p.Path)
}

type WorkspaceFile struct {
	Location WorkspacePath `json:"location"`
	Bytes    uint64        `json:"bytes"`
}

func (f *WorkspaceFile) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "location", "bytes"); err != nil {
		return err
	}
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

// MarshalJSON writes empty lists as `[]`, as serde writes an empty Vec or
// BTreeSet, and the expanded set in its order.
func (a WorkspaceAssessment) MarshalJSON() ([]byte, error) {
	type wire WorkspaceAssessment
	if a.Files == nil {
		a.Files = []WorkspaceFile{}
	}
	if a.Roots == nil {
		a.Roots = []FileNode{}
	}
	a.InitiallyExpanded = sortedWorkspacePaths(a.InitiallyExpanded)
	a.Blockers = nonNilStrings(a.Blockers)
	if a.Storage == nil {
		a.Storage = []WorkspaceStorage{}
	}
	return marshalSerdeJSON(wire(a))
}

func (a *WorkspaceAssessment) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "files", "required_bytes", "ignored_files", "ignored_bytes", "credential_files", "blockers"); err != nil {
		return err
	}
	type wire WorkspaceAssessment
	if err := unmarshalDenyUnknown(data, (*wire)(a)); err != nil {
		return err
	}
	a.InitiallyExpanded = sortedWorkspacePaths(a.InitiallyExpanded)
	return nil
}

type WorkspaceStorage struct {
	FilesystemKey string `json:"filesystem_key"`
	// A sorted set (Rust BTreeSet).
	Allocations    []string `json:"allocations"`
	Location       string   `json:"location"`
	AvailableBytes uint64   `json:"available_bytes"`
	Copies         uint64   `json:"copies"`
}

// MarshalJSON writes the allocations as serde writes a BTreeSet: sorted,
// and `[]` when empty.
func (s WorkspaceStorage) MarshalJSON() ([]byte, error) {
	type wire WorkspaceStorage
	s.Allocations = slices.Compact(slices.Sorted(slices.Values(nonNilStrings(s.Allocations))))
	if s.Allocations == nil {
		s.Allocations = []string{}
	}
	return marshalSerdeJSON(wire(s))
}

func (s *WorkspaceStorage) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "location", "available_bytes", "copies"); err != nil {
		return err
	}
	type wire WorkspaceStorage
	if err := json.Unmarshal(data, (*wire)(s)); err != nil {
		return err
	}
	if len(s.Allocations) > 0 {
		slices.Sort(s.Allocations)
		s.Allocations = slices.Compact(s.Allocations)
	}
	return nil
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

// saturatingAdd and saturatingMul are Rust's u64 saturating arithmetic.
func saturatingAdd(a, b uint64) uint64 {
	if a > math.MaxUint64-b {
		return math.MaxUint64
	}
	return a + b
}

func saturatingMul(a, b uint64) uint64 {
	if a != 0 && b > math.MaxUint64/a {
		return math.MaxUint64
	}
	return a * b
}

func (s *WorkspaceSelection) IncludedBytes(assessment *WorkspaceAssessment) uint64 {
	sum := assessment.RequiredBytes
	for i := range assessment.Files {
		if s.Includes(&assessment.Files[i].Location) {
			sum = saturatingAdd(sum, assessment.Files[i].Bytes)
		}
	}
	return sum
}

func (s *WorkspaceSelection) Validate(assessment *WorkspaceAssessment) error {
	for i := range s.Exclusions {
		path := &s.Exclusions[i]
		if err := path.Validate(); err != nil {
			return err
		}
		eligible := false
		for j := range assessment.Files {
			if path.Contains(&assessment.Files[j].Location) {
				eligible = true
				break
			}
		}
		if !eligible {
			return fmt.Errorf("excluded path is not an eligible untracked file or directory: %s/%s", path.Repository, path.Path)
		}
	}
	if len(assessment.Blockers) > 0 {
		return errors.New(strings.Join(assessment.Blockers, "; "))
	}
	bytes := s.IncludedBytes(assessment)
	for _, storage := range assessment.Storage {
		required := saturatingAdd(saturatingMul(bytes, storage.Copies), 64*1024*1024)
		if storage.AvailableBytes < required {
			return fmt.Errorf(
				"%s needs approximately %s free for this selection; %s available. Choose fewer files or free space before Move",
				storage.Location, FormatBytes(required), FormatBytes(storage.AvailableBytes),
			)
		}
	}
	if bytes >= LargeTransferBytes && !s.AcknowledgeLargeTransfer {
		return LargeTransferConsentRequired(FormatBytes(bytes))
	}
	return nil
}

// SetIncluded: a checked parent includes all its descendants; an unchecked
// parent excludes them. Selecting a child inside an excluded parent expands
// the exclusion into the other exact files, preserving their selection.
func (s *WorkspaceSelection) SetIncluded(assessment *WorkspaceAssessment, path *WorkspacePath, included bool) {
	var excluded []WorkspacePath
	for i := range assessment.Files {
		location := &assessment.Files[i].Location
		var exclude bool
		if path.Contains(location) {
			exclude = !included
		} else {
			exclude = !s.Includes(location)
		}
		if exclude {
			excluded = append(excluded, *location)
		}
	}
	s.Exclusions = nonNilWorkspacePaths(sortedWorkspacePaths(excluded))
	s.AcknowledgeLargeTransfer = false
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

// MarshalJSON writes no children as `[]`, as serde writes an empty Vec.
func (n FileNode) MarshalJSON() ([]byte, error) {
	type wire FileNode
	if n.Children == nil {
		n.Children = []FileNode{}
	}
	return marshalSerdeJSON(wire(n))
}

func (n *FileNode) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "location", "bytes", "files", "children"); err != nil {
		return err
	}
	type wire FileNode
	return json.Unmarshal(data, (*wire)(n))
}

func (n *FileNode) State(assessment *WorkspaceAssessment, selection *WorkspaceSelection) FileSelectionState {
	included, excluded := false, false
	for i := range assessment.Files {
		location := &assessment.Files[i].Location
		if n.Location.Contains(location) {
			if selection.Includes(location) {
				included = true
			} else {
				excluded = true
			}
		}
	}
	switch {
	case included && excluded:
		return FileSelectionStateMixed
	case included:
		return FileSelectionStateIncluded
	}
	return FileSelectionStateExcluded
}

func (n *FileNode) Label() string {
	suffix := ""
	if len(n.Children) > 0 {
		suffix = "/"
	}
	return fmt.Sprintf("%s/%s%s — %s", n.Location.Repository, n.Location.Path, suffix, FormatBytes(n.Bytes))
}

// compareFileNodes orders nodes by size, largest first, then by location.
func compareFileNodes(a, b *FileNode) int {
	switch {
	case a.Bytes > b.Bytes:
		return -1
	case a.Bytes < b.Bytes:
		return 1
	}
	return a.Location.Compare(b.Location)
}

func FileTree(assessment *WorkspaceAssessment) []FileNode {
	var insert func(nodes *[]FileNode, file *WorkspaceFile, parts []string, index int)
	insert = func(nodes *[]FileNode, file *WorkspaceFile, parts []string, index int) {
		path := parts[index]
		position := slices.IndexFunc(*nodes, func(node FileNode) bool {
			return node.Location.Repository == file.Location.Repository && comparePaths(node.Location.Path, path) == 0
		})
		if position < 0 {
			*nodes = append(*nodes, FileNode{
				Location: WorkspacePath{Repository: file.Location.Repository, Path: path},
			})
			position = len(*nodes) - 1
		}
		node := &(*nodes)[position]
		node.Bytes = saturatingAdd(node.Bytes, file.Bytes)
		node.Files++
		if index+1 < len(parts) {
			insert(&node.Children, file, parts, index+1)
		}
	}
	var finish func(nodes []FileNode)
	finish = func(nodes []FileNode) {
		for i := range nodes {
			finish(nodes[i].Children)
			for len(nodes[i].Children) == 1 && len(nodes[i].Children[0].Children) > 0 {
				nodes[i] = nodes[i].Children[0]
			}
		}
		slices.SortStableFunc(nodes, func(a, b FileNode) int { return compareFileNodes(&a, &b) })
	}
	var roots []FileNode
	for i := range assessment.Files {
		file := &assessment.Files[i]
		// Rust's `Path::ancestors`, without the empty one, outermost first.
		var parts []string
		for path, ok := file.Location.Path, true; ok; path, ok = pathParent(path) {
			if path != "" {
				parts = append(parts, path)
			}
		}
		slices.Reverse(parts)
		if len(parts) > 0 {
			insert(&roots, file, parts, 0)
		}
	}
	finish(roots)
	return roots
}

// InitialExpansion returns a sorted set (Rust BTreeSet).
func InitialExpansion(roots []FileNode) []WorkspacePath {
	var expanded []WorkspacePath
	isExpanded := func(location WorkspacePath) bool {
		return slices.ContainsFunc(expanded, func(other WorkspacePath) bool { return other.Compare(location) == 0 })
	}
	var visible []*FileNode
	for i := range roots {
		if roots[i].Bytes >= SubstantialRootBytes && len(visible) < 8 {
			visible = append(visible, &roots[i])
		}
	}
	for {
		slices.SortStableFunc(visible, compareFileNodes)
		candidate := slices.IndexFunc(visible, func(node *FileNode) bool {
			var children []*FileNode
			for i := range node.Children {
				if node.Children[i].Bytes >= SubstantialRootBytes {
					children = append(children, &node.Children[i])
				}
			}
			return !isExpanded(node.Location) &&
				len(children) > 0 &&
				slices.ContainsFunc(children, func(child *FileNode) bool { return len(child.Children) > 0 }) &&
				len(visible)+len(children) <= 8
		})
		if candidate < 0 {
			break
		}
		node := visible[candidate]
		expanded = append(expanded, node.Location)
		for i := range node.Children {
			if node.Children[i].Bytes >= SubstantialRootBytes {
				visible = append(visible, &node.Children[i])
			}
		}
	}
	return sortedWorkspacePaths(expanded)
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

// MarshalJSON writes no exclusions as `[]`, as serde writes an empty Vec.
func (r RetainedMoveSource) MarshalJSON() ([]byte, error) {
	type wire RetainedMoveSource
	r.Exclusions = nonNilWorkspacePaths(r.Exclusions)
	return marshalSerdeJSON(wire(r))
}

func (r *RetainedMoveSource) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "operation_id", "session_id", "source", "exclusions", "created_at"); err != nil {
		return err
	}
	type wire RetainedMoveSource
	return json.Unmarshal(data, (*wire)(r))
}
