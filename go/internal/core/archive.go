// Port of mj-core/src/archive.rs.
//
// Shared checkpoint archive formats and canonical session validation.

package core

import (
	"encoding/json"
	"errors"
	"io"
	"os"
	"strings"
)

// ArchiveSchemaVersion is the baseline schema. Every payload occupies
// exactly one ZIP entry, so any build that understands schema 2 can read the
// archive.
const ArchiveSchemaVersion uint32 = 2

// ArchiveSchemaVersionSharded is schema 2 plus sharded payloads. A payload
// larger than the archive writer's part-size limit is written as several
// `*.helpart.NNNNN` ZIP entries so compression and verification can run in
// parallel. Archives declare this schema only when at least one payload is
// sharded, which keeps small sessions readable by builds that predate
// sharding and makes older builds reject sharded archives with an explicit
// version error instead of misreading part entries.
const ArchiveSchemaVersionSharded uint32 = 3

// ArchiveSchemaVersionAttachments: separate image blobs must be restored
// into the relay, not the harness home.
const ArchiveSchemaVersionAttachments uint32 = 4

// ContextBoundaryPrefix is a durable transcript identity; text alone must
// never reset model history.
const ContextBoundaryPrefix = "context-cleared:"

func IsContextBoundary(stableID string) bool {
	return strings.HasPrefix(stableID, ContextBoundaryPrefix)
}

const ArchiveSchemaVersionContext uint32 = 5

// ArchiveSchemaVersionCloneRefs: Git refs and stash entries are preserved in
// independently owned clones.
const ArchiveSchemaVersionCloneRefs uint32 = 6

// ArchiveSchemaVersionAgentReports adds sub-agent report files, restored
// beside the repositories rather than into the harness home. It supersedes
// every earlier schema: an archive that carries reports declares it whatever
// else it carries.
const ArchiveSchemaVersionAgentReports uint32 = 7

// ArchiveSchemaVersionCheckoutSubdirectory: a repository captured from a
// subdirectory of its checkout, whose untracked files are named from the
// checkout's top level. An older build would unpack them inside the
// subdirectory. It supersedes every earlier schema.
const ArchiveSchemaVersionCheckoutSubdirectory uint32 = 8

// ArchiveSchemaVersionMessages: typed received-message history that older
// canonical readers cannot decode.
const ArchiveSchemaVersionMessages uint32 = 9

// ArchiveSchemaVersionReviewRefs: durable private Git refs, including
// non-commit review baselines.
const ArchiveSchemaVersionReviewRefs uint32 = 10

// ArchiveSchemaVersionLinkedWorktrees: linked Git worktrees inside a
// captured checkout, restored beside it. An older build would drop them
// silently, so an archive that carries any declares this schema whatever
// else it carries.
const ArchiveSchemaVersionLinkedWorktrees uint32 = 11

// ArchiveSchemaVersionNotificationStack: older readers reject the new
// canonical notification record.
const ArchiveSchemaVersionNotificationStack uint32 = 12

// MjolnirRefNamespace holds Mjolnir's private Git refs: review baselines,
// review points and reviewed trees. They are bookkeeping, never pushed, and
// some name a bare tree.
const MjolnirRefNamespace = "refs/mj/"

// IsReviewBaselinePin reports a review baseline pin:
// `refs/mj/review-baseline` for a main worktree, or
// `refs/mj/worktrees/<name>/review-baseline` for a linked one. Releases that
// reviewed each turn wrote them into the user's repositories. Nothing reads
// their value any more, so captures and restores neither carry nor write
// them.
func IsReviewBaselinePin(name string) bool {
	return name == "refs/mj/review-baseline" ||
		(strings.HasPrefix(name, "refs/mj/worktrees/") && strings.HasSuffix(name, "/review-baseline"))
}

// NestedRepositoryCheckpointError is the shared diagnostic identity across
// the target export's stderr boundary.
const NestedRepositoryCheckpointError = "cannot checkpoint nested Git repository directory at "
const ArchiveFormat = "hel-session"
const EventFrontierGenesisDigest = "0000000000000000000000000000000000000000000000000000000000000000"

type SessionManifest struct {
	ID              string      `json:"id"`
	Title           string      `json:"title"`
	HarnessKind     HarnessKind `json:"harness_kind"`
	ProfileID       string      `json:"profile_id"`
	NativeSessionID string      `json:"native_session_id"`
	CreatedAt       string      `json:"created_at"`
	CheckpointedAt  string      `json:"checkpointed_at"`
	HelVersion      string      `json:"hel_version"`
	RelayVersion    string      `json:"relay_version"`
	AdapterVersion  string      `json:"adapter_version"`
}

func (m *SessionManifest) UnmarshalJSON(data []byte) error {
	type plain SessionManifest
	return transcriptStrictDecode(data, (*plain)(m), true)
}

type BundleManifest struct {
	ID                string `json:"id"`
	PrimaryRepository string `json:"primary_repository"`
}

func (m *BundleManifest) UnmarshalJSON(data []byte) error {
	type plain BundleManifest
	return transcriptStrictDecode(data, (*plain)(m), true)
}

type TargetManifest struct {
	TemplateID string `json:"template_id"`
	TargetKind string `json:"target_kind"`
	// Details is informational provenance only. It must not contain credentials.
	Details map[string]string `json:"details"`
}

func (m *TargetManifest) UnmarshalJSON(data []byte) error {
	type plain TargetManifest
	return transcriptStrictDecode(data, (*plain)(m), true)
}

