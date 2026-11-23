// Port of mj-client/src/daemon.rs.

// Authenticated local daemon protocol and client transport.

package client

import (
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"hash"
	"io"
	"io/fs"
	"net"
	"net/netip"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"strings"
	"time"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// StoredTranscriptItem is a single-message read. It has a terminal
// unsupported result on older daemons. A sealed interface over
// StoredTranscriptItemFound, StoredTranscriptItemMissing and
// StoredTranscriptItemUnsupported.
type StoredTranscriptItem interface {
	isStoredTranscriptItem()
}

type StoredTranscriptItemFound struct {
	Item *core.TranscriptItem
}

type StoredTranscriptItemMissing struct{}

type StoredTranscriptItemUnsupported struct{}

func (StoredTranscriptItemFound) isStoredTranscriptItem()       {}
func (StoredTranscriptItemMissing) isStoredTranscriptItem()     {}
func (StoredTranscriptItemUnsupported) isStoredTranscriptItem() {}

// StartupPromptOutcome is the durable terminal result for a caller-owned
// startup prompt identity. Internally tagged by "state" in snake_case.
type StartupPromptOutcome interface {
	isStartupPromptOutcome()
}

type StartupPromptOutcomeAccepted struct{}

type StartupPromptOutcomeWithdrawn struct{}

type StartupPromptOutcomeRefused struct {
	EncodedDraft string `json:"encoded_draft"`
	Reason       string `json:"reason"`
}

func (StartupPromptOutcomeAccepted) isStartupPromptOutcome()  {}
func (StartupPromptOutcomeWithdrawn) isStartupPromptOutcome() {}
func (StartupPromptOutcomeRefused) isStartupPromptOutcome()   {}

func (StartupPromptOutcomeAccepted) MarshalJSON() ([]byte, error) {
	return daemonInternalJSON("state", "accepted", nil)
}

func (StartupPromptOutcomeWithdrawn) MarshalJSON() ([]byte, error) {
	return daemonInternalJSON("state", "withdrawn", nil)
}

func (v StartupPromptOutcomeRefused) MarshalJSON() ([]byte, error) {
	type plain StartupPromptOutcomeRefused
	return daemonInternalJSON("state", "refused", plain(v))
}

// UnmarshalStartupPromptOutcomeJSON decodes a StartupPromptOutcome by its
// "state" tag.
func UnmarshalStartupPromptOutcomeJSON(data []byte) (StartupPromptOutcome, error) {
	name, err := daemonSplitInternal(data, "state")
	if err != nil {
		return nil, err
	}
	switch name {
	case "accepted":
		return StartupPromptOutcomeAccepted{}, nil
	case "withdrawn":
		return StartupPromptOutcomeWithdrawn{}, nil
	case "refused":
		var v StartupPromptOutcomeRefused
		if err := daemonDecodeStruct(data, &v, false); err != nil {
			return nil, err
		}
		return v, nil
	}
	return nil, daemonUnknownVariant(name, []string{"accepted", "withdrawn", "refused"})
}

// MetadataPath is where a running daemon publishes its endpoint.
func MetadataPath() string {
	return filepath.Join(core.DataDir(), "daemon.json")
}

// DaemonContainerEnv is the environment variable naming the container a
// daemon was launched in.
const DaemonContainerEnv = core.DaemonContainerEnv

// DaemonContainerRuntimeEnv is the environment variable naming the engine
// that owns the daemon container.
const DaemonContainerRuntimeEnv = "MJ_DAEMON_CONTAINER_RUNTIME"

// DaemonMetadata is the endpoint a daemon publishes in daemon.json.
type DaemonMetadata struct {
	ProtocolVersion uint32         `json:"protocol_version"`
	Pid             uint32         `json:"pid"`
	Address         netip.AddrPort `json:"address"`
	Token           string         `json:"token"`
	StartedAt       string         `json:"started_at"`
	BuildVersion    string         `json:"build_version"`
	// InstanceID is the random per-start identity. It is skipped on disk so
	// older clients keep reading `daemon.json`; clients ask the daemon for it
	// with DaemonActionIdentity.
	InstanceID string `json:"-"`
	// Placement is where this daemon runs. Also skipped on disk: the running
	// daemon is its owner, and a container daemon can report it on every
	// platform.
	Placement DaemonPlacement `json:"-"`
}

func (v DaemonMetadata) MarshalJSON() ([]byte, error) {
	type plain DaemonMetadata
	return daemonMarshalStruct(plain(v))
}

func (v *DaemonMetadata) UnmarshalJSON(data []byte) error {
	type plain DaemonMetadata
	return daemonDecodeStruct(data, (*plain)(v), true)
}

const ContainerOwnerSchema uint32 = 1

// ContainerDaemonRecord is the launch credentials for a container daemon,
// including before it is ready.
type ContainerDaemonRecord struct {
	// Schema defaults to ContainerOwnerSchema when absent.
	Schema          uint32             `json:"schema" serde:"default"`
	ProtocolVersion *uint32            `json:"protocol_version,omitempty"`
	Runtime         core.DaemonRuntime `json:"runtime"`
	ContainerID     string             `json:"container_id"`
	ContainerName   string             `json:"container_name"`
	Address         netip.AddrPort     `json:"address"`
	Token           string             `json:"token"`
	StartedAt       string             `json:"started_at"`
}

func (v *ContainerDaemonRecord) UnmarshalJSON(data []byte) error {
	type plain ContainerDaemonRecord
	decoded := plain{Schema: ContainerOwnerSchema}
	if err := daemonDecodeStruct(data, &decoded, false); err != nil {
		return err
	}
	*v = ContainerDaemonRecord(decoded)
	return nil
}

func (v ContainerDaemonRecord) Matches(metadata *DaemonMetadata) bool {
	return v.Address == metadata.Address && v.Token == metadata.Token
}

func ContainerOwnerPath() string {
	return filepath.Join(core.DataDir(), "daemon-container.json")
}

// ReadContainerOwner is the recorded container daemon, nil when none is.
func ReadContainerOwner() (*ContainerDaemonRecord, error) {
	return readContainerOwnerAt(ContainerOwnerPath())
}

func readContainerOwnerAt(path string) (*ContainerDaemonRecord, error) {
	body, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", path, err)
	}
	var record ContainerDaemonRecord
	if err := json.Unmarshal(body, &record); err != nil {
		return nil, fmt.Errorf("parse %s: %w", path, err)
	}
	return &record, nil
}

func WriteContainerOwner(record *ContainerDaemonRecord) error {
	path := ContainerOwnerPath()
	parent := filepath.Dir(path)
	if parent == path {
		return errors.New("container owner path has no parent")
	}
	if err := core.CreateOwnerOnlyDirectory(parent); err != nil {
		return err
	}
	body, err := daemonMarshalPretty(record)
	if err != nil {
		return err
	}
	return core.AtomicWriteExisting(path, body)
}

// RemoveContainerOwnerIfMatching removes the record of the container daemon
// at address with token, leaving a replacement's record alone. The caller
// serializes owner changes with the daemon startup lock.
func RemoveContainerOwnerIfMatching(address netip.AddrPort, token string) error {
	return removeContainerOwnerAt(ContainerOwnerPath(), address, token)
}

func removeContainerOwnerAt(path string, address netip.AddrPort, token string) error {
	record, err := readContainerOwnerAt(path)
	if err != nil {
		return err
	}
	if record != nil && record.Address == address && record.Token == token {
		if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
			return fmt.Errorf("remove %s: %w", path, err)
		}
	}
	return nil
}

// daemonMarshalPretty encodes v as serde_json::to_vec_pretty does: two-space
// indentation, no HTML escaping, no trailing newline.
func daemonMarshalPretty(v any) ([]byte, error) {
	var buffer strings.Builder
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return []byte(strings.TrimSuffix(buffer.String(), "\n")), nil
}

// DaemonEndpoint is the process-private credentials returned by
// `mj daemon endpoint --json`. This is not local PID metadata and must never
// be written to daemon.json.
type DaemonEndpoint struct {
	ConfigPath      *string        `json:"config_path" serde:"default"`
	ProtocolVersion uint32         `json:"protocol_version"`
	BuildVersion    string         `json:"build_version"`
	ControlAddress  netip.AddrPort `json:"control_address"`
	Token           string         `json:"token"`
	InstanceID      string         `json:"instance_id"`
}

func (v DaemonEndpoint) MarshalJSON() ([]byte, error) {
	type plain DaemonEndpoint
	return daemonMarshalStruct(plain(v))
}

func (v *DaemonEndpoint) UnmarshalJSON(data []byte) error {
	type plain DaemonEndpoint
	return daemonDecodeStruct(data, (*plain)(v), false)
}

// DaemonPlacementKind says where a daemon runs. The zero value is the Rust
// default, DaemonPlacementKindHost.
type DaemonPlacementKind int

const (
	DaemonPlacementKindHost DaemonPlacementKind = iota
	DaemonPlacementKindContainer
)

// DaemonPlacement is where the daemon runs, as the daemon itself reports it.
// Rust's enum (Host, Container{name, runtime}) is a single struct here so
// that the zero value is the default, Host. It encodes internally tagged by
// "placement" in snake_case.
type DaemonPlacement struct {
	Kind DaemonPlacementKind
	// Name is the container name, for a container placement.
	Name string
	// Runtime is the engine that owns the container, when the launcher
	// reported it. Nil is a container daemon from before this action existed.
	Runtime *core.DaemonRuntime
}

// Container is the container name when this placement is a container.
func (p DaemonPlacement) Container() (string, bool) {
	if p.Kind == DaemonPlacementKindContainer {
		return p.Name, true
	}
	return "", false
}

func (p DaemonPlacement) MarshalJSON() ([]byte, error) {
	if p.Kind != DaemonPlacementKindContainer {
		return daemonInternalJSON("placement", "host", nil)
	}
	type container struct {
		Name    string              `json:"name"`
		Runtime *core.DaemonRuntime `json:"runtime"`
	}
	return daemonInternalJSON("placement", "container", container{Name: p.Name, Runtime: p.Runtime})
}

func (p *DaemonPlacement) UnmarshalJSON(data []byte) error {
	name, err := daemonSplitInternal(data, "placement")
	if err != nil {
		return err
	}
	switch name {
	case "host":
		*p = DaemonPlacement{}
		return nil
	case "container":
		var container struct {
			Name    string              `json:"name"`
			Runtime *core.DaemonRuntime `json:"runtime" serde:"default"`
		}
		if err := daemonDecodeStruct(data, &container, false); err != nil {
			return err
		}
		*p = DaemonPlacement{Kind: DaemonPlacementKindContainer, Name: container.Name, Runtime: container.Runtime}
		return nil
	}
	return daemonUnknownVariant(name, []string{"host", "container"})
}

// DaemonIdentity is one daemon start, as the daemon reports it to a client.
//
// Unlike the frozen DaemonStatus, this action is not part of the management
// subset. It answers the questions clients used to ask the operating system:
// is this the daemon I connected to before, where does it run, and which
// build is it.
type DaemonIdentity struct {
	InstanceID   string          `json:"instance_id"`
	BuildVersion string          `json:"build_version"`
	Placement    DaemonPlacement `json:"placement"`
}

func (v DaemonIdentity) MarshalJSON() ([]byte, error) {
	type plain DaemonIdentity
	return daemonMarshalStruct(plain(v))
}