type RepositoryMetadata struct {
	ID                  string `json:"id"`
	RelativeDestination string `json:"relative_destination"`
	// CheckoutSubdirectory is where the directory at RelativeDestination sits
	// inside its Git checkout, when it is not the checkout's top level. The
	// snapshot still covers the whole checkout: its untracked files are named
	// from the top level, and a restore works from the directory this path
	// leads up to. Absent for a top-level directory, and in archives written
	// before a subdirectory was captured this way; those carry only the
	// subdirectory's untracked files, named from the subdirectory.
	CheckoutSubdirectory *string `json:"checkout_subdirectory,omitempty"`
	Origin               string  `json:"origin"`
	// PushURLs are explicit push destinations configured for origin. An
	// empty list means Git's normal push fallback (the fetch URL), and is
	// omitted from older archive manifests for compatibility.
	PushURLs []string `json:"push_urls,omitempty"`
	// RemoteWorkspace marks a repository cloned for a managed network
	// workspace. The marker makes a nonempty BaseCommit unambiguous: raw
	// DeltaFrom snapshots also carry a base, but must not inherit managed
	// push configuration.
	RemoteWorkspace bool `json:"remote_workspace,omitempty"`
	// PlainDirectory: the directory has no committed Git checkout. A bare
	// local session may run in a plain folder of the user's own or a
	// repository without its first commit; nothing about such a directory is
	// captured, and a restore must leave it alone. The field is absent from
	// every archive written before plain folders were accepted.
	PlainDirectory bool `json:"plain_directory,omitempty"`
	// BaseCommit is the immutable launch base for managed network
	// workspaces; raw DeltaFrom snapshots record their capture base. Legacy
	// SessionDelta snapshots exclude origin refs instead and leave this empty.
	BaseCommit string  `json:"base_commit"`
	HeadCommit string  `json:"head_commit"`
	Branch     *string `json:"branch"`
	// ReviewRefsMetadataOnly: only ref names and object IDs were captured.
	// The raw checkout owns the objects and working files; absent objects do
	// not prevent resume.
	ReviewRefsMetadataOnly bool `json:"review_refs_metadata_only,omitempty"`
	// ReviewBaselineRef is the source checkout's session-start scope name,
	// used to relocate that checkout's session-start refs. Archives written
	// by earlier releases also carried the review baseline pin under this
	// name; restore ignores that pin.
	ReviewBaselineRef *string `json:"review_baseline_ref,omitempty"`
	// SavedRefs are saved local branches, tags, notes, and durable Mjolnir refs.
	SavedRefs map[string]string `json:"saved_refs,omitempty"`
	// StashStack is newest to oldest, including stash entries unreachable
	// from refs/stash.
	StashStack []SavedStashEntry `json:"stash_stack,omitempty"`
	// GitOperation is the Git operation an independent clone was in the
	// middle of: a merge, cherry-pick, revert, rebase or bisect, or unmerged
	// index entries that any command left. Absent when there is none, so
	// every other archive is written exactly as before this field existed.
	GitOperation *SavedGitOperation `json:"git_operation,omitempty"`
}

func (m *RepositoryMetadata) UnmarshalJSON(data []byte) error {
	type plain RepositoryMetadata
	return transcriptStrictDecode(data, (*plain)(m), false)
}

// WorkRefs are saved refs that hold the session's own Git work: everything
// except Mjolnir's private refs, which are never published and so must not
// decide whether the work is.
func (m *RepositoryMetadata) WorkRefs() map[string]string {
	refs := map[string]string{}
	for name, value := range m.SavedRefs {
		if !strings.HasPrefix(name, MjolnirRefNamespace) {
			refs[name] = value
		}
	}
	return refs
}

type SavedStashEntry struct {
	Commit  string `json:"commit"`
	Message string `json:"message"`
}

func (e *SavedStashEntry) UnmarshalJSON(data []byte) error {
	type plain SavedStashEntry
	return transcriptStrictDecode(data, (*plain)(e), false)
}

// LinkedWorktreeMetadata is a linked worktree of the captured repository
// that lives inside the captured checkout, for example one made with
// `git worktree add .mj/lanes/x`. Its commits live in the repository's own
// bundle; this record and its payloads carry the rest of what the worktree
// holds.
type LinkedWorktreeMetadata struct {
	// Path is the worktree's directory below the checkout's top level,
	// written as /-separated text on every OS.
	Path       string `json:"path"`
	HeadCommit string `json:"head_commit"`
	// Branch is the branch checked out in the worktree; nil when HEAD is detached.
	Branch *string `json:"branch"`
	// LockReason is the reason the worktree was locked with, empty for a
	// lock without one. Nil when it is not locked.
	LockReason *string `json:"lock_reason,omitempty"`
	// GitOperation is the operation the worktree was in the middle of, as
	// for a clone.
	GitOperation *SavedGitOperation `json:"git_operation,omitempty"`
}

// MarshalJSON writes Path as /-separated text (serde with = "slash_path").
func (m LinkedWorktreeMetadata) MarshalJSON() ([]byte, error) {
	type plain LinkedWorktreeMetadata
	path, err := slashPathSerialize(m.Path)
	if err != nil {
		return nil, err
	}
	v := plain(m)
	v.Path = path
	return transcriptMarshal(v)
}

func (m *LinkedWorktreeMetadata) UnmarshalJSON(data []byte) error {
	type plain LinkedWorktreeMetadata
	var v plain
	if err := transcriptStrictDecode(data, &v, false); err != nil {
		return err
	}
	path, err := slashPathDeserialize(v.Path)
	if err != nil {
		return err
	}
	v.Path = path
	*m = LinkedWorktreeMetadata(v)
	return nil
}

// slashPathSerialize writes a plain relative path as /-separated text.
func slashPathSerialize(path string) (string, error) {
	panic("unported: mj-core/src/archive.rs slash_path::serialize")
}