func (v *DaemonIdentity) UnmarshalJSON(data []byte) error {
	type plain DaemonIdentity
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// FirstRunSetupReport is what automatic first-run setup found and configured.
type FirstRunSetupReport struct {
	Agents     []core.HarnessKind `json:"agents"`
	Repository *string            `json:"repository"`
}

func (v FirstRunSetupReport) MarshalJSON() ([]byte, error) {
	type plain FirstRunSetupReport
	return daemonMarshalStruct(plain(v))
}

func (v *FirstRunSetupReport) UnmarshalJSON(data []byte) error {
	type plain FirstRunSetupReport
	return daemonDecodeStruct(data, (*plain)(v), true)
}

func (r FirstRunSetupReport) Summary() []string {
	var lines []string
	if len(r.Agents) == 0 {
		lines = append(lines, "No coding agent found yet. Mjolnir can install one and sign you in.")
	} else {
		names := make([]string, 0, len(r.Agents))
		for _, kind := range r.Agents {
			names = append(names, kind.DisplayName())
		}
		lines = append(lines, fmt.Sprintf("Agents: %s.", strings.Join(names, ", ")))
	}
	if r.Repository != nil {
		lines = append(lines, fmt.Sprintf("Project: %s.", *r.Repository))
	}
	return lines
}

// ImportedBundle is a project synthesized while importing a native session.
type ImportedBundle struct {
	BundleID string             `json:"bundle_id"`
	Bundle   core.ProjectBundle `json:"bundle"`
}

func (v ImportedBundle) MarshalJSON() ([]byte, error) {
	type plain ImportedBundle
	return daemonMarshalStruct(plain(v))
}

func (v *ImportedBundle) UnmarshalJSON(data []byte) error {
	type plain ImportedBundle
	return daemonDecodeStruct(data, (*plain)(v), true)
}

type WorkspaceListing struct {
	Workspace core.WorkspaceRecord `json:"workspace"`
}

func (v WorkspaceListing) MarshalJSON() ([]byte, error) {
	type plain WorkspaceListing
	return daemonMarshalStruct(plain(v))
}

func (v *WorkspaceListing) UnmarshalJSON(data []byte) error {
	type plain WorkspaceListing
	return daemonDecodeStruct(data, (*plain)(v), true)
}

type SessionPreview struct {
	ID        string `json:"id"`
	Title     string `json:"title"`
	Project   string `json:"project"`
	Harness   string `json:"harness"`
	State     string `json:"state"`
	Active    bool   `json:"active"`
	UpdatedAt string `json:"updated_at"`
}

func (v SessionPreview) MarshalJSON() ([]byte, error) {
	type plain SessionPreview
	return daemonMarshalStruct(plain(v))
}

func (v *SessionPreview) UnmarshalJSON(data []byte) error {
	type plain SessionPreview
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// WikiRow is one session in the user's SessionWiki index, as a control
// surface shows it.
//
// It is the daemon's own shape rather than SessionWiki's row: it carries the
// search snippet that found the row and, for a Mjolnir session this daemon
// still holds, the id that resumes it instead of restoring it.
type WikiRow struct {
	ID      string  `json:"id"`
	Tool    string  `json:"tool"`
	Project string  `json:"project"`
	Title   string  `json:"title"`
	Started *string `json:"started"`
	Msgs    int64   `json:"msgs"`
	Preview *string `json:"preview"`
	// Archived: the tool deleted its own copy and SessionWiki kept the
	// transcript.
	Archived bool `json:"archived"`
	// NativeID is the session id the tool that ran it knows it by, when its
	// stored path carries one. It is what matches a row against an import
	// scan.
	NativeID *string `json:"native_id"`
	// Snippet is the matching text, when this row came from a search.
	Snippet *string `json:"snippet"`
	// HelSessionID is the live Mjolnir session this row describes, when this
	// daemon has it.
	HelSessionID *string `json:"hel_session_id"`
	// Target is the Mjolnir target template the session ran under, when the
	// index carries it. Only Mjolnir's own rows have one: the daemon writes it
	// into the index as an `mj-target:` tag while the session is still known,
	// so an archived row can still say where it ran.
	Target *string `json:"target" serde:"default"`
	// Profile is the Mjolnir harness profile the session last ran under, from
	// the index's `mj-profile:` tag. Only Mjolnir's own rows have one.
	Profile *string `json:"profile" serde:"default"`
	// Harness is the harness kind the session ran (`codex`, `claude`, `kimi`,
	// `grok`, `muse`, `opencode`, or `dsh`), from the index's `mj-harness:`
	// tag. It stays meaningful after the profile id has been removed from the
	// configuration.
	Harness *string `json:"harness" serde:"default"`
}

func (v WikiRow) MarshalJSON() ([]byte, error) {
	type plain WikiRow
	return daemonMarshalStruct(plain(v))
}

func (v *WikiRow) UnmarshalJSON(data []byte) error {
	type plain WikiRow
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// WikiIndexState is how far along the daemon's SessionWiki index is when a
// search answers. The zero value is the Rust default, WikiIndexStateIndexing.
type WikiIndexState int

const (
	// WikiIndexStateIndexing: the first full build has not finished yet, so
	// a search can miss sessions that exist. This is the state a fresh index
	// starts in.
	WikiIndexStateIndexing WikiIndexState = iota
	// WikiIndexStateReady: the index has completed one full build; its
	// answers are complete.
	WikiIndexStateReady
	// WikiIndexStateVersionMismatch: the index file on disk was written by a
	// newer SessionWiki schema version. Mjolnir will not open it, because
	// rebuilding it down would only make the newer program rebuild it back.
	// An older index is not this state: the daemon rebuilds it, and it reads
	// as Indexing meanwhile.
	WikiIndexStateVersionMismatch
)

func (s WikiIndexState) MarshalText() ([]byte, error) {
	switch s {
	case WikiIndexStateReady:
		return []byte("ready"), nil
	case WikiIndexStateIndexing:
		return []byte("indexing"), nil
	case WikiIndexStateVersionMismatch:
		return []byte("version_mismatch"), nil
	}
	return nil, fmt.Errorf("invalid WikiIndexState %d", int(s))
}

func (s *WikiIndexState) UnmarshalText(text []byte) error {
	switch string(text) {
	case "ready":
		*s = WikiIndexStateReady
	case "indexing":
		*s = WikiIndexStateIndexing
	case "version_mismatch":
		*s = WikiIndexStateVersionMismatch
	default:
		return daemonUnknownVariant(string(text), []string{"ready", "indexing", "version_mismatch"})
	}
	return nil
}

// WikiStatus is what a search says about the index it answered from.
type WikiStatus struct {
	State WikiIndexState `json:"state"`
}

func (v WikiStatus) MarshalJSON() ([]byte, error) {
	type plain WikiStatus
	return daemonMarshalStruct(plain(v))
}

func (v *WikiStatus) UnmarshalJSON(data []byte) error {
	type plain WikiStatus
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// WikiSyncProgress is progress within one adapter of the current sync phase.
type WikiSyncProgress struct {
	Phase    string `json:"phase"`
	Tool     string `json:"tool"`
	Adapter  int    `json:"adapter"`
	Adapters int    `json:"adapters"`
	Done     int    `json:"done"`
	Total    *int   `json:"total"`
}

func (v WikiSyncProgress) MarshalJSON() ([]byte, error) {
	type plain WikiSyncProgress
	return daemonMarshalStruct(plain(v))
}

func (v *WikiSyncProgress) UnmarshalJSON(data []byte) error {
	type plain WikiSyncProgress
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// WikiSyncOutcome is how a SessionWiki sync ended. Externally tagged in
// snake_case: "succeeded" or {"failed": "<reason>"}.
type WikiSyncOutcome interface {
	isWikiSyncOutcome()
}

type WikiSyncOutcomeSucceeded struct{}

type WikiSyncOutcomeFailed struct {
	Reason string
}

func (WikiSyncOutcomeSucceeded) isWikiSyncOutcome() {}
func (WikiSyncOutcomeFailed) isWikiSyncOutcome()    {}

func (WikiSyncOutcomeSucceeded) MarshalJSON() ([]byte, error) {
	return daemonExternalJSON("succeeded", nil)
}

func (v WikiSyncOutcomeFailed) MarshalJSON() ([]byte, error) {
	body, err := daemonMarshalJSON(v.Reason)
	if err != nil {
		return nil, err
	}
	return daemonExternalJSON("failed", body)
}

// UnmarshalWikiSyncOutcomeJSON decodes serde's externally tagged form.
func UnmarshalWikiSyncOutcomeJSON(data []byte) (WikiSyncOutcome, error) {
	name, body, err := daemonSplitExternal(data)
	if err != nil {
		return nil, err
	}
	switch name {
	case "succeeded":
		return WikiSyncOutcomeSucceeded{}, nil
	case "failed":
		var v WikiSyncOutcomeFailed
		if err := daemonDecodeValue(body, &v.Reason); err != nil {
			return nil, err
		}
		return v, nil
	}
	return nil, daemonUnknownVariant(name, []string{"succeeded", "failed"})
}

// WikiSyncStatus is one ordered snapshot from the indexer's generation owner.
type WikiSyncStatus struct {
	Epoch      string `json:"epoch"`
	Version    uint64 `json:"version"`
	Requested  uint64 `json:"requested"`
	Started    uint64 `json:"started"`
	Finished   uint64 `json:"finished"`
	Successful uint64 `json:"successful"`
	// Outcome is nil for none (Rust's Option).
	Outcome WikiSyncOutcome   `json:"outcome" serde:"default"`
	Running *WikiSyncProgress `json:"running"`
	Target  *uint64           `json:"target"`
}

func (v WikiSyncStatus) MarshalJSON() ([]byte, error) {
	type plain WikiSyncStatus
	return daemonMarshalStruct(plain(v))
}

func (v *WikiSyncStatus) UnmarshalJSON(data []byte) error {
	type plain WikiSyncStatus
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// SessionTextMatchKind is where in a session's conversation a text search
// matched. A match in a user message ranks ahead of one in an agent message
// (it orders first); a match only in tool output is not a match at all.
type SessionTextMatchKind int

const (
	SessionTextMatchKindUser SessionTextMatchKind = iota
	SessionTextMatchKindAgent
)

func (k SessionTextMatchKind) MarshalText() ([]byte, error) {
	switch k {
	case SessionTextMatchKindUser:
		return []byte("user"), nil
	case SessionTextMatchKindAgent:
		return []byte("agent"), nil
	}
	return nil, fmt.Errorf("invalid SessionTextMatchKind %d", int(k))
}

func (k *SessionTextMatchKind) UnmarshalText(text []byte) error {
	switch string(text) {
	case "user":
		*k = SessionTextMatchKindUser
	case "agent":
		*k = SessionTextMatchKindAgent
	default:
		return daemonUnknownVariant(string(text), []string{"user", "agent"})
	}
	return nil
}

// SessionTextMatch is one live session whose conversation contains a search
// query.
type SessionTextMatch struct {
	SessionID string               `json:"session_id"`
	Kind      SessionTextMatchKind `json:"kind"`
}

func (v SessionTextMatch) MarshalJSON() ([]byte, error) {
	type plain SessionTextMatch
	return daemonMarshalStruct(plain(v))
}

func (v *SessionTextMatch) UnmarshalJSON(data []byte) error {
	type plain SessionTextMatch
	return daemonDecodeStruct(data, (*plain)(v), true)
}

type SessionTextSearchPage struct {
	Matches []SessionTextMatch `json:"matches"`
}

func (v SessionTextSearchPage) MarshalJSON() ([]byte, error) {
	type plain SessionTextSearchPage
	return daemonMarshalStruct(plain(v))
}

func (v *SessionTextSearchPage) UnmarshalJSON(data []byte) error {
	type plain SessionTextSearchPage
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// WikiSearchPage is one page of search results with the state of the index
// behind them.
type WikiSearchPage struct {
	Rows   []WikiRow  `json:"rows"`
	Status WikiStatus `json:"status"`
}

func (v WikiSearchPage) MarshalJSON() ([]byte, error) {
	type plain WikiSearchPage
	return daemonMarshalStruct(plain(v))
}

func (v *WikiSearchPage) UnmarshalJSON(data []byte) error {
	type plain WikiSearchPage
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// WikiHitBlock is one message of an indexed transcript, reduced to what a
// search preview shows: the text around the query's matches, with the
// matches located in it.
type WikiHitBlock struct {
	// Role is `user`, `assistant` or `tool`.
	Role string `json:"role"`
	// Text is the message text, redacted, and windowed to the caller's
	// per-message budget when the message is longer than that.
	Text string `json:"text"`
	// Hits are byte ranges of the matches inside Text, on character
	// boundaries, in order. A context message has none.
	Hits [][2]int `json:"hits"`
	// OmittedBefore counts messages between the previous block and this one
	// that no group covered. Non-zero only on the first block of a group.
	OmittedBefore int `json:"omitted_before"`
	// Truncated: Text is a window of the message rather than the whole of it.
	Truncated bool `json:"truncated"`
}

func (v WikiHitBlock) MarshalJSON() ([]byte, error) {
	type plain WikiHitBlock
	return daemonMarshalStruct(plain(v))
}

func (v *WikiHitBlock) UnmarshalJSON(data []byte) error {
	type plain WikiHitBlock
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// WikiHitTranscript is the matching passages of one indexed transcript.
type WikiHitTranscript struct {
	Blocks []WikiHitBlock `json:"blocks"`
	// OmittedAfter counts messages after the last block that no group
	// covered.
	OmittedAfter int `json:"omitted_after"`
}

func (v WikiHitTranscript) MarshalJSON() ([]byte, error) {
	type plain WikiHitTranscript
	return daemonMarshalStruct(plain(v))
}

func (v *WikiHitTranscript) UnmarshalJSON(data []byte) error {
	type plain WikiHitTranscript
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// WikiSessionStatus is what one indexed session is, as far as continuing it
// is concerned.
type WikiSessionStatus string

const (
	// WikiSessionStatusMine: a Mjolnir session this daemon still has a
	// record of.
	WikiSessionStatusMine WikiSessionStatus = "mine"
	// WikiSessionStatusArchived: a Mjolnir session whose record the archive
	// job destroyed; only the indexed transcript is left.
	WikiSessionStatusArchived WikiSessionStatus = "archived"
	// WikiSessionStatusNative: another tool's session, which Mjolnir would
	// have to import.
	WikiSessionStatusNative WikiSessionStatus = "native"
)

func (s *WikiSessionStatus) UnmarshalText(text []byte) error {
	switch value := WikiSessionStatus(text); value {
	case WikiSessionStatusMine, WikiSessionStatusArchived, WikiSessionStatusNative:
		*s = value
		return nil
	}
	return daemonUnknownVariant(string(text), []string{"mine", "archived", "native"})
}

// WikiSessionInfo is one row of the SessionWiki index, with what Mjolnir
// knows about it.
//
// This is what `mj resume --wiki` branches on and what `mj sessions
// --session` reports when the id names no Mjolnir session.
type WikiSessionInfo struct {
	// WikiID is the SessionWiki id of the row.
	WikiID string `json:"wiki_id"`
	// Tool is the tool that produced the session, as SessionWiki names it.
	Tool string `json:"tool"`
	// Path is the transcript path the index stores for the row.
	Path   string            `json:"path"`
	Status WikiSessionStatus `json:"status"`
	// MjolnirSessionID is the Mjolnir session id, for a row Mjolnir itself
	// published.
	MjolnirSessionID *string `json:"mjolnir_session_id"`
	// ProfileID is the profile the session last ran under, from the index's
	// own tags.
	ProfileID *string `json:"profile_id"`
	// TargetTemplateID is the target template the session ran on, from the
	// index's own tags.
	TargetTemplateID *string `json:"target_template_id"`
	// Harness is the harness that drove the session, from the tags for a
	// Mjolnir row and from the tool name for a native one.
	Harness *core.HarnessKind `json:"harness"`
	Title   string            `json:"title"`
	Project string            `json:"project"`
	// NothingToRestore is set for an archived Mjolnir row whose transcript
	// holds no prompt, which `mj resume --wiki` has nothing to restore from.
	// Sent only when set, so a daemon that predates it and a client that
	// predates it both read every other row unchanged.
	NothingToRestore bool `json:"nothing_to_restore,omitempty" serde:"default"`
}

func (v WikiSessionInfo) MarshalJSON() ([]byte, error) {
	type plain WikiSessionInfo
	return daemonMarshalStruct(plain(v))
}

func (v *WikiSessionInfo) UnmarshalJSON(data []byte) error {
	type plain WikiSessionInfo
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// WikiRestoreRequest starts a new session carrying a compacted hand-off from
// an archived one.
type WikiRestoreRequest struct {
	// WikiID is the SessionWiki session id to restore from.
	WikiID           string `json:"wiki_id"`
	WorkspaceID      string `json:"workspace_id"`
	ProfileID        string `json:"profile_id"`
	TargetTemplateID string `json:"target_template_id"`
	// ProjectDirectory is where the new session opens. Nil takes the project
	// the archived session ran in, when that directory still exists.
	ProjectDirectory *string                `json:"project_directory" serde:"default"`
	AdditionalMounts []core.AdditionalMount `json:"additional_mounts" serde:"default"`
	// ResourceAllocation is nil for none (Rust's Option).
	ResourceAllocation core.SessionResourceAllocation `json:"resource_allocation" serde:"default"`
}

func (v WikiRestoreRequest) MarshalJSON() ([]byte, error) {
	type plain WikiRestoreRequest
	return daemonMarshalStruct(plain(v))
}

func (v *WikiRestoreRequest) UnmarshalJSON(data []byte) error {
	type plain WikiRestoreRequest
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// DashboardStartup is what the terminal dashboard reads from the store before
// it takes the terminal. The daemon assembles it, so a client never opens the
// store, which may live in a container volume the client cannot reach.
type DashboardStartup struct {
	// Runtime is the snapshot and cursor from which the runtime feed
	// continues.
	Runtime             RuntimeBootstrap                   `json:"runtime"`
	Metadata            StoredSessionMetadata              `json:"metadata"`
	PaneSizes           map[string]core.PaneSizes          `json:"pane_sizes"`
	ConversationLayouts map[string]core.ConversationLayout `json:"conversation_layouts"`
	QueuedPrompts       map[string][]core.QueuedPrompt     `json:"queued_prompts"`
}

func (v DashboardStartup) MarshalJSON() ([]byte, error) {
	type plain DashboardStartup
	return daemonMarshalStruct(plain(v))
}

func (v *DashboardStartup) UnmarshalJSON(data []byte) error {
	type plain DashboardStartup
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// WorkspaceLayouts is the stored per-workspace layouts, keyed by workspace id.
type WorkspaceLayouts struct {
	PaneSizes           map[string]core.PaneSizes          `json:"pane_sizes"`
	ConversationLayouts map[string]core.ConversationLayout `json:"conversation_layouts"`
}

func (v WorkspaceLayouts) MarshalJSON() ([]byte, error) {
	type plain WorkspaceLayouts
	return daemonMarshalStruct(plain(v))
}

func (v *WorkspaceLayouts) UnmarshalJSON(data []byte) error {
	type plain WorkspaceLayouts
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// StoredSessionMetadata is the store-wide metadata the dashboard keeps beside
// its session rows.
type StoredSessionMetadata struct {
	MountHistory   core.SnapshotMap[string, []string]               `json:"mount_history"`
	ContainerSizes core.SnapshotMap[string, core.HostContainerSize] `json:"container_sizes"`
}

func (v StoredSessionMetadata) MarshalJSON() ([]byte, error) {
	type plain StoredSessionMetadata
	return daemonMarshalStruct(plain(v))
}

func (v *StoredSessionMetadata) UnmarshalJSON(data []byte) error {
	type plain StoredSessionMetadata
	return daemonDecodeStruct(data, (*plain)(v), true)
}

type WorkspaceSnapshot struct {
	Workspace core.WorkspaceRecord `json:"workspace"`
	Sessions  []SessionPreview     `json:"sessions"`
	Drafts    []DraftPreview       `json:"drafts"`
}

func (v WorkspaceSnapshot) MarshalJSON() ([]byte, error) {
	type plain WorkspaceSnapshot
	return daemonMarshalStruct(plain(v))
}

func (v *WorkspaceSnapshot) UnmarshalJSON(data []byte) error {
	type plain WorkspaceSnapshot
	return daemonDecodeStruct(data, (*plain)(v), true)
}

type RuntimeSessionView struct {
	// Materialized is the full queue and form bodies, for in-process
	// viewers. Terminal clients receive them through their transcript tails;
	// never serialized.
	Materialized *core.SharedSnapshot[RuntimeMaterializedSession] `json:"-"`
	// MaterializedOnly marks a retained projection for a session with no
	// relay view. Never serialized.
	MaterializedOnly bool                  `json:"-"`
	Attention        core.SessionAttention `json:"attention"`
	// PolicyConfigKeys are config selectors owned by the target permission
	// policy.
	PolicyConfigKeys           []string                                         `json:"policy_config_keys" serde:"default"`
	SessionID                  string                                           `json:"session_id"`
	ProjectionOrdinal          uint64                                           `json:"projection_ordinal"`
	ProjectionDigest           string                                           `json:"projection_digest"`
	Operational                *core.SharedSnapshot[core.RelayOperationalState] `json:"operational"`
	LatestCredentialSyncSignal *core.CredentialSyncSignal                       `json:"latest_credential_sync_signal"`
	Connected                  bool                                             `json:"connected"`
	Error                      *ViewError                                       `json:"error"`
}

func (v RuntimeSessionView) MarshalJSON() ([]byte, error) {
	type plain RuntimeSessionView
	return daemonMarshalStruct(plain(v))
}

func (v *RuntimeSessionView) UnmarshalJSON(data []byte) error {
	type plain RuntimeSessionView
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// RuntimeMaterializedSession is the part of a session's projection an
// in-process viewer reads from the runtime feed.
type RuntimeMaterializedSession struct {
	Execution           core.MaterializedExecutionState
	LastActivityAtMs    *int64
	QueuedPrompts       []core.QueuedPrompt
	PendingElicitations []core.ElicitationRequest
}

// RuntimeSessionViewFromManaged is the runtime feed's view of a managed
// session.
func RuntimeSessionViewFromManaged(sessionID string, view ManagedSessionView) RuntimeSessionView {
	result := RuntimeSessionView{
		SessionID:        sessionID,
		Attention:        view.Attention,
		PolicyConfigKeys: []string{},
		Connected:        view.Connected,
		Error:            view.Error,
	}
	if snapshot := view.Snapshot; snapshot != nil {
		operational := snapshot.Operational
		result.ProjectionOrdinal = snapshot.Materialized.AppliedEventOrdinal
		result.ProjectionDigest = snapshot.Materialized.AppliedEventDigest
		result.Operational = &operational
		result.LatestCredentialSyncSignal = snapshot.LatestCredentialSyncSignal
	}
	return result
}

// RuntimeNotice is something the daemon did on its own that a surface should
// report once.
//
// Background work has no lifecycle entry to hang a message on, so notices
// travel with the snapshot and carry an id: a surface reports the ones newer
// than the last it saw and nothing else, however often it polls.
type RuntimeNotice struct {
	ID        uint64 `json:"id"`
	SessionID string `json:"session_id"`
	Text      string `json:"text"`
}

func (v RuntimeNotice) MarshalJSON() ([]byte, error) {
	type plain RuntimeNotice
	return daemonMarshalStruct(plain(v))
}

func (v *RuntimeNotice) UnmarshalJSON(data []byte) error {
	type plain RuntimeNotice
	return daemonDecodeStruct(data, (*plain)(v), true)
}

type RuntimeLifecycleKind string

const (
	RuntimeLifecycleKindCreate         RuntimeLifecycleKind = "create"
	RuntimeLifecycleKindSuspend        RuntimeLifecycleKind = "suspend"
	RuntimeLifecycleKindResume         RuntimeLifecycleKind = "resume"
	RuntimeLifecycleKindMove           RuntimeLifecycleKind = "move"
	RuntimeLifecycleKindDestroyStopped RuntimeLifecycleKind = "destroy_stopped"
	RuntimeLifecycleKindForceDestroy   RuntimeLifecycleKind = "force_destroy"
	// RuntimeLifecycleKindStopSubagent is a sub-agent stopped and removed
	// because its parent is being suspended. The same teardown as
	// ForceDestroy, which surfaces call a stop, as the suspend does.
	RuntimeLifecycleKindStopSubagent RuntimeLifecycleKind = "stop_subagent"
	RuntimeLifecycleKindCleanup      RuntimeLifecycleKind = "cleanup"
)

func (k *RuntimeLifecycleKind) UnmarshalText(text []byte) error {
	switch value := RuntimeLifecycleKind(text); value {
	case RuntimeLifecycleKindCreate, RuntimeLifecycleKindSuspend, RuntimeLifecycleKindResume,
		RuntimeLifecycleKindMove, RuntimeLifecycleKindDestroyStopped,
		RuntimeLifecycleKindForceDestroy, RuntimeLifecycleKindStopSubagent, RuntimeLifecycleKindCleanup:
		*k = value
		return nil
	}
	return daemonUnknownVariant(string(text), []string{"create", "suspend", "resume", "move",
		"destroy_stopped", "force_destroy", "stop_subagent", "cleanup"})
}

type RuntimeLifecycleView struct {
	OperationID           string                                    `json:"operation_id"`
	MoveOperationID       *string                                   `json:"move_operation_id" serde:"default"`
	Cancellable           bool                                      `json:"cancellable"`
	SessionID             string                                    `json:"session_id"`
	Kind                  RuntimeLifecycleKind                      `json:"kind"`
	StartedAtEpochSeconds uint64                                    `json:"started_at_epoch_seconds"`
	ActiveStages          []DaemonPair[core.ProvisionStage, uint64] `json:"active_stages"`
	ResumeDestination     *[2]string                                `json:"resume_destination"`
	Notice                *string                                   `json:"notice"`
}

func (v RuntimeLifecycleView) MarshalJSON() ([]byte, error) {
	type plain RuntimeLifecycleView
	return daemonMarshalStruct(plain(v))
}

func (v *RuntimeLifecycleView) UnmarshalJSON(data []byte) error {
	type plain RuntimeLifecycleView
	return daemonDecodeStruct(data, (*plain)(v), true)
}

type ResumeSessionRequest struct {
	SessionID        string                  `json:"session_id"`
	WorkspaceID      string                  `json:"workspace_id"`
	ProfileID        string                  `json:"profile_id"`
	TargetTemplateID string                  `json:"target_template_id"`
	AdditionalMounts *[]core.AdditionalMount `json:"additional_mounts"`
	// ResourceAllocation is nil for none (Rust's Option).
	ResourceAllocation  core.SessionResourceAllocation      `json:"resource_allocation" serde:"default"`
	DiscardQueue        bool                                `json:"discard_queue"`
	RepositoryPreflight *core.ResumeRepositorySourceReceipt `json:"repository_preflight"`
}

func (v ResumeSessionRequest) MarshalJSON() ([]byte, error) {
	type plain ResumeSessionRequest
	return daemonMarshalStruct(plain(v))
}

func (v *ResumeSessionRequest) UnmarshalJSON(data []byte) error {
	type plain ResumeSessionRequest
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// SessionLifecycleRequest is a lifecycle mutation whose execution belongs to
// the daemon after acceptance. Adjacently tagged by "operation" with its
// fields under "options", in snake_case.
type SessionLifecycleRequest interface {
	isSessionLifecycleRequest()
}

type SessionLifecycleRequestSuspend struct {
	SessionID           string                `json:"session_id"`
	DiscardUntracked    core.UntrackedConsent `json:"discard_untracked" serde:"default"`
	AbortOperations     core.OperationConsent `json:"abort_operations" serde:"default"`
	WithoutCheckpoint   bool                  `json:"without_checkpoint" serde:"default"`
	ConfirmNoCheckpoint bool                  `json:"confirm_no_checkpoint,omitempty"`
}

type SessionLifecycleRequestRestart struct {
	SessionID                      string `json:"session_id"`
	AcknowledgeContainerRecreation bool   `json:"acknowledge_container_recreation"`
}

type SessionLifecycleRequestResume struct {
	Request ResumeSessionRequest
}

type SessionLifecycleRequestMove struct {
	Request core.MoveSessionRequest
}

type SessionLifecycleRequestDestroyStopped struct {
	SessionID string `json:"session_id"`
}

type SessionLifecycleRequestForceDestroy struct {
	SessionID string `json:"session_id"`
}

func (SessionLifecycleRequestSuspend) isSessionLifecycleRequest()        {}
func (SessionLifecycleRequestRestart) isSessionLifecycleRequest()        {}
func (SessionLifecycleRequestResume) isSessionLifecycleRequest()         {}
func (SessionLifecycleRequestMove) isSessionLifecycleRequest()           {}
func (SessionLifecycleRequestDestroyStopped) isSessionLifecycleRequest() {}
func (SessionLifecycleRequestForceDestroy) isSessionLifecycleRequest()   {}

func (v SessionLifecycleRequestSuspend) MarshalJSON() ([]byte, error) {
	type plain SessionLifecycleRequestSuspend
	return daemonAdjacentStruct("operation", "options", "suspend", plain(v))
}

func (v SessionLifecycleRequestRestart) MarshalJSON() ([]byte, error) {
	type plain SessionLifecycleRequestRestart
	return daemonAdjacentStruct("operation", "options", "restart", plain(v))
}

func (v SessionLifecycleRequestResume) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("operation", "options", "resume", v.Request)
}

func (v SessionLifecycleRequestMove) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("operation", "options", "move", v.Request)
}

func (v SessionLifecycleRequestDestroyStopped) MarshalJSON() ([]byte, error) {
	type plain SessionLifecycleRequestDestroyStopped
	return daemonAdjacentStruct("operation", "options", "destroy_stopped", plain(v))
}

func (v SessionLifecycleRequestForceDestroy) MarshalJSON() ([]byte, error) {
	type plain SessionLifecycleRequestForceDestroy
	return daemonAdjacentStruct("operation", "options", "force_destroy", plain(v))
}

// UnmarshalSessionLifecycleRequestJSON decodes a SessionLifecycleRequest by
// its "operation" tag.
func UnmarshalSessionLifecycleRequestJSON(data []byte) (SessionLifecycleRequest, error) {
	name, body, err := daemonSplitAdjacent(data, "operation", "options")
	if err != nil {
		return nil, err
	}
	switch name {
	case "suspend":
		return daemonStructVariant[SessionLifecycleRequestSuspend, SessionLifecycleRequest](body, "options")
	case "restart":
		return daemonStructVariant[SessionLifecycleRequestRestart, SessionLifecycleRequest](body, "options")
	case "resume":
		var v SessionLifecycleRequestResume
		if err := daemonDecodeRequired(body, "options", &v.Request); err != nil {
			return nil, err
		}
		return v, nil
	case "move":
		var v SessionLifecycleRequestMove
		if err := daemonDecodeRequired(body, "options", &v.Request); err != nil {
			return nil, err
		}
		return v, nil
	case "destroy_stopped":
		return daemonStructVariant[SessionLifecycleRequestDestroyStopped, SessionLifecycleRequest](body, "options")
	case "force_destroy":
		return daemonStructVariant[SessionLifecycleRequestForceDestroy, SessionLifecycleRequest](body, "options")
	}
	return nil, daemonUnknownVariant(name, []string{"suspend", "restart", "resume", "move",
		"destroy_stopped", "force_destroy"})
}

// SessionLifecycleRequestSessionID is the session the request acts on.
func SessionLifecycleRequestSessionID(request SessionLifecycleRequest) string {
	switch v := request.(type) {
	case SessionLifecycleRequestSuspend:
		return v.SessionID
	case SessionLifecycleRequestRestart:
		return v.SessionID
	case SessionLifecycleRequestDestroyStopped:
		return v.SessionID
	case SessionLifecycleRequestForceDestroy:
		return v.SessionID
	case SessionLifecycleRequestResume:
		return v.Request.SessionID
	case SessionLifecycleRequestMove:
		return v.Request.Preparation.Selection.SessionID
	}
	panic(fmt.Sprintf("unknown SessionLifecycleRequest %T", request))
}

// LifecycleAdmissionUnconfirmed: a lost acceptance reply never proves that
// the daemon refused this mutation.
type LifecycleAdmissionUnconfirmed struct {
	SessionID string
}

func (e LifecycleAdmissionUnconfirmed) Error() string {
	return fmt.Sprintf("Could not confirm daemon acceptance for session %s; inspect session status before retrying", e.SessionID)
}

// LifecycleObservationUnavailable: observation was lost; this says nothing
// about the mutation's outcome.
type LifecycleObservationUnavailable struct {
	Receipt LifecycleReceipt
}

func (e LifecycleObservationUnavailable) Error() string {
	return fmt.Sprintf("Observation of operation %s for session %s is unavailable; inspect session status for its current state",
		e.Receipt.OperationID, e.Receipt.SessionID)
}

// LifecycleReceipt identifies the admitted operation, independently of later
// work on its session.
type LifecycleReceipt struct {
	SessionID   string `json:"session_id"`
	OperationID string `json:"operation_id"`
}

func (v LifecycleReceipt) MarshalJSON() ([]byte, error) {
	type plain LifecycleReceipt
	return daemonMarshalStruct(plain(v))
}

func (v *LifecycleReceipt) UnmarshalJSON(data []byte) error {
	type plain LifecycleReceipt
	return daemonDecodeStruct(data, (*plain)(v), false)
}

type CreateSessionRequest struct {
	// RememberLaunchChoices allows this interactive launch to seed later
	// new-session forms.
	RememberLaunchChoices bool `json:"remember_launch_choices,omitempty" serde:"default"`
	// RememberContainerSize saves this session's container size as the
	// default for its physical host. Independent of the broader interactive
	// launch preferences.
	RememberContainerSize bool  `json:"remember_container_size,omitempty" serde:"default"`
	CreateManagedWorktree *bool `json:"create_managed_worktree" serde:"default"`
	// At is the full commit object ID to start the workspace at: an exact
	// checkout of the bundle's primary repository, detached unless Branch is
	// given.
	At *string `json:"at,omitempty" serde:"default"`
	// Branch: with At, the new branch created there; otherwise the existing
	// branch to check out in a new isolated workspace.
	Branch *string `json:"branch,omitempty" serde:"default"`
	// Base is the diff base, when it is not At. Without At, also the starting
	// revision for a raw managed worktree.
	Base *string `json:"base,omitempty" serde:"default"`
	// NoLaterHistory: with At, start the workspace with At, Base and their
	// ancestors only: no `origin` remote, remote-tracking refs, tags, or
	// clone-cache objects. Base must then be a full commit object ID, and the
	// bundle must hold one repository. The session cannot be resumed or
	// moved.
	NoLaterHistory bool `json:"no_later_history,omitempty" serde:"default"`
	// Subagents: omitted uses the selected profile's subagent setting. Also
	// read under its old name, `mjolnir_subagents`, through
	// core.DeserializeOptionalPolicy.
	Subagents    *core.SubagentPolicy `json:"subagents" serde:"default"`
	Oracle       *bool                `json:"oracle,omitempty" serde:"default"`
	OracleChoice *core.SubagentChoice `json:"oracle_choice,omitempty" serde:"default"`
	// Review: omitted (nil) follows `[review]`.
	Review core.SessionReview `json:"review,omitempty" serde:"default"`
	// NoProjectMemory starts the session with no project memory. See
	// core.SessionRecord.NoProjectMemory.
	NoProjectMemory bool `json:"no_project_memory,omitempty" serde:"default"`
	// NoGithubEvents excludes this session from GitHub watcher delivery
	// targets. See core.SessionRecord.NoGithubEvents.
	NoGithubEvents bool `json:"no_github_events,omitempty" serde:"default"`
	// NoGithubCredentials withholds GitHub credentials; inherited by
	// children.
	NoGithubCredentials bool `json:"no_github_credentials,omitempty" serde:"default"`
	// NotificationsIgnored starts with notifications ignored; see
	// core.SessionRecord.NotificationsIgnored.
	NotificationsIgnored bool    `json:"notifications_ignored,omitempty" serde:"default"`
	InitialPrompt        *string `json:"initial_prompt" serde:"default"`
	WorkspaceID          string  `json:"workspace_id"`
	ProfileID            string  `json:"profile_id"`
	// Provider: when ProfileID is empty and this is set, load-balance across
	// enabled profiles belonging to this provider instead of
	// `[[subagents.primary]]`. Nil is none.
	Provider         core.QuotaProvider     `json:"provider,omitempty"`
	BundleID         string                 `json:"bundle_id"`
	ProjectDirectory *string                `json:"project_directory"`
	TargetTemplateID string                 `json:"target_template_id"`
	AdditionalMounts []core.AdditionalMount `json:"additional_mounts"`
	// ResourceAllocation is nil for none (Rust's Option).
	ResourceAllocation   core.SessionResourceAllocation `json:"resource_allocation" serde:"default"`
	Title                string                         `json:"title"`
	SessionTitleOverride *string                        `json:"session_title_override"`
}

func (v CreateSessionRequest) MarshalJSON() ([]byte, error) {
	type plain CreateSessionRequest
	return daemonMarshalStruct(plain(v))
}

// UnmarshalJSON reads the request as serde does, including the
// `mjolnir_subagents` alias and core.DeserializeOptionalPolicy for
// Subagents.
func (v *CreateSessionRequest) UnmarshalJSON(data []byte) error {
	members, err := daemonObject(data)
	if err != nil {
		return err
	}
	raw, present := members["subagents"]
	if alias, aliased := members["mjolnir_subagents"]; aliased {
		if present {
			return errors.New("duplicate field `subagents`")
		}
		raw, present = alias, true
		delete(members, "mjolnir_subagents")
	}
	delete(members, "subagents")
	rest, err := json.Marshal(members)
	if err != nil {
		return err
	}
	type plain CreateSessionRequest
	var decoded plain
	if err := daemonDecodeStruct(rest, &decoded, true); err != nil {
		return err
	}
	if present {
		if decoded.Subagents, err = core.DeserializeOptionalPolicy(raw); err != nil {
			return err
		}
	}
	*v = CreateSessionRequest(decoded)
	return nil
}

type RegisteredSession struct {
	Session                 core.SessionRecord                          `json:"session"`
	RememberedContainerSize *DaemonPair[string, core.HostContainerSize] `json:"remembered_container_size"`
}

func (v RegisteredSession) MarshalJSON() ([]byte, error) {
	type plain RegisteredSession
	return daemonMarshalStruct(plain(v))
}

func (v *RegisteredSession) UnmarshalJSON(data []byte) error {
	type plain RegisteredSession
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// GitIdentity is the commit author in the daemon host's Git configuration.
// Sessions on the host use it directly and container sessions inherit it, so
// without both parts no session can commit.
type GitIdentity struct {
	Name  *string `json:"name"`
	Email *string `json:"email"`
	// PerDirectory: the global configuration picks an identity per directory
	// with `includeIf`, which only applies inside a repository. Writing a
	// plain identity would override those choices, so it is not offered.
	PerDirectory bool `json:"per_directory" serde:"default"`
}

func (v GitIdentity) MarshalJSON() ([]byte, error) {
	type plain GitIdentity
	return daemonMarshalStruct(plain(v))
}

func (v *GitIdentity) UnmarshalJSON(data []byte) error {
	type plain GitIdentity
	return daemonDecodeStruct(data, (*plain)(v), false)
}

func (g GitIdentity) IsComplete() bool {
	return g.Name != nil && g.Email != nil
}

// NeedsSetup reports whether a surface should offer to set the identity.
func (g GitIdentity) NeedsSetup() bool {
	return !g.IsComplete() && !g.PerDirectory
}

type DraftPreview struct {
	ID        string  `json:"id"`
	SessionID *string `json:"session_id"`
	Source    string  `json:"source"`
	OwnerPid  *uint32 `json:"owner_pid"`
	SavedAt   string  `json:"saved_at"`
}

func (v DraftPreview) MarshalJSON() ([]byte, error) {
	type plain DraftPreview
	return daemonMarshalStruct(plain(v))
}

func (v *DraftPreview) UnmarshalJSON(data []byte) error {
	type plain DraftPreview
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// WorkspaceDrafts is one workspace's detached drafts, without their text.
type WorkspaceDrafts struct {
	WorkspaceID string         `json:"workspace_id"`
	Drafts      []DraftPreview `json:"drafts"`
}

func (v WorkspaceDrafts) MarshalJSON() ([]byte, error) {
	type plain WorkspaceDrafts
	return daemonMarshalStruct(plain(v))
}

func (v *WorkspaceDrafts) UnmarshalJSON(data []byte) error {
	type plain WorkspaceDrafts
	return daemonDecodeStruct(data, (*plain)(v), false)
}

type RecoveryAdoptionRequest struct {
	SessionID    string  `json:"session_id"`
	TargetID     string  `json:"target_id"`
	Profile      *string `json:"profile"`
	Bundle       *string `json:"bundle"`
	AllInstances bool    `json:"all_instances"`
}

func (v RecoveryAdoptionRequest) MarshalJSON() ([]byte, error) {
	type plain RecoveryAdoptionRequest
	return daemonMarshalStruct(plain(v))
}

func (v *RecoveryAdoptionRequest) UnmarshalJSON(data []byte) error {
	type plain RecoveryAdoptionRequest
	return daemonDecodeStruct(data, (*plain)(v), false)
}

// RecoveryAdoptionProgress is what an adoption reports while it runs. A
// sealed interface over RecoveryAdoptionProgressWaiting and
// RecoveryAdoptionProgressInstalled.
type RecoveryAdoptionProgress interface {
	isRecoveryAdoptionProgress()
}

type RecoveryAdoptionProgressWaiting struct {
	Blockers       []string
	DestroyCommand *string
}

type RecoveryAdoptionProgressInstalled struct {
	SessionID string
}

func (RecoveryAdoptionProgressWaiting) isRecoveryAdoptionProgress()   {}
func (RecoveryAdoptionProgressInstalled) isRecoveryAdoptionProgress() {}

// DaemonAction is a request a client sends the daemon. Adjacently tagged by
// "action" with its fields under "arguments", in snake_case.
//
// Ping, Status, and Stop form the frozen management subset: their wire
// encoding — together with RequestEnvelope, ResponseEnvelope, DaemonStatus,
// and WebViewerStatus — must never change shape, because clients and daemons
// of *any* protocol version rely on them to identify, stop, and replace each
// other. PrepareUpgrade and its Done / UpgradePending replies are also frozen
// from protocol 33 onward: they drain accepted work before closing admission
// atomically. Every other action may change freely behind a ProtocolVersion
// bump.
type DaemonAction interface {
	isDaemonAction()
}

type DaemonActionDismissNotification struct {
	SessionID string                     `json:"session_id"`
	Target    core.NotificationDismissal `json:"target"`
}

type DaemonActionProveIdentity struct {
	Nonce string `json:"nonce"`
}

type DaemonActionLoginTerminal struct {
	ProfileID string `json:"profile_id"`
	Cols      uint16 `json:"cols"`
	Rows      uint16 `json:"rows"`
}

type DaemonActionPreviewBuildCache struct {
	Machine core.Machine `json:"machine"`
}

type DaemonActionInstallMbx struct {
	Machine core.Machine `json:"machine"`
}

type DaemonActionPreflightNewSession struct {
	Request NewSessionPreflightRequest `json:"request"`
}

type DaemonActionValidateProjectDirectory struct {
	TargetID  string `json:"target_id"`
	Directory string `json:"directory"`
}

type DaemonActionCheckpointArchiveSizes struct {
	Paths map[string]string `json:"paths"`
}

type DaemonActionSaveSpinnerStyle struct {
	Style core.SpinnerStyle `json:"style"`
}

type DaemonActionSetQuotaPreferences struct {
	Prefer []string `json:"prefer"`
}

type DaemonActionTestTarget struct {
	TargetID string `json:"target_id"`
}

type DaemonActionProfileLoginStatuses struct{}

type DaemonActionCheckAvailability struct{}

type DaemonActionDiscoverSetup struct {
	Scope    DetectScope                        `json:"scope"`
	Machines []DaemonPair[string, core.Machine] `json:"machines"`
}

type DaemonActionCompletePath struct {
	Host   core.CompletionHost `json:"host"`
	Prefix string              `json:"prefix"`
	Kind   core.CompletionKind `json:"kind"`
}

type DaemonActionCheckProjectDirectories struct {
	Paths []string `json:"paths"`
}

type DaemonActionDiscoverProjects struct {
	Request core.ProjectDiscoveryRequest `json:"request"`
}

type DaemonActionResolveContainerPath struct {
	TargetID string `json:"target_id"`
	Source   string `json:"source"`
}

type DaemonActionResolveSetupPath struct {
	Machine core.Machine `json:"machine"`
	Source  string       `json:"source"`
}

type DaemonActionValidateMountSource struct {
	TargetID string `json:"target_id"`
	Source   string `json:"source"`
}

type DaemonActionValidateSessionMounts struct {
	TargetID string                 `json:"target_id"`
	Mounts   []core.AdditionalMount `json:"mounts"`
}

// DaemonActionUploadChunk carries raw file fragments. Completion supplies the SHA-256 of all bytes. Bytes
// travel as one base64 string (daemon_chunk_bytes.go).
type DaemonActionUploadChunk struct {
	Bytes  []byte  `json:"bytes"`
	Sha256 *string `json:"sha256"`
}

type DaemonActionInstallAttachment struct {
	SessionID string             `json:"session_id"`
	Reference core.AttachmentRef `json:"reference"`
	Blob      string             `json:"blob"`
}

type DaemonActionTranscriptionAvailable struct {
	PreferredProfile string `json:"preferred_profile"`
}

type DaemonActionTranscribeAudio struct {
	PreferredProfile string `json:"preferred_profile"`
	Blob             string `json:"blob"`
}

type DaemonActionRecordOperatorActivity struct{}

type DaemonActionSetOperatorAway struct {
	Away bool `json:"away"`
}

type DaemonActionNativeAgentHistory struct {
	Owner  string                      `json:"owner"`
	Child  string                      `json:"child"`
	Before *DaemonPair[uint64, string] `json:"before"`
}

type DaemonActionPing struct{}

// DaemonActionPrepareUpgrade is the frozen upgrade handshake, available from daemon protocol 33
// onward. Busy replies leave every operation and control channel running.
type DaemonActionPrepareUpgrade struct{}

// DaemonActionUpgradeBlockers names the daemon-owned work that is currently holding an
// automatic handoff open, for the CLI's wait notice.
//
// Added after protocol 33, so it is not part of the frozen management
// subset. A daemon that predates it cannot deserialize the frame: it fails
// the request and closes the connection. Callers must therefore treat any
// failure, including a dropped connection or an error frame, as "unknown"
// and say nothing about what the upgrade is waiting for.
type DaemonActionUpgradeBlockers struct{}

// DaemonActionIdentity is this daemon's per-start identity, build and placement. Added
// after protocol 74, so it is deliberately not part of the frozen management
// subset: a client asks only a daemon of its own protocol.
type DaemonActionIdentity struct{}

// DaemonActionConfiguration is the configuration the daemon serves. Clients read it here
// instead of opening `config.toml`, which belongs to the daemon.
type DaemonActionConfiguration struct{}

// DaemonActionFirstRunSetupDue asks whether first-run setup has work to do: a new
// installation, or one whose setup did not finish. A dashboard asks before it
// shows the first-run welcome, which takes the keyboard.
type DaemonActionFirstRunSetupDue struct{}

// DaemonActionRunFirstRunSetup discovers and persists the first-run configuration in the
// daemon's own environment. A client in another placement asks its daemon
// instead of writing the client's `config.toml`.
type DaemonActionRunFirstRunSetup struct{}

// DaemonActionRunFirstRunSetupIn uses the client's directory when both processes can see the
// same paths.
type DaemonActionRunFirstRunSetupIn struct {
	ProjectDirectory string `json:"project_directory"`
}

// DaemonActionRunDoctor runs the doctor in the daemon's own environment and returns the
// actionable errors, which a client shows after first-run setup.
type DaemonActionRunDoctor struct{}

// DaemonActionNewSessionRemoteRepairs checks a project's Git remotes before a new session
// starts, in the environment that owns the project paths.
type DaemonActionNewSessionRemoteRepairs struct {
	BundleID string `json:"bundle_id"`
	TargetID string `json:"target_id"`
}

// DaemonActionRepairRepositoryRemotes applies the remote-tracking repairs a preflight
// proposed, in the environment that owns the project checkout.
type DaemonActionRepairRepositoryRemotes struct {
	BundleID string                   `json:"bundle_id"`
	Repairs  []core.LocalRemoteRepair `json:"repairs"`
}

// DaemonActionSessionProjectSource is the project source label for a session, read in the
// environment that owns the checkout.
type DaemonActionSessionProjectSource struct {
	SessionID string `json:"session_id"`
}

type DaemonActionStatus struct{}

type DaemonActionWebViewerAccess struct{}

type DaemonActionApiCredential struct{}

// DaemonActionGitIdentity is the daemon host's Git commit author, which its sessions
// inherit.
type DaemonActionGitIdentity struct{}

// DaemonActionSetGitIdentity writes the daemon host's global Git commit author.
type DaemonActionSetGitIdentity struct {
	Name  string `json:"name"`
	Email string `json:"email"`
}

type DaemonActionRecoverWebViewer struct {
	Recovery WebViewerRecovery
}

type DaemonActionInspectWebListener struct{}

type DaemonActionListWorkspaces struct{}

// DaemonActionListWorkspaceDrafts is every workspace's detached drafts, without their
// text: the workspace manager's details answer. Added in protocol 100.
type DaemonActionListWorkspaceDrafts struct{}

type DaemonActionCreateWorkspace struct {
	Name string `json:"name"`
}

type DaemonActionRenameWorkspace struct {
	WorkspaceID string `json:"workspace_id"`
	Name        string `json:"name"`
}

type DaemonActionTouchWorkspace struct {
	WorkspaceID string `json:"workspace_id"`
}

type DaemonActionCloseWorkspace struct {
	WorkspaceID string `json:"workspace_id"`
}

type DaemonActionCancelWorkspaceClose struct {
	WorkspaceID string `json:"workspace_id"`
}

type DaemonActionDeleteWorkspace struct {
	WorkspaceID string `json:"workspace_id"`
}

// DaemonActionAttach takes or renews a client's attachment lease.
type DaemonActionAttach struct {
	ClientID string `json:"client_id"`
}

type DaemonActionDetach struct {
	ClientID string `json:"client_id"`
}

type DaemonActionPersistReadReceipt struct {
	ClientID    string `json:"client_id"`
	WorkspaceID string `json:"workspace_id"`
	SessionID   string `json:"session_id"`
	Through     uint64 `json:"through"`
}

type DaemonActionPersistTerminalDraft struct {
	ClientID  string                    `json:"client_id"`
	SessionID string                    `json:"session_id"`
	OwnerPid  uint32                    `json:"owner_pid"`
	Revision  uint64                    `json:"revision"`
	Draft     core.DetachedSessionDraft `json:"draft"`
}

type DaemonActionRecordAbandonedPrompt struct {
	SessionID string `json:"session_id"`
	BundleID  string `json:"bundle_id"`
	Text      string `json:"text"`
}

// DaemonActionSaveSidebarSize persists the shared sidebar preference and publishes it
// before replying.
type DaemonActionSaveSidebarSize struct {
	Size core.PaneSize `json:"size"`
}

type DaemonActionSaveWorkspacePaneSizes struct {
	WorkspaceID string         `json:"workspace_id"`
	Sizes       core.PaneSizes `json:"sizes"`
}

type DaemonActionSaveWorkspaceLayout struct {
	WorkspaceID string                  `json:"workspace_id"`
	Layout      core.ConversationLayout `json:"layout"`
}

type DaemonActionImportSession struct {
	Blob    string             `json:"blob"`
	Session core.SessionRecord `json:"session"`
	// Bundle is a project the import synthesized. The daemon writes it with
	// the session, so a client never edits the daemon's configuration.
	Bundle *ImportedBundle `json:"bundle" serde:"default"`
}

type DaemonActionSetSessionTitle struct {
	SessionID string `json:"session_id"`
	Title     string `json:"title"`
}

type DaemonActionSetSessionNotificationsIgnored struct {
	SessionID string `json:"session_id"`
	Ignored   bool   `json:"ignored"`
}

type DaemonActionSetSessionWorkspace struct {
	SessionID   string `json:"session_id"`
	WorkspaceID string `json:"workspace_id"`
}

type DaemonActionSetSessionContainerSettings struct {
	SessionID string `json:"session_id"`
	// ResourceAllocation is nil for none (Rust's Option).
	ResourceAllocation core.SessionResourceAllocation `json:"resource_allocation" serde:"default"`
	Mounts             []core.AdditionalMount         `json:"mounts"`
	MountHistory       []string                       `json:"mount_history"`
}

type DaemonActionMarkSessionTargetMissing struct {
	SessionID string `json:"session_id"`
	Detail    string `json:"detail"`
	UpdatedAt string `json:"updated_at"`
}

// DaemonActionCheckpointSession is a legacy wire tombstone: rejected with guidance to
// suspend.
type DaemonActionCheckpointSession struct {
	SessionID string `json:"session_id"`
}

// DaemonActionProjectCatalog searches the user's SessionWiki index. An empty query lists
// the most recent sessions.
type DaemonActionProjectCatalog struct {
	Refresh bool `json:"refresh"`
	Retry   bool `json:"retry"`
}

type DaemonActionCreateProject struct {
	Sources []string `json:"sources"`
}

type DaemonActionRemoveProject struct {
	BundleID string `json:"bundle_id"`
}

type DaemonActionWikiSyncRequest struct{}

type DaemonActionWikiSyncWatch struct {
	Epoch string `json:"epoch"`
	After uint64 `json:"after"`
}

type DaemonActionWikiSearch struct {
	Query string `json:"query"`
	Limit int    `json:"limit"`
}

// DaemonActionSessionTextSearch asks for the live sessions whose user or agent messages
// contain a query, ignoring case. Tool calls and tool output do not count.
type DaemonActionSessionTextSearch struct {
	Query string `json:"query"`
}

// DaemonActionWikiBrief is the markdown briefing for one indexed session.
type DaemonActionWikiBrief struct {
	WikiID   string `json:"wiki_id"`
	MaxChars int    `json:"max_chars"`
}

// DaemonActionWikiHits asks for the passages of one indexed session that match a
// query, with context.
type DaemonActionWikiHits struct {
	WikiID          string `json:"wiki_id"`
	Query           string `json:"query"`
	ContextMessages int    `json:"context_messages"`
	PerMessageChars int    `json:"per_message_chars"`
}

// DaemonActionWikiSession asks what one indexed session is, and what continuing it
// would mean.
type DaemonActionWikiSession struct {
	WikiID string `json:"wiki_id"`
}

// DaemonActionWikiRestore starts a new session from an archived one's transcript.
type DaemonActionWikiRestore struct {
	Request WikiRestoreRequest
}

type DaemonActionScanRecovery struct {
	AllInstances bool `json:"all_instances"`
}

type DaemonActionAdoptRecovery struct {
	Request RecoveryAdoptionRequest
}

type DaemonActionDestroyRecovery struct {
	SessionID    string `json:"session_id"`
	TargetID     string `json:"target_id"`
	Confirmation string `json:"confirmation"`
	AllInstances bool   `json:"all_instances"`
}

type DaemonActionSnapshot struct {
	WorkspaceID string `json:"workspace_id"`
}

// DaemonActionDashboardStartup asks for everything the terminal dashboard reads from the
// store at startup.
type DaemonActionDashboardStartup struct {
	ClientID    string `json:"client_id"`
	WorkspaceID string `json:"workspace_id"`
}

// DaemonActionWorkspaceLayouts asks for the stored pane sizes and conversation layouts
// of these workspaces.
type DaemonActionWorkspaceLayouts struct {
	WorkspaceIDs []string `json:"workspace_ids"`
}

// DaemonActionStoredSessionMetadata asks for the mount history and remembered container sizes.
type DaemonActionStoredSessionMetadata struct{}

// DaemonActionStoredProjectionTail asks for the last `items` entries of a stored projection.
type DaemonActionStoredProjectionTail struct {
	SessionID string `json:"session_id"`
	Items     int    `json:"items"`
}

// DaemonActionStoredTranscriptItem asks for one stored transcript item, addressed by its
// stable identity.
type DaemonActionStoredTranscriptItem struct {
	SessionID string `json:"session_id"`
	StableID  string `json:"stable_id"`
}

// DaemonActionStoredAgentMessageThrough asks for the newest agent transcript item complete
// through this relay ordinal.
type DaemonActionStoredAgentMessageThrough struct {
	SessionID string `json:"session_id"`
	Through   uint64 `json:"through"`
}

// DaemonActionStoredSessionSummary asks for the stored projection summary of one session.
type DaemonActionStoredSessionSummary struct {
	SessionID string `json:"session_id"`
}

// DaemonActionResumeRepositoryPreflight checks that a stopped session's checkpointed
// repositories can be fetched again before it resumes on `target_id`.
type DaemonActionResumeRepositoryPreflight struct {
	SessionID string `json:"session_id"`
	TargetID  string `json:"target_id"`
}

// DaemonActionReplaceResumeRepositoryOrigin points one of a stopped session's repositories
// at a new origin, then checks the sources again.
type DaemonActionReplaceResumeRepositoryOrigin struct {
	SessionID    string `json:"session_id"`
	RepositoryID string `json:"repository_id"`
	Replacement  string `json:"replacement"`
}

// DaemonActionArchiveSpacePreview asks how much session storage an archive age would free.
type DaemonActionArchiveSpacePreview struct {
	OlderThanDays *uint32 `json:"older_than_days"`
}

// DaemonActionSaveSetup saves the terminal Setup editor's configuration, checked against
// the stored sessions it would disturb. Both are configuration JSON.
type DaemonActionSaveSetup struct {
	Original string `json:"original"`
	Updated  string `json:"updated"`
}

type DaemonActionRuntimeChanges struct {
	Cursor *RuntimeCursor `json:"cursor"`
	Wait   bool           `json:"wait"`
	// SubagentParents is a set: sorted and without repeats, as Rust's BTreeSet
	// serializes.
	SubagentParents []string `json:"subagent_parents"`
}

// DaemonActionSessionTail asks for one live session's transcript tail as it was at
// `cursor`, so the deltas after that cursor apply to it exactly.
type DaemonActionSessionTail struct {
	SessionID string        `json:"session_id"`
	Cursor    RuntimeCursor `json:"cursor"`
}

// DaemonActionResumeCandidates asks for the sessions the resume dialog lists. The runtime
// feed carries only live sessions, so the dialog asks for these when it
// opens.
type DaemonActionResumeCandidates struct{}

// DaemonActionSessionRecord asks for one session's whole record, live or stopped. The
// resume wizard reads it for the row picked in the resume dialog.
type DaemonActionSessionRecord struct {
	SessionID string `json:"session_id"`
}

// DaemonActionGoStartupSession asks for the session `mj go` opens: the remembered one
// while it is still eligible, otherwise the most recently updated eligible
// session of the workspace, live or stopped.
type DaemonActionGoStartupSession struct {
	WorkspaceID   string  `json:"workspace_id"`
	LastSessionID *string `json:"last_session_id"`
}

// DaemonActionHostCapacity asks for every host and fleet with the capacity service's
// latest CPU, RAM and filesystem readings. Added in protocol 101.
type DaemonActionHostCapacity struct{}

// DaemonActionRefreshCapacity re-reads the daemon probe definitions and
// samples every host.
type DaemonActionRefreshCapacity struct{}

// DaemonActionRefreshQuota asks the daemon's quota poller to probe now. The daemon is the
// only process that probes; the result arrives in the runtime feed. Added in
// protocol 43.
type DaemonActionRefreshQuota struct{}

// DaemonActionProbeQuotaProfiles probes exactly these profiles once, even disabled
// ones, and serves their reports in the runtime feed. Added in protocol 99;
// `forced` (probe now, off the profile's schedule, even with a fresh report)
// in protocol 100.
type DaemonActionProbeQuotaProfiles struct {
	ProfileIDs []string `json:"profile_ids"`
	Forced     bool     `json:"forced"`
}

// DaemonActionSubagentOptions discovers eligible subagent models in the daemon, which owns
// the cache.
type DaemonActionSubagentOptions struct {
	Profile string  `json:"profile"`
	Model   *string `json:"model"`
	// Config lets Setup probe an unsaved draft without changing live
	// settings.
	Config *core.Config `json:"config,omitempty" serde:"default"`
}

// DaemonActionWarmProfileCapabilities ensures valid unsaved profile definitions are
// hydrating. Results arrive through runtime publications; this request does
// no discovery itself.
type DaemonActionWarmProfileCapabilities struct {
	Config core.Config `json:"config"`
}

type DaemonActionRenameProfile struct {
	OldID string `json:"old_id"`
	NewID string `json:"new_id"`
}

type DaemonActionRenameTarget struct {
	OldID string `json:"old_id"`
	NewID string `json:"new_id"`
}

type DaemonActionInterruptAll struct {
	SessionID string `json:"session_id"`
}

type DaemonActionSubmitSessionCommand struct {
	InheritedDraft *string           `json:"inherited_draft" serde:"default"`
	SessionID      string            `json:"session_id"`
	CommandID      string            `json:"command_id"`
	Command        core.RelayCommand `json:"command"`
}

// DaemonActionQueueStartupPrompt delivers a prompt typed while a session was still starting,
// once the daemon sees that session's harness become ready. The daemon owns
// the wait, so the prompt arrives whether or not this client is still running
// or still showing that session.
type DaemonActionQueueStartupPrompt struct {
	SessionID       string                   `json:"session_id"`
	CommandID       string                   `json:"command_id"`
	Text            string                   `json:"text"`
	Prompt          []acpschema.ContentBlock `json:"prompt"`
	EncodedDraft    string                   `json:"encoded_draft"`
	MoveOperationID *string                  `json:"move_operation_id" serde:"default"`
	// InheritedDraft is the saved draft text this prompt was typed from, if
	// the client also persisted it. Cleared after a successful submit so the
	// delivered prompt does not reappear as a draft.
	InheritedDraft *string `json:"inherited_draft" serde:"default"`
}

// DaemonActionWithdrawStartupPrompt takes back a prompt queued with QueueStartupPrompt that
// the daemon has not started delivering, so the person can edit it. The daemon
// cancels the prompt with this caller-owned command ID and replies
// PromptWithdrawn(true), or PromptWithdrawn(false) when none is waiting
// because delivery already started.
type DaemonActionWithdrawStartupPrompt struct {
	SessionID string `json:"session_id"`
	CommandID string `json:"command_id"`
}

type DaemonActionStartupPromptOutcomes struct {
	SessionID  string   `json:"session_id"`
	CommandIDs []string `json:"command_ids"`
}

type DaemonActionSyncSession struct {
	SessionID string `json:"session_id"`
}

type DaemonActionRespondElicitation struct {
	SessionID     string                   `json:"session_id"`
	ElicitationID string                   `json:"elicitation_id"`
	Response      core.ElicitationResponse `json:"response"`
}

type DaemonActionStopBackgroundTask struct {
	SessionID        string `json:"session_id"`
	BackgroundTaskID string `json:"background_task_id"`
}

// DaemonActionStartTurnReview reviews the turn this session just finished, on a
// surface's request.
type DaemonActionStartTurnReview struct {
	SessionID string `json:"session_id"`
}

// DaemonActionResolveTurnReview forwards, dismisses, or cancels the open review.
type DaemonActionResolveTurnReview struct {
	SessionID  string          `json:"session_id"`
	Resolution core.Resolution `json:"resolution"`
}

type DaemonActionStartSessionLifecycle struct {
	Request SessionLifecycleRequest
}

type DaemonActionWaitSessionLifecycle struct {
	Receipt LifecycleReceipt
}

type DaemonActionSuspendPreflight struct {
	SessionID         string `json:"session_id"`
	DryRun            bool   `json:"dry_run" serde:"default"`
	WithoutCheckpoint bool   `json:"without_checkpoint" serde:"default"`
}

type DaemonActionSuspendSession struct {
	SessionID           string                `json:"session_id"`
	DiscardUntracked    core.UntrackedConsent `json:"discard_untracked" serde:"default"`
	AbortOperations     core.OperationConsent `json:"abort_operations" serde:"default"`
	WithoutCheckpoint   bool                  `json:"without_checkpoint" serde:"default"`
	ConfirmNoCheckpoint bool                  `json:"confirm_no_checkpoint,omitempty"`
}

type DaemonActionRestartSession struct {
	SessionID                      string `json:"session_id"`
	AcknowledgeContainerRecreation bool   `json:"acknowledge_container_recreation,omitempty" serde:"default"`
}

type DaemonActionPreviewContainerUpgrade struct {
	SessionID string `json:"session_id"`
}

type DaemonActionUpgradeContainer struct {
	Request core.ContainerUpgradeRequest
}

type DaemonActionStartCreateSession struct {
	Request CreateSessionRequest
}

type DaemonActionWaitCreateSession struct {
	SessionID string `json:"session_id"`
}

type DaemonActionResumeSession struct {
	Request ResumeSessionRequest
}

type DaemonActionPrepareMoveSession struct {
	Selection core.MoveSelection
}

type DaemonActionMoveSession struct {
	Request core.MoveSessionRequest
}

type DaemonActionMoveSources struct {
	SessionID          string  `json:"session_id"`
	CleanupOperationID *string `json:"cleanup_operation_id"`
}

type DaemonActionDestroyStoppedSession struct {
	SessionID string `json:"session_id"`
}

type DaemonActionForceDestroySession struct {
	SessionID string `json:"session_id"`
}

type DaemonActionCancelLifecycle struct {
	SessionID string `json:"session_id"`
}

type DaemonActionCancelContainerUpgrade struct {
	SessionID string `json:"session_id"`
}

type DaemonActionRecoverDraft struct {
	DraftID string `json:"draft_id"`
}

type DaemonActionStop struct{}

func (DaemonActionDismissNotification) isDaemonAction()            {}
func (DaemonActionProveIdentity) isDaemonAction()                  {}
func (DaemonActionLoginTerminal) isDaemonAction()                  {}
func (DaemonActionPreviewBuildCache) isDaemonAction()              {}
func (DaemonActionInstallMbx) isDaemonAction()                     {}
func (DaemonActionPreflightNewSession) isDaemonAction()            {}
func (DaemonActionValidateProjectDirectory) isDaemonAction()       {}
func (DaemonActionCheckpointArchiveSizes) isDaemonAction()         {}
func (DaemonActionSaveSpinnerStyle) isDaemonAction()               {}
func (DaemonActionSetQuotaPreferences) isDaemonAction()            {}
func (DaemonActionTestTarget) isDaemonAction()                     {}
func (DaemonActionProfileLoginStatuses) isDaemonAction()           {}
func (DaemonActionCheckAvailability) isDaemonAction()              {}
func (DaemonActionDiscoverSetup) isDaemonAction()                  {}
func (DaemonActionCompletePath) isDaemonAction()                   {}
func (DaemonActionCheckProjectDirectories) isDaemonAction()        {}
func (DaemonActionDiscoverProjects) isDaemonAction()               {}
func (DaemonActionResolveContainerPath) isDaemonAction()           {}
func (DaemonActionResolveSetupPath) isDaemonAction()               {}
func (DaemonActionValidateMountSource) isDaemonAction()            {}
func (DaemonActionValidateSessionMounts) isDaemonAction()          {}
func (DaemonActionUploadChunk) isDaemonAction()                    {}
func (DaemonActionInstallAttachment) isDaemonAction()              {}
func (DaemonActionTranscriptionAvailable) isDaemonAction()         {}
func (DaemonActionTranscribeAudio) isDaemonAction()                {}
func (DaemonActionRecordOperatorActivity) isDaemonAction()         {}
func (DaemonActionSetOperatorAway) isDaemonAction()                {}
func (DaemonActionNativeAgentHistory) isDaemonAction()             {}
func (DaemonActionPing) isDaemonAction()                           {}
func (DaemonActionPrepareUpgrade) isDaemonAction()                 {}
func (DaemonActionUpgradeBlockers) isDaemonAction()                {}
func (DaemonActionIdentity) isDaemonAction()                       {}
func (DaemonActionConfiguration) isDaemonAction()                  {}
func (DaemonActionFirstRunSetupDue) isDaemonAction()               {}
func (DaemonActionRunFirstRunSetup) isDaemonAction()               {}
func (DaemonActionRunFirstRunSetupIn) isDaemonAction()             {}
func (DaemonActionRunDoctor) isDaemonAction()                      {}
func (DaemonActionNewSessionRemoteRepairs) isDaemonAction()        {}
func (DaemonActionRepairRepositoryRemotes) isDaemonAction()        {}
func (DaemonActionSessionProjectSource) isDaemonAction()           {}
func (DaemonActionStatus) isDaemonAction()                         {}
func (DaemonActionWebViewerAccess) isDaemonAction()                {}
func (DaemonActionApiCredential) isDaemonAction()                  {}
func (DaemonActionGitIdentity) isDaemonAction()                    {}
func (DaemonActionSetGitIdentity) isDaemonAction()                 {}
func (DaemonActionRecoverWebViewer) isDaemonAction()               {}
func (DaemonActionInspectWebListener) isDaemonAction()             {}
func (DaemonActionListWorkspaces) isDaemonAction()                 {}
func (DaemonActionListWorkspaceDrafts) isDaemonAction()            {}
func (DaemonActionCreateWorkspace) isDaemonAction()                {}
func (DaemonActionRenameWorkspace) isDaemonAction()                {}
func (DaemonActionTouchWorkspace) isDaemonAction()                 {}
func (DaemonActionCloseWorkspace) isDaemonAction()                 {}
func (DaemonActionCancelWorkspaceClose) isDaemonAction()           {}
func (DaemonActionDeleteWorkspace) isDaemonAction()                {}
func (DaemonActionAttach) isDaemonAction()                         {}
func (DaemonActionDetach) isDaemonAction()                         {}
func (DaemonActionPersistReadReceipt) isDaemonAction()             {}
func (DaemonActionPersistTerminalDraft) isDaemonAction()           {}
func (DaemonActionRecordAbandonedPrompt) isDaemonAction()          {}
func (DaemonActionSaveSidebarSize) isDaemonAction()                {}
func (DaemonActionSaveWorkspacePaneSizes) isDaemonAction()         {}
func (DaemonActionSaveWorkspaceLayout) isDaemonAction()            {}
func (DaemonActionImportSession) isDaemonAction()                  {}
func (DaemonActionSetSessionTitle) isDaemonAction()                {}
func (DaemonActionSetSessionNotificationsIgnored) isDaemonAction() {}
func (DaemonActionSetSessionWorkspace) isDaemonAction()            {}
func (DaemonActionSetSessionContainerSettings) isDaemonAction()    {}
func (DaemonActionMarkSessionTargetMissing) isDaemonAction()       {}
func (DaemonActionCheckpointSession) isDaemonAction()              {}
func (DaemonActionProjectCatalog) isDaemonAction()                 {}
func (DaemonActionCreateProject) isDaemonAction()                  {}
func (DaemonActionRemoveProject) isDaemonAction()                  {}
func (DaemonActionWikiSyncRequest) isDaemonAction()                {}
func (DaemonActionWikiSyncWatch) isDaemonAction()                  {}
func (DaemonActionWikiSearch) isDaemonAction()                     {}
func (DaemonActionSessionTextSearch) isDaemonAction()              {}
func (DaemonActionWikiBrief) isDaemonAction()                      {}
func (DaemonActionWikiHits) isDaemonAction()                       {}
func (DaemonActionWikiSession) isDaemonAction()                    {}
func (DaemonActionWikiRestore) isDaemonAction()                    {}
func (DaemonActionScanRecovery) isDaemonAction()                   {}
func (DaemonActionAdoptRecovery) isDaemonAction()                  {}
func (DaemonActionDestroyRecovery) isDaemonAction()                {}
func (DaemonActionSnapshot) isDaemonAction()                       {}
func (DaemonActionDashboardStartup) isDaemonAction()               {}
func (DaemonActionWorkspaceLayouts) isDaemonAction()               {}
func (DaemonActionStoredSessionMetadata) isDaemonAction()          {}
func (DaemonActionStoredProjectionTail) isDaemonAction()           {}
func (DaemonActionStoredTranscriptItem) isDaemonAction()           {}
func (DaemonActionStoredAgentMessageThrough) isDaemonAction()      {}
func (DaemonActionStoredSessionSummary) isDaemonAction()           {}
func (DaemonActionResumeRepositoryPreflight) isDaemonAction()      {}
func (DaemonActionReplaceResumeRepositoryOrigin) isDaemonAction()  {}
func (DaemonActionArchiveSpacePreview) isDaemonAction()            {}
func (DaemonActionSaveSetup) isDaemonAction()                      {}
func (DaemonActionRuntimeChanges) isDaemonAction()                 {}
func (DaemonActionSessionTail) isDaemonAction()                    {}
func (DaemonActionResumeCandidates) isDaemonAction()               {}
func (DaemonActionSessionRecord) isDaemonAction()                  {}
func (DaemonActionGoStartupSession) isDaemonAction()               {}
func (DaemonActionHostCapacity) isDaemonAction()                   {}
func (DaemonActionRefreshCapacity) isDaemonAction()                {}
func (DaemonActionRefreshQuota) isDaemonAction()                   {}
func (DaemonActionProbeQuotaProfiles) isDaemonAction()             {}
func (DaemonActionSubagentOptions) isDaemonAction()                {}
func (DaemonActionWarmProfileCapabilities) isDaemonAction()        {}
func (DaemonActionRenameProfile) isDaemonAction()                  {}
func (DaemonActionRenameTarget) isDaemonAction()                   {}
func (DaemonActionInterruptAll) isDaemonAction()                   {}
func (DaemonActionSubmitSessionCommand) isDaemonAction()           {}
func (DaemonActionQueueStartupPrompt) isDaemonAction()             {}
func (DaemonActionWithdrawStartupPrompt) isDaemonAction()          {}
func (DaemonActionStartupPromptOutcomes) isDaemonAction()          {}
func (DaemonActionSyncSession) isDaemonAction()                    {}
func (DaemonActionRespondElicitation) isDaemonAction()             {}
func (DaemonActionStopBackgroundTask) isDaemonAction()             {}
func (DaemonActionStartTurnReview) isDaemonAction()                {}
func (DaemonActionResolveTurnReview) isDaemonAction()              {}
func (DaemonActionStartSessionLifecycle) isDaemonAction()          {}
func (DaemonActionWaitSessionLifecycle) isDaemonAction()           {}
func (DaemonActionSuspendPreflight) isDaemonAction()               {}
func (DaemonActionSuspendSession) isDaemonAction()                 {}
func (DaemonActionRestartSession) isDaemonAction()                 {}
func (DaemonActionPreviewContainerUpgrade) isDaemonAction()        {}
func (DaemonActionUpgradeContainer) isDaemonAction()               {}
func (DaemonActionStartCreateSession) isDaemonAction()             {}
func (DaemonActionWaitCreateSession) isDaemonAction()              {}
func (DaemonActionResumeSession) isDaemonAction()                  {}
func (DaemonActionPrepareMoveSession) isDaemonAction()             {}
func (DaemonActionMoveSession) isDaemonAction()                    {}
func (DaemonActionMoveSources) isDaemonAction()                    {}
func (DaemonActionDestroyStoppedSession) isDaemonAction()          {}
func (DaemonActionForceDestroySession) isDaemonAction()            {}
func (DaemonActionCancelLifecycle) isDaemonAction()                {}
func (DaemonActionCancelContainerUpgrade) isDaemonAction()         {}
func (DaemonActionRecoverDraft) isDaemonAction()                   {}
func (DaemonActionStop) isDaemonAction()                           {}

func (v DaemonActionDismissNotification) MarshalJSON() ([]byte, error) {
	type plain DaemonActionDismissNotification
	return daemonAdjacentStruct("action", "arguments", "dismiss_notification", plain(v))
}

func (v DaemonActionProveIdentity) MarshalJSON() ([]byte, error) {
	type plain DaemonActionProveIdentity
	return daemonAdjacentStruct("action", "arguments", "prove_identity", plain(v))
}

func (v DaemonActionLoginTerminal) MarshalJSON() ([]byte, error) {
	type plain DaemonActionLoginTerminal
	return daemonAdjacentStruct("action", "arguments", "login_terminal", plain(v))
}

func (v DaemonActionPreviewBuildCache) MarshalJSON() ([]byte, error) {
	type plain DaemonActionPreviewBuildCache
	return daemonAdjacentStruct("action", "arguments", "preview_build_cache", plain(v))
}

func (v DaemonActionInstallMbx) MarshalJSON() ([]byte, error) {
	type plain DaemonActionInstallMbx
	return daemonAdjacentStruct("action", "arguments", "install_mbx", plain(v))
}

func (v DaemonActionPreflightNewSession) MarshalJSON() ([]byte, error) {
	type plain DaemonActionPreflightNewSession
	return daemonAdjacentStruct("action", "arguments", "preflight_new_session", plain(v))
}

func (v DaemonActionValidateProjectDirectory) MarshalJSON() ([]byte, error) {
	type plain DaemonActionValidateProjectDirectory
	return daemonAdjacentStruct("action", "arguments", "validate_project_directory", plain(v))
}

func (v DaemonActionCheckpointArchiveSizes) MarshalJSON() ([]byte, error) {
	type plain DaemonActionCheckpointArchiveSizes
	return daemonAdjacentStruct("action", "arguments", "checkpoint_archive_sizes", plain(v))
}

func (v DaemonActionSaveSpinnerStyle) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSaveSpinnerStyle
	return daemonAdjacentStruct("action", "arguments", "save_spinner_style", plain(v))
}

func (v DaemonActionSetQuotaPreferences) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSetQuotaPreferences
	return daemonAdjacentStruct("action", "arguments", "set_quota_preferences", plain(v))
}

func (v DaemonActionTestTarget) MarshalJSON() ([]byte, error) {
	type plain DaemonActionTestTarget
	return daemonAdjacentStruct("action", "arguments", "test_target", plain(v))
}

func (DaemonActionProfileLoginStatuses) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "profile_login_statuses", nil)
}

func (DaemonActionCheckAvailability) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "check_availability", nil)
}

func (v DaemonActionDiscoverSetup) MarshalJSON() ([]byte, error) {
	type plain DaemonActionDiscoverSetup
	return daemonAdjacentStruct("action", "arguments", "discover_setup", plain(v))
}

func (v DaemonActionCompletePath) MarshalJSON() ([]byte, error) {
	type plain DaemonActionCompletePath
	return daemonAdjacentStruct("action", "arguments", "complete_path", plain(v))
}

func (v DaemonActionCheckProjectDirectories) MarshalJSON() ([]byte, error) {
	type plain DaemonActionCheckProjectDirectories
	return daemonAdjacentStruct("action", "arguments", "check_project_directories", plain(v))
}

func (v DaemonActionDiscoverProjects) MarshalJSON() ([]byte, error) {
	type plain DaemonActionDiscoverProjects
	return daemonAdjacentStruct("action", "arguments", "discover_projects", plain(v))
}

func (v DaemonActionResolveContainerPath) MarshalJSON() ([]byte, error) {
	type plain DaemonActionResolveContainerPath
	return daemonAdjacentStruct("action", "arguments", "resolve_container_path", plain(v))
}

func (v DaemonActionResolveSetupPath) MarshalJSON() ([]byte, error) {
	type plain DaemonActionResolveSetupPath
	return daemonAdjacentStruct("action", "arguments", "resolve_setup_path", plain(v))
}

func (v DaemonActionValidateMountSource) MarshalJSON() ([]byte, error) {
	type plain DaemonActionValidateMountSource
	return daemonAdjacentStruct("action", "arguments", "validate_mount_source", plain(v))
}

func (v DaemonActionValidateSessionMounts) MarshalJSON() ([]byte, error) {
	type plain DaemonActionValidateSessionMounts
	return daemonAdjacentStruct("action", "arguments", "validate_session_mounts", plain(v))
}

func (v DaemonActionInstallAttachment) MarshalJSON() ([]byte, error) {
	type plain DaemonActionInstallAttachment
	return daemonAdjacentStruct("action", "arguments", "install_attachment", plain(v))
}

func (v DaemonActionTranscriptionAvailable) MarshalJSON() ([]byte, error) {
	type plain DaemonActionTranscriptionAvailable
	return daemonAdjacentStruct("action", "arguments", "transcription_available", plain(v))
}

func (v DaemonActionTranscribeAudio) MarshalJSON() ([]byte, error) {
	type plain DaemonActionTranscribeAudio
	return daemonAdjacentStruct("action", "arguments", "transcribe_audio", plain(v))
}

func (DaemonActionRecordOperatorActivity) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "record_operator_activity", nil)
}

func (v DaemonActionSetOperatorAway) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSetOperatorAway
	return daemonAdjacentStruct("action", "arguments", "set_operator_away", plain(v))
}

func (v DaemonActionNativeAgentHistory) MarshalJSON() ([]byte, error) {
	type plain DaemonActionNativeAgentHistory
	return daemonAdjacentStruct("action", "arguments", "native_agent_history", plain(v))
}

func (DaemonActionPing) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "ping", nil)
}

func (DaemonActionPrepareUpgrade) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "prepare_upgrade", nil)
}

func (DaemonActionUpgradeBlockers) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "upgrade_blockers", nil)
}

func (DaemonActionIdentity) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "identity", nil)
}