// slashPathDeserialize reads /-separated text as a native relative path.
func slashPathDeserialize(text string) (string, error) {
	if text == "" {
		return "", errors.New("worktree path is empty")
	}
	panic("unported: mj-core/src/archive.rs slash_path::deserialize")
}

// LinkedWorktreeManifest is a linked worktree's manifest entry: its record
// and where its payloads are.
type LinkedWorktreeManifest struct {
	Metadata          LinkedWorktreeMetadata `json:"-"`
	StagedPatchPath   string                 `json:"staged_patch_path"`
	UnstagedPatchPath string                 `json:"unstaged_patch_path"`
	UntrackedTarPath  string                 `json:"untracked_tar_path"`
	// ConflictedWorktreeTarPath is present exactly when Metadata.GitOperation is.
	ConflictedWorktreeTarPath *string `json:"conflicted_worktree_tar_path,omitempty"`
	OperationStateTarPath     *string `json:"operation_state_tar_path,omitempty"`
}

// MarshalJSON flattens Metadata ahead of the payload paths.
func (m LinkedWorktreeManifest) MarshalJSON() ([]byte, error) {
	type plain LinkedWorktreeManifest
	return archiveMarshalFlattened(m.Metadata, plain(m))
}

func (m *LinkedWorktreeManifest) UnmarshalJSON(data []byte) error {
	type plain LinkedWorktreeManifest
	var v plain
	if err := transcriptStrictDecode(data, &v, false); err != nil {
		return err
	}
	if err := json.Unmarshal(data, &v.Metadata); err != nil {
		return err
	}
	*m = LinkedWorktreeManifest(v)
	return nil
}

// SavedGitOperation is the index side of an unfinished Git operation, and
// the objects it needs. The operation's working-tree files and its state
// files in the Git directory are the repository's two operation payloads.
type SavedGitOperation struct {
	// Unmerged is the index's unmerged entries, in index order.
	Unmerged []SavedUnmergedEntry `json:"unmerged,omitempty"`
	// MissingWorktreePaths are unmerged paths that had no file in the
	// working tree.
	MissingWorktreePaths []string `json:"missing_worktree_paths,omitempty"`
	// Objects is every object the unmerged entries and the state files
	// name. The committed bundle carries each one the restored clone could
	// lack, and a restore checks that all of them exist before it writes the
	// operation.
	Objects []string `json:"objects,omitempty"`
}

// SavedUnmergedEntry is one stage of an unmerged path, as
// `git ls-files --unmerged` lists it.
type SavedUnmergedEntry struct {
	Path string `json:"path"`
	// Stage is 1 for the common ancestor, 2 for ours, 3 for theirs.
	Stage uint8 `json:"stage"`
	// Mode is the octal mode as Git writes it, such as 100644.
	Mode   string `json:"mode"`
	Object string `json:"object"`
}

func (e *SavedUnmergedEntry) UnmarshalJSON(data []byte) error {
	type plain SavedUnmergedEntry
	return transcriptStrictDecode(data, (*plain)(e), false)
}

type RepositoryManifest struct {
	Metadata            RepositoryMetadata `json:"-"`
	CommittedBundlePath string             `json:"committed_bundle_path"`
	StagedPatchPath     string             `json:"staged_patch_path"`
	UnstagedPatchPath   string             `json:"unstaged_patch_path"`
	UntrackedTarPath    string             `json:"untracked_tar_path"`
	// ConflictedWorktreeTarPath holds the working-tree files of the unmerged
	// paths. Present exactly when Metadata.GitOperation is.
	ConflictedWorktreeTarPath *string `json:"conflicted_worktree_tar_path,omitempty"`
	// OperationStateTarPath holds the operation's state files from the Git
	// directory. Present exactly when Metadata.GitOperation is.
	OperationStateTarPath *string `json:"operation_state_tar_path,omitempty"`
	// LinkedWorktrees are linked worktrees inside the checkout, parents
	// before children. An older reader would ignore this field, which is why
	// an archive that has any declares ArchiveSchemaVersionLinkedWorktrees.
	LinkedWorktrees []LinkedWorktreeManifest `json:"linked_worktrees,omitempty"`
}

// MarshalJSON flattens Metadata ahead of the payload paths.
func (m RepositoryManifest) MarshalJSON() ([]byte, error) {
	type plain RepositoryManifest
	return archiveMarshalFlattened(m.Metadata, plain(m))
}

func (m *RepositoryManifest) UnmarshalJSON(data []byte) error {
	type plain RepositoryManifest
	var v plain
	if err := transcriptStrictDecode(data, &v, false); err != nil {
		return err
	}
	if err := json.Unmarshal(data, &v.Metadata); err != nil {
		return err
	}
	*m = RepositoryManifest(v)
	return nil
}

// archiveMarshalFlattened writes the members of flattened followed by the
// members of rest, as serde's #[serde(flatten)] on a leading field does.
func archiveMarshalFlattened(flattened, rest any) ([]byte, error) {
	first, err := transcriptMarshal(flattened)
	if err != nil {
		return nil, err
	}
	second, err := transcriptMarshal(rest)
	if err != nil {
		return nil, err
	}
	switch {
	case len(first) <= 2:
		return second, nil
	case len(second) <= 2:
		return first, nil
	}
	out := append([]byte{}, first[:len(first)-1]...)
	out = append(out, ',')
	return append(out, second[1:]...), nil
}

// CanonicalSessionSnapshot is controller-owned, materialized state captured
// at a checkpoint barrier.
//
// These types deliberately do not depend on the live ACP or relay types. ACP
// content blocks and evolving tool/plan details are stored as JSON values at
// the stable archive boundary, while the identity, ordering, and timestamps
// needed to rebuild controller state remain explicit.
type CanonicalSessionSnapshot struct {
	// CommandLedger is ignored. Archives written by protocol 26 workers
	// carried a command receipt ledger here; it is accepted so those
	// archives still restore.
	CommandLedger json.RawMessage `json:"command_ledger,omitempty"`
	// AssessmentState is versioned worker assessment state, validated by
	// assessment Checkpoint.
	AssessmentState json.RawMessage `json:"assessment_state,omitempty"`
	// NotificationStack is worker-owned state outside the bounded assessment record.
	NotificationStack *NotificationStack `json:"notification_stack,omitempty"`
	// EventFrontier is the highest relay event ordinal incorporated into
	// this projection.
	EventFrontier uint64 `json:"event_frontier"`
	// EventFrontierDigest is the relay-authored rolling digest of the exact
	// event prefix at the frontier.
	EventFrontierDigest string                    `json:"event_frontier_digest"`
	Session             CanonicalSessionState     `json:"session"`
	Transcript          []CanonicalTranscriptItem `json:"transcript"`
	QueuedPrompts       []CanonicalQueuedPrompt   `json:"queued_prompts"`
}

func (s *CanonicalSessionSnapshot) UnmarshalJSON(data []byte) error {
	type plain CanonicalSessionSnapshot
	return transcriptStrictDecode(data, (*plain)(s), true)
}

func (s *CanonicalSessionSnapshot) CurrentContextStart() uint64 {
	var start uint64
	for i := range s.Transcript {
		if IsContextBoundary(s.Transcript[i].StableID) && s.Transcript[i].Position > start {
			start = s.Transcript[i].Position
		}
	}
	return start
}

func (s *CanonicalSessionSnapshot) Validate() error {
	return validateCanonicalSession(s)
}

// ContentMatches reports whether two snapshots carry the same session
// content.
//
// The event frontier and its digest are deliberately ignored: relay
// bookkeeping advances them without changing what the session contains.
// LastActivityAtMs is a watermark of the same kind.
func (s *CanonicalSessionSnapshot) ContentMatches(other *CanonicalSessionSnapshot) bool {
	panic("unported: mj-core/src/archive.rs CanonicalSessionSnapshot::content_matches")
}

type CanonicalSessionState struct {
	Execution CanonicalExecutionState `json:"execution"`
	// LastActivityAtMs is the monotonic controller projection watermark
	// derived from relay events.
	LastActivityAtMs *int64               `json:"last_activity_at_ms"`
	SessionTitle     *string              `json:"session_title"`
	Configuration    SessionConfiguration `json:"configuration"`
}

func (s *CanonicalSessionState) UnmarshalJSON(data []byte) error {
	type plain CanonicalSessionState
	return transcriptStrictDecode(data, (*plain)(s), true)
}

// withoutActivityWatermark is the same state with its volatile activity
// watermark cleared, so two states can be compared on content alone.
// Copying keeps every other field in the comparison, including fields added
// later.
func (s CanonicalSessionState) withoutActivityWatermark() CanonicalSessionState {
	s.LastActivityAtMs = nil
	return s
}

// CanonicalExecutionStateKind is the "state" tag of a CanonicalExecutionState.
type CanonicalExecutionStateKind string

const (
	CanonicalExecutionStateIdle    CanonicalExecutionStateKind = "idle"
	CanonicalExecutionStateRunning CanonicalExecutionStateKind = "running"
	CanonicalExecutionStateClosing CanonicalExecutionStateKind = "closing"
	CanonicalExecutionStateClosed  CanonicalExecutionStateKind = "closed"
)

// CanonicalExecutionState is one struct with its "state" tag rather than a
// sealed interface: it is a small comparable value, and StartedAtMs is set
// only for the running state.
type CanonicalExecutionState struct {
	State       CanonicalExecutionStateKind
	StartedAtMs int64
}

func (s CanonicalExecutionState) MarshalJSON() ([]byte, error) {
	if s.State == CanonicalExecutionStateRunning {
		return transcriptMarshalTagged("state", string(s.State), struct {
			StartedAtMs int64 `json:"started_at_ms"`
		}{s.StartedAtMs})
	}
	return transcriptMarshalTagged("state", string(s.State), nil)
}

func (s *CanonicalExecutionState) UnmarshalJSON(data []byte) error {
	tag, err := transcriptTag(data, "state", "CanonicalExecutionState")
	if err != nil {
		return err
	}
	switch kind := CanonicalExecutionStateKind(tag); kind {
	case CanonicalExecutionStateRunning:
		var v struct {
			StartedAtMs int64 `json:"started_at_ms"`
		}
		if err := transcriptStrictDecode(data, &v, true, "state"); err != nil {
			return err
		}
		*s = CanonicalExecutionState{State: kind, StartedAtMs: v.StartedAtMs}
		return nil
	case CanonicalExecutionStateIdle, CanonicalExecutionStateClosing, CanonicalExecutionStateClosed:
		if err := transcriptStrictDecode(data, &struct{}{}, true, "state"); err != nil {
			return err
		}
		*s = CanonicalExecutionState{State: kind}
		return nil
	}
	return transcriptUnknownVariant(tag, "idle", "running", "closing", "closed")
}

type CanonicalTranscriptItem struct {
	StableID string `json:"stable_id"`
	// Position is the ordinal of the event that created this logical
	// transcript item.
	Position uint64 `json:"position"`
	// LatestContentEventOrdinal is the ordinal of the most recent content
	// chunk for an agent message. It is nil for every other logical item.
	LatestContentEventOrdinal *uint64                 `json:"latest_content_event_ordinal"`
	CreatedAtMs               int64                   `json:"created_at_ms"`
	LastChangedAtMs           int64                   `json:"last_changed_at_ms"`
	Body                      CanonicalTranscriptBody `json:"body"`
}