func (DaemonActionConfiguration) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "configuration", nil)
}

func (DaemonActionFirstRunSetupDue) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "first_run_setup_due", nil)
}

func (DaemonActionRunFirstRunSetup) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "run_first_run_setup", nil)
}

func (v DaemonActionRunFirstRunSetupIn) MarshalJSON() ([]byte, error) {
	type plain DaemonActionRunFirstRunSetupIn
	return daemonAdjacentStruct("action", "arguments", "run_first_run_setup_in", plain(v))
}

func (DaemonActionRunDoctor) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "run_doctor", nil)
}

func (v DaemonActionNewSessionRemoteRepairs) MarshalJSON() ([]byte, error) {
	type plain DaemonActionNewSessionRemoteRepairs
	return daemonAdjacentStruct("action", "arguments", "new_session_remote_repairs", plain(v))
}

func (v DaemonActionRepairRepositoryRemotes) MarshalJSON() ([]byte, error) {
	type plain DaemonActionRepairRepositoryRemotes
	return daemonAdjacentStruct("action", "arguments", "repair_repository_remotes", plain(v))
}

func (v DaemonActionSessionProjectSource) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSessionProjectSource
	return daemonAdjacentStruct("action", "arguments", "session_project_source", plain(v))
}

func (DaemonActionStatus) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "status", nil)
}

func (DaemonActionWebViewerAccess) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "web_viewer_access", nil)
}

func (DaemonActionApiCredential) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "api_credential", nil)
}

func (DaemonActionGitIdentity) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "git_identity", nil)
}

func (v DaemonActionSetGitIdentity) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSetGitIdentity
	return daemonAdjacentStruct("action", "arguments", "set_git_identity", plain(v))
}

func (v DaemonActionRecoverWebViewer) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("action", "arguments", "recover_web_viewer", v.Recovery)
}

func (DaemonActionInspectWebListener) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "inspect_web_listener", nil)
}

func (DaemonActionListWorkspaces) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "list_workspaces", nil)
}

func (DaemonActionListWorkspaceDrafts) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "list_workspace_drafts", nil)
}

func (v DaemonActionCreateWorkspace) MarshalJSON() ([]byte, error) {
	type plain DaemonActionCreateWorkspace
	return daemonAdjacentStruct("action", "arguments", "create_workspace", plain(v))
}

func (v DaemonActionRenameWorkspace) MarshalJSON() ([]byte, error) {
	type plain DaemonActionRenameWorkspace
	return daemonAdjacentStruct("action", "arguments", "rename_workspace", plain(v))
}

func (v DaemonActionTouchWorkspace) MarshalJSON() ([]byte, error) {
	type plain DaemonActionTouchWorkspace
	return daemonAdjacentStruct("action", "arguments", "touch_workspace", plain(v))
}

func (v DaemonActionCloseWorkspace) MarshalJSON() ([]byte, error) {
	type plain DaemonActionCloseWorkspace
	return daemonAdjacentStruct("action", "arguments", "close_workspace", plain(v))
}

func (v DaemonActionCancelWorkspaceClose) MarshalJSON() ([]byte, error) {
	type plain DaemonActionCancelWorkspaceClose
	return daemonAdjacentStruct("action", "arguments", "cancel_workspace_close", plain(v))
}

func (v DaemonActionDeleteWorkspace) MarshalJSON() ([]byte, error) {
	type plain DaemonActionDeleteWorkspace
	return daemonAdjacentStruct("action", "arguments", "delete_workspace", plain(v))
}

func (v DaemonActionAttach) MarshalJSON() ([]byte, error) {
	type plain DaemonActionAttach
	return daemonAdjacentStruct("action", "arguments", "attach", plain(v))
}

func (v DaemonActionDetach) MarshalJSON() ([]byte, error) {
	type plain DaemonActionDetach
	return daemonAdjacentStruct("action", "arguments", "detach", plain(v))
}

func (v DaemonActionPersistReadReceipt) MarshalJSON() ([]byte, error) {
	type plain DaemonActionPersistReadReceipt
	return daemonAdjacentStruct("action", "arguments", "persist_read_receipt", plain(v))
}

func (v DaemonActionPersistTerminalDraft) MarshalJSON() ([]byte, error) {
	type plain DaemonActionPersistTerminalDraft
	return daemonAdjacentStruct("action", "arguments", "persist_terminal_draft", plain(v))
}

func (v DaemonActionRecordAbandonedPrompt) MarshalJSON() ([]byte, error) {
	type plain DaemonActionRecordAbandonedPrompt
	return daemonAdjacentStruct("action", "arguments", "record_abandoned_prompt", plain(v))
}

func (v DaemonActionSaveSidebarSize) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSaveSidebarSize
	return daemonAdjacentStruct("action", "arguments", "save_sidebar_size", plain(v))
}

func (v DaemonActionSaveWorkspacePaneSizes) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSaveWorkspacePaneSizes
	return daemonAdjacentStruct("action", "arguments", "save_workspace_pane_sizes", plain(v))
}

func (v DaemonActionSaveWorkspaceLayout) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSaveWorkspaceLayout
	return daemonAdjacentStruct("action", "arguments", "save_workspace_layout", plain(v))
}

func (v DaemonActionImportSession) MarshalJSON() ([]byte, error) {
	type plain DaemonActionImportSession
	return daemonAdjacentStruct("action", "arguments", "import_session", plain(v))
}

func (v DaemonActionSetSessionTitle) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSetSessionTitle
	return daemonAdjacentStruct("action", "arguments", "set_session_title", plain(v))
}

func (v DaemonActionSetSessionNotificationsIgnored) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSetSessionNotificationsIgnored
	return daemonAdjacentStruct("action", "arguments", "set_session_notifications_ignored", plain(v))
}

func (v DaemonActionSetSessionWorkspace) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSetSessionWorkspace
	return daemonAdjacentStruct("action", "arguments", "set_session_workspace", plain(v))
}

func (v DaemonActionSetSessionContainerSettings) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSetSessionContainerSettings
	return daemonAdjacentStruct("action", "arguments", "set_session_container_settings", plain(v))
}

func (v DaemonActionMarkSessionTargetMissing) MarshalJSON() ([]byte, error) {
	type plain DaemonActionMarkSessionTargetMissing
	return daemonAdjacentStruct("action", "arguments", "mark_session_target_missing", plain(v))
}

func (v DaemonActionCheckpointSession) MarshalJSON() ([]byte, error) {
	type plain DaemonActionCheckpointSession
	return daemonAdjacentStruct("action", "arguments", "checkpoint_session", plain(v))
}

func (v DaemonActionProjectCatalog) MarshalJSON() ([]byte, error) {
	type plain DaemonActionProjectCatalog
	return daemonAdjacentStruct("action", "arguments", "project_catalog", plain(v))
}

func (v DaemonActionCreateProject) MarshalJSON() ([]byte, error) {
	type plain DaemonActionCreateProject
	return daemonAdjacentStruct("action", "arguments", "create_project", plain(v))
}

func (v DaemonActionRemoveProject) MarshalJSON() ([]byte, error) {
	type plain DaemonActionRemoveProject
	return daemonAdjacentStruct("action", "arguments", "remove_project", plain(v))
}

func (DaemonActionWikiSyncRequest) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "wiki_sync_request", nil)
}

func (v DaemonActionWikiSyncWatch) MarshalJSON() ([]byte, error) {
	type plain DaemonActionWikiSyncWatch
	return daemonAdjacentStruct("action", "arguments", "wiki_sync_watch", plain(v))
}

func (v DaemonActionWikiSearch) MarshalJSON() ([]byte, error) {
	type plain DaemonActionWikiSearch
	return daemonAdjacentStruct("action", "arguments", "wiki_search", plain(v))
}

func (v DaemonActionSessionTextSearch) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSessionTextSearch
	return daemonAdjacentStruct("action", "arguments", "session_text_search", plain(v))
}

func (v DaemonActionWikiBrief) MarshalJSON() ([]byte, error) {
	type plain DaemonActionWikiBrief
	return daemonAdjacentStruct("action", "arguments", "wiki_brief", plain(v))
}

func (v DaemonActionWikiHits) MarshalJSON() ([]byte, error) {
	type plain DaemonActionWikiHits
	return daemonAdjacentStruct("action", "arguments", "wiki_hits", plain(v))
}

func (v DaemonActionWikiSession) MarshalJSON() ([]byte, error) {
	type plain DaemonActionWikiSession
	return daemonAdjacentStruct("action", "arguments", "wiki_session", plain(v))
}

func (v DaemonActionWikiRestore) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("action", "arguments", "wiki_restore", v.Request)
}

func (v DaemonActionScanRecovery) MarshalJSON() ([]byte, error) {
	type plain DaemonActionScanRecovery
	return daemonAdjacentStruct("action", "arguments", "scan_recovery", plain(v))
}

func (v DaemonActionAdoptRecovery) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("action", "arguments", "adopt_recovery", v.Request)
}

func (v DaemonActionDestroyRecovery) MarshalJSON() ([]byte, error) {
	type plain DaemonActionDestroyRecovery
	return daemonAdjacentStruct("action", "arguments", "destroy_recovery", plain(v))
}

func (v DaemonActionSnapshot) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSnapshot
	return daemonAdjacentStruct("action", "arguments", "snapshot", plain(v))
}

func (v DaemonActionDashboardStartup) MarshalJSON() ([]byte, error) {
	type plain DaemonActionDashboardStartup
	return daemonAdjacentStruct("action", "arguments", "dashboard_startup", plain(v))
}

func (v DaemonActionWorkspaceLayouts) MarshalJSON() ([]byte, error) {
	type plain DaemonActionWorkspaceLayouts
	return daemonAdjacentStruct("action", "arguments", "workspace_layouts", plain(v))
}

func (DaemonActionStoredSessionMetadata) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "stored_session_metadata", nil)
}

func (v DaemonActionStoredProjectionTail) MarshalJSON() ([]byte, error) {
	type plain DaemonActionStoredProjectionTail
	return daemonAdjacentStruct("action", "arguments", "stored_projection_tail", plain(v))
}

func (v DaemonActionStoredTranscriptItem) MarshalJSON() ([]byte, error) {
	type plain DaemonActionStoredTranscriptItem
	return daemonAdjacentStruct("action", "arguments", "stored_transcript_item", plain(v))
}

func (v DaemonActionStoredAgentMessageThrough) MarshalJSON() ([]byte, error) {
	type plain DaemonActionStoredAgentMessageThrough
	return daemonAdjacentStruct("action", "arguments", "stored_agent_message_through", plain(v))
}

func (v DaemonActionStoredSessionSummary) MarshalJSON() ([]byte, error) {
	type plain DaemonActionStoredSessionSummary
	return daemonAdjacentStruct("action", "arguments", "stored_session_summary", plain(v))
}

func (v DaemonActionResumeRepositoryPreflight) MarshalJSON() ([]byte, error) {
	type plain DaemonActionResumeRepositoryPreflight
	return daemonAdjacentStruct("action", "arguments", "resume_repository_preflight", plain(v))
}

func (v DaemonActionReplaceResumeRepositoryOrigin) MarshalJSON() ([]byte, error) {
	type plain DaemonActionReplaceResumeRepositoryOrigin
	return daemonAdjacentStruct("action", "arguments", "replace_resume_repository_origin", plain(v))
}

func (v DaemonActionArchiveSpacePreview) MarshalJSON() ([]byte, error) {
	type plain DaemonActionArchiveSpacePreview
	return daemonAdjacentStruct("action", "arguments", "archive_space_preview", plain(v))
}

func (v DaemonActionSaveSetup) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSaveSetup
	return daemonAdjacentStruct("action", "arguments", "save_setup", plain(v))
}

func (v DaemonActionRuntimeChanges) MarshalJSON() ([]byte, error) {
	type plain DaemonActionRuntimeChanges
	return daemonAdjacentStruct("action", "arguments", "runtime_changes", plain(v))
}

func (v DaemonActionSessionTail) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSessionTail
	return daemonAdjacentStruct("action", "arguments", "session_tail", plain(v))
}

func (DaemonActionResumeCandidates) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "resume_candidates", nil)
}

func (v DaemonActionSessionRecord) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSessionRecord
	return daemonAdjacentStruct("action", "arguments", "session_record", plain(v))
}

func (v DaemonActionGoStartupSession) MarshalJSON() ([]byte, error) {
	type plain DaemonActionGoStartupSession
	return daemonAdjacentStruct("action", "arguments", "go_startup_session", plain(v))
}

func (DaemonActionHostCapacity) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "host_capacity", nil)
}

func (DaemonActionRefreshCapacity) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "refresh_capacity", nil)
}

func (DaemonActionRefreshQuota) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "refresh_quota", nil)
}

func (v DaemonActionProbeQuotaProfiles) MarshalJSON() ([]byte, error) {
	type plain DaemonActionProbeQuotaProfiles
	return daemonAdjacentStruct("action", "arguments", "probe_quota_profiles", plain(v))
}

func (v DaemonActionSubagentOptions) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSubagentOptions
	return daemonAdjacentStruct("action", "arguments", "subagent_options", plain(v))
}

func (v DaemonActionWarmProfileCapabilities) MarshalJSON() ([]byte, error) {
	type plain DaemonActionWarmProfileCapabilities
	return daemonAdjacentStruct("action", "arguments", "warm_profile_capabilities", plain(v))
}

func (v DaemonActionRenameProfile) MarshalJSON() ([]byte, error) {
	type plain DaemonActionRenameProfile
	return daemonAdjacentStruct("action", "arguments", "rename_profile", plain(v))
}

func (v DaemonActionRenameTarget) MarshalJSON() ([]byte, error) {
	type plain DaemonActionRenameTarget
	return daemonAdjacentStruct("action", "arguments", "rename_target", plain(v))
}

func (v DaemonActionInterruptAll) MarshalJSON() ([]byte, error) {
	type plain DaemonActionInterruptAll
	return daemonAdjacentStruct("action", "arguments", "interrupt_all", plain(v))
}

func (v DaemonActionSubmitSessionCommand) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSubmitSessionCommand
	return daemonAdjacentStruct("action", "arguments", "submit_session_command", plain(v))
}

func (v DaemonActionQueueStartupPrompt) MarshalJSON() ([]byte, error) {
	type plain DaemonActionQueueStartupPrompt
	return daemonAdjacentStruct("action", "arguments", "queue_startup_prompt", plain(v))
}

func (v DaemonActionWithdrawStartupPrompt) MarshalJSON() ([]byte, error) {
	type plain DaemonActionWithdrawStartupPrompt
	return daemonAdjacentStruct("action", "arguments", "withdraw_startup_prompt", plain(v))
}

func (v DaemonActionStartupPromptOutcomes) MarshalJSON() ([]byte, error) {
	type plain DaemonActionStartupPromptOutcomes
	return daemonAdjacentStruct("action", "arguments", "startup_prompt_outcomes", plain(v))
}

func (v DaemonActionSyncSession) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSyncSession
	return daemonAdjacentStruct("action", "arguments", "sync_session", plain(v))
}

func (v DaemonActionRespondElicitation) MarshalJSON() ([]byte, error) {
	type plain DaemonActionRespondElicitation
	return daemonAdjacentStruct("action", "arguments", "respond_elicitation", plain(v))
}

func (v DaemonActionStopBackgroundTask) MarshalJSON() ([]byte, error) {
	type plain DaemonActionStopBackgroundTask
	return daemonAdjacentStruct("action", "arguments", "stop_background_task", plain(v))
}

func (v DaemonActionStartTurnReview) MarshalJSON() ([]byte, error) {
	type plain DaemonActionStartTurnReview
	return daemonAdjacentStruct("action", "arguments", "start_turn_review", plain(v))
}

func (v DaemonActionResolveTurnReview) MarshalJSON() ([]byte, error) {
	type plain DaemonActionResolveTurnReview
	return daemonAdjacentStruct("action", "arguments", "resolve_turn_review", plain(v))
}

func (v DaemonActionStartSessionLifecycle) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("action", "arguments", "start_session_lifecycle", v.Request)
}

func (v DaemonActionWaitSessionLifecycle) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("action", "arguments", "wait_session_lifecycle", v.Receipt)
}

func (v DaemonActionSuspendSession) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSuspendSession
	return daemonAdjacentStruct("action", "arguments", "suspend_session", plain(v))
}

func (v DaemonActionSuspendPreflight) MarshalJSON() ([]byte, error) {
	type plain DaemonActionSuspendPreflight
	return daemonAdjacentStruct("action", "arguments", "suspend_preflight", plain(v))
}

func (v DaemonActionRestartSession) MarshalJSON() ([]byte, error) {
	type plain DaemonActionRestartSession
	return daemonAdjacentStruct("action", "arguments", "restart_session", plain(v))
}

func (v DaemonActionPreviewContainerUpgrade) MarshalJSON() ([]byte, error) {
	type plain DaemonActionPreviewContainerUpgrade
	return daemonAdjacentStruct("action", "arguments", "preview_container_upgrade", plain(v))
}

func (v DaemonActionUpgradeContainer) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("action", "arguments", "upgrade_container", v.Request)
}

func (v DaemonActionStartCreateSession) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("action", "arguments", "start_create_session", v.Request)
}

func (v DaemonActionWaitCreateSession) MarshalJSON() ([]byte, error) {
	type plain DaemonActionWaitCreateSession
	return daemonAdjacentStruct("action", "arguments", "wait_create_session", plain(v))
}

func (v DaemonActionResumeSession) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("action", "arguments", "resume_session", v.Request)
}

func (v DaemonActionPrepareMoveSession) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("action", "arguments", "prepare_move_session", v.Selection)
}

func (v DaemonActionMoveSession) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("action", "arguments", "move_session", v.Request)
}

func (v DaemonActionMoveSources) MarshalJSON() ([]byte, error) {
	type plain DaemonActionMoveSources
	return daemonAdjacentStruct("action", "arguments", "move_sources", plain(v))
}

func (v DaemonActionDestroyStoppedSession) MarshalJSON() ([]byte, error) {
	type plain DaemonActionDestroyStoppedSession
	return daemonAdjacentStruct("action", "arguments", "destroy_stopped_session", plain(v))
}

func (v DaemonActionForceDestroySession) MarshalJSON() ([]byte, error) {
	type plain DaemonActionForceDestroySession
	return daemonAdjacentStruct("action", "arguments", "force_destroy_session", plain(v))
}

func (v DaemonActionCancelLifecycle) MarshalJSON() ([]byte, error) {
	type plain DaemonActionCancelLifecycle
	return daemonAdjacentStruct("action", "arguments", "cancel_lifecycle", plain(v))
}

func (v DaemonActionCancelContainerUpgrade) MarshalJSON() ([]byte, error) {
	type plain DaemonActionCancelContainerUpgrade
	return daemonAdjacentStruct("action", "arguments", "cancel_container_upgrade", plain(v))
}

func (v DaemonActionRecoverDraft) MarshalJSON() ([]byte, error) {
	type plain DaemonActionRecoverDraft
	return daemonAdjacentStruct("action", "arguments", "recover_draft", plain(v))
}

func (DaemonActionStop) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("action", "arguments", "stop", nil)
}

// daemonactionVariantNames lists the wire names in declaration order, for
// unknown-variant errors.
var daemonActionVariantNames = []string{
	"dismiss_notification",
	"prove_identity",
	"login_terminal",
	"preview_build_cache",
	"install_mbx",
	"preflight_new_session",
	"validate_project_directory",
	"checkpoint_archive_sizes",
	"save_spinner_style",
	"set_quota_preferences",
	"test_target",
	"profile_login_statuses",
	"check_availability",
	"discover_setup",
	"complete_path",
	"check_project_directories",
	"discover_projects",
	"resolve_container_path",
	"resolve_setup_path",
	"validate_mount_source",
	"validate_session_mounts",
	"upload_chunk",
	"install_attachment",
	"transcription_available",
	"transcribe_audio",
	"record_operator_activity",
	"set_operator_away",
	"native_agent_history",
	"ping",
	"prepare_upgrade",
	"upgrade_blockers",
	"identity",
	"configuration",
	"first_run_setup_due",
	"run_first_run_setup",
	"run_first_run_setup_in",
	"run_doctor",
	"new_session_remote_repairs",
	"repair_repository_remotes",
	"session_project_source",
	"status",
	"web_viewer_access",
	"api_credential",
	"git_identity",
	"set_git_identity",
	"recover_web_viewer",
	"inspect_web_listener",
	"list_workspaces",
	"list_workspace_drafts",
	"create_workspace",
	"rename_workspace",
	"touch_workspace",
	"close_workspace",
	"cancel_workspace_close",
	"delete_workspace",
	"attach",
	"detach",
	"persist_read_receipt",
	"persist_terminal_draft",
	"record_abandoned_prompt",
	"save_sidebar_size",
	"save_workspace_pane_sizes",
	"save_workspace_layout",
	"import_session",
	"set_session_title",
	"set_session_notifications_ignored",
	"set_session_workspace",
	"set_session_container_settings",
	"mark_session_target_missing",
	"checkpoint_session",
	"project_catalog",
	"create_project",
	"remove_project",
	"wiki_sync_request",
	"wiki_sync_watch",
	"wiki_search",
	"session_text_search",
	"wiki_brief",
	"wiki_hits",
	"wiki_session",
	"wiki_restore",
	"scan_recovery",
	"adopt_recovery",
	"destroy_recovery",
	"snapshot",
	"dashboard_startup",
	"workspace_layouts",
	"stored_session_metadata",
	"stored_projection_tail",
	"stored_transcript_item",
	"stored_agent_message_through",
	"stored_session_summary",
	"resume_repository_preflight",
	"replace_resume_repository_origin",
	"archive_space_preview",
	"save_setup",
	"runtime_changes",
	"session_tail",
	"resume_candidates",
	"session_record",
	"go_startup_session",
	"host_capacity",
	"refresh_capacity",
	"refresh_quota",
	"probe_quota_profiles",
	"subagent_options",
	"warm_profile_capabilities",
	"rename_profile",
	"rename_target",
	"interrupt_all",
	"submit_session_command",
	"queue_startup_prompt",
	"withdraw_startup_prompt",
	"startup_prompt_outcomes",
	"sync_session",
	"respond_elicitation",
	"stop_background_task",
	"start_turn_review",
	"resolve_turn_review",
	"start_session_lifecycle",
	"wait_session_lifecycle",
	"suspend_preflight",
	"suspend_session",
	"restart_session",
	"preview_container_upgrade",
	"upgrade_container",
	"start_create_session",
	"wait_create_session",
	"resume_session",
	"prepare_move_session",
	"move_session",
	"move_sources",
	"destroy_stopped_session",
	"force_destroy_session",
	"cancel_lifecycle",
	"cancel_container_upgrade",
	"recover_draft",
	"stop",
}

// UnmarshalDaemonActionJSON decodes a DaemonAction by its "action" tag, with its
// fields under "arguments".
func UnmarshalDaemonActionJSON(data []byte) (DaemonAction, error) {
	name, body, err := daemonSplitAdjacent(data, "action", "arguments")
	if err != nil {
		return nil, err
	}
	switch name {
	case "dismiss_notification":
		return daemonStructVariant[DaemonActionDismissNotification, DaemonAction](body, "arguments")
	case "prove_identity":
		return daemonStructVariant[DaemonActionProveIdentity, DaemonAction](body, "arguments")
	case "login_terminal":
		return daemonStructVariant[DaemonActionLoginTerminal, DaemonAction](body, "arguments")
	case "preview_build_cache":
		return daemonStructVariant[DaemonActionPreviewBuildCache, DaemonAction](body, "arguments")
	case "install_mbx":
		return daemonStructVariant[DaemonActionInstallMbx, DaemonAction](body, "arguments")
	case "preflight_new_session":
		return daemonStructVariant[DaemonActionPreflightNewSession, DaemonAction](body, "arguments")
	case "validate_project_directory":
		return daemonStructVariant[DaemonActionValidateProjectDirectory, DaemonAction](body, "arguments")
	case "checkpoint_archive_sizes":
		return daemonStructVariant[DaemonActionCheckpointArchiveSizes, DaemonAction](body, "arguments")
	case "save_spinner_style":
		return daemonStructVariant[DaemonActionSaveSpinnerStyle, DaemonAction](body, "arguments")
	case "set_quota_preferences":
		return daemonStructVariant[DaemonActionSetQuotaPreferences, DaemonAction](body, "arguments")
	case "test_target":
		return daemonStructVariant[DaemonActionTestTarget, DaemonAction](body, "arguments")
	case "profile_login_statuses":
		return DaemonActionProfileLoginStatuses{}, nil
	case "check_availability":
		return DaemonActionCheckAvailability{}, nil
	case "discover_setup":
		return daemonStructVariant[DaemonActionDiscoverSetup, DaemonAction](body, "arguments")
	case "complete_path":
		return daemonStructVariant[DaemonActionCompletePath, DaemonAction](body, "arguments")
	case "check_project_directories":
		return daemonStructVariant[DaemonActionCheckProjectDirectories, DaemonAction](body, "arguments")
	case "discover_projects":
		return daemonStructVariant[DaemonActionDiscoverProjects, DaemonAction](body, "arguments")
	case "resolve_container_path":
		return daemonStructVariant[DaemonActionResolveContainerPath, DaemonAction](body, "arguments")
	case "resolve_setup_path":
		return daemonStructVariant[DaemonActionResolveSetupPath, DaemonAction](body, "arguments")
	case "validate_mount_source":
		return daemonStructVariant[DaemonActionValidateMountSource, DaemonAction](body, "arguments")
	case "validate_session_mounts":
		return daemonStructVariant[DaemonActionValidateSessionMounts, DaemonAction](body, "arguments")
	case "upload_chunk":
		return decodeDaemonActionUploadChunk(body)
	case "install_attachment":
		return daemonStructVariant[DaemonActionInstallAttachment, DaemonAction](body, "arguments")
	case "transcription_available":
		return daemonStructVariant[DaemonActionTranscriptionAvailable, DaemonAction](body, "arguments")
	case "transcribe_audio":
		return daemonStructVariant[DaemonActionTranscribeAudio, DaemonAction](body, "arguments")
	case "record_operator_activity":
		return DaemonActionRecordOperatorActivity{}, nil
	case "set_operator_away":
		return daemonStructVariant[DaemonActionSetOperatorAway, DaemonAction](body, "arguments")
	case "native_agent_history":
		return daemonStructVariant[DaemonActionNativeAgentHistory, DaemonAction](body, "arguments")
	case "ping":
		return DaemonActionPing{}, nil
	case "prepare_upgrade":
		return DaemonActionPrepareUpgrade{}, nil
	case "upgrade_blockers":
		return DaemonActionUpgradeBlockers{}, nil
	case "identity":
		return DaemonActionIdentity{}, nil
	case "configuration":
		return DaemonActionConfiguration{}, nil
	case "first_run_setup_due":
		return DaemonActionFirstRunSetupDue{}, nil
	case "run_first_run_setup":
		return DaemonActionRunFirstRunSetup{}, nil
	case "run_first_run_setup_in":
		return daemonStructVariant[DaemonActionRunFirstRunSetupIn, DaemonAction](body, "arguments")
	case "run_doctor":
		return DaemonActionRunDoctor{}, nil
	case "new_session_remote_repairs":
		return daemonStructVariant[DaemonActionNewSessionRemoteRepairs, DaemonAction](body, "arguments")
	case "repair_repository_remotes":
		return daemonStructVariant[DaemonActionRepairRepositoryRemotes, DaemonAction](body, "arguments")
	case "session_project_source":
		return daemonStructVariant[DaemonActionSessionProjectSource, DaemonAction](body, "arguments")
	case "status":
		return DaemonActionStatus{}, nil
	case "web_viewer_access":
		return DaemonActionWebViewerAccess{}, nil
	case "api_credential":
		return DaemonActionApiCredential{}, nil
	case "git_identity":
		return DaemonActionGitIdentity{}, nil
	case "set_git_identity":
		return daemonStructVariant[DaemonActionSetGitIdentity, DaemonAction](body, "arguments")
	case "recover_web_viewer":
		var v DaemonActionRecoverWebViewer
		if err := daemonDecodeRequired(body, "arguments", &v.Recovery); err != nil {
			return nil, err
		}
		return v, nil
	case "inspect_web_listener":
		return DaemonActionInspectWebListener{}, nil
	case "list_workspaces":
		return DaemonActionListWorkspaces{}, nil
	case "list_workspace_drafts":
		return DaemonActionListWorkspaceDrafts{}, nil
	case "create_workspace":
		return daemonStructVariant[DaemonActionCreateWorkspace, DaemonAction](body, "arguments")
	case "rename_workspace":
		return daemonStructVariant[DaemonActionRenameWorkspace, DaemonAction](body, "arguments")
	case "touch_workspace":
		return daemonStructVariant[DaemonActionTouchWorkspace, DaemonAction](body, "arguments")
	case "close_workspace":
		return daemonStructVariant[DaemonActionCloseWorkspace, DaemonAction](body, "arguments")
	case "cancel_workspace_close":
		return daemonStructVariant[DaemonActionCancelWorkspaceClose, DaemonAction](body, "arguments")
	case "delete_workspace":
		return daemonStructVariant[DaemonActionDeleteWorkspace, DaemonAction](body, "arguments")
	case "attach":
		return daemonStructVariant[DaemonActionAttach, DaemonAction](body, "arguments")
	case "detach":
		return daemonStructVariant[DaemonActionDetach, DaemonAction](body, "arguments")
	case "persist_read_receipt":
		return daemonStructVariant[DaemonActionPersistReadReceipt, DaemonAction](body, "arguments")
	case "persist_terminal_draft":
		return daemonStructVariant[DaemonActionPersistTerminalDraft, DaemonAction](body, "arguments")
	case "record_abandoned_prompt":
		return daemonStructVariant[DaemonActionRecordAbandonedPrompt, DaemonAction](body, "arguments")
	case "save_sidebar_size":
		return daemonStructVariant[DaemonActionSaveSidebarSize, DaemonAction](body, "arguments")
	case "save_workspace_pane_sizes":
		return daemonStructVariant[DaemonActionSaveWorkspacePaneSizes, DaemonAction](body, "arguments")
	case "save_workspace_layout":
		return daemonStructVariant[DaemonActionSaveWorkspaceLayout, DaemonAction](body, "arguments")
	case "import_session":
		return daemonStructVariant[DaemonActionImportSession, DaemonAction](body, "arguments")
	case "set_session_title":
		return daemonStructVariant[DaemonActionSetSessionTitle, DaemonAction](body, "arguments")
	case "set_session_notifications_ignored":
		return daemonStructVariant[DaemonActionSetSessionNotificationsIgnored, DaemonAction](body, "arguments")
	case "set_session_workspace":
		return daemonStructVariant[DaemonActionSetSessionWorkspace, DaemonAction](body, "arguments")
	case "set_session_container_settings":
		return daemonStructVariant[DaemonActionSetSessionContainerSettings, DaemonAction](body, "arguments")
	case "mark_session_target_missing":
		return daemonStructVariant[DaemonActionMarkSessionTargetMissing, DaemonAction](body, "arguments")
	case "checkpoint_session":
		return daemonStructVariant[DaemonActionCheckpointSession, DaemonAction](body, "arguments")
	case "project_catalog":
		return daemonStructVariant[DaemonActionProjectCatalog, DaemonAction](body, "arguments")
	case "create_project":
		return daemonStructVariant[DaemonActionCreateProject, DaemonAction](body, "arguments")
	case "remove_project":
		return daemonStructVariant[DaemonActionRemoveProject, DaemonAction](body, "arguments")
	case "wiki_sync_request":
		return DaemonActionWikiSyncRequest{}, nil
	case "wiki_sync_watch":
		return daemonStructVariant[DaemonActionWikiSyncWatch, DaemonAction](body, "arguments")
	case "wiki_search":
		return daemonStructVariant[DaemonActionWikiSearch, DaemonAction](body, "arguments")
	case "session_text_search":
		return daemonStructVariant[DaemonActionSessionTextSearch, DaemonAction](body, "arguments")
	case "wiki_brief":
		return daemonStructVariant[DaemonActionWikiBrief, DaemonAction](body, "arguments")
	case "wiki_hits":
		return daemonStructVariant[DaemonActionWikiHits, DaemonAction](body, "arguments")
	case "wiki_session":
		return daemonStructVariant[DaemonActionWikiSession, DaemonAction](body, "arguments")
	case "wiki_restore":
		var v DaemonActionWikiRestore
		if err := daemonDecodeRequired(body, "arguments", &v.Request); err != nil {
			return nil, err
		}
		return v, nil
	case "scan_recovery":
		return daemonStructVariant[DaemonActionScanRecovery, DaemonAction](body, "arguments")
	case "adopt_recovery":
		var v DaemonActionAdoptRecovery
		if err := daemonDecodeRequired(body, "arguments", &v.Request); err != nil {
			return nil, err
		}
		return v, nil
	case "destroy_recovery":
		return daemonStructVariant[DaemonActionDestroyRecovery, DaemonAction](body, "arguments")
	case "snapshot":
		return daemonStructVariant[DaemonActionSnapshot, DaemonAction](body, "arguments")
	case "dashboard_startup":
		return daemonStructVariant[DaemonActionDashboardStartup, DaemonAction](body, "arguments")
	case "workspace_layouts":
		return daemonStructVariant[DaemonActionWorkspaceLayouts, DaemonAction](body, "arguments")
	case "stored_session_metadata":
		return DaemonActionStoredSessionMetadata{}, nil
	case "stored_projection_tail":
		return daemonStructVariant[DaemonActionStoredProjectionTail, DaemonAction](body, "arguments")
	case "stored_transcript_item":
		return daemonStructVariant[DaemonActionStoredTranscriptItem, DaemonAction](body, "arguments")
	case "stored_agent_message_through":
		return daemonStructVariant[DaemonActionStoredAgentMessageThrough, DaemonAction](body, "arguments")
	case "stored_session_summary":
		return daemonStructVariant[DaemonActionStoredSessionSummary, DaemonAction](body, "arguments")
	case "resume_repository_preflight":
		return daemonStructVariant[DaemonActionResumeRepositoryPreflight, DaemonAction](body, "arguments")
	case "replace_resume_repository_origin":
		return daemonStructVariant[DaemonActionReplaceResumeRepositoryOrigin, DaemonAction](body, "arguments")
	case "archive_space_preview":
		return daemonStructVariant[DaemonActionArchiveSpacePreview, DaemonAction](body, "arguments")
	case "save_setup":
		return daemonStructVariant[DaemonActionSaveSetup, DaemonAction](body, "arguments")
	case "runtime_changes":
		return daemonStructVariant[DaemonActionRuntimeChanges, DaemonAction](body, "arguments")
	case "session_tail":
		return daemonStructVariant[DaemonActionSessionTail, DaemonAction](body, "arguments")
	case "resume_candidates":
		return DaemonActionResumeCandidates{}, nil
	case "session_record":
		return daemonStructVariant[DaemonActionSessionRecord, DaemonAction](body, "arguments")
	case "go_startup_session":
		return daemonStructVariant[DaemonActionGoStartupSession, DaemonAction](body, "arguments")
	case "host_capacity":
		return DaemonActionHostCapacity{}, nil
	case "refresh_capacity":
		return DaemonActionRefreshCapacity{}, nil
	case "refresh_quota":
		return DaemonActionRefreshQuota{}, nil
	case "probe_quota_profiles":
		return daemonStructVariant[DaemonActionProbeQuotaProfiles, DaemonAction](body, "arguments")
	case "subagent_options":
		return daemonStructVariant[DaemonActionSubagentOptions, DaemonAction](body, "arguments")
	case "warm_profile_capabilities":
		return daemonStructVariant[DaemonActionWarmProfileCapabilities, DaemonAction](body, "arguments")
	case "rename_profile":
		return daemonStructVariant[DaemonActionRenameProfile, DaemonAction](body, "arguments")
	case "rename_target":
		return daemonStructVariant[DaemonActionRenameTarget, DaemonAction](body, "arguments")
	case "interrupt_all":
		return daemonStructVariant[DaemonActionInterruptAll, DaemonAction](body, "arguments")
	case "submit_session_command":
		return daemonStructVariant[DaemonActionSubmitSessionCommand, DaemonAction](body, "arguments")
	case "queue_startup_prompt":
		return daemonStructVariant[DaemonActionQueueStartupPrompt, DaemonAction](body, "arguments")
	case "withdraw_startup_prompt":
		return daemonStructVariant[DaemonActionWithdrawStartupPrompt, DaemonAction](body, "arguments")
	case "startup_prompt_outcomes":
		return daemonStructVariant[DaemonActionStartupPromptOutcomes, DaemonAction](body, "arguments")
	case "sync_session":
		return daemonStructVariant[DaemonActionSyncSession, DaemonAction](body, "arguments")
	case "respond_elicitation":
		return daemonStructVariant[DaemonActionRespondElicitation, DaemonAction](body, "arguments")
	case "stop_background_task":
		return daemonStructVariant[DaemonActionStopBackgroundTask, DaemonAction](body, "arguments")
	case "start_turn_review":
		return daemonStructVariant[DaemonActionStartTurnReview, DaemonAction](body, "arguments")
	case "resolve_turn_review":
		return daemonStructVariant[DaemonActionResolveTurnReview, DaemonAction](body, "arguments")
	case "start_session_lifecycle":
		var v DaemonActionStartSessionLifecycle
		if err := daemonDecodeRequired(body, "arguments", &v.Request); err != nil {
			return nil, err
		}
		return v, nil
	case "wait_session_lifecycle":
		var v DaemonActionWaitSessionLifecycle
		if err := daemonDecodeRequired(body, "arguments", &v.Receipt); err != nil {
			return nil, err
		}
		return v, nil
	case "suspend_preflight":
		return daemonStructVariant[DaemonActionSuspendPreflight, DaemonAction](body, "arguments")
	case "suspend_session":
		return daemonStructVariant[DaemonActionSuspendSession, DaemonAction](body, "arguments")
	case "restart_session":
		return daemonStructVariant[DaemonActionRestartSession, DaemonAction](body, "arguments")
	case "preview_container_upgrade":
		return daemonStructVariant[DaemonActionPreviewContainerUpgrade, DaemonAction](body, "arguments")
	case "upgrade_container":
		var v DaemonActionUpgradeContainer
		if err := daemonDecodeRequired(body, "arguments", &v.Request); err != nil {
			return nil, err
		}
		return v, nil
	case "start_create_session":
		var v DaemonActionStartCreateSession
		if err := daemonDecodeRequired(body, "arguments", &v.Request); err != nil {
			return nil, err
		}
		return v, nil
	case "wait_create_session":
		return daemonStructVariant[DaemonActionWaitCreateSession, DaemonAction](body, "arguments")
	case "resume_session":
		var v DaemonActionResumeSession
		if err := daemonDecodeRequired(body, "arguments", &v.Request); err != nil {
			return nil, err
		}
		return v, nil
	case "prepare_move_session":
		var v DaemonActionPrepareMoveSession
		if err := daemonDecodeRequired(body, "arguments", &v.Selection); err != nil {
			return nil, err
		}
		return v, nil
	case "move_session":
		var v DaemonActionMoveSession
		if err := daemonDecodeRequired(body, "arguments", &v.Request); err != nil {
			return nil, err
		}
		return v, nil
	case "move_sources":
		return daemonStructVariant[DaemonActionMoveSources, DaemonAction](body, "arguments")
	case "destroy_stopped_session":
		return daemonStructVariant[DaemonActionDestroyStoppedSession, DaemonAction](body, "arguments")
	case "force_destroy_session":
		return daemonStructVariant[DaemonActionForceDestroySession, DaemonAction](body, "arguments")
	case "cancel_lifecycle":
		return daemonStructVariant[DaemonActionCancelLifecycle, DaemonAction](body, "arguments")
	case "cancel_container_upgrade":
		return daemonStructVariant[DaemonActionCancelContainerUpgrade, DaemonAction](body, "arguments")
	case "recover_draft":
		return daemonStructVariant[DaemonActionRecoverDraft, DaemonAction](body, "arguments")
	case "stop":
		return DaemonActionStop{}, nil
	}
	return nil, daemonUnknownVariant(name, daemonActionVariantNames)
}

func (v DaemonActionUploadChunk) MarshalJSON() ([]byte, error) {
	bytes, err := chunkBytesSerialize(v.Bytes)
	if err != nil {
		return nil, err
	}
	type wire struct {
		Bytes  json.RawMessage `json:"bytes"`
		Sha256 *string         `json:"sha256"`
	}
	return daemonAdjacentStruct("action", "arguments", "upload_chunk", wire{Bytes: bytes, Sha256: v.Sha256})
}

func decodeDaemonActionUploadChunk(body json.RawMessage) (DaemonAction, error) {
	var wire struct {
		Bytes  json.RawMessage `json:"bytes"`
		Sha256 *string         `json:"sha256"`
	}
	if body == nil {
		return nil, daemonMissingField("arguments")
	}
	if err := daemonDecodeStruct(body, &wire, false); err != nil {
		return nil, err
	}
	bytes, err := chunkBytesDeserialize(wire.Bytes)
	if err != nil {
		return nil, err
	}
	return DaemonActionUploadChunk{Bytes: bytes, Sha256: wire.Sha256}, nil
}

type RequestEnvelope struct {
	ProtocolVersion uint32       `json:"protocol_version"`
	RequestID       uint64       `json:"request_id"`
	Token           string       `json:"token"`
	Action          DaemonAction `json:"action"`
}

func (v RequestEnvelope) MarshalJSON() ([]byte, error) {
	type plain RequestEnvelope
	return daemonMarshalStruct(plain(v))
}

func (v *RequestEnvelope) UnmarshalJSON(data []byte) error {
	type plain RequestEnvelope
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// DaemonRefusal: the daemon answered a request with an error. Unlike a lost
// connection, this is a complete round trip: the daemon read the request and
// said why it did not carry it out.
type DaemonRefusal struct {
	Message string
}

func (e DaemonRefusal) Error() string { return e.Message }

// DeliveryUnconfirmed reports whether the daemon itself could not confirm
// delivery. Its message is then led by session's DeliveryUnconfirmed.
func (e DaemonRefusal) DeliveryUnconfirmed() bool {
	return strings.HasPrefix(e.Message, DeliveryUnconfirmed{}.Error())
}

// DaemonTransportLost: the connection to the daemon dropped after the request
// was sent and before an answer arrived. The daemon may or may not have acted
// on it, so only a read-only request may be repeated on that evidence.
// Failing to connect, a protocol mismatch and a refusal are different errors
// and never carry this.
type DaemonTransportLost struct{}

func (DaemonTransportLost) Error() string {
	return "lost the daemon connection before it answered"
}

// transportLost marks a failure of the established stream as a lost
// transport. An error that did not come from the socket (an oversized frame,
// an undecodable reply) stays as it is.
func transportLost(err error) error {
	if err == nil || !daemonIsIOError(err) {
		return err
	}
	return fmt.Errorf("%w: %w", DaemonTransportLost{}, err)
}

// daemonIsIOError is Rust's `error.chain().any(|cause| cause.is::<std::io::Error>())`:
// whether the failure came from the socket.
func daemonIsIOError(err error) bool {
	var netError net.Error
	var syscallError *os.SyscallError
	var pathError *fs.PathError
	return errors.Is(err, io.EOF) || errors.Is(err, io.ErrUnexpectedEOF) || errors.Is(err, net.ErrClosed) ||
		errors.As(err, &netError) || errors.As(err, &syscallError) || errors.As(err, &pathError)
}

// StartupPromptIdentityConflict: an existing startup ID belongs to another
// immutable request. Its original work may already be accepted, so this never
// proves that work was refused.
type StartupPromptIdentityConflict struct{}

func (StartupPromptIdentityConflict) Error() string {
	return "startup command ID was reused with a different payload"
}

type ResponseEnvelope struct {
	ProtocolVersion uint32 `json:"protocol_version"`
	RequestID       uint64 `json:"request_id"`
	// Result is the reply, or the daemon's refusal text.
	Result DaemonResult[DaemonReply] `json:"result"`
}

func (v ResponseEnvelope) MarshalJSON() ([]byte, error) {
	type plain ResponseEnvelope
	return daemonMarshalStruct(plain(v))
}

func (v *ResponseEnvelope) UnmarshalJSON(data []byte) error {
	type plain ResponseEnvelope
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// DaemonReply is the daemon's answer to one DaemonAction. Adjacently tagged
// by "reply" with its value under "value", in snake_case.
type DaemonReply interface {
	isDaemonReply()
}

type DaemonReplyInterruptedAll struct {
	Result InterruptAllResult
}

type DaemonReplyAdoptionInstalled struct {
	SessionID string `json:"session_id"`
}

type DaemonReplyAdoptionWaiting struct {
	Blockers       []string `json:"blockers"`
	DestroyCommand *string  `json:"destroy_command,omitempty" serde:"default"`
}

type DaemonReplyIdentityProof struct {
	// Mac is a JSON array of numbers, as serde writes a Vec<u8>.
	Mac []byte `json:"mac"`
}

type DaemonReplyLoginTerminalReady struct{}

type DaemonReplyBuildCachePreview struct {
	Preview   *core.BuildCachePreview
	Available bool
}

type DaemonReplyMbxInstalled struct {
	Message string
}

type DaemonReplyCheckpointArchiveSizes struct {
	Sizes map[string]DaemonResult[*uint64]
}

type DaemonReplySetupDiscovery struct {
	Detection SetupDetection
}

type DaemonReplyProfileLoginStatuses struct {
	Statuses map[string]core.LoginStatus
}

type DaemonReplyNewSessionPreflight struct {
	Preflight PreflightNew
}

type DaemonReplyPathCompletion struct {
	Completion core.PathCompletion
}

type DaemonReplyProjectDirectories struct {
	Directories []DaemonPair[string, bool]
}

type DaemonReplyProjectDiscovery struct {
	Discovery core.ProjectDiscovery
}

type DaemonReplyResolvedPath struct {
	Path string
}

type DaemonReplyMountValidation struct {
	Path   string
	Forced *string
}

// DaemonReplySessionMountValidation carries the first mount source that failed and why, or nil when
// every mount is valid.
type DaemonReplySessionMountValidation struct {
	Failure *[2]string
}

type DaemonReplyTranscriptionAvailable struct {
	Available bool
}

type DaemonReplyTranscription struct {
	Text string
}

type DaemonReplyBlobUploaded struct {
	Blob string
}

type DaemonReplyAttachmentInstalled struct {
	Reference core.AttachmentRef
}

type DaemonReplyLifecycleAccepted struct {
	Receipt LifecycleReceipt
}

// DaemonReplyLifecycleFailed is the failure of a captured operation result; only such a
// result may report it.
type DaemonReplyLifecycleFailed struct {
	Detail string
}

type DaemonReplyLifecycleObservationUnavailable struct {
	Receipt LifecycleReceipt
}

type DaemonReplyNativeAgentHistory struct {
	Page core.NativeAgentHistoryPage
}

type DaemonReplyPong struct{}

type DaemonReplyUpgradePending struct{}

// DaemonReplyUpgradeBlockers labels the work holding an automatic handoff open, empty when
// nothing is. It answers DaemonActionUpgradeBlockers.
type DaemonReplyUpgradeBlockers struct {
	Blockers []string
}

type DaemonReplyIdentity struct {
	Identity DaemonIdentity
}

type DaemonReplyConfiguration struct {
	Config core.Config
}

type DaemonReplyFirstRunSetupDue struct {
	Due bool
}

type DaemonReplyFirstRunSetup struct {
	Report *FirstRunSetupReport
}

type DaemonReplyDoctorErrors struct {
	Errors []string
}

type DaemonReplyRemoteRepairs struct {
	Repairs []core.LocalRemoteRepair
}

type DaemonReplyProjectSource struct {
	Source core.ProjectSourceIdentity
}

type DaemonReplyStatus struct {
	Status DaemonStatus
}

type DaemonReplyWebViewerAccess struct {
	Access WebViewerAccess
}

type DaemonReplyApiCredential struct {
	Token string
}

type DaemonReplyGitIdentity struct {
	Identity GitIdentity
}

type DaemonReplyWebListeners struct {
	Processes []WebListenerProcess
}

type DaemonReplySubagentOptions struct {
	Options core.SubagentOptions
}

type DaemonReplyWorkspaces struct {
	Workspaces []WorkspaceListing
}

type DaemonReplyWorkspace struct {
	Workspace core.WorkspaceRecord
}

// DaemonReplyWorkspaceDrafts is every workspace's detached drafts, without their text.
type DaemonReplyWorkspaceDrafts struct {
	Drafts []WorkspaceDrafts
}

type DaemonReplyHostCapacity struct {
	Rows []core.HostCapacityRow
}

type DaemonReplySnapshot struct {
	Snapshot WorkspaceSnapshot
}

type DaemonReplyDashboardStartup struct {
	Startup DashboardStartup
}

type DaemonReplyWorkspaceLayouts struct {
	Layouts WorkspaceLayouts
}

type DaemonReplyStoredSessionMetadata struct {
	Metadata StoredSessionMetadata
}

// DaemonReplyStoredProjectionTail is the legacy count-mode reply: older clients require it to
// stay unchunked. It is written through serializeClientProjection.
type DaemonReplyStoredProjectionTail struct {
	Tail *core.MaterializedSession
}

type DaemonReplyStoredSessionSummary struct {
	Summary *core.MaterializedSessionSummary
}

type DaemonReplyStoredTranscriptItem struct {
	Item *core.TranscriptItem
}

type DaemonReplySavedSetup struct {
	Config core.Config
}

type DaemonReplyArchiveSpacePreview struct {
	Preview core.ArchiveSpacePreview
}

type DaemonReplyResumeRepositoryPreflight struct {
	Preflight core.ResumeRepositorySourcePreflight
}

type DaemonReplyRuntimeChanges struct {
	Frame RuntimeFrame
}

type DaemonReplySessionTail struct {
	Reply SessionTailReply
}

type DaemonReplyResumeCandidates struct {
	Candidates ResumeCandidates
}

type DaemonReplyGoStartupSession struct {
	Record *core.SessionRecord
}

type DaemonReplySessionRecord struct {
	Record *SessionRecordWithVerdict
}

// DaemonReplyReplyChunk is one transport fragment of a chunked reply (see
// DaemonReplyIsChunked); never exposed to consumers. Bytes travel as one
// base64 string (daemon_chunk_bytes.go).
type DaemonReplyReplyChunk struct {
	Bytes    []byte `json:"bytes"`
	Finished bool   `json:"finished"`
}

type DaemonReplyRegisteredSession struct {
	Registered RegisteredSession
}

type DaemonReplyContainerUpgradePreview struct {
	Preview core.ContainerUpgradePreview
}

type DaemonReplySuspendPreflight struct {
	Preflight core.SuspendPreflight
}

type DaemonReplyMovePreparation struct {
	Preparation core.MovePreparation
}

type DaemonReplyMoveOutcome struct {
	Outcome core.MoveOutcome
}

type DaemonReplyMoveSources struct {
	Sources []core.RetainedMoveSource
}

type DaemonReplyOrdinal struct {
	Ordinal uint64
}

type DaemonReplyText struct {
	Text string
}

type DaemonReplyOptionalSessionState struct {
	State *core.SessionState
}

type DaemonReplyRecoveryScan struct {
	Scan core.RecoveryScan
}

type DaemonReplyWikiRows struct {
	Page WikiSearchPage
}

type DaemonReplyWikiSyncStatus struct {
	Status WikiSyncStatus
}

type DaemonReplyProjectCatalog struct {
	View core.ProjectCatalogView
}

type DaemonReplyProjectCreated struct {
	Project core.SavedProject
}

type DaemonReplySessionTextMatches struct {
	Page SessionTextSearchPage
}

type DaemonReplyWikiHits struct {
	Transcript *WikiHitTranscript
}

type DaemonReplyWikiSession struct {
	Info *WikiSessionInfo
}

// DaemonReplyPromptWithdrawn says whether a queued startup prompt was withdrawn before
// delivery.
type DaemonReplyPromptWithdrawn struct {
	Withdrawn bool
}

type DaemonReplyStartupPromptIdentityConflict struct{}

type DaemonReplyStartupPromptOutcomes struct {
	Outcomes map[string]StartupPromptOutcome
}

type DaemonReplyDone struct{}

func (DaemonReplyInterruptedAll) isDaemonReply()                  {}
func (DaemonReplyAdoptionInstalled) isDaemonReply()               {}
func (DaemonReplyAdoptionWaiting) isDaemonReply()                 {}
func (DaemonReplyIdentityProof) isDaemonReply()                   {}
func (DaemonReplyLoginTerminalReady) isDaemonReply()              {}
func (DaemonReplyBuildCachePreview) isDaemonReply()               {}
func (DaemonReplyMbxInstalled) isDaemonReply()                    {}
func (DaemonReplyCheckpointArchiveSizes) isDaemonReply()          {}
func (DaemonReplySetupDiscovery) isDaemonReply()                  {}
func (DaemonReplyProfileLoginStatuses) isDaemonReply()            {}
func (DaemonReplyNewSessionPreflight) isDaemonReply()             {}
func (DaemonReplyPathCompletion) isDaemonReply()                  {}
func (DaemonReplyProjectDirectories) isDaemonReply()              {}
func (DaemonReplyProjectDiscovery) isDaemonReply()                {}
func (DaemonReplyResolvedPath) isDaemonReply()                    {}
func (DaemonReplyMountValidation) isDaemonReply()                 {}
func (DaemonReplySessionMountValidation) isDaemonReply()          {}
func (DaemonReplyTranscriptionAvailable) isDaemonReply()          {}
func (DaemonReplyTranscription) isDaemonReply()                   {}
func (DaemonReplyBlobUploaded) isDaemonReply()                    {}
func (DaemonReplyAttachmentInstalled) isDaemonReply()             {}
func (DaemonReplyLifecycleAccepted) isDaemonReply()               {}
func (DaemonReplyLifecycleFailed) isDaemonReply()                 {}
func (DaemonReplyLifecycleObservationUnavailable) isDaemonReply() {}
func (DaemonReplyNativeAgentHistory) isDaemonReply()              {}
func (DaemonReplyPong) isDaemonReply()                            {}
func (DaemonReplyUpgradePending) isDaemonReply()                  {}
func (DaemonReplyUpgradeBlockers) isDaemonReply()                 {}
func (DaemonReplyIdentity) isDaemonReply()                        {}
func (DaemonReplyConfiguration) isDaemonReply()                   {}
func (DaemonReplyFirstRunSetupDue) isDaemonReply()                {}
func (DaemonReplyFirstRunSetup) isDaemonReply()                   {}
func (DaemonReplyDoctorErrors) isDaemonReply()                    {}
func (DaemonReplyRemoteRepairs) isDaemonReply()                   {}
func (DaemonReplyProjectSource) isDaemonReply()                   {}
func (DaemonReplyStatus) isDaemonReply()                          {}
func (DaemonReplyWebViewerAccess) isDaemonReply()                 {}
func (DaemonReplyApiCredential) isDaemonReply()                   {}
func (DaemonReplyGitIdentity) isDaemonReply()                     {}
func (DaemonReplyWebListeners) isDaemonReply()                    {}
func (DaemonReplySubagentOptions) isDaemonReply()                 {}
func (DaemonReplyWorkspaces) isDaemonReply()                      {}
func (DaemonReplyWorkspace) isDaemonReply()                       {}
func (DaemonReplyWorkspaceDrafts) isDaemonReply()                 {}
func (DaemonReplyHostCapacity) isDaemonReply()                    {}
func (DaemonReplySnapshot) isDaemonReply()                        {}
func (DaemonReplyDashboardStartup) isDaemonReply()                {}
func (DaemonReplyWorkspaceLayouts) isDaemonReply()                {}
func (DaemonReplyStoredSessionMetadata) isDaemonReply()           {}
func (DaemonReplyStoredProjectionTail) isDaemonReply()            {}
func (DaemonReplyStoredSessionSummary) isDaemonReply()            {}
func (DaemonReplyStoredTranscriptItem) isDaemonReply()            {}
func (DaemonReplySavedSetup) isDaemonReply()                      {}
func (DaemonReplyArchiveSpacePreview) isDaemonReply()             {}
func (DaemonReplyResumeRepositoryPreflight) isDaemonReply()       {}
func (DaemonReplyRuntimeChanges) isDaemonReply()                  {}
func (DaemonReplySessionTail) isDaemonReply()                     {}
func (DaemonReplyResumeCandidates) isDaemonReply()                {}
func (DaemonReplyGoStartupSession) isDaemonReply()                {}
func (DaemonReplySessionRecord) isDaemonReply()                   {}
func (DaemonReplyReplyChunk) isDaemonReply()                      {}
func (DaemonReplyRegisteredSession) isDaemonReply()               {}
func (DaemonReplyContainerUpgradePreview) isDaemonReply()         {}
func (DaemonReplySuspendPreflight) isDaemonReply()                {}
func (DaemonReplyMovePreparation) isDaemonReply()                 {}
func (DaemonReplyMoveOutcome) isDaemonReply()                     {}
func (DaemonReplyMoveSources) isDaemonReply()                     {}
func (DaemonReplyOrdinal) isDaemonReply()                         {}
func (DaemonReplyText) isDaemonReply()                            {}
func (DaemonReplyOptionalSessionState) isDaemonReply()            {}
func (DaemonReplyRecoveryScan) isDaemonReply()                    {}
func (DaemonReplyWikiRows) isDaemonReply()                        {}
func (DaemonReplyWikiSyncStatus) isDaemonReply()                  {}
func (DaemonReplyProjectCatalog) isDaemonReply()                  {}
func (DaemonReplyProjectCreated) isDaemonReply()                  {}
func (DaemonReplySessionTextMatches) isDaemonReply()              {}
func (DaemonReplyWikiHits) isDaemonReply()                        {}
func (DaemonReplyWikiSession) isDaemonReply()                     {}
func (DaemonReplyPromptWithdrawn) isDaemonReply()                 {}
func (DaemonReplyStartupPromptIdentityConflict) isDaemonReply()   {}
func (DaemonReplyStartupPromptOutcomes) isDaemonReply()           {}
func (DaemonReplyDone) isDaemonReply()                            {}

func (v DaemonReplyInterruptedAll) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "interrupted_all", v.Result)
}