// UnmarshalJSON applies deny_unknown_fields and decodes the tagged body.
func (t *CanonicalTranscriptItem) UnmarshalJSON(data []byte) error {
	type plain struct {
		StableID                  string          `json:"stable_id"`
		Position                  uint64          `json:"position"`
		LatestContentEventOrdinal *uint64         `json:"latest_content_event_ordinal"`
		CreatedAtMs               int64           `json:"created_at_ms"`
		LastChangedAtMs           int64           `json:"last_changed_at_ms"`
		Body                      json.RawMessage `json:"body"`
	}
	var v plain
	if err := transcriptStrictDecode(data, &v, true); err != nil {
		return err
	}
	body, err := DecodeCanonicalTranscriptBody(v.Body)
	if err != nil {
		return err
	}
	*t = CanonicalTranscriptItem{v.StableID, v.Position, v.LatestContentEventOrdinal, v.CreatedAtMs, v.LastChangedAtMs, body}
	return nil
}

// CanonicalTranscriptBody is the archived body of a transcript item,
// internally tagged by "kind" with unknown fields denied. Decode with
// DecodeCanonicalTranscriptBody.
type CanonicalTranscriptBody interface {
	isCanonicalTranscriptBody()
}

type CanonicalTranscriptBodyMessage struct {
	Event *MailboxEvent `json:"event"`
}

type CanonicalTranscriptBodyUser struct {
	// Content holds ACP content blocks in their JSON representation.
	Content []json.RawMessage `json:"content"`
}

type CanonicalTranscriptBodyAgent struct {
	// Chunks are complete ACP ContentChunk values.
	Chunks    []json.RawMessage `json:"chunks"`
	Streaming bool              `json:"streaming"`
}

type CanonicalTranscriptBodyThought struct {
	// Chunks are complete ACP ContentChunk values.
	Chunks    []json.RawMessage `json:"chunks"`
	Streaming bool              `json:"streaming"`
}

type CanonicalTranscriptBodyTool struct {
	// Call is the complete current ACP ToolCall value.
	Call json.RawMessage `json:"call"`
	// TerminalOutputs is the output of the terminals this call's content
	// refers to.
	TerminalOutputs []CanonicalTerminalOutput `json:"terminal_outputs,omitempty"`
	// TerminalRefs is every terminal this call has ever referred to,
	// including references a later content update dropped.
	TerminalRefs []string `json:"terminal_refs,omitempty"`
	// Presentation is cached compact label data. This is a sidecar rather
	// than ACP metadata so legacy transcript conversions preserve their
	// summary without changing the provider-owned tool call.
	Presentation *ToolCallPresentation `json:"presentation,omitempty"`
}

// CanonicalTranscriptBodyTerminalOutput is terminal output no tool call
// refers to.
type CanonicalTranscriptBodyTerminalOutput struct {
	Record CanonicalTerminalOutput `json:"record"`
}

type CanonicalTranscriptBodyPlan struct {
	// Plan is the complete current ACP Plan value.
	Plan json.RawMessage `json:"plan"`
}

// CanonicalTranscriptBodyPlanProposal is a plan the harness asked the user
// to approve, kept verbatim.
type CanonicalTranscriptBodyPlanProposal struct {
	ProposalID string `json:"proposal_id"`
	Plan       string `json:"plan"`
}

type CanonicalTranscriptBodySystem struct {
	Text string `json:"text"`
}

func (CanonicalTranscriptBodyMessage) isCanonicalTranscriptBody()        {}
func (CanonicalTranscriptBodyUser) isCanonicalTranscriptBody()           {}
func (CanonicalTranscriptBodyAgent) isCanonicalTranscriptBody()          {}
func (CanonicalTranscriptBodyThought) isCanonicalTranscriptBody()        {}
func (CanonicalTranscriptBodyTool) isCanonicalTranscriptBody()           {}
func (CanonicalTranscriptBodyTerminalOutput) isCanonicalTranscriptBody() {}
func (CanonicalTranscriptBodyPlan) isCanonicalTranscriptBody()           {}
func (CanonicalTranscriptBodyPlanProposal) isCanonicalTranscriptBody()   {}
func (CanonicalTranscriptBodySystem) isCanonicalTranscriptBody()         {}

func (b CanonicalTranscriptBodyMessage) MarshalJSON() ([]byte, error) {
	type plain CanonicalTranscriptBodyMessage
	return transcriptMarshalTagged("kind", "message", plain(b))
}

func (b CanonicalTranscriptBodyUser) MarshalJSON() ([]byte, error) {
	type plain CanonicalTranscriptBodyUser
	return transcriptMarshalTagged("kind", "user", plain(b))
}

func (b CanonicalTranscriptBodyAgent) MarshalJSON() ([]byte, error) {
	type plain CanonicalTranscriptBodyAgent
	return transcriptMarshalTagged("kind", "agent", plain(b))
}

func (b CanonicalTranscriptBodyThought) MarshalJSON() ([]byte, error) {
	type plain CanonicalTranscriptBodyThought
	return transcriptMarshalTagged("kind", "thought", plain(b))
}

func (b CanonicalTranscriptBodyTool) MarshalJSON() ([]byte, error) {
	type plain CanonicalTranscriptBodyTool
	return transcriptMarshalTagged("kind", "tool", plain(b))
}

func (b CanonicalTranscriptBodyTerminalOutput) MarshalJSON() ([]byte, error) {
	type plain CanonicalTranscriptBodyTerminalOutput
	return transcriptMarshalTagged("kind", "terminal_output", plain(b))
}