func (v DaemonReplyAdoptionInstalled) MarshalJSON() ([]byte, error) {
	type plain DaemonReplyAdoptionInstalled
	return daemonAdjacentStruct("reply", "value", "adoption_installed", plain(v))
}

func (v DaemonReplyAdoptionWaiting) MarshalJSON() ([]byte, error) {
	type plain DaemonReplyAdoptionWaiting
	return daemonAdjacentStruct("reply", "value", "adoption_waiting", plain(v))
}

func (DaemonReplyLoginTerminalReady) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("reply", "value", "login_terminal_ready", nil)
}

func (v DaemonReplyBuildCachePreview) MarshalJSON() ([]byte, error) {
	return daemonAdjacentTuple("reply", "value", "build_cache_preview", v.Preview, v.Available)
}

func (v DaemonReplyMbxInstalled) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "mbx_installed", v.Message)
}

func (v DaemonReplyCheckpointArchiveSizes) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "checkpoint_archive_sizes", v.Sizes)
}

func (v DaemonReplySetupDiscovery) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "setup_discovery", v.Detection)
}

func (v DaemonReplyProfileLoginStatuses) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "profile_login_statuses", v.Statuses)
}

func (v DaemonReplyNewSessionPreflight) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "new_session_preflight", v.Preflight)
}

func (v DaemonReplyPathCompletion) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "path_completion", v.Completion)
}

func (v DaemonReplyProjectDirectories) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "project_directories", v.Directories)
}

func (v DaemonReplyProjectDiscovery) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "project_discovery", v.Discovery)
}

func (v DaemonReplyResolvedPath) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "resolved_path", v.Path)
}

func (v DaemonReplyMountValidation) MarshalJSON() ([]byte, error) {
	return daemonAdjacentTuple("reply", "value", "mount_validation", v.Path, v.Forced)
}

func (v DaemonReplySessionMountValidation) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "session_mount_validation", v.Failure)
}

func (v DaemonReplyTranscriptionAvailable) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "transcription_available", v.Available)
}

func (v DaemonReplyTranscription) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "transcription", v.Text)
}

func (v DaemonReplyBlobUploaded) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "blob_uploaded", v.Blob)
}

func (v DaemonReplyAttachmentInstalled) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "attachment_installed", v.Reference)
}

func (v DaemonReplyLifecycleAccepted) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "lifecycle_accepted", v.Receipt)
}

func (v DaemonReplyLifecycleFailed) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "lifecycle_failed", v.Detail)
}

func (v DaemonReplyLifecycleObservationUnavailable) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "lifecycle_observation_unavailable", v.Receipt)
}

func (v DaemonReplyNativeAgentHistory) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "native_agent_history", v.Page)
}

func (DaemonReplyPong) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("reply", "value", "pong", nil)
}

func (DaemonReplyUpgradePending) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("reply", "value", "upgrade_pending", nil)
}

func (v DaemonReplyUpgradeBlockers) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "upgrade_blockers", v.Blockers)
}

func (v DaemonReplyIdentity) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "identity", v.Identity)
}

func (v DaemonReplyConfiguration) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "configuration", v.Config)
}

func (v DaemonReplyFirstRunSetupDue) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "first_run_setup_due", v.Due)
}

func (v DaemonReplyFirstRunSetup) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "first_run_setup", v.Report)
}

func (v DaemonReplyDoctorErrors) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "doctor_errors", v.Errors)
}

func (v DaemonReplyRemoteRepairs) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "remote_repairs", v.Repairs)
}

func (v DaemonReplyProjectSource) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "project_source", v.Source)
}

func (v DaemonReplyStatus) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "status", v.Status)
}

func (v DaemonReplyWebViewerAccess) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "web_viewer_access", v.Access)
}

func (v DaemonReplyApiCredential) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "api_credential", v.Token)
}

func (v DaemonReplyGitIdentity) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "git_identity", v.Identity)
}

func (v DaemonReplyWebListeners) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "web_listeners", v.Processes)
}

func (v DaemonReplySubagentOptions) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "subagent_options", v.Options)
}

func (v DaemonReplyWorkspaces) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "workspaces", v.Workspaces)
}

func (v DaemonReplyWorkspace) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "workspace", v.Workspace)
}

func (v DaemonReplyWorkspaceDrafts) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "workspace_drafts", v.Drafts)
}

func (v DaemonReplyHostCapacity) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "host_capacity", v.Rows)
}

func (v DaemonReplySnapshot) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "snapshot", v.Snapshot)
}

func (v DaemonReplyDashboardStartup) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "dashboard_startup", v.Startup)
}

func (v DaemonReplyWorkspaceLayouts) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "workspace_layouts", v.Layouts)
}

func (v DaemonReplyStoredSessionMetadata) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "stored_session_metadata", v.Metadata)
}

func (v DaemonReplyStoredSessionSummary) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "stored_session_summary", v.Summary)
}

func (v DaemonReplyStoredTranscriptItem) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "stored_transcript_item", v.Item)
}

func (v DaemonReplySavedSetup) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "saved_setup", v.Config)
}

func (v DaemonReplyArchiveSpacePreview) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "archive_space_preview", v.Preview)
}

func (v DaemonReplyResumeRepositoryPreflight) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "resume_repository_preflight", v.Preflight)
}

func (v DaemonReplyRuntimeChanges) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "runtime_changes", v.Frame)
}

func (v DaemonReplySessionTail) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "session_tail", v.Reply)
}

func (v DaemonReplyResumeCandidates) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "resume_candidates", v.Candidates)
}

func (v DaemonReplyGoStartupSession) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "go_startup_session", v.Record)
}

func (v DaemonReplySessionRecord) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "session_record", v.Record)
}

func (v DaemonReplyRegisteredSession) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "registered_session", v.Registered)
}

func (v DaemonReplyContainerUpgradePreview) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "container_upgrade_preview", v.Preview)
}

func (v DaemonReplySuspendPreflight) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "suspend_preflight", v.Preflight)
}

func (v DaemonReplyMovePreparation) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "move_preparation", v.Preparation)
}

func (v DaemonReplyMoveOutcome) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "move_outcome", v.Outcome)
}

func (v DaemonReplyMoveSources) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "move_sources", v.Sources)
}

func (v DaemonReplyOrdinal) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "ordinal", v.Ordinal)
}

func (v DaemonReplyText) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "text", v.Text)
}

func (v DaemonReplyOptionalSessionState) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "optional_session_state", v.State)
}

func (v DaemonReplyRecoveryScan) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "recovery_scan", v.Scan)
}

func (v DaemonReplyWikiRows) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "wiki_rows", v.Page)
}

func (v DaemonReplyWikiSyncStatus) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "wiki_sync_status", v.Status)
}

func (v DaemonReplyProjectCatalog) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "project_catalog", v.View)
}

func (v DaemonReplyProjectCreated) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "project_created", v.Project)
}

func (v DaemonReplySessionTextMatches) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "session_text_matches", v.Page)
}

func (v DaemonReplyWikiHits) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "wiki_hits", v.Transcript)
}

func (v DaemonReplyWikiSession) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "wiki_session", v.Info)
}

func (v DaemonReplyPromptWithdrawn) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "prompt_withdrawn", v.Withdrawn)
}

func (DaemonReplyStartupPromptIdentityConflict) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("reply", "value", "startup_prompt_identity_conflict", nil)
}

func (v DaemonReplyStartupPromptOutcomes) MarshalJSON() ([]byte, error) {
	return daemonAdjacentValue("reply", "value", "startup_prompt_outcomes", v.Outcomes)
}

func (DaemonReplyDone) MarshalJSON() ([]byte, error) {
	return daemonAdjacentJSON("reply", "value", "done", nil)
}

// daemonreplyVariantNames lists the wire names in declaration order, for
// unknown-variant errors.
var daemonReplyVariantNames = []string{
	"interrupted_all",
	"adoption_installed",
	"adoption_waiting",
	"identity_proof",
	"login_terminal_ready",
	"build_cache_preview",
	"mbx_installed",
	"checkpoint_archive_sizes",
	"setup_discovery",
	"profile_login_statuses",
	"new_session_preflight",
	"path_completion",
	"project_directories",
	"project_discovery",
	"resolved_path",
	"mount_validation",
	"session_mount_validation",
	"transcription_available",
	"transcription",
	"blob_uploaded",
	"attachment_installed",
	"lifecycle_accepted",
	"lifecycle_failed",
	"lifecycle_observation_unavailable",
	"native_agent_history",
	"pong",
	"upgrade_pending",
	"upgrade_blockers",
	"identity",
	"configuration",
	"first_run_setup_due",
	"first_run_setup",
	"doctor_errors",
	"remote_repairs",
	"project_source",
	"status",
	"web_viewer_access",
	"api_credential",
	"git_identity",
	"web_listeners",
	"subagent_options",
	"workspaces",
	"workspace",
	"workspace_drafts",
	"host_capacity",
	"snapshot",
	"dashboard_startup",
	"workspace_layouts",
	"stored_session_metadata",
	"stored_projection_tail",
	"stored_session_summary",
	"stored_transcript_item",
	"saved_setup",
	"archive_space_preview",
	"resume_repository_preflight",
	"runtime_changes",
	"session_tail",
	"resume_candidates",
	"go_startup_session",
	"session_record",
	"reply_chunk",
	"registered_session",
	"container_upgrade_preview",
	"suspend_preflight",
	"move_preparation",
	"move_outcome",
	"move_sources",
	"ordinal",
	"text",
	"optional_session_state",
	"recovery_scan",
	"wiki_rows",
	"wiki_sync_status",
	"project_catalog",
	"project_created",
	"session_text_matches",
	"wiki_hits",
	"wiki_session",
	"prompt_withdrawn",
	"startup_prompt_identity_conflict",
	"startup_prompt_outcomes",
	"done",
}

// UnmarshalDaemonReplyJSON decodes a DaemonReply by its "reply" tag, with its
// fields under "value".
func UnmarshalDaemonReplyJSON(data []byte) (DaemonReply, error) {
	name, body, err := daemonSplitAdjacent(data, "reply", "value")
	if err != nil {
		return nil, err
	}
	switch name {
	case "interrupted_all":
		var v DaemonReplyInterruptedAll
		if err := daemonDecodeRequired(body, "value", &v.Result); err != nil {
			return nil, err
		}
		return v, nil
	case "adoption_installed":
		return daemonStructVariant[DaemonReplyAdoptionInstalled, DaemonReply](body, "value")
	case "adoption_waiting":
		return daemonStructVariant[DaemonReplyAdoptionWaiting, DaemonReply](body, "value")
	case "identity_proof":
		return decodeDaemonReplyIdentityProof(body)
	case "login_terminal_ready":
		return DaemonReplyLoginTerminalReady{}, nil
	case "build_cache_preview":
		var v DaemonReplyBuildCachePreview
		if err := daemonDecodeTuple(body, "value", &v.Preview, &v.Available); err != nil {
			return nil, err
		}
		return v, nil
	case "mbx_installed":
		var v DaemonReplyMbxInstalled
		if err := daemonDecodeRequired(body, "value", &v.Message); err != nil {
			return nil, err
		}
		return v, nil
	case "checkpoint_archive_sizes":
		var v DaemonReplyCheckpointArchiveSizes
		if err := daemonDecodeRequired(body, "value", &v.Sizes); err != nil {
			return nil, err
		}
		return v, nil
	case "setup_discovery":
		var v DaemonReplySetupDiscovery
		if err := daemonDecodeRequired(body, "value", &v.Detection); err != nil {
			return nil, err
		}
		return v, nil
	case "profile_login_statuses":
		var v DaemonReplyProfileLoginStatuses
		if err := daemonDecodeRequired(body, "value", &v.Statuses); err != nil {
			return nil, err
		}
		return v, nil
	case "new_session_preflight":
		var v DaemonReplyNewSessionPreflight
		if err := daemonDecodeRequired(body, "value", &v.Preflight); err != nil {
			return nil, err
		}
		return v, nil
	case "path_completion":
		var v DaemonReplyPathCompletion
		if err := daemonDecodeRequired(body, "value", &v.Completion); err != nil {
			return nil, err
		}
		return v, nil
	case "project_directories":
		var v DaemonReplyProjectDirectories
		if err := daemonDecodeRequired(body, "value", &v.Directories); err != nil {
			return nil, err
		}
		return v, nil
	case "project_discovery":
		var v DaemonReplyProjectDiscovery
		if err := daemonDecodeRequired(body, "value", &v.Discovery); err != nil {
			return nil, err
		}
		return v, nil
	case "resolved_path":
		var v DaemonReplyResolvedPath
		if err := daemonDecodeRequired(body, "value", &v.Path); err != nil {
			return nil, err
		}
		return v, nil
	case "mount_validation":
		var v DaemonReplyMountValidation
		if err := daemonDecodeTuple(body, "value", &v.Path, &v.Forced); err != nil {
			return nil, err
		}
		return v, nil
	case "session_mount_validation":
		var v DaemonReplySessionMountValidation
		if err := daemonDecodeValue(body, &v.Failure); err != nil {
			return nil, err
		}
		return v, nil
	case "transcription_available":
		var v DaemonReplyTranscriptionAvailable
		if err := daemonDecodeRequired(body, "value", &v.Available); err != nil {
			return nil, err
		}
		return v, nil
	case "transcription":
		var v DaemonReplyTranscription
		if err := daemonDecodeRequired(body, "value", &v.Text); err != nil {
			return nil, err
		}
		return v, nil
	case "blob_uploaded":
		var v DaemonReplyBlobUploaded
		if err := daemonDecodeRequired(body, "value", &v.Blob); err != nil {
			return nil, err
		}
		return v, nil
	case "attachment_installed":
		var v DaemonReplyAttachmentInstalled
		if err := daemonDecodeRequired(body, "value", &v.Reference); err != nil {
			return nil, err
		}
		return v, nil
	case "lifecycle_accepted":
		var v DaemonReplyLifecycleAccepted
		if err := daemonDecodeRequired(body, "value", &v.Receipt); err != nil {
			return nil, err
		}
		return v, nil
	case "lifecycle_failed":
		var v DaemonReplyLifecycleFailed
		if err := daemonDecodeRequired(body, "value", &v.Detail); err != nil {
			return nil, err
		}
		return v, nil
	case "lifecycle_observation_unavailable":
		var v DaemonReplyLifecycleObservationUnavailable
		if err := daemonDecodeRequired(body, "value", &v.Receipt); err != nil {
			return nil, err
		}
		return v, nil
	case "native_agent_history":
		var v DaemonReplyNativeAgentHistory
		if err := daemonDecodeRequired(body, "value", &v.Page); err != nil {
			return nil, err
		}
		return v, nil
	case "pong":
		return DaemonReplyPong{}, nil
	case "upgrade_pending":
		return DaemonReplyUpgradePending{}, nil
	case "upgrade_blockers":
		var v DaemonReplyUpgradeBlockers
		if err := daemonDecodeRequired(body, "value", &v.Blockers); err != nil {
			return nil, err
		}
		return v, nil
	case "identity":
		var v DaemonReplyIdentity
		if err := daemonDecodeRequired(body, "value", &v.Identity); err != nil {
			return nil, err
		}
		return v, nil
	case "configuration":
		var v DaemonReplyConfiguration
		if err := daemonDecodeRequired(body, "value", &v.Config); err != nil {
			return nil, err
		}
		return v, nil
	case "first_run_setup_due":
		var v DaemonReplyFirstRunSetupDue
		if err := daemonDecodeRequired(body, "value", &v.Due); err != nil {
			return nil, err
		}
		return v, nil
	case "first_run_setup":
		var v DaemonReplyFirstRunSetup
		if err := daemonDecodeValue(body, &v.Report); err != nil {
			return nil, err
		}
		return v, nil
	case "doctor_errors":
		var v DaemonReplyDoctorErrors
		if err := daemonDecodeRequired(body, "value", &v.Errors); err != nil {
			return nil, err
		}
		return v, nil
	case "remote_repairs":
		var v DaemonReplyRemoteRepairs
		if err := daemonDecodeRequired(body, "value", &v.Repairs); err != nil {
			return nil, err
		}
		return v, nil
	case "project_source":
		var v DaemonReplyProjectSource
		if err := daemonDecodeRequired(body, "value", &v.Source); err != nil {
			return nil, err
		}
		return v, nil
	case "status":
		var v DaemonReplyStatus
		if err := daemonDecodeRequired(body, "value", &v.Status); err != nil {
			return nil, err
		}
		return v, nil
	case "web_viewer_access":
		var v DaemonReplyWebViewerAccess
		if err := daemonDecodeRequired(body, "value", &v.Access); err != nil {
			return nil, err
		}
		return v, nil
	case "api_credential":
		var v DaemonReplyApiCredential
		if err := daemonDecodeRequired(body, "value", &v.Token); err != nil {
			return nil, err
		}
		return v, nil
	case "git_identity":
		var v DaemonReplyGitIdentity
		if err := daemonDecodeRequired(body, "value", &v.Identity); err != nil {
			return nil, err
		}
		return v, nil
	case "web_listeners":
		var v DaemonReplyWebListeners
		if err := daemonDecodeRequired(body, "value", &v.Processes); err != nil {
			return nil, err
		}
		return v, nil
	case "subagent_options":
		var v DaemonReplySubagentOptions
		if err := daemonDecodeRequired(body, "value", &v.Options); err != nil {
			return nil, err
		}
		return v, nil
	case "workspaces":
		var v DaemonReplyWorkspaces
		if err := daemonDecodeRequired(body, "value", &v.Workspaces); err != nil {
			return nil, err
		}
		return v, nil
	case "workspace":
		var v DaemonReplyWorkspace
		if err := daemonDecodeRequired(body, "value", &v.Workspace); err != nil {
			return nil, err
		}
		return v, nil
	case "workspace_drafts":
		var v DaemonReplyWorkspaceDrafts
		if err := daemonDecodeRequired(body, "value", &v.Drafts); err != nil {
			return nil, err
		}
		return v, nil
	case "host_capacity":
		var v DaemonReplyHostCapacity
		if err := daemonDecodeRequired(body, "value", &v.Rows); err != nil {
			return nil, err
		}
		return v, nil
	case "snapshot":
		var v DaemonReplySnapshot
		if err := daemonDecodeRequired(body, "value", &v.Snapshot); err != nil {
			return nil, err
		}
		return v, nil
	case "dashboard_startup":
		var v DaemonReplyDashboardStartup
		if err := daemonDecodeRequired(body, "value", &v.Startup); err != nil {
			return nil, err
		}
		return v, nil
	case "workspace_layouts":
		var v DaemonReplyWorkspaceLayouts
		if err := daemonDecodeRequired(body, "value", &v.Layouts); err != nil {
			return nil, err
		}
		return v, nil
	case "stored_session_metadata":
		var v DaemonReplyStoredSessionMetadata
		if err := daemonDecodeRequired(body, "value", &v.Metadata); err != nil {
			return nil, err
		}
		return v, nil
	case "stored_projection_tail":
		var v DaemonReplyStoredProjectionTail
		if err := daemonDecodeValue(body, &v.Tail); err != nil {
			return nil, err
		}
		return v, nil
	case "stored_session_summary":
		var v DaemonReplyStoredSessionSummary
		if err := daemonDecodeValue(body, &v.Summary); err != nil {
			return nil, err
		}
		return v, nil
	case "stored_transcript_item":
		var v DaemonReplyStoredTranscriptItem
		if err := daemonDecodeValue(body, &v.Item); err != nil {
			return nil, err
		}
		return v, nil
	case "saved_setup":
		var v DaemonReplySavedSetup
		if err := daemonDecodeRequired(body, "value", &v.Config); err != nil {
			return nil, err
		}
		return v, nil
	case "archive_space_preview":
		var v DaemonReplyArchiveSpacePreview
		if err := daemonDecodeRequired(body, "value", &v.Preview); err != nil {
			return nil, err
		}
		return v, nil
	case "resume_repository_preflight":
		var v DaemonReplyResumeRepositoryPreflight
		if err := daemonDecodeRequired(body, "value", &v.Preflight); err != nil {
			return nil, err
		}
		return v, nil
	case "runtime_changes":
		var v DaemonReplyRuntimeChanges
		if err := daemonDecodeRequired(body, "value", &v.Frame); err != nil {
			return nil, err
		}
		return v, nil
	case "session_tail":
		var v DaemonReplySessionTail
		if err := daemonDecodeRequired(body, "value", &v.Reply); err != nil {
			return nil, err
		}
		return v, nil
	case "resume_candidates":
		var v DaemonReplyResumeCandidates
		if err := daemonDecodeRequired(body, "value", &v.Candidates); err != nil {
			return nil, err
		}
		return v, nil
	case "go_startup_session":
		var v DaemonReplyGoStartupSession
		if err := daemonDecodeValue(body, &v.Record); err != nil {
			return nil, err
		}
		return v, nil
	case "session_record":
		var v DaemonReplySessionRecord
		if err := daemonDecodeValue(body, &v.Record); err != nil {
			return nil, err
		}
		return v, nil
	case "reply_chunk":
		return decodeDaemonReplyReplyChunk(body)
	case "registered_session":
		var v DaemonReplyRegisteredSession
		if err := daemonDecodeRequired(body, "value", &v.Registered); err != nil {
			return nil, err
		}
		return v, nil
	case "container_upgrade_preview":
		var v DaemonReplyContainerUpgradePreview
		if err := daemonDecodeRequired(body, "value", &v.Preview); err != nil {
			return nil, err
		}
		return v, nil
	case "suspend_preflight":
		var v DaemonReplySuspendPreflight
		if err := daemonDecodeRequired(body, "value", &v.Preflight); err != nil {
			return nil, err
		}
		return v, nil
	case "move_preparation":
		var v DaemonReplyMovePreparation
		if err := daemonDecodeRequired(body, "value", &v.Preparation); err != nil {
			return nil, err
		}
		return v, nil
	case "move_outcome":
		var v DaemonReplyMoveOutcome
		if err := daemonDecodeRequired(body, "value", &v.Outcome); err != nil {
			return nil, err
		}
		return v, nil
	case "move_sources":
		var v DaemonReplyMoveSources
		if err := daemonDecodeRequired(body, "value", &v.Sources); err != nil {
			return nil, err
		}
		return v, nil
	case "ordinal":
		var v DaemonReplyOrdinal
		if err := daemonDecodeRequired(body, "value", &v.Ordinal); err != nil {
			return nil, err
		}
		return v, nil
	case "text":
		var v DaemonReplyText
		if err := daemonDecodeRequired(body, "value", &v.Text); err != nil {
			return nil, err
		}
		return v, nil
	case "optional_session_state":
		var v DaemonReplyOptionalSessionState
		if err := daemonDecodeValue(body, &v.State); err != nil {
			return nil, err
		}
		return v, nil
	case "recovery_scan":
		var v DaemonReplyRecoveryScan
		if err := daemonDecodeRequired(body, "value", &v.Scan); err != nil {
			return nil, err
		}
		return v, nil
	case "wiki_rows":
		var v DaemonReplyWikiRows
		if err := daemonDecodeRequired(body, "value", &v.Page); err != nil {
			return nil, err
		}
		return v, nil
	case "wiki_sync_status":
		var v DaemonReplyWikiSyncStatus
		if err := daemonDecodeRequired(body, "value", &v.Status); err != nil {
			return nil, err
		}
		return v, nil
	case "project_catalog":
		var v DaemonReplyProjectCatalog
		if err := daemonDecodeRequired(body, "value", &v.View); err != nil {
			return nil, err
		}
		return v, nil
	case "project_created":
		var v DaemonReplyProjectCreated
		if err := daemonDecodeRequired(body, "value", &v.Project); err != nil {
			return nil, err
		}
		return v, nil
	case "session_text_matches":
		var v DaemonReplySessionTextMatches
		if err := daemonDecodeRequired(body, "value", &v.Page); err != nil {
			return nil, err
		}
		return v, nil
	case "wiki_hits":
		var v DaemonReplyWikiHits
		if err := daemonDecodeValue(body, &v.Transcript); err != nil {
			return nil, err
		}
		return v, nil
	case "wiki_session":
		var v DaemonReplyWikiSession
		if err := daemonDecodeValue(body, &v.Info); err != nil {
			return nil, err
		}
		return v, nil
	case "prompt_withdrawn":
		var v DaemonReplyPromptWithdrawn
		if err := daemonDecodeRequired(body, "value", &v.Withdrawn); err != nil {
			return nil, err
		}
		return v, nil
	case "startup_prompt_identity_conflict":
		return DaemonReplyStartupPromptIdentityConflict{}, nil
	case "startup_prompt_outcomes":
		var v DaemonReplyStartupPromptOutcomes
		if err := daemonDecodeRequired(body, "value", &v.Outcomes); err != nil {
			return nil, err
		}
		return v, nil
	case "done":
		return DaemonReplyDone{}, nil
	}
	return nil, daemonUnknownVariant(name, daemonReplyVariantNames)
}

func (v DaemonReplyIdentityProof) MarshalJSON() ([]byte, error) {
	type wire struct {
		Mac json.RawMessage `json:"mac"`
	}
	return daemonAdjacentStruct("reply", "value", "identity_proof", wire{Mac: daemonByteArrayJSON(v.Mac)})
}

func decodeDaemonReplyIdentityProof(body json.RawMessage) (DaemonReply, error) {
	var wire struct {
		Mac json.RawMessage `json:"mac"`
	}
	if body == nil {
		return nil, daemonMissingField("value")
	}
	if err := daemonDecodeStruct(body, &wire, false); err != nil {
		return nil, err
	}
	mac, err := daemonDecodeByteArray(wire.Mac)
	if err != nil {
		return nil, err
	}
	return DaemonReplyIdentityProof{Mac: mac}, nil
}

func (v DaemonReplyStoredProjectionTail) MarshalJSON() ([]byte, error) {
	body := []byte("null")
	if v.Tail != nil {
		projection, err := serializeClientProjection(v.Tail)
		if err != nil {
			return nil, err
		}
		body = projection
	}
	return daemonAdjacentJSON("reply", "value", "stored_projection_tail", body)
}

func (v DaemonReplyReplyChunk) MarshalJSON() ([]byte, error) {
	bytes, err := chunkBytesSerialize(v.Bytes)
	if err != nil {
		return nil, err
	}
	type wire struct {
		Bytes    json.RawMessage `json:"bytes"`
		Finished bool            `json:"finished"`
	}
	return daemonAdjacentStruct("reply", "value", "reply_chunk", wire{Bytes: bytes, Finished: v.Finished})
}

func decodeDaemonReplyReplyChunk(body json.RawMessage) (DaemonReply, error) {
	var wire struct {
		Bytes    json.RawMessage `json:"bytes"`
		Finished bool            `json:"finished"`
	}
	if body == nil {
		return nil, daemonMissingField("value")
	}
	if err := daemonDecodeStruct(body, &wire, false); err != nil {
		return nil, err
	}
	bytes, err := chunkBytesDeserialize(wire.Bytes)
	if err != nil {
		return nil, err
	}
	return DaemonReplyReplyChunk{Bytes: bytes, Finished: wire.Finished}, nil
}

// DaemonReplyIsChunked reports the replies that can outgrow one frame. The
// daemon sends them as DaemonReplyReplyChunk fragments and the client
// reassembles them.
func DaemonReplyIsChunked(reply DaemonReply) bool {
	switch reply.(type) {
	case DaemonReplyConfiguration, DaemonReplyDashboardStartup, DaemonReplyRuntimeChanges,
		DaemonReplySessionTail, DaemonReplyStoredTranscriptItem, DaemonReplyResumeCandidates:
		return true
	}
	return false
}

type InterruptAllResult struct {
	Requested uint64 `json:"requested"`
	// Failures are (session id, error) pairs, serialized as two-element
	// arrays.
	Failures []DaemonPair[string, string] `json:"failures"`
}

func (v InterruptAllResult) MarshalJSON() ([]byte, error) {
	type plain InterruptAllResult
	return daemonMarshalStruct(plain(v))
}

func (v *InterruptAllResult) UnmarshalJSON(data []byte) error {
	type plain InterruptAllResult
	return daemonDecodeStruct(data, (*plain)(v), false)
}

// SessionRecordWithVerdict is an on-demand record and its verdict from the
// same capture.
type SessionRecordWithVerdict struct {
	Record  core.SessionRecord `json:"record"`
	Verdict SessionVerdict     `json:"verdict"`
}