func (b CanonicalTranscriptBodyPlan) MarshalJSON() ([]byte, error) {
	type plain CanonicalTranscriptBodyPlan
	return transcriptMarshalTagged("kind", "plan", plain(b))
}

func (b CanonicalTranscriptBodyPlanProposal) MarshalJSON() ([]byte, error) {
	type plain CanonicalTranscriptBodyPlanProposal
	return transcriptMarshalTagged("kind", "plan_proposal", plain(b))
}

func (b CanonicalTranscriptBodySystem) MarshalJSON() ([]byte, error) {
	type plain CanonicalTranscriptBodySystem
	return transcriptMarshalTagged("kind", "system", plain(b))
}

// DecodeCanonicalTranscriptBody reads a CanonicalTranscriptBody, tagged by
// "kind", denying unknown fields.
func DecodeCanonicalTranscriptBody(data []byte) (CanonicalTranscriptBody, error) {
	tag, err := transcriptTag(data, "kind", "CanonicalTranscriptBody")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "message":
		type plain CanonicalTranscriptBodyMessage
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return CanonicalTranscriptBodyMessage(v), err
	case "user":
		type plain CanonicalTranscriptBodyUser
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return CanonicalTranscriptBodyUser(v), err
	case "agent":
		type plain CanonicalTranscriptBodyAgent
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return CanonicalTranscriptBodyAgent(v), err
	case "thought":
		type plain CanonicalTranscriptBodyThought
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return CanonicalTranscriptBodyThought(v), err
	case "tool":
		type plain CanonicalTranscriptBodyTool
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return CanonicalTranscriptBodyTool(v), err
	case "terminal_output":
		type plain CanonicalTranscriptBodyTerminalOutput
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return CanonicalTranscriptBodyTerminalOutput(v), err
	case "plan":
		type plain CanonicalTranscriptBodyPlan
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return CanonicalTranscriptBodyPlan(v), err
	case "plan_proposal":
		type plain CanonicalTranscriptBodyPlanProposal
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return CanonicalTranscriptBodyPlanProposal(v), err
	case "system":
		type plain CanonicalTranscriptBodySystem
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return CanonicalTranscriptBodySystem(v), err
	}
	return nil, transcriptUnknownVariant(tag, "message", "user", "agent", "thought", "tool",
		"terminal_output", "plan", "plan_proposal", "system")
}

// CanonicalTerminalOutput is the archived form of one client-run terminal's
// output. Mirrors the state module's TerminalOutputRecord.
type CanonicalTerminalOutput struct {
	TerminalID string  `json:"terminal_id"`
	Output     string  `json:"output"`
	Truncated  bool    `json:"truncated,omitempty"`
	ExitCode   *uint32 `json:"exit_code,omitempty"`
	Signal     *string `json:"signal,omitempty"`
}

func (o *CanonicalTerminalOutput) UnmarshalJSON(data []byte) error {
	type plain CanonicalTerminalOutput
	return transcriptStrictDecode(data, (*plain)(o), true)
}

// CanonicalQueuedCommandKindTag names what a queued entry does.
type CanonicalQueuedCommandKindTag string

const (
	CanonicalQueuedCommandKindPrompt    CanonicalQueuedCommandKindTag = "prompt"
	CanonicalQueuedCommandKindSetConfig CanonicalQueuedCommandKindTag = "set_config"
)

// CanonicalQueuedCommandKind is what a queued entry does when its turn
// comes. Archives written before configuration changes could be queued
// carry no kind, so it defaults to a prompt (the zero value).
//
// One struct with its tag: Key and Value are set only for set_config.
// Serialized externally tagged: "prompt" or {"set_config":{"key":..,"value":..}}.
type CanonicalQueuedCommandKind struct {
	Kind  CanonicalQueuedCommandKindTag
	Key   string
	Value string
}

func (k CanonicalQueuedCommandKind) MarshalJSON() ([]byte, error) {
	if k.Kind == CanonicalQueuedCommandKindSetConfig {
		return transcriptMarshalMembers([]transcriptMember{{"set_config", struct {
			Key   string `json:"key"`
			Value string `json:"value"`
		}{k.Key, k.Value}}})
	}
	return transcriptMarshal(string(CanonicalQueuedCommandKindPrompt))
}

func (k *CanonicalQueuedCommandKind) UnmarshalJSON(data []byte) error {
	var unit string
	if json.Unmarshal(data, &unit) == nil {
		if unit != string(CanonicalQueuedCommandKindPrompt) {
			return transcriptUnknownVariant(unit, "prompt", "set_config")
		}
		*k = CanonicalQueuedCommandKind{Kind: CanonicalQueuedCommandKindPrompt}
		return nil
	}
	members, err := transcriptObject(data)
	if err != nil {
		return err
	}
	if len(members) != 1 {
		return errors.New("invalid type: map, expected map with a single key")
	}
	raw, ok := members["set_config"]
	if !ok {
		for tag := range members {
			return transcriptUnknownVariant(tag, "prompt", "set_config")
		}
	}
	var v struct {
		Key   string `json:"key"`
		Value string `json:"value"`
	}
	if err := transcriptStrictDecode(raw, &v, false); err != nil {
		return err
	}
	*k = CanonicalQueuedCommandKind{Kind: CanonicalQueuedCommandKindSetConfig, Key: v.Key, Value: v.Value}
	return nil
}

func (k CanonicalQueuedCommandKind) isPrompt() bool {
	return k.Kind != CanonicalQueuedCommandKindSetConfig
}

type CanonicalQueuedPrompt struct {
	CommandID string                     `json:"command_id"`
	Kind      CanonicalQueuedCommandKind `json:"kind"`
	// Content holds ACP content blocks in their JSON representation. A
	// queued configuration change carries the composer text that produced it.
	Content    []json.RawMessage `json:"content"`
	QueuedAtMs int64             `json:"queued_at_ms"`
}

// MarshalJSON omits Kind for a prompt.
func (p CanonicalQueuedPrompt) MarshalJSON() ([]byte, error) {
	members := []transcriptMember{{"command_id", p.CommandID}}
	if !p.Kind.isPrompt() {
		members = append(members, transcriptMember{"kind", p.Kind})
	}
	members = append(members,
		transcriptMember{"content", p.Content},
		transcriptMember{"queued_at_ms", p.QueuedAtMs})
	return transcriptMarshalMembers(members)
}

func (p *CanonicalQueuedPrompt) UnmarshalJSON(data []byte) error {
	type plain struct {
		CommandID  string                     `json:"command_id"`
		Kind       CanonicalQueuedCommandKind `json:"kind" serde:"default"`
		Content    []json.RawMessage          `json:"content"`
		QueuedAtMs int64                      `json:"queued_at_ms"`
	}
	var v plain
	if err := transcriptStrictDecode(data, &v, true); err != nil {
		return err
	}
	*p = CanonicalQueuedPrompt(v)
	return nil
}

// PayloadRoleKind is the "kind" tag of a PayloadRole.
type PayloadRoleKind string

const (
	PayloadRoleCanonicalSession                       PayloadRoleKind = "canonical_session"
	PayloadRoleNativeArtifact                         PayloadRoleKind = "native_artifact"
	PayloadRoleGitBundle                              PayloadRoleKind = "git_bundle"
	PayloadRoleGitStagedPatch                         PayloadRoleKind = "git_staged_patch"
	PayloadRoleGitUnstagedPatch                       PayloadRoleKind = "git_unstaged_patch"
	PayloadRoleGitUntrackedTar                        PayloadRoleKind = "git_untracked_tar"
	PayloadRoleGitConflictedWorktreeTar               PayloadRoleKind = "git_conflicted_worktree_tar"
	PayloadRoleGitOperationStateTar                   PayloadRoleKind = "git_operation_state_tar"
	PayloadRoleGitLinkedWorktreeStagedPatch           PayloadRoleKind = "git_linked_worktree_staged_patch"
	PayloadRoleGitLinkedWorktreeUnstagedPatch         PayloadRoleKind = "git_linked_worktree_unstaged_patch"
	PayloadRoleGitLinkedWorktreeUntrackedTar          PayloadRoleKind = "git_linked_worktree_untracked_tar"
	PayloadRoleGitLinkedWorktreeConflictedWorktreeTar PayloadRoleKind = "git_linked_worktree_conflicted_worktree_tar"
	PayloadRoleGitLinkedWorktreeOperationStateTar     PayloadRoleKind = "git_linked_worktree_operation_state_tar"
)

// PayloadRole is what an archive payload holds. It is one struct with its
// "kind" tag rather than a sealed interface: every variant carries a subset
// of the same three fields.
//
//   - native_artifact carries RelativePath.
//   - every git_* kind carries RepositoryID.
//   - git_conflicted_worktree_tar holds the working-tree files of a
//     repository's unmerged paths, conflict markers included. A build that
//     predates it rejects the manifest as a whole, so it cannot restore the
//     clone without its operation.
//   - git_operation_state_tar holds the state files of a repository's
//     unfinished Git operation, named as `git rev-parse --git-path` names them.
//   - every git_linked_worktree_* kind also carries Worktree: the payloads of
//     the repository's Worktree-th linked worktree, which are those of the
//     checkout itself for that worktree's directory.
type PayloadRole struct {
	Kind         PayloadRoleKind
	RelativePath string
	RepositoryID string
	Worktree     uint32
}

func (r PayloadRole) MarshalJSON() ([]byte, error) {
	switch {
	case r.Kind == PayloadRoleCanonicalSession:
		return transcriptMarshalTagged("kind", string(r.Kind), nil)
	case r.Kind == PayloadRoleNativeArtifact:
		return transcriptMarshalTagged("kind", string(r.Kind), struct {
			RelativePath string `json:"relative_path"`
		}{r.RelativePath})
	case strings.HasPrefix(string(r.Kind), "git_linked_worktree_"):
		return transcriptMarshalTagged("kind", string(r.Kind), struct {
			RepositoryID string `json:"repository_id"`
			Worktree     uint32 `json:"worktree"`
		}{r.RepositoryID, r.Worktree})
	}
	return transcriptMarshalTagged("kind", string(r.Kind), struct {
		RepositoryID string `json:"repository_id"`
	}{r.RepositoryID})
}