func (v *SessionRecordWithVerdict) UnmarshalJSON(data []byte) error {
	type plain SessionRecordWithVerdict
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// ResumeCandidates is what the resume dialog lists, read on demand.
type ResumeCandidates struct {
	// Candidates has one row for every inactive session that is not a
	// sub-agent.
	Candidates []ResumeCandidate `json:"candidates"`
	// Moves are durable moves of those sessions, for "move needs recovery"
	// marks.
	Moves []core.MoveOperation `json:"moves"`
	// AdoptedNativeSessions are native sessions that some record, live or
	// not, already holds. The import list must not offer them again.
	AdoptedNativeSessions []DaemonPair[core.HarnessKind, string] `json:"adopted_native_sessions"`
	// LocalCheckoutRoots are local checkouts that Mjolnir created for its
	// sessions. Every native thread that ran inside one belongs to that
	// session.
	LocalCheckoutRoots []string `json:"local_checkout_roots"`
}

func (v ResumeCandidates) MarshalJSON() ([]byte, error) {
	type plain ResumeCandidates
	return daemonMarshalStruct(plain(v))
}

func (v *ResumeCandidates) UnmarshalJSON(data []byte) error {
	type plain ResumeCandidates
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// ResumeCandidate is one row of the resume dialog's Mjolnir tab: what the row
// shows, without the rest of the record. A session's title can be tens of
// kilobytes, and most of a store's history is never resumed, so the resume
// wizard fetches the whole record (DaemonActionSessionRecord) only for the
// row picked.
type ResumeCandidate struct {
	SessionID   string            `json:"session_id"`
	State       core.SessionState `json:"state"`
	LastProfile string            `json:"last_profile"`
	// Title is the listed title, cut to ResumeCandidateTitleChars.
	Title string `json:"title"`
	// Origin is where the session ran, as the session list names it.
	Origin        string `json:"origin"`
	Project       string `json:"project"`
	HasCheckpoint bool   `json:"has_checkpoint"`
	// LastActivityMs is the checkpoint's creation time, or the record's last
	// update without one; nil when neither parses.
	LastActivityMs *int64                 `json:"last_activity_ms"`
	Publication    *core.PublicationState `json:"publication"`
	// CheckpointArchive is the checkpoint archive whose size the row shows.
	// Only a stopped record is described by its checkpoint.
	CheckpointArchive *string `json:"checkpoint_archive"`
	// WorktreeCheckout: whether destroying the session can also delete its
	// worktree branch.
	WorktreeCheckout bool `json:"worktree_checkout"`
}

func (v ResumeCandidate) MarshalJSON() ([]byte, error) {
	type plain ResumeCandidate
	return daemonMarshalStruct(plain(v))
}

func (v *ResumeCandidate) UnmarshalJSON(data []byte) error {
	type plain ResumeCandidate
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// ResumeCandidateTitleChars is longer than any row draws a title.
const ResumeCandidateTitleChars = 200

// ResumeCandidateOf is the row for record. The daemon and the dashboard both
// build rows here, so a row reads the same whichever of them held the
// record.
func ResumeCandidateOf(record *core.SessionRecord, config *core.Config) ResumeCandidate {
	timestampMs := func(timestamp string) *int64 {
		parsed, ok := core.ParseChronoRFC3339(timestamp)
		if !ok {
			return nil
		}
		millis := parsed.UnixMilli()
		return &millis
	}
	title := []rune(record.ListedTitle())
	if len(title) > ResumeCandidateTitleChars {
		title = title[:ResumeCandidateTitleChars]
	}
	var lastActivityMs *int64
	if record.Checkpoint != nil {
		lastActivityMs = timestampMs(record.Checkpoint.CreatedAt)
	}
	if lastActivityMs == nil {
		lastActivityMs = timestampMs(record.UpdatedAt)
	}
	var publication *core.PublicationState
	if state, ok := record.PublicationState(); ok {
		publication = &state
	}
	var checkpointArchive *string
	if record.Checkpoint != nil && record.State == core.SessionStateStopped {
		archive := record.Checkpoint.ArchivePath
		checkpointArchive = &archive
	}
	// An omitted managed checkout kind is a worktree, as serde's default
	// reads it.
	owned := record.RecordCheckout().ManagedWorktree()
	worktreeCheckout := owned != nil &&
		(owned.Kind == core.ManagedCheckoutKindWorktree || owned.Kind == "")
	return ResumeCandidate{
		SessionID:         record.ID,
		State:             record.State,
		LastProfile:       record.LastProfile,
		Title:             string(title),
		Origin:            record.ProjectTarget(config, record.TargetTemplateID),
		Project:           record.ProjectName(config),
		HasCheckpoint:     record.Checkpoint != nil,
		LastActivityMs:    lastActivityMs,
		Publication:       publication,
		CheckpointArchive: checkpointArchive,
		WorktreeCheckout:  worktreeCheckout,
	}
}

type DaemonStatus struct {
	Pid             uint32          `json:"pid"`
	StartedAt       string          `json:"started_at"`
	BuildVersion    string          `json:"build_version"`
	AttachedClients int             `json:"attached_clients"`
	PhoneStatus     WebViewerStatus `json:"phone_status"`
}

func (v DaemonStatus) MarshalJSON() ([]byte, error) {
	type plain DaemonStatus
	return daemonMarshalStruct(plain(v))
}

func (v *DaemonStatus) UnmarshalJSON(data []byte) error {
	type plain DaemonStatus
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// WebViewerStatus is the phone viewer's state. Internally tagged by "state"
// in snake_case. String is Rust's Display; GoString is its Debug, which
// redacts the QR login URL.
type WebViewerStatus interface {
	isWebViewerStatus()
	String() string
}

type WebViewerStatusDisabled struct{}

type WebViewerStatusStarting struct{}

type WebViewerStatusReady struct {
	ViewerURL      string  `json:"viewer_url"`
	ViewerCode     string  `json:"viewer_code"`
	QrLoginURL     *string `json:"qr_login_url"`
	FallbackReason *string `json:"fallback_reason"`
}

type WebViewerStatusStopped struct{}

type WebViewerStatusError struct {
	Message string `json:"message"`
}

func (WebViewerStatusDisabled) isWebViewerStatus() {}
func (WebViewerStatusStarting) isWebViewerStatus() {}
func (WebViewerStatusReady) isWebViewerStatus()    {}
func (WebViewerStatusStopped) isWebViewerStatus()  {}
func (WebViewerStatusError) isWebViewerStatus()    {}

func (WebViewerStatusDisabled) MarshalJSON() ([]byte, error) {
	return daemonInternalJSON("state", "disabled", nil)
}

func (WebViewerStatusStarting) MarshalJSON() ([]byte, error) {
	return daemonInternalJSON("state", "starting", nil)
}

func (v WebViewerStatusReady) MarshalJSON() ([]byte, error) {
	type plain WebViewerStatusReady
	return daemonInternalJSON("state", "ready", plain(v))
}

func (WebViewerStatusStopped) MarshalJSON() ([]byte, error) {
	return daemonInternalJSON("state", "stopped", nil)
}

func (v WebViewerStatusError) MarshalJSON() ([]byte, error) {
	type plain WebViewerStatusError
	return daemonInternalJSON("state", "error", plain(v))
}

// UnmarshalWebViewerStatusJSON decodes a WebViewerStatus by its "state" tag.
func UnmarshalWebViewerStatusJSON(data []byte) (WebViewerStatus, error) {
	name, err := daemonSplitInternal(data, "state")
	if err != nil {
		return nil, err
	}
	switch name {
	case "disabled":
		return WebViewerStatusDisabled{}, nil
	case "starting":
		return WebViewerStatusStarting{}, nil
	case "ready":
		var v WebViewerStatusReady
		if err := daemonDecodeStruct(data, &v, false); err != nil {
			return nil, err
		}
		return v, nil
	case "stopped":
		return WebViewerStatusStopped{}, nil
	case "error":
		var v WebViewerStatusError
		if err := daemonDecodeStruct(data, &v, false); err != nil {
			return nil, err
		}
		return v, nil
	}
	return nil, daemonUnknownVariant(name, []string{"disabled", "starting", "ready", "stopped", "error"})
}

func (WebViewerStatusDisabled) String() string { return "disabled" }
func (WebViewerStatusStarting) String() string { return "starting" }
func (WebViewerStatusStopped) String() string  { return "stopped unexpectedly" }
func (v WebViewerStatusError) String() string  { return "error: " + v.Message }

func (v WebViewerStatusReady) String() string {
	text := fmt.Sprintf("%s; viewer code %s", v.ViewerURL, v.ViewerCode)
	if v.FallbackReason != nil {
		text += "; local only because Tailscale HTTPS is unavailable: " + *v.FallbackReason
	}
	return text
}

func (WebViewerStatusDisabled) GoString() string { return "Disabled" }
func (WebViewerStatusStarting) GoString() string { return "Starting" }
func (WebViewerStatusStopped) GoString() string  { return "Stopped" }
func (v WebViewerStatusError) GoString() string  { return fmt.Sprintf("Error(%q)", v.Message) }

func (v WebViewerStatusReady) GoString() string {
	reason := "None"
	if v.FallbackReason != nil {
		reason = fmt.Sprintf("Some(%q)", *v.FallbackReason)
	}
	return fmt.Sprintf("Ready { viewer_url: %q, viewer_code: %q, qr_login_url: \"[redacted]\", fallback_reason: %s }",
		v.ViewerURL, v.ViewerCode, reason)
}

// ReadMetadata reads the published endpoint of a daemon that speaks this
// build's protocol.
func ReadMetadata() (DaemonMetadata, error) {
	metadata, err := ReadMetadataAny()
	if err != nil {
		return DaemonMetadata{}, err
	}
	if metadata.ProtocolVersion != ProtocolVersion {
		return DaemonMetadata{}, fmt.Errorf("daemon protocol %d is incompatible with client protocol %d",
			metadata.ProtocolVersion, ProtocolVersion)
	}
	return metadata, nil
}

// DaemonNotRunning: no daemon has published its endpoint: `daemon.json` does
// not exist.
//
// This is the ordinary stopped state, not a failure. A daemon removes the
// file when it stops, so callers show "not running" rather than the raw file
// error.
type DaemonNotRunning struct {
	// MetadataPath is where the endpoint would be, which also names the
	// instance.
	MetadataPath string
}

// DaemonNotRunningMessage is what a caller shows for DaemonNotRunning.
const DaemonNotRunningMessage = "the Mjolnir daemon is not running"

func (DaemonNotRunning) Error() string { return DaemonNotRunningMessage }

// DaemonNotRunningOf is the stopped state, if err reports it anywhere in its
// chain. (Rust's daemon_not_running; renamed because the type holds the
// name.)
func DaemonNotRunningOf(err error) (*DaemonNotRunning, bool) {
	var stopped DaemonNotRunning
	if errors.As(err, &stopped) {
		return &stopped, true
	}
	var stoppedPointer *DaemonNotRunning
	if errors.As(err, &stoppedPointer) && stoppedPointer != nil {
		return stoppedPointer, true
	}
	return nil, false
}

// ReadMetadataAny reads the published endpoint whatever its protocol.
func ReadMetadataAny() (DaemonMetadata, error) {
	return readMetadataAt(MetadataPath())
}

func readMetadataAt(path string) (DaemonMetadata, error) {
	body, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return DaemonMetadata{}, DaemonNotRunning{MetadataPath: path}
	}
	if err != nil {
		return DaemonMetadata{}, fmt.Errorf("read %s: %w", path, err)
	}
	var metadata DaemonMetadata
	if err := json.Unmarshal(body, &metadata); err != nil {
		return DaemonMetadata{}, fmt.Errorf("parse %s: %w", path, err)
	}
	return metadata, nil
}

// WriteFrame writes value as one length-prefixed JSON frame.
func WriteFrame(w io.Writer, value any) error {
	body, err := daemonMarshalJSON(value)
	if err != nil {
		return err
	}
	return WriteEncodedFrame(w, body)
}

// WriteEncodedFrame writes an encoded body behind its big-endian u32 length.
func WriteEncodedFrame(w io.Writer, body []byte) error {
	if len(body) > MaxFrameBytes {
		return fmt.Errorf("daemon frame is too large: %d bytes exceeds %d", len(body), MaxFrameBytes)
	}
	frame := binary.BigEndian.AppendUint32(make([]byte, 0, 4+len(body)), uint32(len(body)))
	_, err := w.Write(append(frame, body...))
	return err
}

// ReadFrame reads one length-prefixed JSON frame.
func ReadFrame[T any](r io.Reader) (T, error) {
	var value T
	var length [4]byte
	if _, err := io.ReadFull(r, length[:]); err != nil {
		return value, err
	}
	size := binary.BigEndian.Uint32(length[:])
	if uint64(size) > MaxFrameBytes {
		return value, fmt.Errorf("daemon frame exceeds %d bytes", MaxFrameBytes)
	}
	body := make([]byte, size)
	if _, err := io.ReadFull(r, body); err != nil {
		return value, err
	}
	if err := json.Unmarshal(body, &value); err != nil {
		return value, fmt.Errorf("decode daemon frame: %w", err)
	}
	return value, nil
}

// ReadResponse reads one reply, reassembling a chunked one before exposing
// it. All fragments belong to the same response; disconnects discard partial
// state.
func ReadResponse(r io.Reader) (ResponseEnvelope, error) {
	response, err := ReadFrame[ResponseEnvelope](r)
	if err != nil {
		return ResponseEnvelope{}, err
	}
	if _, chunked := response.Result.Ok.(DaemonReplyReplyChunk); response.Result.Err != nil || !chunked {
		return response, nil
	}
	protocolVersion := response.ProtocolVersion
	requestID := response.RequestID
	var body []byte
	for {
		if response.ProtocolVersion != protocolVersion || response.RequestID != requestID {
			return ResponseEnvelope{}, errors.New("daemon crossed reply fragment identities")
		}
		chunk, ok := response.Result.Ok.(DaemonReplyReplyChunk)
		if response.Result.Err != nil || !ok {
			return ResponseEnvelope{}, errors.New("daemon interrupted a chunked reply")
		}
		if len(chunk.Bytes) == 0 {
			return ResponseEnvelope{}, errors.New("empty reply fragment")
		}
		body = append(body, chunk.Bytes...)
		if chunk.Finished {
			break
		}
		if response, err = ReadFrame[ResponseEnvelope](r); err != nil {
			return ResponseEnvelope{}, err
		}
	}
	reply, err := UnmarshalDaemonReplyJSON(body)
	if err != nil {
		return ResponseEnvelope{}, err
	}
	if !DaemonReplyIsChunked(reply) {
		return ResponseEnvelope{}, errors.New("daemon chunked a reply that is never chunked")
	}
	return ResponseEnvelope{
		ProtocolVersion: protocolVersion,
		RequestID:       requestID,
		Result:          DaemonOk(reply),
	}, nil
}

// daemonWatch closes stream when ctx ends, as dropping the Rust future that
// owns an exchange drops its connection. The returned release stops watching
// and reports whether the stream is still intact; after false the stream is
// closed and must never be reused.
func daemonWatch(ctx context.Context, stream net.Conn) (release func() bool) {
	return context.AfterFunc(ctx, func() { _ = stream.Close() })
}

// daemonAbandon drops a stream whose exchange failed, so it can never carry
// another request, and reports why: the caller's cancellation when ctx
// ended, otherwise err.
func daemonAbandon(ctx context.Context, stream net.Conn, release func() bool, err error) error {
	release()
	_ = stream.Close()
	if ctx.Err() != nil {
		return ctx.Err()
	}
	return err
}

// DaemonClient is one authenticated control connection to a daemon.
type DaemonClient struct {
	sshLease      *SshLease
	connector     *DaemonConnector
	metadata      DaemonMetadata
	stream        net.Conn
	nextRequestID uint64
}

// DaemonClientConnect dials the daemon metadata advertises, within one
// second.
func DaemonClientConnect(ctx context.Context, metadata DaemonMetadata) (*DaemonClient, error) {
	dialer := net.Dialer{Timeout: time.Second}
	stream, err := dialer.DialContext(ctx, "tcp", metadata.Address.String())
	if err != nil {
		var timeout net.Error
		if errors.As(err, &timeout) && timeout.Timeout() {
			return nil, fmt.Errorf("time out connecting to Mjolnir daemon: %w", err)
		}
		return nil, err
	}
	return &DaemonClient{metadata: metadata, stream: stream, nextRequestID: 1}, nil
}

// daemonClientConnectProven connects through an SSH tunnel's local port and
// requires the daemon to prove it holds the token on this very connection
// before any request carries the token. The tunnel may have exited since an
// earlier proof, and another process may now hold its port.
func daemonClientConnectProven(ctx context.Context, metadata DaemonMetadata) (*DaemonClient, error) {
	client, err := DaemonClientConnect(ctx, metadata)
	if err != nil {
		return nil, err
	}
	stream, err := client.takeStream()
	if err != nil {
		return nil, err
	}
	proven, err := proveStream(ctx, stream, client.metadata.Token)
	if err != nil {
		_ = stream.Close()
		return nil, err
	}
	if !proven {
		_ = stream.Close()
		return nil, fmt.Errorf("the SSH tunnel's local port %s did not prove it reaches the daemon", client.metadata.Address)
	}
	client.stream = stream
	return client, nil
}

// Close drops the control connection, as dropping the Rust client does.
func (c *DaemonClient) Close() {
	if c.stream != nil {
		_ = c.stream.Close()
		c.stream = nil
	}
}

// replace makes this client the reconnected one, dropping its own
// connection (Rust's `*self = reconnect().await?`).
func (c *DaemonClient) replace(next *DaemonClient) {
	c.Close()
	*c = *next
}

// Connector is the connector-owned SSH origin; never inferred from
// daemon-host placement.
func (c *DaemonClient) Connector() *DaemonConnector { return c.connector }

// Endpoint describes the authenticated endpoint, retaining the address
// actually dialed (including a container's published loopback port).
func (c *DaemonClient) Endpoint(ctx context.Context) (DaemonEndpoint, error) {
	identity, err := c.Identity(ctx)
	if err != nil {
		return DaemonEndpoint{}, err
	}
	return DaemonEndpoint{
		ProtocolVersion: c.metadata.ProtocolVersion,
		BuildVersion:    identity.BuildVersion,
		ControlAddress:  c.metadata.Address,
		Token:           c.metadata.Token,
		InstanceID:      identity.InstanceID,
	}, nil
}

// Request speaks the daemon's advertised dialect, not this build's:
// management requests must reach daemons of any protocol version, and the
// frozen subset encodes identically across all of them.
func (c *DaemonClient) Request(ctx context.Context, action DaemonAction) (DaemonReply, error) {
	connector := c.connector
	previous := c.metadata
	return c.requestWithReconnect(ctx, action, func(ctx context.Context) (*DaemonClient, error) {
		if connector != nil {
			return connector.refresh(ctx, previous)
		}
		for {
			if client, err := ConnectExisting(ctx); err == nil {
				return client, nil
			}
			if err := daemonSleep(ctx, 250*time.Millisecond); err != nil {
				return nil, err
			}
		}
	})
}

func (c *DaemonClient) requestWithReconnect(
	ctx context.Context,
	action DaemonAction,
	reconnect func(context.Context) (*DaemonClient, error),
) (DaemonReply, error) {
	for {
		response, err := c.requestOnce(ctx, action)
		if err != nil {
			return nil, err
		}
		_, pending := response.(DaemonReplyUpgradePending)
		if _, prepare := action.(DaemonActionPrepareUpgrade); !pending || prepare {
			return response, nil
		}
		// Only an explicit refusal guarantees non-admission. Never replay
		// arbitrary mutations after a lost acknowledgement.
		if err := daemonSleep(ctx, 100*time.Millisecond); err != nil {
			return nil, err
		}
		client, err := reconnect(ctx)
		if err != nil {
			return nil, err
		}
		c.replace(client)
	}
}

// daemonSleep waits for duration, or for ctx to end.
func daemonSleep(ctx context.Context, duration time.Duration) error {
	timer := time.NewTimer(duration)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-timer.C:
		return nil
	}
}

// Upload streams raw bytes on this connection. The returned blob is
// connection-scoped; consume it before disconnecting. Delivery failures are
// never replayed. Cancelling ctx drops the connection; a read blocked in
// reader still ends only when reader returns.
func (c *DaemonClient) Upload(ctx context.Context, reader io.Reader) (string, error) {
	stream, err := c.takeStream()
	if err != nil {
		return "", err
	}
	release := daemonWatch(ctx, stream)
	digest := sha256.New()
	requestID := c.nextRequestID
	c.nextRequestID++
	buffer := make([]byte, 64*1024)
	send := func(bytes []byte, sha *string) error {
		return WriteFrame(stream, RequestEnvelope{
			ProtocolVersion: c.metadata.ProtocolVersion,
			RequestID:       requestID,
			Token:           c.metadata.Token,
			Action:          DaemonActionUploadChunk{Bytes: bytes, Sha256: sha},
		})
	}
	for {
		n, readErr := reader.Read(buffer)
		if readErr != nil && !errors.Is(readErr, io.EOF) {
			return "", daemonAbandon(ctx, stream, release, readErr)
		}
		if n > 0 {
			digest.Write(buffer[:n])
			if err := send(append([]byte(nil), buffer[:n]...), nil); err != nil {
				return "", daemonAbandon(ctx, stream, release, err)
			}
		}
		if readErr != nil {
			// The end of the source: an empty fragment carries the digest of
			// every byte sent.
			sum := core.LowerHex(digest.Sum(nil))
			if err := send([]byte{}, &sum); err != nil {
				return "", daemonAbandon(ctx, stream, release, err)
			}
			break
		}
	}
	if !release() {
		_ = stream.Close()
		return "", ctx.Err()
	}
	response, err := c.finishExchange(ctx, stream, requestID)
	if err != nil {
		return "", err
	}
	if response.Result.Err != nil {
		return "", errors.New(*response.Result.Err)
	}
	if blob, ok := response.Result.Ok.(DaemonReplyBlobUploaded); ok {
		return blob.Blob, nil
	}
	return "", fmt.Errorf("unexpected blob upload reply %s", daemonReplyDebug(response.Result.Ok))
}

// UploadAndConsume uploads and consumes on one connection. Only explicit
// non-admission permits replay; a transport error may hide acceptance and is
// returned to the caller.
func (c *DaemonClient) UploadAndConsume(
	ctx context.Context,
	reader io.ReadSeeker,
	action func(blob string) DaemonAction,
) (DaemonReply, error) {
	return c.uploadAndConsumeWithReconnect(ctx, reader, action, reconnectAfterUploadRefusal)
}

func (c *DaemonClient) uploadAndConsumeWithReconnect(
	ctx context.Context,
	reader io.ReadSeeker,
	action func(blob string) DaemonAction,
	reconnect func(context.Context) (*DaemonClient, error),
) (DaemonReply, error) {
	return c.uploadAndConsumeObservedWithReconnect(ctx, reader, action, reconnect,
		func(context.Context, bool) error { return nil }, nil)
}

// UploadAndConsumeObserved reports each phase (true while bytes upload);
// cancel is observed only during byte upload. A nil cancel never fires.
func (c *DaemonClient) UploadAndConsumeObserved(
	ctx context.Context,
	reader io.ReadSeeker,
	action func(blob string) DaemonAction,
	phase func(ctx context.Context, uploading bool) error,
	cancel <-chan struct{},
) (DaemonReply, error) {
	return c.uploadAndConsumeObservedWithReconnect(ctx, reader, action, reconnectAfterUploadRefusal, phase, cancel)
}

func (c *DaemonClient) uploadAndConsumeObservedWithReconnect(
	ctx context.Context,
	reader io.ReadSeeker,
	action func(blob string) DaemonAction,
	reconnect func(context.Context) (*DaemonClient, error),
	phase func(ctx context.Context, uploading bool) error,
	cancel <-chan struct{},
) (DaemonReply, error) {
	start, err := reader.Seek(0, io.SeekCurrent)
	if err != nil {
		return nil, err
	}
	if err := phase(ctx, true); err != nil {
		return nil, err
	}
	// The upload and the reconnection run under a context that cancel also
	// ends, as Rust selects on cancel around both.
	errUploadCancelled := errors.New("upload cancelled")
	cancellable, stop := context.WithCancelCause(ctx)
	defer stop(nil)
	go func() {
		select {
		case <-cancel:
			stop(errUploadCancelled)
		case <-cancellable.Done():
		}
	}()
	interrupted := func(err error) error {
		if ctx.Err() == nil && errors.Is(context.Cause(cancellable), errUploadCancelled) {
			return errUploadCancelled
		}
		return err
	}
	cancelled := func() bool {
		select {
		case <-cancel:
			return true
		default:
			return false
		}
	}
	for {
		// Cancellation is checked first, before the upload takes the
		// connection.
		if cancelled() {
			return nil, errUploadCancelled
		}
		blob, err := c.Upload(cancellable, reader)
		if err != nil {
			return nil, interrupted(err)
		}
		if err := phase(ctx, false); err != nil {
			return nil, err
		}
		reply, err := c.requestOnce(ctx, action(blob))
		if err != nil {
			return nil, err
		}
		if _, pending := reply.(DaemonReplyUpgradePending); !pending {
			return reply, nil
		}
		if err := phase(ctx, true); err != nil {
			return nil, err
		}
		if cancelled() {
			return nil, errUploadCancelled
		}
		if err := daemonSleep(cancellable, 100*time.Millisecond); err != nil {
			return nil, interrupted(err)
		}
		client, err := reconnect(cancellable)
		if err != nil {
			return nil, interrupted(err)
		}
		c.replace(client)
		if _, err := reader.Seek(start, io.SeekStart); err != nil {
			return nil, err
		}
	}
}

// RequestSingle exchanges one request on this connection, exposing an
// upgrade refusal to the caller.
func (c *DaemonClient) RequestSingle(ctx context.Context, action DaemonAction) (DaemonReply, error) {
	return c.requestOnce(ctx, action)
}

func (c *DaemonClient) requestOnce(ctx context.Context, action DaemonAction) (DaemonReply, error) {
	stream, err := c.takeStream()
	if err != nil {
		return nil, err
	}
	release := daemonWatch(ctx, stream)
	requestID := c.nextRequestID
	c.nextRequestID++
	err = WriteFrame(stream, RequestEnvelope{
		ProtocolVersion: c.metadata.ProtocolVersion,
		RequestID:       requestID,
		Token:           c.metadata.Token,
		Action:          action,
	})
	if err != nil {
		return nil, daemonAbandon(ctx, stream, release, transportLost(err))
	}
	if !release() {
		_ = stream.Close()
		return nil, ctx.Err()
	}
	response, err := c.finishExchange(ctx, stream, requestID)
	if err != nil {
		return nil, err
	}
	if response.Result.Err != nil {
		return nil, DaemonRefusal{Message: *response.Result.Err}
	}
	return response.Result.Ok, nil
}

// takeStream hands the connection to one exchange. The exchange owns framing
// until a complete, checked response arrives; cancellation or an I/O error
// drops the stream instead of leaving it reusable.
func (c *DaemonClient) takeStream() (net.Conn, error) {
	stream := c.stream
	if stream == nil {
		return nil, fmt.Errorf("%w: %w", DaemonTransportLost{},
			errors.New("connection lost to an interrupted request"))
	}
	c.stream = nil
	return stream, nil
}

func (c *DaemonClient) finishExchange(ctx context.Context, stream net.Conn, requestID uint64) (ResponseEnvelope, error) {
	release := daemonWatch(ctx, stream)
	response, err := ReadResponse(stream)
	if err != nil {
		return ResponseEnvelope{}, daemonAbandon(ctx, stream, release, transportLost(err))
	}
	if response.ProtocolVersion != c.metadata.ProtocolVersion {
		return ResponseEnvelope{}, daemonAbandon(ctx, stream, release, errors.New("daemon changed protocol"))
	}
	if response.RequestID != requestID {
		return ResponseEnvelope{}, daemonAbandon(ctx, stream, release,
			fmt.Errorf("daemon crossed request IDs: expected %d, received %d", requestID, response.RequestID))
	}
	if !release() {
		_ = stream.Close()
		return ResponseEnvelope{}, ctx.Err()
	}
	c.stream = stream
	return response, nil
}

func (c *DaemonClient) Identity(ctx context.Context) (DaemonIdentity, error) {
	reply, err := c.Request(ctx, DaemonActionIdentity{})
	matched, err := daemonExpect[DaemonReplyIdentity](reply, err, "unexpected daemon identity reply")
	return matched.Identity, err
}

// Config is the configuration the daemon serves. Clients use this instead of
// opening the daemon's `config.toml`.
func (c *DaemonClient) Config(ctx context.Context) (core.Config, error) {
	reply, err := c.Request(ctx, DaemonActionConfiguration{})
	matched, err := daemonExpect[DaemonReplyConfiguration](reply, err, "unexpected daemon configuration reply")
	return matched.Config, err
}

// SharesClientFilesystem reports whether the connected daemon can resolve
// paths from this client.
func (c *DaemonClient) SharesClientFilesystem(identity DaemonIdentity) bool {
	if c.connector != nil && c.connector.IsSSH() {
		return false
	}
	daemonContainer, inContainer := identity.Placement.Container()
	clientContainer, clientInContainer := core.DaemonContainer()
	return inContainer == clientInContainer && daemonContainer == clientContainer
}

func (c *DaemonClient) FirstRunSetupDue(ctx context.Context) (bool, error) {
	reply, err := c.Request(ctx, DaemonActionFirstRunSetupDue{})
	matched, err := daemonExpect[DaemonReplyFirstRunSetupDue](reply, err, "unexpected first-run setup reply")
	return matched.Due, err
}

func (c *DaemonClient) RunFirstRunSetup(ctx context.Context, projectDirectory *string) (*FirstRunSetupReport, error) {
	var action DaemonAction = DaemonActionRunFirstRunSetup{}
	if projectDirectory != nil {
		action = DaemonActionRunFirstRunSetupIn{ProjectDirectory: *projectDirectory}
	}
	reply, err := c.Request(ctx, action)
	matched, err := daemonExpect[DaemonReplyFirstRunSetup](reply, err, "unexpected first-run setup reply")
	return matched.Report, err
}

func (c *DaemonClient) RunDoctor(ctx context.Context) ([]string, error) {
	reply, err := c.Request(ctx, DaemonActionRunDoctor{})
	matched, err := daemonExpect[DaemonReplyDoctorErrors](reply, err, "unexpected doctor reply")
	return matched.Errors, err
}

func (c *DaemonClient) NewSessionRemoteRepairs(ctx context.Context, bundleID, targetID string) ([]core.LocalRemoteRepair, error) {
	reply, err := c.Request(ctx, DaemonActionNewSessionRemoteRepairs{BundleID: bundleID, TargetID: targetID})
	matched, err := daemonExpect[DaemonReplyRemoteRepairs](reply, err, "unexpected new-session preflight reply")
	return matched.Repairs, err
}

func (c *DaemonClient) RepairRepositoryRemotes(ctx context.Context, bundleID string, repairs []core.LocalRemoteRepair) error {
	reply, err := c.Request(ctx, DaemonActionRepairRepositoryRemotes{BundleID: bundleID, Repairs: repairs})
	return daemonExpectDone(reply, err, "unexpected remote-repair reply")
}

func (c *DaemonClient) SessionProjectSource(ctx context.Context, sessionID string) (core.ProjectSourceIdentity, error) {
	reply, err := c.Request(ctx, DaemonActionSessionProjectSource{SessionID: sessionID})
	matched, err := daemonExpect[DaemonReplyProjectSource](reply, err, "unexpected project-source reply")
	return matched.Source, err
}

func (c *DaemonClient) Status(ctx context.Context) (DaemonStatus, error) {
	reply, err := c.Request(ctx, DaemonActionStatus{})
	matched, err := daemonExpect[DaemonReplyStatus](reply, err, "unexpected daemon status reply")
	return matched.Status, err
}

// APICredential obtains the HTTP bearer credential from its daemon owner.
func (c *DaemonClient) APICredential(ctx context.Context) (string, error) {
	reply, err := c.Request(ctx, DaemonActionApiCredential{})
	matched, err := daemonExpect[DaemonReplyApiCredential](reply, err, "unexpected API credential reply:")
	return matched.Token, err
}

func (c *DaemonClient) WebAccess(ctx context.Context) (WebViewerAccess, error) {
	reply, err := c.Request(ctx, DaemonActionWebViewerAccess{})
	matched, err := daemonExpect[DaemonReplyWebViewerAccess](reply, err, "unexpected web viewer reply")
	return matched.Access, err
}

func (c *DaemonClient) SubagentOptions(ctx context.Context, profile string, model *string, config *core.Config) (core.SubagentOptions, error) {
	reply, err := c.Request(ctx, DaemonActionSubagentOptions{Profile: profile, Model: model, Config: config})
	matched, err := daemonExpect[DaemonReplySubagentOptions](reply, err, "unexpected subagent options reply")
	return matched.Options, err
}

func (c *DaemonClient) WarmProfileCapabilities(ctx context.Context, config core.Config) error {
	reply, err := c.Request(ctx, DaemonActionWarmProfileCapabilities{Config: config})
	return daemonExpectDone(reply, err, "unexpected capability hydration reply")
}

func (c *DaemonClient) RecoverWebViewer(ctx context.Context, action WebViewerRecovery) error {
	reply, err := c.Request(ctx, DaemonActionRecoverWebViewer{Recovery: action})
	return daemonExpectDone(reply, err, "unexpected web viewer recovery reply")
}

func (c *DaemonClient) InspectWebListener(ctx context.Context) ([]WebListenerProcess, error) {
	reply, err := c.Request(ctx, DaemonActionInspectWebListener{})
	matched, err := daemonExpect[DaemonReplyWebListeners](reply, err, "unexpected listener inspection reply")
	return matched.Processes, err
}

func (c *DaemonClient) ListWorkspaces(ctx context.Context) ([]WorkspaceListing, error) {
	reply, err := c.Request(ctx, DaemonActionListWorkspaces{})
	matched, err := daemonExpect[DaemonReplyWorkspaces](reply, err, "unexpected daemon workspace reply")
	return matched.Workspaces, err
}

// ListWorkspaceDrafts is every workspace's detached drafts, without their
// text.
func (c *DaemonClient) ListWorkspaceDrafts(ctx context.Context) ([]WorkspaceDrafts, error) {
	reply, err := c.Request(ctx, DaemonActionListWorkspaceDrafts{})
	matched, err := daemonExpect[DaemonReplyWorkspaceDrafts](reply, err, "unexpected daemon workspace-drafts reply")
	return matched.Drafts, err
}

// HostCapacity is the capacity service's latest reading of every host and
// fleet. This reads what the daemon holds and never probes.
func (c *DaemonClient) HostCapacity(ctx context.Context) ([]core.HostCapacityRow, error) {
	reply, err := c.Request(ctx, DaemonActionHostCapacity{})
	matched, err := daemonExpect[DaemonReplyHostCapacity](reply, err, "unexpected host-capacity reply")
	return matched.Rows, err
}

// RefreshCapacity asks the daemon to refresh host capacity. Readings arrive
// on the feed.
func (c *DaemonClient) RefreshCapacity(ctx context.Context) error {
	reply, err := c.Request(ctx, DaemonActionRefreshCapacity{})
	return daemonExpectDone(reply, err, "unexpected refresh-capacity reply")
}

// RefreshQuota asks the daemon to probe every profile that is not on hold.
// It returns when the daemon has accepted the request, not when the probes
// finish.
func (c *DaemonClient) RefreshQuota(ctx context.Context) error {
	reply, err := c.Request(ctx, DaemonActionRefreshQuota{})
	return daemonExpectDone(reply, err, "unexpected refresh-quota reply")
}

// ProbeQuotaProfiles asks the daemon to probe exactly these profiles once,
// even disabled ones. forced probes now, off each profile's schedule, even
// with a fresh report. It returns when the daemon has queued the probes, not
// when they finish; the reports arrive in the runtime feed.
func (c *DaemonClient) ProbeQuotaProfiles(ctx context.Context, profileIDs []string, forced bool) error {
	reply, err := c.Request(ctx, DaemonActionProbeQuotaProfiles{ProfileIDs: profileIDs, Forced: forced})
	return daemonExpectDone(reply, err, "unexpected probe-quota reply")
}

func (c *DaemonClient) GitIdentity(ctx context.Context) (GitIdentity, error) {
	reply, err := c.Request(ctx, DaemonActionGitIdentity{})
	matched, err := daemonExpect[DaemonReplyGitIdentity](reply, err, "unexpected git-identity reply")
	return matched.Identity, err
}

func (c *DaemonClient) SetGitIdentity(ctx context.Context, name, email string) (GitIdentity, error) {
	reply, err := c.Request(ctx, DaemonActionSetGitIdentity{Name: name, Email: email})
	matched, err := daemonExpect[DaemonReplyGitIdentity](reply, err, "unexpected set-git-identity reply")
	return matched.Identity, err
}

func (c *DaemonClient) RenameProfile(ctx context.Context, oldID, newID string) error {
	reply, err := c.Request(ctx, DaemonActionRenameProfile{OldID: oldID, NewID: newID})
	return daemonExpectDone(reply, err, "unexpected rename-profile reply")
}

func (c *DaemonClient) RenameTarget(ctx context.Context, oldID, newID string) error {
	reply, err := c.Request(ctx, DaemonActionRenameTarget{OldID: oldID, NewID: newID})
	return daemonExpectDone(reply, err, "unexpected rename-target reply")
}

func (c *DaemonClient) CreateWorkspace(ctx context.Context, name string) (core.WorkspaceRecord, error) {
	reply, err := c.Request(ctx, DaemonActionCreateWorkspace{Name: name})
	matched, err := daemonExpect[DaemonReplyWorkspace](reply, err, "unexpected create-workspace reply")
	return matched.Workspace, err
}

func (c *DaemonClient) RenameWorkspace(ctx context.Context, workspaceID, name string) error {
	reply, err := c.Request(ctx, DaemonActionRenameWorkspace{WorkspaceID: workspaceID, Name: name})
	return daemonExpectDone(reply, err, "unexpected rename-workspace reply")
}

func (c *DaemonClient) TouchWorkspace(ctx context.Context, workspaceID string) error {
	reply, err := c.Request(ctx, DaemonActionTouchWorkspace{WorkspaceID: workspaceID})
	return daemonExpectDone(reply, err, "unexpected touch-workspace reply")
}

func (c *DaemonClient) CancelWorkspaceClose(ctx context.Context, workspaceID string) error {
	reply, err := c.Request(ctx, DaemonActionCancelWorkspaceClose{WorkspaceID: workspaceID})
	return daemonExpectDone(reply, err, "unexpected cancel workspace close reply:")
}

func (c *DaemonClient) CloseWorkspace(ctx context.Context, workspaceID string) error {
	reply, err := c.Request(ctx, DaemonActionCloseWorkspace{WorkspaceID: workspaceID})
	return daemonExpectDone(reply, err, "unexpected close workspace reply:")
}

func (c *DaemonClient) DeleteWorkspace(ctx context.Context, workspaceID string) error {
	reply, err := c.Request(ctx, DaemonActionDeleteWorkspace{WorkspaceID: workspaceID})
	return daemonExpectDone(reply, err, "unexpected delete-workspace reply")
}

func (c *DaemonClient) Attach(ctx context.Context, clientID string) error {
	reply, err := c.Request(ctx, DaemonActionAttach{ClientID: clientID})
	return daemonExpectDone(reply, err, "unexpected attach reply")
}

func (c *DaemonClient) Detach(ctx context.Context, clientID string) error {
	reply, err := c.Request(ctx, DaemonActionDetach{ClientID: clientID})
	return daemonExpectDone(reply, err, "unexpected detach reply")
}

func (c *DaemonClient) PersistReadReceipt(ctx context.Context, clientID, workspaceID, sessionID string, through uint64) (uint64, error) {
	reply, err := c.Request(ctx, DaemonActionPersistReadReceipt{
		ClientID: clientID, WorkspaceID: workspaceID, SessionID: sessionID, Through: through,
	})
	matched, err := daemonExpect[DaemonReplyOrdinal](reply, err, "unexpected read-receipt reply")
	return matched.Ordinal, err
}

func (c *DaemonClient) PersistTerminalDraft(
	ctx context.Context,
	clientID, sessionID string,
	ownerPid uint32,
	revision uint64,
	draft core.DetachedSessionDraft,
) error {
	reply, err := c.Request(ctx, DaemonActionPersistTerminalDraft{
		ClientID: clientID, SessionID: sessionID, OwnerPid: ownerPid, Revision: revision, Draft: draft,
	})
	return daemonExpectDone(reply, err, "unexpected terminal-draft reply")
}

// RecordAbandonedPrompt persists a prompt the user abandoned in the chat
// composer.
func (c *DaemonClient) RecordAbandonedPrompt(ctx context.Context, sessionID, bundleID, text string) error {
	reply, err := c.Request(ctx, DaemonActionRecordAbandonedPrompt{SessionID: sessionID, BundleID: bundleID, Text: text})
	return daemonExpectDone(reply, err, "unexpected abandoned-prompt reply")
}

func (c *DaemonClient) SaveSidebarSize(ctx context.Context, size core.PaneSize) error {
	reply, err := c.Request(ctx, DaemonActionSaveSidebarSize{Size: size})
	return daemonExpectDone(reply, err, "unexpected sidebar-size save reply")
}

// SetQuotaPreferences writes the load-balancing preference order, most
// preferred first.
func (c *DaemonClient) SetQuotaPreferences(ctx context.Context, prefer []string) error {
	reply, err := c.Request(ctx, DaemonActionSetQuotaPreferences{Prefer: prefer})
	return daemonExpectDone(reply, err, "unexpected quota-preference save reply")
}

func (c *DaemonClient) SaveWorkspacePaneSizes(ctx context.Context, workspaceID string, sizes core.PaneSizes) error {
	reply, err := c.Request(ctx, DaemonActionSaveWorkspacePaneSizes{WorkspaceID: workspaceID, Sizes: sizes})
	return daemonExpectDone(reply, err, "unexpected pane-size save reply")
}

func (c *DaemonClient) SaveWorkspaceLayout(ctx context.Context, workspaceID string, layout core.ConversationLayout) error {
	reply, err := c.Request(ctx, DaemonActionSaveWorkspaceLayout{WorkspaceID: workspaceID, Layout: layout})
	return daemonExpectDone(reply, err, "unexpected layout save reply")
}

func (c *DaemonClient) ImportSession(
	ctx context.Context,
	session core.SessionRecord,
	bundle *ImportedBundle,
	reader io.ReadSeeker,
) error {
	reply, err := c.UploadAndConsume(ctx, reader, func(blob string) DaemonAction {
		return DaemonActionImportSession{Blob: blob, Session: session, Bundle: bundle}
	})
	return daemonExpectDone(reply, err, "unexpected imported-session reply")
}

func (c *DaemonClient) SetSessionTitle(ctx context.Context, sessionID, title string) (string, error) {
	reply, err := c.Request(ctx, DaemonActionSetSessionTitle{SessionID: sessionID, Title: title})
	matched, err := daemonExpect[DaemonReplyText](reply, err, "unexpected session-title reply")
	return matched.Text, err
}

func (c *DaemonClient) SetSessionNotificationsIgnored(ctx context.Context, sessionID string, ignored bool) error {
	reply, err := c.Request(ctx, DaemonActionSetSessionNotificationsIgnored{SessionID: sessionID, Ignored: ignored})
	return daemonExpectDone(reply, err, "unexpected notifications-ignored reply")
}

func (c *DaemonClient) SetSessionWorkspace(ctx context.Context, sessionID, workspaceID string) error {
	reply, err := c.Request(ctx, DaemonActionSetSessionWorkspace{SessionID: sessionID, WorkspaceID: workspaceID})
	return daemonExpectDone(reply, err, "unexpected session-workspace reply")
}

// SetSessionContainerSettings returns the container upgrade the change needs,
// or nil when it applied directly.
func (c *DaemonClient) SetSessionContainerSettings(
	ctx context.Context,
	sessionID string,
	resourceAllocation core.SessionResourceAllocation,
	mounts []core.AdditionalMount,
	mountHistory []string,
) (*core.ContainerUpgradePreview, error) {
	reply, err := c.Request(ctx, DaemonActionSetSessionContainerSettings{
		SessionID: sessionID, ResourceAllocation: resourceAllocation, Mounts: mounts, MountHistory: mountHistory,
	})
	if err != nil {
		return nil, err
	}
	switch reply := reply.(type) {
	case DaemonReplyDone:
		return nil, nil
	case DaemonReplyContainerUpgradePreview:
		return &reply.Preview, nil
	}
	return nil, fmt.Errorf("unexpected container-settings reply %s", daemonReplyDebug(reply))
}

func (c *DaemonClient) MarkSessionTargetMissing(ctx context.Context, sessionID, detail, updatedAt string) (*core.SessionState, error) {
	reply, err := c.Request(ctx, DaemonActionMarkSessionTargetMissing{SessionID: sessionID, Detail: detail, UpdatedAt: updatedAt})
	matched, err := daemonExpect[DaemonReplyOptionalSessionState](reply, err, "unexpected target-missing reply")
	return matched.State, err
}

// ProjectCatalog reads the project catalog the daemon keeps.
func (c *DaemonClient) ProjectCatalog(ctx context.Context, refresh, retry bool) (core.ProjectCatalogView, error) {
	reply, err := c.Request(ctx, DaemonActionProjectCatalog{Refresh: refresh, Retry: retry})
	matched, err := daemonExpect[DaemonReplyProjectCatalog](reply, err, "unexpected project catalog reply:")
	return matched.View, err
}

func (c *DaemonClient) CreateProject(ctx context.Context, sources []string) (core.SavedProject, error) {
	reply, err := c.Request(ctx, DaemonActionCreateProject{Sources: sources})
	matched, err := daemonExpect[DaemonReplyProjectCreated](reply, err, "unexpected project creation reply:")
	return matched.Project, err
}

func (c *DaemonClient) RemoveProject(ctx context.Context, bundleID string) error {
	reply, err := c.Request(ctx, DaemonActionRemoveProject{BundleID: bundleID})
	return daemonExpectDone(reply, err, "unexpected project removal reply:")
}

func (c *DaemonClient) requestWithoutReconnect(ctx context.Context, action DaemonAction) (DaemonReply, error) {
	reply, err := c.requestOnce(ctx, action)
	if err != nil {
		return nil, err
	}
	if _, pending := reply.(DaemonReplyUpgradePending); pending {
		return nil, errors.New("the daemon refused admission while being replaced; reopen to sync again")
	}
	return reply, nil
}

func (c *DaemonClient) WikiSyncRequest(ctx context.Context) (WikiSyncStatus, error) {
	reply, err := c.requestWithoutReconnect(ctx, DaemonActionWikiSyncRequest{})
	matched, err := daemonExpect[DaemonReplyWikiSyncStatus](reply, err, "unexpected SessionWiki sync reply")
	return matched.Status, err
}

func (c *DaemonClient) WikiSyncWatch(ctx context.Context, epoch string, after uint64) (WikiSyncStatus, error) {
	reply, err := c.requestWithoutReconnect(ctx, DaemonActionWikiSyncWatch{Epoch: epoch, After: after})
	matched, err := daemonExpect[DaemonReplyWikiSyncStatus](reply, err, "unexpected SessionWiki sync reply")
	return matched.Status, err
}

// WikiSearch searches the user's SessionWiki index, newest first when the
// query is empty and best match first otherwise. The reply carries the state
// of the index as well as the rows, so a caller can say the first build is
// still running.
func (c *DaemonClient) WikiSearch(ctx context.Context, query string, limit int) (WikiSearchPage, error) {
	reply, err := c.requestWithoutReconnect(ctx, DaemonActionWikiSearch{Query: query, Limit: limit})
	matched, err := daemonExpect[DaemonReplyWikiRows](reply, err, "unexpected SessionWiki search reply")
	return matched.Page, err
}

// SessionTextSearch is the live sessions whose user or agent messages contain
// query, and which of the two matched. Answers from the SessionWiki index, so
// a very new message can be missing until the next sync.
func (c *DaemonClient) SessionTextSearch(ctx context.Context, query string) (SessionTextSearchPage, error) {
	reply, err := c.requestWithoutReconnect(ctx, DaemonActionSessionTextSearch{Query: query})
	matched, err := daemonExpect[DaemonReplySessionTextMatches](reply, err, "unexpected session text search reply")
	return matched.Page, err
}

// WikiBrief is the markdown briefing for one indexed session.
func (c *DaemonClient) WikiBrief(ctx context.Context, wikiID string, maxChars int) (string, error) {
	reply, err := c.Request(ctx, DaemonActionWikiBrief{WikiID: wikiID, MaxChars: maxChars})
	matched, err := daemonExpect[DaemonReplyText](reply, err, "unexpected SessionWiki brief reply")
	return matched.Text, err
}

// WikiHits is the passages of one indexed session that match a query, each
// matching message with contextMessages neighbours on either side and its
// text capped at perMessageChars. Nil when the index holds no session with
// that id.
func (c *DaemonClient) WikiHits(ctx context.Context, wikiID, query string, contextMessages, perMessageChars int) (*WikiHitTranscript, error) {
	reply, err := c.Request(ctx, DaemonActionWikiHits{
		WikiID: wikiID, Query: query, ContextMessages: contextMessages, PerMessageChars: perMessageChars,
	})
	matched, err := daemonExpect[DaemonReplyWikiHits](reply, err, "unexpected SessionWiki hits reply")
	return matched.Transcript, err
}

// WikiSession is what the index knows about one session, or nil when the
// index holds no session with that id.
func (c *DaemonClient) WikiSession(ctx context.Context, wikiID string) (*WikiSessionInfo, error) {
	reply, err := c.Request(ctx, DaemonActionWikiSession{WikiID: wikiID})
	matched, err := daemonExpect[DaemonReplyWikiSession](reply, err, "unexpected SessionWiki session reply")
	return matched.Info, err
}

// WikiRestore starts a new session carrying a hand-off compacted from an
// archived one. It answers like any other session start: the record exists
// and is provisioning, and the hand-off follows once the harness is ready.
func (c *DaemonClient) WikiRestore(ctx context.Context, request WikiRestoreRequest) (RegisteredSession, error) {
	reply, err := c.Request(ctx, DaemonActionWikiRestore{Request: request})
	matched, err := daemonExpect[DaemonReplyRegisteredSession](reply, err, "unexpected SessionWiki restore reply")
	return matched.Registered, err
}

func (c *DaemonClient) ScanRecovery(ctx context.Context, allInstances bool) (core.RecoveryScan, error) {
	reply, err := c.Request(ctx, DaemonActionScanRecovery{AllInstances: allInstances})
	matched, err := daemonExpect[DaemonReplyRecoveryScan](reply, err, "unexpected recovery-scan reply")
	return matched.Scan, err
}

// AdoptRecovery runs one adoption on this connection, reporting each wait and
// the installed session to progress as the daemon streams them.
func (c *DaemonClient) AdoptRecovery(
	ctx context.Context,
	adoption RecoveryAdoptionRequest,
	progress func(RecoveryAdoptionProgress),
) error {
	stream, err := c.takeStream()
	if err != nil {
		return err
	}
	release := daemonWatch(ctx, stream)
	requestID := c.nextRequestID
	c.nextRequestID++
	err = WriteFrame(stream, RequestEnvelope{
		ProtocolVersion: c.metadata.ProtocolVersion,
		RequestID:       requestID,
		Token:           c.metadata.Token,
		Action:          DaemonActionAdoptRecovery{Request: adoption},
	})
	if err != nil {
		return daemonAbandon(ctx, stream, release, transportLost(err))
	}
	for {
		response, err := ReadResponse(stream)
		if err != nil {
			return daemonAbandon(ctx, stream, release, fmt.Errorf(
				"adoption connection ended before completion; check mj sessions, then rerun recover adopt if needed: %w",
				transportLost(err)))
		}
		if response.ProtocolVersion != c.metadata.ProtocolVersion {
			return daemonAbandon(ctx, stream, release, errors.New("daemon changed protocol"))
		}
		if response.RequestID != requestID {
			return daemonAbandon(ctx, stream, release,
				fmt.Errorf("daemon crossed request IDs: expected %d, received %d", requestID, response.RequestID))
		}
		if response.Result.Err == nil {
			switch reply := response.Result.Ok.(type) {
			case DaemonReplyAdoptionWaiting:
				progress(RecoveryAdoptionProgressWaiting{Blockers: reply.Blockers, DestroyCommand: reply.DestroyCommand})
				continue
			case DaemonReplyAdoptionInstalled:
				progress(RecoveryAdoptionProgressInstalled{SessionID: reply.SessionID})
				continue
			}
		}
		if !release() {
			_ = stream.Close()
			return ctx.Err()
		}
		c.stream = stream
		if response.Result.Err != nil {
			return errors.New(*response.Result.Err)
		}
		if _, done := response.Result.Ok.(DaemonReplyDone); done {
			return nil
		}
		return fmt.Errorf("unexpected recovery-adoption reply %s", daemonReplyDebug(response.Result.Ok))
	}
}

func (c *DaemonClient) DestroyRecovery(ctx context.Context, sessionID, targetID, confirmation string, allInstances bool) error {
	reply, err := c.Request(ctx, DaemonActionDestroyRecovery{
		SessionID: sessionID, TargetID: targetID, Confirmation: confirmation, AllInstances: allInstances,
	})
	return daemonExpectDone(reply, err, "unexpected recovery-destroy reply")
}

func (c *DaemonClient) Snapshot(ctx context.Context, workspaceID string) (WorkspaceSnapshot, error) {
	reply, err := c.Request(ctx, DaemonActionSnapshot{WorkspaceID: workspaceID})
	matched, err := daemonExpect[DaemonReplySnapshot](reply, err, "unexpected snapshot reply")
	return matched.Snapshot, err
}

func (c *DaemonClient) DashboardStartup(ctx context.Context, clientID, workspaceID string) (DashboardStartup, error) {
	reply, err := c.Request(ctx, DaemonActionDashboardStartup{ClientID: clientID, WorkspaceID: workspaceID})
	matched, err := daemonExpect[DaemonReplyDashboardStartup](reply, err, "unexpected dashboard startup reply")
	return matched.Startup, err
}

func (c *DaemonClient) WorkspaceLayouts(ctx context.Context, workspaceIDs []string) (WorkspaceLayouts, error) {
	reply, err := c.Request(ctx, DaemonActionWorkspaceLayouts{WorkspaceIDs: workspaceIDs})
	matched, err := daemonExpect[DaemonReplyWorkspaceLayouts](reply, err, "unexpected workspace layouts reply")
	return matched.Layouts, err
}

func (c *DaemonClient) StoredSessionMetadata(ctx context.Context) (StoredSessionMetadata, error) {
	reply, err := c.Request(ctx, DaemonActionStoredSessionMetadata{})
	matched, err := daemonExpect[DaemonReplyStoredSessionMetadata](reply, err, "unexpected stored session metadata reply")
	return matched.Metadata, err
}

func (c *DaemonClient) StoredProjectionTail(ctx context.Context, sessionID string, items int) (*core.MaterializedSession, error) {
	reply, err := c.Request(ctx, DaemonActionStoredProjectionTail{SessionID: sessionID, Items: items})
	matched, err := daemonExpect[DaemonReplyStoredProjectionTail](reply, err, "unexpected stored projection reply")
	return matched.Tail, err
}

func (c *DaemonClient) StoredTranscriptItem(ctx context.Context, sessionID, stableID string) (StoredTranscriptItem, error) {
	// Never send an unknown action to an older daemon or retry its refusal.
	if c.metadata.ProtocolVersion < StoredTranscriptReadProtocol {
		return StoredTranscriptItemUnsupported{}, nil
	}
	reply, err := c.Request(ctx, DaemonActionStoredTranscriptItem{SessionID: sessionID, StableID: stableID})
	matched, err := daemonExpect[DaemonReplyStoredTranscriptItem](reply, err, "unexpected stored transcript item reply")
	return daemonStoredTranscriptItem(matched, err)
}

func (c *DaemonClient) StoredAgentMessageThrough(ctx context.Context, sessionID string, through uint64) (StoredTranscriptItem, error) {
	if c.metadata.ProtocolVersion < NotificationStackProtocol {
		return StoredTranscriptItemUnsupported{}, nil
	}
	reply, err := c.Request(ctx, DaemonActionStoredAgentMessageThrough{SessionID: sessionID, Through: through})
	matched, err := daemonExpect[DaemonReplyStoredTranscriptItem](reply, err, "unexpected stored agent message reply")
	return daemonStoredTranscriptItem(matched, err)
}

// daemonStoredTranscriptItem is a stored transcript reply as the result a
// caller branches on.
func daemonStoredTranscriptItem(reply DaemonReplyStoredTranscriptItem, err error) (StoredTranscriptItem, error) {
	if err != nil {
		return nil, err
	}
	if reply.Item == nil {
		return StoredTranscriptItemMissing{}, nil
	}
	return StoredTranscriptItemFound{Item: reply.Item}, nil
}

func (c *DaemonClient) StoredSessionSummary(ctx context.Context, sessionID string) (*core.MaterializedSessionSummary, error) {
	reply, err := c.Request(ctx, DaemonActionStoredSessionSummary{SessionID: sessionID})
	matched, err := daemonExpect[DaemonReplyStoredSessionSummary](reply, err, "unexpected stored session summary reply")
	return matched.Summary, err
}

func (c *DaemonClient) ResumeRepositoryPreflight(ctx context.Context, sessionID, targetID string) (core.ResumeRepositorySourcePreflight, error) {
	reply, err := c.Request(ctx, DaemonActionResumeRepositoryPreflight{SessionID: sessionID, TargetID: targetID})
	matched, err := daemonExpect[DaemonReplyResumeRepositoryPreflight](reply, err, "unexpected resume repository preflight reply")
	return matched.Preflight, err
}

func (c *DaemonClient) ReplaceResumeRepositoryOrigin(
	ctx context.Context,
	sessionID, repositoryID, replacement string,
) (core.ResumeRepositorySourcePreflight, error) {
	reply, err := c.Request(ctx, DaemonActionReplaceResumeRepositoryOrigin{
		SessionID: sessionID, RepositoryID: repositoryID, Replacement: replacement,
	})
	matched, err := daemonExpect[DaemonReplyResumeRepositoryPreflight](reply, err, "unexpected repository origin replacement reply")
	return matched.Preflight, err
}

func (c *DaemonClient) ArchiveSpacePreview(ctx context.Context, olderThanDays *uint32) (core.ArchiveSpacePreview, error) {
	reply, err := c.Request(ctx, DaemonActionArchiveSpacePreview{OlderThanDays: olderThanDays})
	matched, err := daemonExpect[DaemonReplyArchiveSpacePreview](reply, err, "unexpected archive space preview reply")
	return matched.Preview, err
}

func (c *DaemonClient) SaveSetup(ctx context.Context, original, updated string) (core.Config, error) {
	reply, err := c.Request(ctx, DaemonActionSaveSetup{Original: original, Updated: updated})
	matched, err := daemonExpect[DaemonReplySavedSetup](reply, err, "unexpected setup save reply")
	return matched.Config, err
}

func (c *DaemonClient) RuntimeChanges(ctx context.Context, cursor *RuntimeCursor, wait bool) (RuntimeFrame, error) {
	return c.RuntimeChangesForSubagentParents(ctx, cursor, wait, []string{})
}

// RuntimeChangesForSubagentParents asks for the runtime changes since
// cursor, scoped to the sub-agents of subagentParents (a sorted set).
func (c *DaemonClient) RuntimeChangesForSubagentParents(
	ctx context.Context,
	cursor *RuntimeCursor,
	wait bool,
	subagentParents []string,
) (RuntimeFrame, error) {
	reply, err := c.Request(ctx, DaemonActionRuntimeChanges{Cursor: cursor, Wait: wait, SubagentParents: subagentParents})
	matched, err := daemonExpect[DaemonReplyRuntimeChanges](reply, err, "unexpected runtime changes reply")
	return matched.Frame, err
}

func (c *DaemonClient) SessionTail(ctx context.Context, sessionID string, cursor RuntimeCursor) (SessionTailReply, error) {
	reply, err := c.Request(ctx, DaemonActionSessionTail{SessionID: sessionID, Cursor: cursor})
	matched, err := daemonExpect[DaemonReplySessionTail](reply, err, "unexpected session tail reply")
	return matched.Reply, err
}

func (c *DaemonClient) ResumeCandidates(ctx context.Context) (ResumeCandidates, error) {
	reply, err := c.Request(ctx, DaemonActionResumeCandidates{})
	matched, err := daemonExpect[DaemonReplyResumeCandidates](reply, err, "unexpected resume candidates reply")
	return matched.Candidates, err
}

func (c *DaemonClient) SessionRecord(ctx context.Context, sessionID string) (*core.SessionRecord, error) {
	session, err := c.SessionRecordWithVerdict(ctx, sessionID)
	if err != nil || session == nil {
		return nil, err
	}
	return &session.Record, nil
}

func (c *DaemonClient) SessionRecordWithVerdict(ctx context.Context, sessionID string) (*SessionRecordWithVerdict, error) {
	reply, err := c.Request(ctx, DaemonActionSessionRecord{SessionID: sessionID})
	matched, err := daemonExpect[DaemonReplySessionRecord](reply, err, "unexpected session record reply")
	return matched.Record, err
}

func (c *DaemonClient) GoStartupSession(ctx context.Context, workspaceID string, lastSessionID *string) (*core.SessionRecord, error) {
	reply, err := c.Request(ctx, DaemonActionGoStartupSession{WorkspaceID: workspaceID, LastSessionID: lastSessionID})
	matched, err := daemonExpect[DaemonReplyGoStartupSession](reply, err, "unexpected go startup session reply")
	return matched.Record, err
}

func (c *DaemonClient) SubmitSessionCommand(
	ctx context.Context,
	sessionID, commandID string,
	command core.RelayCommand,
	inheritedDraft *string,
) (uint64, error) {
	reply, err := c.Request(ctx, DaemonActionSubmitSessionCommand{
		InheritedDraft: inheritedDraft, SessionID: sessionID, CommandID: commandID, Command: command,
	})
	matched, err := daemonExpect[DaemonReplyOrdinal](reply, err, "unexpected session command reply")
	return matched.Ordinal, err
}

// QueueStartupPrompt hands the daemon a prompt for a session that is still
// starting. The daemon replies as soon as the prompt is queued, not when it
// is delivered; delivery failures come back as a session notice.
func (c *DaemonClient) QueueStartupPrompt(
	ctx context.Context,
	sessionID, commandID, text string,
	prompt []acpschema.ContentBlock,
	inheritedDraft *string,
	encodedDraft string,
	moveOperationID *string,
) error {
	reply, err := c.Request(ctx, DaemonActionQueueStartupPrompt{
		SessionID: sessionID, CommandID: commandID, Text: text, Prompt: prompt,
		EncodedDraft: encodedDraft, MoveOperationID: moveOperationID, InheritedDraft: inheritedDraft,
	})
	if _, conflict := reply.(DaemonReplyStartupPromptIdentityConflict); err == nil && conflict {
		return StartupPromptIdentityConflict{}
	}
	return daemonExpectDone(reply, err, "unexpected startup prompt reply")
}

// WithdrawStartupPrompt takes a prompt back from the daemon's startup queue
// before delivery. True means it is withdrawn and will not be sent; false
// means delivery had already started, so it was sent. An error means the
// daemon could not be asked, and the prompt may still be queued.
func (c *DaemonClient) WithdrawStartupPrompt(ctx context.Context, sessionID, commandID string) (bool, error) {
	reply, err := c.Request(ctx, DaemonActionWithdrawStartupPrompt{SessionID: sessionID, CommandID: commandID})
	matched, err := daemonExpect[DaemonReplyPromptWithdrawn](reply, err, "unexpected startup prompt withdrawal reply")
	return matched.Withdrawn, err
}

// StartupPromptOutcomes reads only the requested durable terminal receipts
// (at most 128 IDs).
func (c *DaemonClient) StartupPromptOutcomes(ctx context.Context, sessionID string, commandIDs []string) (map[string]StartupPromptOutcome, error) {
	reply, err := c.Request(ctx, DaemonActionStartupPromptOutcomes{SessionID: sessionID, CommandIDs: commandIDs})
	matched, err := daemonExpect[DaemonReplyStartupPromptOutcomes](reply, err, "unexpected startup prompt outcomes reply")
	return matched.Outcomes, err
}

// StartTurnReview asks the daemon to review committed work through the push
// gate.
//
// The refusal includes how to proceed, such as committing tracked changes or
// choosing a harness with push-gate support. It travels as text rather than
// as a code every surface would have to translate.
func (c *DaemonClient) StartTurnReview(ctx context.Context, sessionID string) error {
	reply, err := c.Request(ctx, DaemonActionStartTurnReview{SessionID: sessionID})
	return daemonExpectDone(reply, err, "unexpected review reply")
}

func (c *DaemonClient) SyncSession(ctx context.Context, sessionID string) error {
	reply, err := c.Request(ctx, DaemonActionSyncSession{SessionID: sessionID})
	return daemonExpectDone(reply, err, "unexpected session sync reply")
}

func (c *DaemonClient) RespondElicitation(ctx context.Context, sessionID, elicitationID string, response core.ElicitationResponse) error {
	reply, err := c.Request(ctx, DaemonActionRespondElicitation{
		SessionID: sessionID, ElicitationID: elicitationID, Response: response,
	})
	return daemonExpectDone(reply, err, "unexpected elicitation reply")
}

func (c *DaemonClient) NativeAgentHistory(
	ctx context.Context,
	owner, child string,
	before *DaemonPair[uint64, string],
) (core.NativeAgentHistoryPage, error) {
	reply, err := c.Request(ctx, DaemonActionNativeAgentHistory{Owner: owner, Child: child, Before: before})
	matched, err := daemonExpect[DaemonReplyNativeAgentHistory](reply, err, "unexpected native agent history reply")
	return matched.Page, err
}

func (c *DaemonClient) StopBackgroundTask(ctx context.Context, sessionID, backgroundTaskID string) error {
	reply, err := c.Request(ctx, DaemonActionStopBackgroundTask{SessionID: sessionID, BackgroundTaskID: backgroundTaskID})
	return daemonExpectDone(reply, err, "unexpected background task stop reply")
}

// StartSessionLifecycle submits once and retains the receipt; a lost reply
// does not permit replay.
//
// Any failure other than the daemon's refusal is a
// LifecycleAdmissionUnconfirmed carrying the failure's text, but not its
// type, as Rust's anyhow context does.
func (c *DaemonClient) StartSessionLifecycle(ctx context.Context, request SessionLifecycleRequest) (LifecycleReceipt, error) {
	sessionID := SessionLifecycleRequestSessionID(request)
	reply, err := c.Request(ctx, DaemonActionStartSessionLifecycle{Request: request})
	if err != nil {
		var refusal DaemonRefusal
		if errors.As(err, &refusal) {
			return LifecycleReceipt{}, err
		}
		return LifecycleReceipt{}, fmt.Errorf("%s: %w", err, LifecycleAdmissionUnconfirmed{SessionID: sessionID})
	}
	if accepted, ok := reply.(DaemonReplyLifecycleAccepted); ok {
		return accepted.Receipt, nil
	}
	return LifecycleReceipt{}, fmt.Errorf("unexpected lifecycle acceptance reply %s: %w",
		daemonReplyDebug(reply), LifecycleAdmissionUnconfirmed{SessionID: sessionID})
}

// WaitSessionLifecycle observes exactly this operation. Abandoning the
// observer never cancels it.
//
// A failure to observe is a LifecycleObservationUnavailable carrying the
// failure's text, but not its type: a refused observation is not the
// operation's refusal. Only a captured operation failure is a DaemonRefusal.
func (c *DaemonClient) WaitSessionLifecycle(ctx context.Context, receipt LifecycleReceipt) (DaemonReply, error) {
	reply, err := c.Request(ctx, DaemonActionWaitSessionLifecycle{Receipt: receipt})
	if err != nil {
		return nil, fmt.Errorf("%s: %w", err, LifecycleObservationUnavailable{Receipt: receipt})
	}
	switch reply := reply.(type) {
	case DaemonReplyDone, DaemonReplyMoveOutcome:
		return reply, nil
	case DaemonReplyLifecycleFailed:
		return nil, DaemonRefusal{Message: reply.Detail}
	case DaemonReplyLifecycleObservationUnavailable:
		return nil, LifecycleObservationUnavailable{Receipt: reply.Receipt}
	}
	return nil, fmt.Errorf("unexpected lifecycle completion reply %s: %w",
		daemonReplyDebug(reply), LifecycleObservationUnavailable{Receipt: receipt})
}

func (c *DaemonClient) SuspendPreflight(ctx context.Context, sessionID string, dryRun, withoutCheckpoint bool) (core.SuspendPreflight, error) {
	reply, err := c.Request(ctx, DaemonActionSuspendPreflight{
		SessionID: sessionID, DryRun: dryRun, WithoutCheckpoint: withoutCheckpoint,
	})
	matched, err := daemonExpect[DaemonReplySuspendPreflight](reply, err, "unexpected suspend-preflight reply")
	return matched.Preflight, err
}

func (c *DaemonClient) SuspendSession(ctx context.Context, sessionID string) error {
	return c.SuspendSessionWithConsent(ctx, sessionID, core.SuspendConsent{})
}

func (c *DaemonClient) SuspendSessionWithConsent(ctx context.Context, sessionID string, consent core.SuspendConsent) error {
	reply, err := c.Request(ctx, DaemonActionSuspendSession{
		SessionID:           sessionID,
		DiscardUntracked:    consent.DiscardUntracked,
		AbortOperations:     consent.AbortOperations,
		WithoutCheckpoint:   consent.WithoutCheckpoint,
		ConfirmNoCheckpoint: consent.ConfirmNoCheckpoint,
	})
	return daemonExpectDone(reply, err, "unexpected close-session reply")
}

func (c *DaemonClient) RestartSession(ctx context.Context, sessionID string) error {
	return c.RestartSessionWithAck(ctx, sessionID, false)
}

func (c *DaemonClient) RestartSessionWithAck(ctx context.Context, sessionID string, acknowledgeContainerRecreation bool) error {
	if c.metadata.ProtocolVersion != ProtocolVersion {
		return fmt.Errorf("cannot Restart a session: daemon protocol %d does not match client protocol %d; restart the daemon, then try again",
			c.metadata.ProtocolVersion, ProtocolVersion)
	}
	reply, err := c.Request(ctx, DaemonActionRestartSession{
		SessionID: sessionID, AcknowledgeContainerRecreation: acknowledgeContainerRecreation,
	})
	return daemonExpectDone(reply, err, "unexpected restart-session reply")
}

func (c *DaemonClient) StartCreateSession(ctx context.Context, request CreateSessionRequest) (RegisteredSession, error) {
	reply, err := c.Request(ctx, DaemonActionStartCreateSession{Request: request})
	matched, err := daemonExpect[DaemonReplyRegisteredSession](reply, err, "unexpected start-create reply")
	return matched.Registered, err
}

// WaitCreateSession waits for a session's creation to end. Answered from
// durable state, so any daemon gives the same answer; the wait is read-only
// and repeatable. One attempt with no internal reconnect: a daemon that stops
// drops the connection, which carries DaemonTransportLost, and the caller
// starts the next daemon and asks again.
func (c *DaemonClient) WaitCreateSession(ctx context.Context, sessionID string) error {
	reply, err := c.requestOnce(ctx, DaemonActionWaitCreateSession{SessionID: sessionID})
	return daemonExpectDone(reply, err, "unexpected wait-create reply")
}

func (c *DaemonClient) ResumeSession(ctx context.Context, request ResumeSessionRequest) error {
	reply, err := c.Request(ctx, DaemonActionResumeSession{Request: request})
	return daemonExpectDone(reply, err, "unexpected resume-session reply")
}

func (c *DaemonClient) DestroyStoppedSession(ctx context.Context, sessionID string) error {
	reply, err := c.Request(ctx, DaemonActionDestroyStoppedSession{SessionID: sessionID})
	return daemonExpectDone(reply, err, "unexpected destroy-stopped reply")
}

func (c *DaemonClient) ForceDestroySession(ctx context.Context, sessionID string) error {
	reply, err := c.Request(ctx, DaemonActionForceDestroySession{SessionID: sessionID})
	return daemonExpectDone(reply, err, "unexpected force-destroy reply")
}

func (c *DaemonClient) CancelLifecycle(ctx context.Context, sessionID string) error {
	reply, err := c.Request(ctx, DaemonActionCancelLifecycle{SessionID: sessionID})
	return daemonExpectDone(reply, err, "unexpected cancel-lifecycle reply")
}

func (c *DaemonClient) CancelContainerUpgrade(ctx context.Context, sessionID string) error {
	reply, err := c.Request(ctx, DaemonActionCancelContainerUpgrade{SessionID: sessionID})
	return daemonExpectDone(reply, err, "unexpected container-upgrade cancellation reply")
}

func (c *DaemonClient) RecoverDraft(ctx context.Context, draftID string) error {
	reply, err := c.Request(ctx, DaemonActionRecoverDraft{DraftID: draftID})
	return daemonExpectDone(reply, err, "unexpected recover-draft reply")
}

func (c *DaemonClient) Stop(ctx context.Context) error {
	reply, err := c.Request(ctx, DaemonActionStop{})
	return daemonExpectDone(reply, err, "unexpected stop reply")
}

func (c *DaemonClient) PreviewContainerUpgrade(ctx context.Context, sessionID string) (core.ContainerUpgradePreview, error) {
	reply, err := c.Request(ctx, DaemonActionPreviewContainerUpgrade{SessionID: sessionID})
	matched, err := daemonExpect[DaemonReplyContainerUpgradePreview](reply, err, "unexpected container upgrade preview reply:")
	return matched.Preview, err
}

func (c *DaemonClient) UpgradeContainer(ctx context.Context, request core.ContainerUpgradeRequest) error {
	reply, err := c.Request(ctx, DaemonActionUpgradeContainer{Request: request})
	return daemonExpectDone(reply, err, "unexpected container upgrade reply:")
}

func (c *DaemonClient) PrepareMoveSession(ctx context.Context, selection core.MoveSelection) (core.MovePreparation, error) {
	reply, err := c.Request(ctx, DaemonActionPrepareMoveSession{Selection: selection})
	if err != nil {
		return core.MovePreparation{}, err
	}
	if matched, ok := reply.(DaemonReplyMovePreparation); ok {
		return matched.Preparation, nil
	}
	return core.MovePreparation{}, errors.New("daemon returned an unexpected move preparation reply")
}

func (c *DaemonClient) MoveSources(ctx context.Context, sessionID string, cleanupOperationID *string) ([]core.RetainedMoveSource, error) {
	reply, err := c.Request(ctx, DaemonActionMoveSources{SessionID: sessionID, CleanupOperationID: cleanupOperationID})
	matched, err := daemonExpect[DaemonReplyMoveSources](reply, err, "unexpected Move sources reply:")
	return matched.Sources, err
}

func (c *DaemonClient) MoveSession(ctx context.Context, request core.MoveSessionRequest) (core.MoveOutcome, error) {
	reply, err := c.Request(ctx, DaemonActionMoveSession{Request: request})
	if err != nil {
		return core.MoveOutcome{}, err
	}
	if matched, ok := reply.(DaemonReplyMoveOutcome); ok {
		return matched.Outcome, nil
	}
	return core.MoveOutcome{}, errors.New("daemon returned an unexpected move reply")
}

// ConnectExisting connects to the daemon this process's placement names,
// without starting one.
func ConnectExisting(ctx context.Context) (*DaemonClient, error) {
	connector, err := DaemonConnectorShared(ctx)
	if err != nil {
		return nil, err
	}
	return connector.ConnectExisting(ctx)
}

// ManagementClient is a handle to whatever daemon the metadata file
// advertises, regardless of its protocol version. It only exposes the frozen
// management subset (Ping, Status, Stop), which encodes identically in every
// protocol version.
type ManagementClient struct {
	inner *DaemonClient
}

func NewManagementClient(inner *DaemonClient) *ManagementClient {
	return &ManagementClient{inner: inner}
}

func (m *ManagementClient) ProtocolVersion() uint32 {
	return m.inner.metadata.ProtocolVersion
}

// Metadata is the endpoint this management client dialed.
func (m *ManagementClient) Metadata() DaemonMetadata {
	return m.inner.metadata
}

// Close drops the management connection, as dropping the Rust client does.
func (m *ManagementClient) Close() { m.inner.Close() }

func (m *ManagementClient) Identity(ctx context.Context) (DaemonIdentity, error) {
	return m.inner.Identity(ctx)
}

func (m *ManagementClient) Status(ctx context.Context) (DaemonStatus, error) {
	return m.inner.Status(ctx)
}

// Stop asks the daemon to stop, waiting up to StopTimeout for it to
// acknowledge.
func (m *ManagementClient) Stop(ctx context.Context) error {
	ctx, cancel := context.WithTimeout(ctx, StopTimeout)
	defer cancel()
	err := m.inner.Stop(ctx)
	if err != nil && errors.Is(ctx.Err(), context.DeadlineExceeded) {
		return fmt.Errorf("Mjolnir daemon did not acknowledge the stop before the deadline: %w", err)
	}
	return err
}

// ConnectManagement dials whatever daemon the metadata file advertises.
func ConnectManagement(ctx context.Context) (*ManagementClient, error) {
	metadata, err := ReadMetadataAny()
	if err != nil {
		return nil, err
	}
	inner, err := DaemonClientConnect(ctx, metadata)
	if err != nil {
		return nil, err
	}
	return &ManagementClient{inner: inner}, nil
}

// EnsureSupportedDaemonProtocol refuses to speak to a daemon whose protocol
// this build does not know.
//
// The two protocol numbers alone do not say which `mj` ran. The usual cause
// is a second installation: a `cargo install`ed client sits earlier on PATH
// than the build whose daemon is running, so every command fails here while
// the other binary works, and nothing in the message says where either
// lives. It therefore names both executables and both versions.
func EnsureSupportedDaemonProtocol(metadata DaemonMetadata) error {
	if metadata.ProtocolVersion <= ProtocolVersion {
		return nil
	}
	return errors.New(unsupportedDaemonProtocolMessage(
		metadata.ProtocolVersion,
		DescribeRunningDaemonAndClientBuilds(metadata.Pid, metadata.BuildVersion),
	))
}

// unsupportedDaemonProtocolMessage is the message
// EnsureSupportedDaemonProtocol fails with, given the sentence that names
// both builds, so it can be read without a daemon.
func unsupportedDaemonProtocolMessage(daemonProtocol uint32, builds string) string {
	return fmt.Sprintf("the daemon uses a newer protocol (%d) than this client (%d). %s. "+
		"Put the daemon's directory first on PATH, or reinstall this client from that build.",
		daemonProtocol, ProtocolVersion, builds)
}

// Protocol history, newest last:
// Plan-only review persistence actions were removed.
// Settings can warm draft profile capabilities through the daemon-owned catalog.
// Runtime deltas carry transcript item changes, and a client fetches a
// session's tail from the daemon at its cursor instead of reading SQLite.
// Lifecycle stages can name the harness preparation step a worker reports.
// A pending form can say it is a tool permission and whether it offers Skip.
// A usage report cost is a Result whose Err means the cost is unknown.
// Creation and runtime metadata carry interactive launch-choice provenance.
// New-session creation independently controls whether its container size
// becomes the host's saved default.
// Operator-away action and runtime metadata.
// Structured startup prompts, caller-owned command IDs, and ID withdrawal.
// Terminals no longer save harness titles; the daemon alone projects them.
// Human interface activity can reset the daemon's automatic AFK clock.
// Project removal runs through the daemon-owned database writer.
// Per-profile remembered subagent preferences change runtime metadata shape.
// Explicit Ignore and Suspend preserves checkpoint verification while omitting nested repositories.
// Web viewer access includes the bound local connection address.
// Review role rows carry their child session IDs; older strict clients reject them.
// Dashboards read stored state through the daemon instead of opening the store,
// and client attachments are renewable leases rather than client process IDs.
// Nodes connect with NodeConnect and then speak the node protocol.
// Dashboard startup uses chunked replies, which older clients reject for this reply type.
// Shared state publishes child metadata; launch intent remains in the daemon store.
// Startup seeds runtime cursors with family scope; reply chunks use base64 bytes.
// Clients read and set the daemon host's Git commit author.
// The daemon reports its per-start identity, build and placement instead of
// clients reading its PID and `/proc`, and it serves the configuration that
// owns the store instead of clients opening `config.toml`.
// SessionReview::Inherit adds a value older API clients cannot deserialize.
// Client views no longer decode forwarding phases or turn-review resolutions;
// reviewer capture actions no longer carry turn scope or baselines.
// RuntimeSessionView carries policy-owned config keys; older strict readers reject them.
// Terminal draft persistence carries per-client revisions and clear tombstones.
// The host node is gone: NodeConnect is no longer an action.
// Lifecycle submission acknowledges ownership separately from completion.
// Clients obtain the HTTP API credential over control with ApiCredential.
// The dashboard's path, project and preflight work are daemon actions, and
// closing a cancellable action's connection cancels it.
// Version 91: unauthenticated nonce proof before SSH tunnel credentials.
// Authenticated chunked client uploads and daemon-owned attachment installation.
// Daemon-owned voice credential discovery and uploaded-audio transcription.
// Version 92: streamed uploads, client imports and daemon voice actions.
// Version 93: Antigravity harness identities in session and profile snapshots.
// Version 94: served unread attention, notification dismissal targets and
// unread frontiers.
// Version 95: authenticated daemon-owned login terminal streams.
// Version 95: milestone-4 setup discovery, availability, login status and machine actions.
// Version 97: the daemon decides which sessions are notifications.
// Version 98: container settings use the single resource allocation.
// Version 99: one-shot quota probes for profiles outside the enabled set.
// Version 100: a one-shot quota probe can be forced off the profile's
// schedule; the daemon answers one all-workspaces draft listing.
// Version 101: the daemon answers the capacity service's per-host readings.
// Version 102: review views carry the writing_brief phase and brief-writer role.
// Version 103: configuration carries the top-level `[images]` section (project
// images built by `mj image build`), which older clients' strict config reader
// rejects.
// Version 104: clients read configuration keys they have no field for, so a
// configuration key added later needs no protocol version of its own; only
// the configuration file refuses a key no field reads. `SubagentChoice` is
// the exception and still refuses unknown keys everywhere.
// Version 105: indexed single-item stored transcript reads for notification
// text use chunked replies; legacy count-mode stored projection tails remain
// unchunked.
// Version 107: explicit SessionWiki sync generations and progress watches.
// Version 109: the daemon answers whether first-run setup is due, so a
// dashboard shows the first-run welcome only for an installation that needs it.

const (
	// NotificationStackProtocol is the first protocol with notification cards
	// and completion-bound transcript reads.
	NotificationStackProtocol uint32 = 106
	// AlertEventsProtocol is the first protocol with daemon-owned notify-once
	// alert events.
	AlertEventsProtocol uint32 = 109
	// SuspendOnlyCheckpointProtocol: explicit checkpoint actions were
	// removed; legacy requests get a clear refusal.
	SuspendOnlyCheckpointProtocol uint32 = 110
	// QuiescentAdoptionProtocol: adoption streams cancellable quiescence
	// waits before its atomic install.
	QuiescentAdoptionProtocol = SuspendOnlyCheckpointProtocol + 1
	// ToolPermissionCallProtocol: tool permission forms name their
	// transcript call, which older clients reject.
	ToolPermissionCallProtocol = QuiescentAdoptionProtocol + 1
	// RuntimeHostCapacityProtocol: the runtime feed carries host capacity,
	// and clients can refresh it.
	RuntimeHostCapacityProtocol = ToolPermissionCallProtocol + 1
	// RuntimeAvailabilityProtocol: availability checks are requested
	// independently of their runtime feed results.
	RuntimeAvailabilityProtocol        = RuntimeHostCapacityProtocol + 1
	RuntimeSessionMaterializedProtocol = RuntimeAvailabilityProtocol + 1
	RuntimeSessionVerdictsProtocol     = RuntimeSessionMaterializedProtocol + 1
	RuntimeShellInterruptProtocol      = RuntimeSessionVerdictsProtocol + 1
	// RuntimeReviewIdentityProtocol: a review view names its review and the
	// repositories it covers, and a session can have several open. Older
	// readers reject the new fields.
	RuntimeReviewIdentityProtocol = RuntimeShellInterruptProtocol + 1
	// SuspendConsentProtocol: typed suspend consent, suspend preflight, and
	// removal of publication consent and delete_branch.
	SuspendConsentProtocol  = RuntimeReviewIdentityProtocol + 1
	MoveOriginDeltaProtocol = SuspendConsentProtocol + 1
	// SupervisorsProtocol: supervisor delegation adds `supervisors` on
	// session creation/move and runtime feed.
	SupervisorsProtocol = MoveOriginDeltaProtocol + 1
	// OrchestratorsProtocol: orchestrator opt-in is part of the delegation
	// policy.
	OrchestratorsProtocol = SupervisorsProtocol + 1
	ProtocolVersion       = OrchestratorsProtocol
	// RemovedCheckpointMessage is the guidance for clients sending the
	// removed checkpoint action.
	RemovedCheckpointMessage = "The checkpoint action was removed; suspend saves the session."
	// StoredTranscriptReadProtocol is the first protocol with indexed stored
	// transcript item reads.
	StoredTranscriptReadProtocol uint32 = 105
	MaxFrameBytes                       = 8 * 1024 * 1024
	// StopTimeout is how long a daemon is given to exit after it accepts a
	// stop.
	//
	// Stopping cancels a token and returns immediately; the daemon then
	// unwinds its session manager, its phone server and its pollers. That is
	// normally fast, but a daemon whose database has been migrated out from
	// under it fails every read while it winds down and has been observed
	// taking over five seconds — which the previous five-second bound missed
	// by a fraction, reporting a stop that had in fact worked as `did not
	// stop` and aborting the restart that depended on it.
	StopTimeout = 30 * time.Second
	// StopDrainTimeout is how long a requested stop may take: the daemon
	// waits up to 60 seconds for session destroys in flight (#1191) before it
	// winds down.
	StopDrainTimeout = 90 * time.Second
	RetryDelay       = 40 * time.Millisecond
)

// DaemonActionIsCancellableMachineAction reports disposable machine work,
// which uses dedicated connections. Closing one or handing off the daemon
// cancels it; interrupted installs can be rerun.
func DaemonActionIsCancellableMachineAction(action DaemonAction) bool {
	if preflight, ok := action.(DaemonActionPreflightNewSession); ok && len(preflight.Request.RemoteRepairs) > 0 {
		return false
	}
	switch action.(type) {
	case DaemonActionInstallMbx, DaemonActionPreviewBuildCache, DaemonActionCheckpointArchiveSizes,
		DaemonActionTestTarget, DaemonActionProfileLoginStatuses,
		DaemonActionDiscoverSetup, DaemonActionPreflightNewSession, DaemonActionSuspendPreflight,
		DaemonActionValidateProjectDirectory,
		DaemonActionCompletePath, DaemonActionCheckProjectDirectories, DaemonActionDiscoverProjects,
		DaemonActionResolveContainerPath, DaemonActionResolveSetupPath, DaemonActionValidateMountSource,
		DaemonActionValidateSessionMounts:
		return true
	}
	return false
}

// PreflightRepository is one configured repository's network clone and
// publication destinations. URLs have already been passed through the shared
// display sanitizer before they reach a phone.
type PreflightRepository struct {
	ID            string   `json:"id"`
	FetchURL      string   `json:"fetch_url"`
	DefaultBranch string   `json:"default_branch"`
	PushURLs      []string `json:"push_urls"`
}

func (v PreflightRepository) MarshalJSON() ([]byte, error) {
	type plain PreflightRepository
	return daemonMarshalStruct(plain(v))
}

func (v *PreflightRepository) UnmarshalJSON(data []byte) error {
	type plain PreflightRepository
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// PreflightNew is what a preflight found. Isolated sessions expose their
// complete network source plan so the person can review it before creation.
// Raw-local targets leave the plan empty because they use the selected
// checkout directly; isolated targets set LocalChangesExcluded to make the
// copy boundary explicit.
type PreflightNew struct {
	ProjectDirectory     *string                     `json:"project_directory,omitempty" serde:"default"`
	ManagedWorktree      core.ManagedWorktreeOptions `json:"managed_worktree" serde:"default"`
	RemoteRepairs        []core.LocalRemoteRepair    `json:"remote_repairs" serde:"default"`
	DirtyRepositories    []string                    `json:"dirty_repositories" serde:"default"`
	RemoteRepositories   []PreflightRepository       `json:"remote_repositories" serde:"default"`
	LocalChangesExcluded bool                        `json:"local_changes_excluded"`
	// ReviewNotice is why creating this session turns its review off, when
	// it will.
	ReviewNotice *string `json:"review_notice,omitempty" serde:"default"`
}

func (v PreflightNew) MarshalJSON() ([]byte, error) {
	type plain PreflightNew
	return daemonMarshalStruct(plain(v))
}

func (v *PreflightNew) UnmarshalJSON(data []byte) error {
	type plain PreflightNew
	return daemonDecodeStruct(data, (*plain)(v), true)
}

type NewSessionPreflightRequest struct {
	AdditionalMounts []core.AdditionalMount   `json:"additional_mounts" serde:"default"`
	RemoteRepairs    []core.LocalRemoteRepair `json:"remote_repairs" serde:"default"`
	WorkspaceID      string                   `json:"workspace_id" serde:"default"`
	ProfileID        string                   `json:"profile_id"`
	BundleID         string                   `json:"bundle_id"`
	TargetID         string                   `json:"target_id"`
	ProjectDirectory *string                  `json:"project_directory" serde:"default"`
}

func (v NewSessionPreflightRequest) MarshalJSON() ([]byte, error) {
	type plain NewSessionPreflightRequest
	return daemonMarshalStruct(plain(v))
}

func (v *NewSessionPreflightRequest) UnmarshalJSON(data []byte) error {
	type plain NewSessionPreflightRequest
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// IdentityProof is the domain-separated proof shared by the daemon and SSH
// transport: HMAC-SHA256 keyed by the token over a fixed label and the
// nonce. Compare its Sum with hmac.Equal.
func IdentityProof(token, nonce string) hash.Hash {
	mac := hmac.New(sha256.New, []byte(token))
	mac.Write([]byte("mj-daemon-identity-v1"))
	mac.Write([]byte(nonce))
	return mac
}

func reconnectAfterUploadRefusal(ctx context.Context) (*DaemonClient, error) {
	for {
		if client, err := ConnectExisting(ctx); err == nil {
			return client, nil
		}
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(250 * time.Millisecond):
		}
	}
}

// DaemonReplyDebug is DaemonReply's Debug, for callers outside this package
// that report an unexpected reply with `{reply:?}`.
func DaemonReplyDebug(reply DaemonReply) string { return daemonReplyDebug(reply) }

// daemonReplyDebug names a reply for "unexpected reply" errors, as Rust's
// `{reply:?}` does.
func daemonReplyDebug(reply DaemonReply) string {
	name := strings.TrimPrefix(fmt.Sprintf("%T", reply), "client.DaemonReply")
	// A unit variant prints as its bare name.
	if value := reflect.ValueOf(reply); value.Kind() == reflect.Struct && value.NumField() == 0 {
		return name
	}
	return fmt.Sprintf("%s%+v", name, reply)
}

// daemonExpect is a reply of the type a request expects, or the request's
// error, or an error naming the unexpected reply after the given prefix.
func daemonExpect[R DaemonReply](reply DaemonReply, err error, unexpected string) (R, error) {
	var zero R
	if err != nil {
		return zero, err
	}
	if matched, ok := reply.(R); ok {
		return matched, nil
	}
	return zero, fmt.Errorf("%s %s", unexpected, daemonReplyDebug(reply))
}

// daemonExpectDone is daemonExpect for requests answered with Done.
func daemonExpectDone(reply DaemonReply, err error, unexpected string) error {
	_, err = daemonExpect[DaemonReplyDone](reply, err, unexpected)
	return err
}

// --- serde encoding shared by this package's wire types ---

// DaemonPair is a Rust two-element tuple, which serde writes as a JSON array
// [First, Second].
type DaemonPair[A, B any] struct {
	First  A
	Second B
}

func (p DaemonPair[A, B]) MarshalJSON() ([]byte, error) {
	first, err := daemonMarshalValue(p.First)
	if err != nil {
		return nil, err
	}
	second, err := daemonMarshalValue(p.Second)
	if err != nil {
		return nil, err
	}
	return daemonMarshalJSON([]json.RawMessage{first, second})
}

func (p *DaemonPair[A, B]) UnmarshalJSON(data []byte) error {
	return daemonDecodeTuple(data, "", &p.First, &p.Second)
}

// DaemonResult is Rust's Result<T, String> as serde writes it: {"Ok": value}
// or {"Err": "message"}.
type DaemonResult[T any] struct {
	Ok T
	// Err is the error text; nil for Ok.
	Err *string
}

// DaemonOk is a successful DaemonResult.
func DaemonOk[T any](value T) DaemonResult[T] {
	return DaemonResult[T]{Ok: value}
}

// DaemonErr is a failed DaemonResult.
func DaemonErr[T any](message string) DaemonResult[T] {
	return DaemonResult[T]{Err: &message}
}

func (r DaemonResult[T]) MarshalJSON() ([]byte, error) {
	if r.Err != nil {
		body, err := daemonMarshalJSON(*r.Err)
		if err != nil {
			return nil, err
		}
		return daemonExternalJSON("Err", body)
	}
	body, err := daemonMarshalValue(r.Ok)
	if err != nil {
		return nil, err
	}
	return daemonExternalJSON("Ok", body)
}

func (r *DaemonResult[T]) UnmarshalJSON(data []byte) error {
	name, body, err := daemonSplitExternal(data)
	if err != nil {
		return err
	}
	switch name {
	case "Ok":
		*r = DaemonResult[T]{}
		return daemonDecodeValue(body, &r.Ok)
	case "Err":
		var message string
		if err := daemonDecodeValue(body, &message); err != nil {
			return err
		}
		*r = DaemonResult[T]{Err: &message}
		return nil
	}
	return daemonUnknownVariant(name, []string{"Ok", "Err"})
}

// daemonMarshalJSON encodes v as serde_json does: without Go's HTML escaping
// of <, > and &, and without a trailing newline.
func daemonMarshalJSON(v any) ([]byte, error) {
	var buffer strings.Builder
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return []byte(strings.TrimSuffix(buffer.String(), "\n")), nil
}

// daemonMarshalValue is daemonMarshalJSON with a nil slice or map written as
// [] or {}, as serde writes an empty Vec or map.
func daemonMarshalValue(v any) ([]byte, error) {
	value := reflect.ValueOf(v)
	if value.IsValid() && value.Kind() == reflect.Slice && value.IsNil() {
		return []byte("[]"), nil
	}
	if value.IsValid() && value.Kind() == reflect.Map && value.IsNil() {
		return []byte("{}"), nil
	}
	return daemonMarshalJSON(v)
}

// daemonMarshalStruct encodes a struct value (a method-less copy of the type
// being encoded) with its nil slice and map fields written as [] and {}, as
// serde writes an empty Vec or map. Fields tagged omitempty stay omitted.
func daemonMarshalStruct(v any) ([]byte, error) {
	value := reflect.ValueOf(v)
	filled := reflect.New(value.Type()).Elem()
	filled.Set(value)
	for i := range filled.NumField() {
		field := filled.Field(i)
		if !field.CanSet() {
			continue
		}
		switch {
		case field.Kind() == reflect.Slice && field.IsNil():
			field.Set(reflect.MakeSlice(field.Type(), 0, 0))
		case field.Kind() == reflect.Map && field.IsNil():
			field.Set(reflect.MakeMap(field.Type()))
		case field.Kind() == reflect.Pointer && !field.IsNil() && field.Elem().Kind() == reflect.Slice && field.Elem().IsNil():
			empty := reflect.New(field.Type().Elem())
			empty.Elem().Set(reflect.MakeSlice(field.Type().Elem(), 0, 0))
			field.Set(empty)
		}
	}
	return daemonMarshalJSON(filled.Interface())
}

// daemonAdjacentJSON writes a serde adjacently tagged variant:
// {"<tag>":"<name>","<content>":<body>}, or {"<tag>":"<name>"} for a unit
// variant (body nil).
func daemonAdjacentJSON(tag, content, name string, body []byte) ([]byte, error) {
	var buffer strings.Builder
	buffer.WriteByte('{')
	daemonWriteMember(&buffer, tag, daemonQuote(name))
	if body != nil {
		buffer.WriteByte(',')
		daemonWriteMember(&buffer, content, body)
	}
	buffer.WriteByte('}')
	return []byte(buffer.String()), nil
}

// daemonAdjacentStruct is daemonAdjacentJSON for a struct variant; fields is
// a method-less copy of the variant.
func daemonAdjacentStruct(tag, content, name string, fields any) ([]byte, error) {
	body, err := daemonMarshalStruct(fields)
	if err != nil {
		return nil, err
	}
	return daemonAdjacentJSON(tag, content, name, body)
}

// daemonAdjacentValue is daemonAdjacentJSON for a newtype variant.
func daemonAdjacentValue(tag, content, name string, value any) ([]byte, error) {
	body, err := daemonMarshalValue(value)
	if err != nil {
		return nil, err
	}
	return daemonAdjacentJSON(tag, content, name, body)
}

// daemonAdjacentTuple is daemonAdjacentJSON for a tuple variant, whose
// content is an array of its values.
func daemonAdjacentTuple(tag, content, name string, values ...any) ([]byte, error) {
	items := make([]json.RawMessage, 0, len(values))
	for _, value := range values {
		item, err := daemonMarshalValue(value)
		if err != nil {
			return nil, err
		}
		items = append(items, item)
	}
	body, err := daemonMarshalJSON(items)
	if err != nil {
		return nil, err
	}
	return daemonAdjacentJSON(tag, content, name, body)
}

// daemonInternalJSON writes a serde internally tagged variant: the tag first,
// then the members of fields (a method-less struct copy, or nil for a unit
// variant).
func daemonInternalJSON(tag, name string, fields any) ([]byte, error) {
	var buffer strings.Builder
	buffer.WriteByte('{')
	daemonWriteMember(&buffer, tag, daemonQuote(name))
	if fields != nil {
		body, err := daemonMarshalStruct(fields)
		if err != nil {
			return nil, err
		}
		inner := strings.TrimSpace(string(body))
		if len(inner) < 2 || inner[0] != '{' {
			return nil, fmt.Errorf("internally tagged variant %q is not an object", name)
		}
		if members := strings.TrimSpace(inner[1 : len(inner)-1]); members != "" {
			buffer.WriteByte(',')
			buffer.WriteString(members)
		}
	}
	buffer.WriteByte('}')
	return []byte(buffer.String()), nil
}

// daemonExternalJSON writes a serde externally tagged variant:
// {"<name>":<body>}, or "<name>" for a unit variant (body nil).
func daemonExternalJSON(name string, body []byte) ([]byte, error) {
	if body == nil {
		return daemonQuote(name), nil
	}
	var buffer strings.Builder
	buffer.WriteByte('{')
	daemonWriteMember(&buffer, name, body)
	buffer.WriteByte('}')
	return []byte(buffer.String()), nil
}

func daemonQuote(text string) []byte {
	quoted, _ := daemonMarshalJSON(text)
	return quoted
}

func daemonWriteMember(buffer *strings.Builder, key string, value []byte) {
	buffer.Write(daemonQuote(key))
	buffer.WriteByte(':')
	buffer.Write(value)
}

// daemonObject reads a JSON object's members.
func daemonObject(data []byte) (map[string]json.RawMessage, error) {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return nil, err
	}
	if members == nil {
		return nil, errors.New("invalid type: null, expected a map")
	}
	return members, nil
}

// daemonSplitAdjacent reads the tag and content of a serde adjacently tagged
// value. The content is nil when absent.
func daemonSplitAdjacent(data []byte, tag, content string) (string, json.RawMessage, error) {
	members, err := daemonObject(data)
	if err != nil {
		return "", nil, err
	}
	raw, ok := members[tag]
	if !ok {
		return "", nil, daemonMissingField(tag)
	}
	var name string
	if err := json.Unmarshal(raw, &name); err != nil {
		return "", nil, fmt.Errorf("field `%s`: %w", tag, err)
	}
	return name, members[content], nil
}

// daemonSplitInternal reads the tag of a serde internally tagged value.
func daemonSplitInternal(data []byte, tag string) (string, error) {
	name, _, err := daemonSplitAdjacent(data, tag, "")
	return name, err
}

// daemonSplitExternal reads a serde externally tagged value: a bare string
// for a unit variant (nil content), or an object with one member.
func daemonSplitExternal(data []byte) (string, json.RawMessage, error) {
	var name string
	if err := json.Unmarshal(data, &name); err == nil {
		return name, nil, nil
	}
	members, err := daemonObject(data)
	if err != nil {
		return "", nil, err
	}
	if len(members) != 1 {
		return "", nil, errors.New("invalid type: map, expected a map with a single key")
	}
	for key, body := range members {
		return key, body, nil
	}
	panic("unreachable")
}

// daemonDecodeStruct decodes a JSON object into the struct v points to, as
// serde's derive does: every member through daemonDecodeValue, a missing
// member refused unless its field is a pointer, omitempty or tagged
// serde:"default", and with deny any member that is not a field (or one of
// extra) refused.
func daemonDecodeStruct(data []byte, v any, deny bool, extra ...string) error {
	if data == nil {
		return errors.New("invalid type: unit value, expected a struct")
	}
	members, err := daemonObject(data)
	if err != nil {
		return err
	}
	target := reflect.ValueOf(v).Elem()
	structType := target.Type()
	known := make(map[string]bool, structType.NumField()+len(extra))
	for _, name := range extra {
		known[name] = true
	}
	var names []string
	for i := range structType.NumField() {
		field := structType.Field(i)
		tag := field.Tag.Get("json")
		if tag == "-" || !field.IsExported() {
			continue
		}
		name, options, _ := strings.Cut(tag, ",")
		if name == "" {
			name = field.Name
		}
		known[name] = true
		names = append(names, name)
		raw, present := members[name]
		if !present {
			optional := field.Type.Kind() == reflect.Pointer || strings.Contains(","+options+",", ",omitempty,") ||
				field.Tag.Get("serde") == "default"
			if !optional {
				return daemonMissingField(name)
			}
			continue
		}
		if err := daemonDecodeValue(raw, target.Field(i).Addr().Interface()); err != nil {
			return fmt.Errorf("field `%s`: %w", name, err)
		}
	}
	if deny {
		keys := make([]string, 0, len(members))
		for key := range members {
			keys = append(keys, key)
		}
		sort.Strings(keys)
		for _, key := range keys {
			if !known[key] {
				quoted := make([]string, len(names))
				for i, name := range names {
					quoted[i] = "`" + name + "`"
				}
				return fmt.Errorf("unknown field `%s`, expected one of %s", key, strings.Join(quoted, ", "))
			}
		}
	}
	return nil
}

// daemonInterfaceDecoders decodes the sealed interfaces that plain
// encoding/json cannot fill, keyed by interface type. Filled in init so the
// decoders, which reach this map, take no part in package initialization
// order.
var daemonInterfaceDecoders = map[reflect.Type]func([]byte) (any, error){}

func daemonRegister[I any](decode func([]byte) (I, error)) {
	daemonInterfaceDecoders[reflect.TypeFor[I]()] = func(data []byte) (any, error) {
		return decode(data)
	}
}

func init() {
	daemonRegister(core.UnmarshalMachineJSON)
	daemonRegister(core.UnmarshalQuotaProviderJSON)
	daemonRegister(core.UnmarshalTargetTemplateJSON)
	daemonRegister(core.UnmarshalLoginStatus)
	daemonRegister(core.UnmarshalNotificationDismissal)
	daemonRegister(core.UnmarshalCompletionHostJSON)
	daemonRegister(core.UnmarshalProjectDiscoveryRequestJSON)
	daemonRegister(core.UnmarshalSessionResourceAllocationJSON)
	daemonRegister(core.UnmarshalRelayCommandJSON)
	daemonRegister(core.UnmarshalSessionReviewJSON)
	daemonRegister(core.UnmarshalTurnReviewPhase)
	daemonRegister(core.UnmarshalResumeRepositorySourcePreflight)
	daemonRegister(UnmarshalDaemonActionJSON)
	daemonRegister(UnmarshalDaemonReplyJSON)
	daemonRegister(UnmarshalSessionLifecycleRequestJSON)
	daemonRegister(UnmarshalStartupPromptOutcomeJSON)
	daemonRegister(UnmarshalWebViewerStatusJSON)
	daemonRegister(UnmarshalWikiSyncOutcomeJSON)
	daemonRegister(UnmarshalWebViewerAccessJSON)
	daemonRegister(UnmarshalWebViewerRecoveryJSON)
	daemonRegister(UnmarshalMachineFindingJSON)
	daemonRegister(UnmarshalReviewerActionJSON)
	daemonRegister(UnmarshalReviewerOutcomeJSON)
	daemonRegister(UnmarshalUiActionJSON)
	daemonRegister(UnmarshalTerminalInputJSON)
	daemonRegister(UnmarshalTerminalOutputJSON)
}

// daemonDecodeValue decodes one JSON value into target (a pointer), routing
// the sealed interfaces in daemonInterfaceDecoders, and slices and maps of
// them, through their owners' decoders. Empty data reads as null.
func daemonDecodeValue(data []byte, target any) error {
	if len(data) == 0 {
		data = []byte("null")
	}
	pointer := reflect.ValueOf(target)
	if !daemonNeedsDecoder(pointer.Type().Elem()) {
		return json.Unmarshal(data, target)
	}
	element := pointer.Elem()
	elementType := element.Type()
	null := string(data) == "null"
	if decode, ok := daemonInterfaceDecoders[elementType]; ok {
		if null {
			element.Set(reflect.Zero(elementType))
			return nil
		}
		value, err := decode(data)
		if err != nil {
			return err
		}
		if value == nil {
			element.Set(reflect.Zero(elementType))
		} else {
			element.Set(reflect.ValueOf(value))
		}
		return nil
	}
	switch elementType.Kind() {
	case reflect.Slice:
		if null {
			element.Set(reflect.Zero(elementType))
			return nil
		}
		var items []json.RawMessage
		if err := json.Unmarshal(data, &items); err != nil {
			return err
		}
		decoded := reflect.MakeSlice(elementType, len(items), len(items))
		for i, item := range items {
			if err := daemonDecodeValue(item, decoded.Index(i).Addr().Interface()); err != nil {
				return err
			}
		}
		element.Set(decoded)
		return nil
	case reflect.Map:
		if null {
			element.Set(reflect.Zero(elementType))
			return nil
		}
		var members map[string]json.RawMessage
		if err := json.Unmarshal(data, &members); err != nil {
			return err
		}
		decoded := reflect.MakeMapWithSize(elementType, len(members))
		for key, member := range members {
			item := reflect.New(elementType.Elem())
			if err := daemonDecodeValue(member, item.Interface()); err != nil {
				return err
			}
			decoded.SetMapIndex(reflect.ValueOf(key).Convert(elementType.Key()), item.Elem())
		}
		element.Set(decoded)
		return nil
	case reflect.Pointer:
		if null {
			element.Set(reflect.Zero(elementType))
			return nil
		}
		item := reflect.New(elementType.Elem())
		if err := daemonDecodeValue(data, item.Interface()); err != nil {
			return err
		}
		element.Set(item)
		return nil
	}
	return json.Unmarshal(data, target)
}

// daemonNeedsDecoder reports whether values of t hold a registered sealed
// interface directly, or through slices, maps or pointers.
func daemonNeedsDecoder(t reflect.Type) bool {
	for {
		if _, ok := daemonInterfaceDecoders[t]; ok {
			return true
		}
		switch t.Kind() {
		case reflect.Slice, reflect.Map, reflect.Pointer:
			t = t.Elem()
		default:
			return false
		}
	}
}

// daemonDecodeRequired decodes a newtype variant's content, which serde
// requires to be present.
func daemonDecodeRequired(body json.RawMessage, content string, target any) error {
	if body == nil {
		return daemonMissingField(content)
	}
	return daemonDecodeValue(body, target)
}

// daemonDecodeTuple decodes a JSON array with exactly one element per
// target, as serde reads a tuple. content names the missing member for an
// absent body; empty means the body is required data.
func daemonDecodeTuple(body json.RawMessage, content string, targets ...any) error {
	if body == nil && content != "" {
		return daemonMissingField(content)
	}
	var items []json.RawMessage
	if err := json.Unmarshal(body, &items); err != nil {
		return err
	}
	if items == nil || len(items) != len(targets) {
		return fmt.Errorf("invalid length %d, expected a tuple of size %d", len(items), len(targets))
	}
	for i, target := range targets {
		if err := daemonDecodeValue(items[i], target); err != nil {
			return err
		}
	}
	return nil
}

// daemonStructVariant decodes the content of one struct variant of an
// adjacently tagged enum into T.
func daemonStructVariant[T any, I any](body json.RawMessage, content string) (I, error) {
	var value T
	var zero I
	if body == nil {
		return zero, daemonMissingField(content)
	}
	if err := daemonDecodeStruct(body, &value, false); err != nil {
		return zero, err
	}
	return any(value).(I), nil
}

func daemonMissingField(name string) error {
	return fmt.Errorf("missing field `%s`", name)
}

// daemonUnknownVariant is serde's error for a tag no variant has.
func daemonUnknownVariant(name string, expected []string) error {
	quoted := make([]string, len(expected))
	for i, variant := range expected {
		quoted[i] = "`" + variant + "`"
	}
	switch len(quoted) {
	case 1:
		return fmt.Errorf("unknown variant `%s`, expected %s", name, quoted[0])
	case 2:
		return fmt.Errorf("unknown variant `%s`, expected %s or %s", name, quoted[0], quoted[1])
	}
	return fmt.Errorf("unknown variant `%s`, expected one of %s", name, strings.Join(quoted, ", "))
}

// daemonByteArrayJSON writes bytes as serde writes a Vec<u8>: an array of
// numbers.
func daemonByteArrayJSON(bytes []byte) json.RawMessage {
	numbers := make([]uint16, len(bytes))
	for i, b := range bytes {
		numbers[i] = uint16(b)
	}
	encoded, _ := json.Marshal(numbers)
	return encoded
}

// daemonDecodeByteArray reads serde's Vec<u8>: an array of numbers 0-255.
func daemonDecodeByteArray(data json.RawMessage) ([]byte, error) {
	var numbers []uint8
	var raw []json.RawMessage
	if err := json.Unmarshal(data, &raw); err != nil || raw == nil {
		return nil, fmt.Errorf("invalid type: expected a sequence of bytes")
	}
	numbers = make([]uint8, len(raw))
	for i, item := range raw {
		if err := json.Unmarshal(item, &numbers[i]); err != nil {
			return nil, err
		}
	}
	return numbers, nil
}