func (r *PayloadRole) UnmarshalJSON(data []byte) error {
	tag, err := transcriptTag(data, "kind", "PayloadRole")
	if err != nil {
		return err
	}
	kind := PayloadRoleKind(tag)
	switch kind {
	case PayloadRoleCanonicalSession:
		err = transcriptStrictDecode(data, &struct{}{}, true, "kind")
		*r = PayloadRole{Kind: kind}
	case PayloadRoleNativeArtifact:
		var v struct {
			RelativePath string `json:"relative_path"`
		}
		err = transcriptStrictDecode(data, &v, true, "kind")
		*r = PayloadRole{Kind: kind, RelativePath: v.RelativePath}
	case PayloadRoleGitBundle, PayloadRoleGitStagedPatch, PayloadRoleGitUnstagedPatch,
		PayloadRoleGitUntrackedTar, PayloadRoleGitConflictedWorktreeTar, PayloadRoleGitOperationStateTar:
		var v struct {
			RepositoryID string `json:"repository_id"`
		}
		err = transcriptStrictDecode(data, &v, true, "kind")
		*r = PayloadRole{Kind: kind, RepositoryID: v.RepositoryID}
	case PayloadRoleGitLinkedWorktreeStagedPatch, PayloadRoleGitLinkedWorktreeUnstagedPatch,
		PayloadRoleGitLinkedWorktreeUntrackedTar, PayloadRoleGitLinkedWorktreeConflictedWorktreeTar,
		PayloadRoleGitLinkedWorktreeOperationStateTar:
		var v struct {
			RepositoryID string `json:"repository_id"`
			Worktree     uint32 `json:"worktree"`
		}
		err = transcriptStrictDecode(data, &v, true, "kind")
		*r = PayloadRole{Kind: kind, RepositoryID: v.RepositoryID, Worktree: v.Worktree}
	default:
		return transcriptUnknownVariant(tag, "canonical_session", "native_artifact", "git_bundle",
			"git_staged_patch", "git_unstaged_patch", "git_untracked_tar", "git_conflicted_worktree_tar",
			"git_operation_state_tar", "git_linked_worktree_staged_patch",
			"git_linked_worktree_unstaged_patch", "git_linked_worktree_untracked_tar",
			"git_linked_worktree_conflicted_worktree_tar", "git_linked_worktree_operation_state_tar")
	}
	return err
}

// PayloadPartDescriptor is one byte range of a sharded payload, stored as
// its own ZIP entry.
//
// Parts are contiguous and ordered: part i covers the bytes right after part
// i - 1, and concatenating every part in order reproduces the payload.
type PayloadPartDescriptor struct {
	Path   string `json:"path"`
	Sha256 string `json:"sha256"`
	Size   uint64 `json:"size"`
}

func (d *PayloadPartDescriptor) UnmarshalJSON(data []byte) error {
	type plain PayloadPartDescriptor
	return transcriptStrictDecode(data, (*plain)(d), true)
}

type PayloadDescriptor struct {
	Path   string      `json:"path"`
	Sha256 string      `json:"sha256"`
	Size   uint64      `json:"size"`
	Mode   uint32      `json:"mode"`
	Role   PayloadRole `json:"role"`
	// Parts is empty for a whole payload stored in one ZIP entry. Otherwise
	// the ordered parts the payload was split into; Path then names no ZIP
	// entry of its own and Sha256/Size describe the reassembled payload.
	// The field is absent from schema-2 manifests, so builds that predate
	// sharding also reject it through deny_unknown_fields.
	Parts []PayloadPartDescriptor `json:"parts,omitempty"`
}

func (d *PayloadDescriptor) UnmarshalJSON(data []byte) error {
	type plain PayloadDescriptor
	return transcriptStrictDecode(data, (*plain)(d), true)
}

type ArchiveManifest struct {
	SchemaVersion uint32               `json:"schema_version"`
	Format        string               `json:"format"`
	Session       SessionManifest      `json:"session"`
	Target        TargetManifest       `json:"target"`
	Bundle        BundleManifest       `json:"bundle"`
	Repositories  []RepositoryManifest `json:"repositories"`
	Payloads      []PayloadDescriptor  `json:"payloads"`
}

func (m *ArchiveManifest) UnmarshalJSON(data []byte) error {
	type plain ArchiveManifest
	return transcriptStrictDecode(data, (*plain)(m), true)
}

type NativeArtifact struct {
	RelativePath string
	Data         NativeArtifactData
	Mode         uint32
}

// NativeArtifactData is the one owner for native bytes: generated runtime
// state (NativeArtifactDataBytes) or a disk source (NativeArtifactDataFile).
type NativeArtifactData interface {
	isNativeArtifactData()
}

type NativeArtifactDataBytes []byte

// NativeArtifactDataFile is the path of a file on disk.
type NativeArtifactDataFile string

func (NativeArtifactDataBytes) isNativeArtifactData() {}
func (NativeArtifactDataFile) isNativeArtifactData()  {}

// NativeArtifactDataOpenFile opens path for reading without following a
// final symlink, and refuses anything but a regular file.
func NativeArtifactDataOpenFile(path string) (*os.File, error) {
	panic("unported: mj-core/src/archive.rs NativeArtifactData::open_file")
}

// NativeArtifactDataBytesOf materializes a small provider record when its
// format requires rewriting. Checkpoint capture and archive writing must use
// NativeArtifact.Reader. (Rust NativeArtifactData::bytes.)
func NativeArtifactDataBytesOf(data NativeArtifactData) ([]byte, error) {
	switch data := data.(type) {
	case NativeArtifactDataBytes:
		return data, nil
	case NativeArtifactDataFile:
		return os.ReadFile(string(data))
	}
	return nil, errors.New("native artifact has no data")
}

func (a *NativeArtifact) Size() (uint64, error) {
	panic("unported: mj-core/src/archive.rs NativeArtifact::size")
}

func (a *NativeArtifact) Reader() (io.ReadCloser, error) {
	panic("unported: mj-core/src/archive.rs NativeArtifact::reader")
}

func validateCanonicalSession(snapshot *CanonicalSessionSnapshot) error {
	panic("unported: mj-core/src/archive.rs validate_canonical_session")
}

func validateCanonicalTerminalOutput(output *CanonicalTerminalOutput, stableID string) error {
	if strings.TrimSpace(output.TerminalID) == "" {
		return errors.New("canonical transcript item '" + stableID + "' has terminal output with an empty terminal id")
	}
	return nil
}

func IsLowerHexSha256(value string) bool {
	if len(value) != 64 {
		return false
	}
	for i := 0; i < len(value); i++ {
		b := value[i]
		if !(b >= '0' && b <= '9') && !(b >= 'a' && b <= 'f') {
			return false
		}
	}
	return true
}
