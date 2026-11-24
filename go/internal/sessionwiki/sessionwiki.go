// Port of mj-sessionwiki/src/sessionwiki.rs.
//
// Publishing Mjolnir's own checkpointed sessions into the user's SessionWiki
// index, and the daemon-side job that keeps that index current.
//
// SessionWiki keeps one searchable index of AI coding sessions across every
// tool a user runs. Mjolnir links it as a library and registers MjolnirAdapter
// beside SessionWiki's built-in adapters, so a Mjolnir session is searchable
// next to a Claude Code or Codex one. The adapter is a "shared store" adapter:
// checkpoints are not one-file-per-session in a shape SessionWiki can parse,
// so the indexer enumerates sessions by key and asks this adapter to parse
// the ones whose checkpoint changed.
//
// Rust re-exports mj_core::state::ArchiveSpacePreview here; Go has no
// re-exports, so callers use core.ArchiveSpacePreview.

package sessionwiki

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"math"
	"os"
	"path/filepath"
	"runtime/debug"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
	acp "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/brokksessionwiki"
	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/mjimport"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// mjolnirTool is Rust's TOOL: the tool name every Mjolnir instance publishes
// under. One name means one search partition; reconciliation is scoped per
// instance instead (see MjolnirAdapter.ReconcileScope).
const mjolnirTool = "mjolnir"

// conversationRoles is Rust's CONVERSATION_ROLES.
var conversationRoles = []brokksessionwiki.Role{brokksessionwiki.RoleUser, brokksessionwiki.RoleAssistant}

// sessionParseFormatVersion: the low token slot identifies the Session
// projection format. Raising it makes rows indexed by older parse rules
// stale exactly once.
const sessionParseFormatVersion int64 = 1
const changeTokenSlotBits uint32 = 10

// sessionChangeToken packs a session's change token with the summary and
// parse format versions, saturating as Rust does.
func sessionChangeToken(token int64) int64 {
	return saturatingAddInt64(
		saturatingAddInt64(
			saturatingMulInt64(token, 1<<(changeTokenSlotBits*2)),
			saturatingMulInt64(int64(transcript.SummaryVersion), 1<<changeTokenSlotBits),
		),
		sessionParseFormatVersion,
	)
}

// archiveFile is one checkpoint archive on disk, reduced to what indexing
// needs.
type archiveFile struct {
	path     string
	frontier uint64
	// token is the modification time in epoch seconds, SessionWiki's change
	// token.
	token int64
}

// indexedProjectDirectoryResult is one session's entry of
// indexedSessions.projectDirectories (Rust's `Result<Option<PathBuf>,
// String>`): err when it could not be resolved, otherwise path, nil for
// none.
type indexedProjectDirectoryResult struct {
	path *string
	err  *string
}

// indexedSessions is Rust's `Sessions`: what indexing needs from controller
// state: which sessions exist, which of them are sub-agent children, and
// which are still running with their conversation in the daemon's own
// database rather than in a checkpoint.
type indexedSessions struct {
	records   core.SnapshotMap[string, core.SessionRecord]
	ownership topLevelSnapshot
	// projectDirectories is the effective agent working directory derived
	// through State.Checkout. Errors are retained so a malformed bundle
	// cannot silently index with an empty project.
	projectDirectories map[string]indexedProjectDirectoryResult
	// live is session id to change token, for sessions indexed from the
	// projection.
	live map[string]int64
}

// indexedSessionsOf is Rust's Sessions::of.
func indexedSessionsOf(state *core.StateState, config *core.Config) indexedSessions {
	return indexedSessions{
		records:            state.Sessions,
		ownership:          topLevelSnapshotFromState(state),
		projectDirectories: projectDirectoriesOf(state, config),
		live:               liveTokens(state),
	}
}

func projectDirectoriesOf(state *core.StateState, config *core.Config) map[string]indexedProjectDirectoryResult {
	directories := map[string]indexedProjectDirectoryResult{}
	for sessionID, record := range state.Sessions.All() {
		directory, ok, err := IndexedProjectDirectory(state, config, sessionID, record)
		switch {
		case err != nil:
			message := err.Error()
			directories[sessionID] = indexedProjectDirectoryResult{err: &message}
		case ok:
			directories[sessionID] = indexedProjectDirectoryResult{path: &directory}
		default:
			directories[sessionID] = indexedProjectDirectoryResult{}
		}
	}
	return directories
}

// IndexedProjectDirectory is the project path SessionWiki should search for
// this session: the attached or managed raw checkout, or the primary
// repository in a managed workspace. false when there is none.
func IndexedProjectDirectory(state *core.StateState, config *core.Config, sessionID string, record *core.SessionRecord) (string, bool, error) {
	checkout, err := state.Checkout(sessionID)
	if err != nil {
		return "", false, err
	}
	switch effective := checkout.Effective().(type) {
	case core.CheckoutAttached:
		return effective.Path, true, nil
	case core.CheckoutManagedWorktree:
		if effective.ProjectDir == nil {
			return "", false, nil
		}
		return *effective.ProjectDir, true, nil
	case core.CheckoutManagedWorkspace:
		if config == nil {
			return "", false, nil
		}
		bundle := record.ProjectBundle(config)
		if bundle == nil {
			return "", false, nil
		}
		var primary *core.ProjectRepository
		for index := range bundle.Repositories {
			if bundle.Repositories[index].ID == bundle.PrimaryRepo {
				primary = &bundle.Repositories[index]
				break
			}
		}
		if primary == nil {
			return "", false, fmt.Errorf("session bundle has no primary repository %s", core.RustDebugString(bundle.PrimaryRepo))
		}
		var workspaceRoot string
		if record.Target != nil {
			backend, err := targets.BackendLocator(record.Target, record, config)
			if err != nil {
				return "", false, fmt.Errorf("resolve the session target for SessionWiki: %w", err)
			}
			workspaceRoot = targets.WorkspaceRoot(&backend, record.ContainerWorkspace)
		} else {
			workspaceRoot = core.ContainerWorkspaceRoot(record.ContainerWorkspace)
		}
		return targets.AgentWorkingDirectoryAt(workspaceRoot, primary.Destination), true, nil
	case core.CheckoutBorrowed:
		panic("State::checkout resolves borrowed sessions")
	}
	panic(fmt.Sprintf("unknown checkout %T", checkout.Effective()))
}

// liveTokens is the change token of every session whose transcript is still
// only in the daemon's database: its activity watermark in whole seconds.
//
// A stopped session keeps being indexed from its checkpoint, which never
// changes again. Everything else is indexed from the projection, so a running
// session is findable before it has ever been closed.
func liveTokens(state *core.StateState) map[string]int64 {
	activity, err := store.LoadTranscribedSessionActivity()
	if err != nil {
		slog.Warn("could not read session activity for SessionWiki", "error", err)
		return map[string]int64{}
	}
	tokens := map[string]int64{}
	for sessionID, record := range state.Sessions.All() {
		if record.State == core.SessionStateStopped {
			continue
		}
		watermark, ok := activity[sessionID]
		if !ok {
			continue
		}
		var millis int64
		if watermark != nil {
			millis = *watermark
		}
		tokens[sessionID] = millis / 1000
	}
	return tokens
}

// MjolnirAdapter is Mjolnir's sessions, as SessionWiki sees them.
//
// Rust hands the indexer a SharedMjolnirAdapter wrapping an Arc so the sync
// can keep its own handle on the adapter the indexer consumes; a
// *MjolnirAdapter is shared as it is, so Go has no wrapper.
type MjolnirAdapter struct {
	// AdapterDefaults supplies NativeID, ModifiedAt and PathKeyDepth.
	brokksessionwiki.AdapterDefaults
	sessionsDir string
	// mu guards sessions (Rust's Mutex<Sessions>).
	mu       sync.Mutex
	sessions indexedSessions
	// reload re-reads controller state when the indexer reaches this
	// adapter.
	reload bool
}

var (
	_ brokksessionwiki.Adapter = (*MjolnirAdapter)(nil)
	_ brokksessionwiki.Adapter = (*capturedAdapter)(nil)
	_ brokksessionwiki.Adapter = (*preparedFiles)(nil)
)

// MjolnirAdapterFromState is a fixed view of the given state, which is what
// a caller with a state in hand wants.
func MjolnirAdapterFromState(state *core.StateState) *MjolnirAdapter {
	return &MjolnirAdapter{sessionsDir: core.SessionsDir(), sessions: indexedSessionsOf(state, nil)}
}

// MjolnirAdapterFromStateWithConfig is a fixed view that can resolve
// bundle-backed session directories from the configuration snapshot paired
// with the state.
func MjolnirAdapterFromStateWithConfig(state *core.StateState, config *core.Config) *MjolnirAdapter {
	return &MjolnirAdapter{sessionsDir: core.SessionsDir(), sessions: indexedSessionsOf(state, config)}
}

// MjolnirAdapterReloading is the same, but re-reading controller state when
// the indexer reaches this adapter.
//
// One sync pass walks every other tool's store first, which can take minutes
// on a large corpus. Without the reload, sessions that closed during that
// walk would be indexed with no record: no project, no start time, and the
// title guessed from the first prompt. Their checkpoints do not change
// afterwards, so nothing would ever correct them.
func MjolnirAdapterReloading(state *core.StateState) *MjolnirAdapter {
	adapter := MjolnirAdapterFromState(state)
	adapter.reload = true
	return adapter
}

// IndexedTags is Mjolnir's own metadata for every session this adapter knows
// about, to be stored in the index beside the transcripts.
//
// Read from the adapter's own snapshot rather than from the controller state
// the sync loaded, because MjolnirAdapterReloading replaces that snapshot
// when the indexer reaches this adapter. A session that closed during a long
// first pass is indexed from the reloaded state, so its metadata has to come
// from the same state that produced its row.
func (a *MjolnirAdapter) IndexedTags() map[string]MjTags {
	a.mu.Lock()
	defer a.mu.Unlock()
	indexed := map[string]MjTags{}
	for sessionID, record := range a.sessions.records.All() {
		if a.sessions.ownership.ownsSession(sessionID) {
			continue
		}
		var tags MjTags
		if record.TargetTemplateID != "" {
			target := record.TargetTemplateID
			tags.Target = &target
		}
		if record.LastProfile != "" {
			profile := record.LastProfile
			tags.Profile = &profile
		}
		harness := record.HarnessKind.ID()
		tags.Harness = &harness
		indexed[sessionID] = tags
	}
	return indexed
}

// reloadSessions is Rust's MjolnirAdapter::reload (renamed because the
// struct has a reload field).
func (a *MjolnirAdapter) reloadSessions() {
	if !a.reload {
		return
	}
	config, state, err := store.LoadConfigAndState()
	if err != nil {
		slog.Warn("could not refresh session records for SessionWiki", "error", err)
		return
	}
	sessions := indexedSessionsOf(&state, &config)
	a.mu.Lock()
	a.sessions = sessions
	a.mu.Unlock()
}

// checkpointedTranscript is the conversation of a stopped session, read from
// its newest checkpoint, with the title the checkpoint recorded.
func (a *MjolnirAdapter) checkpointedTranscript(sessionID string) (indexedTranscript, error) {
	newest, _ := a.newestArchives()
	archive, ok := newest[sessionID]
	if !ok {
		return indexedTranscript{}, fmt.Errorf("no checkpoint archive for session %s", sessionID)
	}
	verified, err := checkpoint.VerifyArchiveStreaming(archive.path)
	if err != nil {
		return indexedTranscript{}, fmt.Errorf("read checkpoint %s: %w", archive.path, err)
	}
	snapshot := &verified.CanonicalSession
	index := newTranscriptIndex[core.CanonicalTerminalOutput]()
	for position := range snapshot.Transcript {
		item := &snapshot.Transcript[position]
		index.observeBoundary(item.StableID, item.Position)
		var role brokksessionwiki.Role
		var text string
		switch body := item.Body.(type) {
		case core.CanonicalTranscriptBodyUser:
			role, text = userMessage(body.Content)
		case core.CanonicalTranscriptBodyAgent:
			role, text = agentMessage(body.Chunks)
		case core.CanonicalTranscriptBodyMessage:
			role, text = brokksessionwiki.RoleTool, core.RenderMailboxEvent(body.Event)
		case core.CanonicalTranscriptBodyTool:
			index.tool(item.StableID, item.Position, item.CreatedAtMs, body.Call, body.Presentation, body.TerminalOutputs, body.TerminalRefs)
			continue
		case core.CanonicalTranscriptBodyTerminalOutput:
			index.terminals.Standalone(body.Record)
			continue
		default:
			continue
		}
		index.plain(item.Position, item.CreatedAtMs, role, text)
	}
	return index.finish(snapshot.Session.SessionTitle, func(records []core.CanonicalTerminalOutput, call *acp.ToolCall) (string, bool) {
		terminals := make([]core.TerminalOutputRecord, len(records))
		for position := range records {
			terminals[position] = core.TerminalOutputRecordFromCanonical(&records[position])
		}
		return fillToolOutput(call, terminals)
	}), nil
}

// projectedTranscript is the conversation of a session that has not
// stopped, read from the daemon's own projection. It is the same
// conversation the checkpoint would hold, minus whatever has not happened
// yet.
func (a *MjolnirAdapter) projectedTranscript(sessionID string) (indexedTranscript, error) {
	projection, err := store.LoadMaterializedSession(sessionID)
	if err != nil {
		return indexedTranscript{}, fmt.Errorf("read the stored transcript of session %s: %w", sessionID, err)
	}
	if projection == nil {
		return indexedTranscript{}, fmt.Errorf("no stored transcript for session %s", sessionID)
	}
	return projectedTranscriptOf(projection), nil
}

// keyFor is the stable key for one session: its checkpoint directory and
// id. The directory is per instance, which is what scopes reconciliation.
func (a *MjolnirAdapter) keyFor(sessionID string) string {
	return a.sessionsDir + "/" + sessionID
}

// newestArchives is the newest checkpoint of every session in the
// directory, by session id.
//
// The second result is true when the directory exists but could not be read
// in full; the indexer then skips deletion reconciliation rather than
// archiving every Mjolnir session off a partial listing.
func (a *MjolnirAdapter) newestArchives() (map[string]archiveFile, bool) {
	newest := map[string]archiveFile{}
	hadError := false
	entries, err := os.ReadDir(a.sessionsDir)
	if err != nil {
		if len(entries) == 0 {
			if _, statErr := os.Stat(a.sessionsDir); statErr == nil {
				slog.Debug("could not list the checkpoint directory for SessionWiki",
					"directory", a.sessionsDir, "error", err)
				hadError = true
			}
			return newest, hadError
		}
		// A listing that failed part way still names what it read.
		hadError = true
	}
	for _, entry := range entries {
		sessionID, frontier, ok := checkpointArchiveSession(entry.Name())
		if !ok {
			continue
		}
		var token int64
		if info, err := entry.Info(); err == nil {
			if modified := info.ModTime(); !modified.Before(time.Unix(0, 0)) {
				token = modified.Unix()
			}
		}
		candidate := archiveFile{path: filepath.Join(a.sessionsDir, entry.Name()), frontier: frontier, token: token}
		if existing, ok := newest[sessionID]; ok && existing.frontier >= candidate.frontier {
			continue
		}
		newest[sessionID] = candidate
	}
	return newest, hadError
}

type indexedTranscript struct {
	messages []brokksessionwiki.Message
	title    *string
	evidence provenanceEvidence
}

// checkpointArchiveSession is the session a checkpoint file name belongs to,
// with its generation.
//
// Managed checkpoints carry a frontier and a nonce; an imported archive is
// named for its session alone and counts as generation zero.
func checkpointArchiveSession(name string) (string, uint64, bool) {
	if parsed := store.ParseManagedCheckpointArchiveName(name); parsed != nil {
		return parsed.SessionID, parsed.Frontier, true
	}
	stem, ok := strings.CutSuffix(name, ".hel.zip")
	if !ok || !utf8.ValidString(name) || core.ValidateID("session", stem) != nil {
		return "", 0, false
	}
	return stem, 0, true
}

// NewestCheckpointArchive is the newest full checkpoint for one session, if
// Mjolnir still has one. Observe uses this after SessionWiki discovers a
// session whose operational record has been removed but whose canonical
// archive has not.
func NewestCheckpointArchive(sessionID string) (string, bool, error) {
	directory := core.SessionsDir()
	entries, err := os.ReadDir(directory)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return "", false, nil
		}
		return "", false, fmt.Errorf("list checkpoint directory %s: %w", directory, err)
	}
	var newestFrontier uint64
	newestPath := ""
	found := false
	for _, entry := range entries {
		if !entry.Type().IsRegular() {
			continue
		}
		id, frontier, ok := checkpointArchiveSession(entry.Name())
		if !ok {
			continue
		}
		if id == sessionID && (!found || frontier > newestFrontier) {
			newestFrontier, newestPath, found = frontier, filepath.Join(directory, entry.Name()), true
		}
	}
	return newestPath, found, nil
}

// projectedTranscriptOf is Rust's free function `projected_transcript`: a
// running session's conversation and evidence, as SessionWiki stores them.
func projectedTranscriptOf(projection *core.MaterializedSession) indexedTranscript {
	index := newTranscriptIndex[core.TerminalOutputRecord]()
	for _, item := range projection.Transcript {
		index.observeBoundary(item.StableID, item.Position)
		var role brokksessionwiki.Role
		var text string
		switch body := item.Body.(type) {
		case core.TranscriptBodyUser:
			role, text = userMessage(body.Content)
		case core.TranscriptBodyAgent:
			role, text = agentMessage(body.Chunks)
		case core.TranscriptBodyMessage:
			role, text = brokksessionwiki.RoleTool, core.RenderMailboxEvent(body.Event)
		case core.TranscriptBodyTool:
			index.tool(item.StableID, item.Position, item.CreatedAtMs, body.Call.Value(), body.Presentation, body.TerminalOutputs, body.TerminalRefs)
			continue
		case core.TranscriptBodyTerminalOutput:
			index.terminals.Standalone(body.Record)
			continue
		default:
			continue
		}
		index.plain(item.Position, item.CreatedAtMs, role, text)
	}
	var title *string
	if projection.SessionTitle != nil {
		copied := *projection.SessionTitle
		title = &copied
	}
	return index.finish(title, func(records []core.TerminalOutputRecord, call *acp.ToolCall) (string, bool) {
		return fillToolOutput(call, records)
	})
}

func userMessage(content []json.RawMessage) (brokksessionwiki.Role, string) {
	return brokksessionwiki.RoleUser, transcript.UserText(core.MaterializedContentText(content))
}

func agentMessage(chunks []json.RawMessage) (brokksessionwiki.Role, string) {
	return brokksessionwiki.RoleAssistant, core.MaterializedChunksText(chunks)
}

// positionedMessage is one indexed message with its transcript position.
type positionedMessage struct {
	position uint64
	message  brokksessionwiki.Message
}

// legacyTool is a settled tool whose output is resolved after the last item:
// its index in transcriptIndex.messages, its stable id and its call.
type legacyTool struct {
	index    int
	stableID string
	call     acp.ToolCall
}

// transcriptIndex visits each item once. Legacy tools with terminal links
// are resolved after visiting the last item because standalone close-time
// output can follow the call it belongs to. Modern summaries are complete at
// the tool row.
type transcriptIndex[R core.TerminalRecord] struct {
	messages     []positionedMessage
	evidence     provenanceEvidence
	contextStart uint64
	terminals    *core.TerminalOutputs[R]
	legacyTools  []legacyTool
}

// newTranscriptIndex is Rust's TranscriptIndex::default.
func newTranscriptIndex[R core.TerminalRecord]() *transcriptIndex[R] {
	return &transcriptIndex[R]{terminals: core.NewTerminalOutputs[R]()}
}

func (x *transcriptIndex[R]) observeBoundary(stableID string, position uint64) {
	if core.IsContextBoundary(stableID) {
		x.contextStart = max(x.contextStart, position)
	}
}

func (x *transcriptIndex[R]) plain(position uint64, createdAtMs int64, role brokksessionwiki.Role, text string) {
	if indexed, ok := message(role, text, createdAtMs); ok {
		x.messages = append(x.messages, positionedMessage{position: position, message: indexed})
	}
}

// tool indexes one tool item. value is the stored ACP tool call.
func (x *transcriptIndex[R]) tool(stableID string, position uint64, createdAtMs int64, value json.RawMessage, presentation *core.ToolCallPresentation, terminalOutputs []R, terminalRefs []string) {
	parsed, _ := jsonvalue.Parse(value)
	var title *string
	if text, ok := jsonMember(parsed, "title").(string); ok && strings.TrimSpace(text) != "" {
		title = &text
	}
	status, hasStatus := jsonMember(parsed, "status").(string)
	settled := hasStatus && (status == "completed" || status == "failed")
	compact := presentation != nil && presentation.OutputLineCount != nil && presentation.OutputIsError != nil
	var call *acp.ToolCall
	if title == nil || (settled && !compact) {
		var decoded acp.ToolCall
		if json.Unmarshal(value, &decoded) == nil {
			call = &decoded
		}
	}
	if compact || call != nil {
		x.evidence.observeTool(value, presentation, createdAtMs)
	}
	var description string
	if title != nil {
		description = *title
	} else {
		summary := "[invalid tool call]"
		if call != nil {
			summary = transcript.MaterializedToolCallPresentation(presentation, call).Summary
		}
		description = core.ToolTitle(summary)
	}
	name, ok := jsonMember(parsed, "kind").(string)
	if !ok {
		name = "other"
	}
	summary := brokksessionwiki.ToolSummary{
		Name: name,
		Args: jsonvalue.Object{{Key: "description", Value: description}},
	}
	if settled {
		switch {
		case compact:
			if presentation.FirstErrorLine != nil {
				line := *presentation.FirstErrorLine
				summary.Output = &line
			}
			lines := int(*presentation.OutputLineCount)
			isError := *presentation.OutputIsError
			summary.Lines = &lines
			summary.IsError = &isError
		case call != nil:
			if len(terminalOutputs) == 0 && len(terminalRefs) == 0 {
				output, isError := fillToolOutput(call, nil)
				setToolOutput(&summary, output, isError)
			} else {
				x.terminals.Tool(stableID, terminalOutputs, terminalRefs)
				x.legacyTools = append(x.legacyTools, legacyTool{index: len(x.messages), stableID: stableID, call: *call})
			}
		default:
			outcome := transcript.ToolOutcomeFromRawOutput(jsonMember(parsed, "rawOutput"))
			setToolOutput(&summary, "", status == "failed" || outcome.IsError())
		}
	}
	x.messages = append(x.messages, positionedMessage{
		position: position,
		message: brokksessionwiki.Message{
			Role: brokksessionwiki.RoleTool,
			Text: brokksessionwiki.RenderSummary(&summary),
			Ts:   chronoTimestampMillis(createdAtMs),
			Tool: &brokksessionwiki.ToolEventSummary{Summary: summary},
		},
	})
}

// finish resolves the legacy tools with legacyOutput and keeps the messages
// after the last context boundary.
func (x *transcriptIndex[R]) finish(title *string, legacyOutput func(records []R, call *acp.ToolCall) (string, bool)) indexedTranscript {
	inContext := func(position uint64) bool { return x.contextStart == 0 || position > x.contextStart }
	for _, legacy := range x.legacyTools {
		positioned := &x.messages[legacy.index]
		if !inContext(positioned.position) {
			continue
		}
		event, ok := positioned.message.Tool.(*brokksessionwiki.ToolEventSummary)
		if !ok {
			continue
		}
		output, isError := legacyOutput(x.terminals.ForTool(legacy.stableID), &legacy.call)
		setToolOutput(&event.Summary, output, isError)
		positioned.message.Text = brokksessionwiki.RenderSummary(&event.Summary)
	}
	var messages []brokksessionwiki.Message
	for _, positioned := range x.messages {
		if inContext(positioned.position) {
			messages = append(messages, positioned.message)
		}
	}
	return indexedTranscript{messages: messages, title: title, evidence: x.evidence}
}

func fillToolOutput(call *acp.ToolCall, terminals []core.TerminalOutputRecord) (string, bool) {
	return transcript.ToolOutput(call, terminals), transcript.ToolOutputIsError(call, terminals)
}

func setToolOutput(summary *brokksessionwiki.ToolSummary, output string, isError bool) {
	// Rust's str::lines: a final line ending does not start another line.
	lines := strings.Count(output, "\n")
	if output != "" && !strings.HasSuffix(output, "\n") {
		lines++
	}
	summary.Lines = &lines
	summary.IsError = &isError
	summary.Output = &output
}

// message is one indexed message, or false when the item carried no text.
func message(role brokksessionwiki.Role, text string, createdAtMs int64) (brokksessionwiki.Message, bool) {
	text = strings.TrimSpace(text)
	if text == "" {
		return brokksessionwiki.Message{}, false
	}
	return brokksessionwiki.Message{
		Role: role,
		Text: text,
		Ts:   chronoTimestampMillis(createdAtMs),
	}, true
}

func parseTime(value string) (time.Time, bool) {
	parsed, ok := core.ParseChronoRFC3339(value)
	return parsed.UTC(), ok
}

func (a *MjolnirAdapter) Name() string { return mjolnirTool }

func (a *MjolnirAdapter) Root() (string, bool) { return a.sessionsDir, true }

// Discover is unused: this is a shared-store adapter, so the indexer
// enumerates sessions through Store instead of walking files.
func (a *MjolnirAdapter) Discover() brokksessionwiki.Discovered {
	return brokksessionwiki.Discovered{}
}

func (a *MjolnirAdapter) Parse(string) (*brokksessionwiki.Session, error) {
	return nil, errors.New("Mjolnir sessions are parsed by key, not by file")
}

func (a *MjolnirAdapter) Store() *brokksessionwiki.Store {
	a.reloadSessions()
	newest, hadError := a.newestArchives()
	a.mu.Lock()
	defer a.mu.Unlock()
	sessions := &a.sessions
	tokens := map[string]int64{}
	files := make([]string, 0, len(newest))
	for _, sessionID := range sortedKeys(newest) {
		if sessions.ownership.ownsSession(sessionID) {
			continue
		}
		archive := newest[sessionID]
		tokens[sessionID] = archive.token
		files = append(files, archive.path)
	}
	// A session that is still running is indexed from the projection, and its
	// own token replaces any checkpoint token it has: the conversation has
	// moved on since that checkpoint was written. Listing it also keeps
	// reconciliation from archiving a running session.
	for sessionID, token := range sessions.live {
		if !sessions.ownership.ownsSession(sessionID) {
			tokens[sessionID] = token
		}
	}
	// A rename changes the record and not the conversation, so the record's
	// own last update is part of the change token. Without it a renamed
	// session would keep its old title in the index for as long as its
	// transcript stood still.
	for sessionID, token := range tokens {
		if record := sessions.records.GetShared(sessionID); record != nil {
			if updated, ok := parseTime(record.UpdatedAt); ok {
				tokens[sessionID] = max(token, updated.Unix())
			}
		}
	}
	keys := make([]brokksessionwiki.StoreKey, 0, len(tokens))
	for _, sessionID := range sortedKeys(tokens) {
		keys = append(keys, brokksessionwiki.StoreKey{Key: a.keyFor(sessionID), Token: sessionChangeToken(tokens[sessionID])})
	}
	return &brokksessionwiki.Store{Keys: keys, Files: files, HadError: hadError}
}

// ReconcileScope: every Mjolnir instance publishes under one tool name, so
// this instance speaks only for keys under its own checkpoint directory.
// Without the scope, two instances would archive each other's rows on every
// sync.
func (a *MjolnirAdapter) ReconcileScope() (string, bool) {
	return a.sessionsDir + "/", true
}

func (a *MjolnirAdapter) ParseKey(key string) (*brokksessionwiki.Session, error) {
	sessionID := key[strings.LastIndexByte(key, '/')+1:]
	if sessionID == "" {
		return nil, fmt.Errorf("no session id in key %s", core.RustDebugString(key))
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	sessions := &a.sessions
	if sessions.ownership.ownsSession(sessionID) {
		return nil, errors.New("sub-agent sessions are not indexed")
	}
	var indexed indexedTranscript
	var err error
	if _, live := sessions.live[sessionID]; live {
		indexed, err = a.projectedTranscript(sessionID)
	} else {
		indexed, err = a.checkpointedTranscript(sessionID)
	}
	if err != nil {
		return nil, err
	}
	record := sessions.records.GetShared(sessionID)
	project := ""
	if directory, known := sessions.projectDirectories[sessionID]; known {
		switch {
		case directory.err != nil:
			return nil, fmt.Errorf("resolve the session project directory: %s", *directory.err)
		case directory.path != nil:
			project = *directory.path
		}
	} else if record != nil {
		if directory, ok := record.RecordCheckout().ProjectDirectory(); ok {
			project = directory
		}
	}

	// A manual name, harness title, or first-prompt provisional title, as the
	// session list shows it. Without one the index prefers the transcript's
	// title over the creation title.
	var title string
	switch {
	case record != nil && record.HasGivenName():
		title = record.ListedTitle()
	case indexed.title != nil:
		title = *indexed.title
	default:
		for _, message := range indexed.messages {
			if message.Role == brokksessionwiki.RoleUser {
				title = firstChars(message.Text, 80)
				break
			}
		}
	}

	session := &brokksessionwiki.Session{
		ID:       sessionID,
		Tool:     mjolnirTool,
		Path:     key,
		Project:  project,
		Title:    title,
		Subagent: sessions.ownership.ownsSession(sessionID),
		Messages: indexed.messages,
		Touched:  indexed.evidence.sortedPaths(),
		Edits:    indexed.evidence.edits,
	}
	if record != nil {
		if started, ok := parseTime(record.CreatedAt); ok {
			session.Started = &started
		}
		if ended, ok := parseTime(record.UpdatedAt); ok {
			session.Ended = &ended
		}
	}
	return session, nil
}

// WikiIndexer is the daemon's SessionWiki sync job.
//
// Triggers coalesce: a request while a sync is running marks a rerun instead
// of queueing a second one, so a burst of closing sessions costs one extra
// pass. Syncs are single-flight because SessionWiki holds a write transaction
// per adapter batch, and two writers only produce a busy error.
type WikiIndexer struct {
	inner *indexer
}

// WikiSyncState is the complete externally observable generation state.
type WikiSyncState = client.WikiSyncStatus

type indexer struct {
	// mu guards state (Rust's Mutex<SyncState>).
	mu      sync.Mutex
	state   syncState
	updates *core.Watch[WikiSyncState]
	// notify is tokio's Notify: one stored permit wakes run.
	notify          chan struct{}
	nativeScanCache *mjimport.NativeScanCache
	completed       *core.Watch[uint64]
}

// syncCountKey is a phase and adapter index, the key of syncState.counts.
type syncCountKey struct {
	phase   string
	adapter int
}

type syncState struct {
	// view is the published state. Copies handed out by State and the watch
	// share its Running pointer and Outcome, so a transition replaces them
	// and never changes them in place.
	view          WikiSyncState
	fullRequested bool
	// lastSuccess is the epoch second the last successful pass started; nil
	// before one.
	lastSuccess *int64
	counts      map[syncCountKey]int
}

// finishPass records how pass generation ended and returns its outcome.
func (s *syncState) finishPass(generation uint64, full bool, startedEpoch int64, result error) client.WikiSyncOutcome {
	var outcome client.WikiSyncOutcome
	if result == nil {
		started := startedEpoch
		s.lastSuccess = &started
		s.view.Successful = generation
		outcome = client.WikiSyncOutcomeSucceeded{}
	} else {
		// Preserve the demand, without requesting any retry. A queued or
		// later explicit request must cover the failed full pass.
		s.fullRequested = s.fullRequested || full
		outcome = client.WikiSyncOutcomeFailed{Reason: result.Error()}
	}
	s.view.Finished = generation
	s.view.Outcome = outcome
	s.view.Running = nil
	return outcome
}

// newIndexer is Rust's Indexer::default.
func newIndexer() *indexer {
	epoch, err := core.NewSessionID()
	if err != nil {
		panic(fmt.Sprintf("generate SessionWiki indexer epoch: %v", err))
	}
	view := WikiSyncState{Epoch: epoch}
	return &indexer{
		state:           syncState{view: view, counts: map[syncCountKey]int{}},
		updates:         core.NewWatch(view),
		notify:          make(chan struct{}, 1),
		nativeScanCache: mjimport.NewNativeScanCache(),
		completed:       core.NewWatch[uint64](0),
	}
}

// SpawnWikiIndexer is Rust's WikiIndexer::spawn: the indexer with its sync
// loop running.
func SpawnWikiIndexer() WikiIndexer {
	inner := newIndexer()
	inner.nativeScanCache = mjimport.NativeScanCacheShared()
	go inner.run()
	return WikiIndexer{inner: inner}
}

// RequestSync requests the next pass. Requests during a pass coalesce into
// its successor. It returns the generation that will cover the request.
func (w WikiIndexer) RequestSync(full bool) uint64 {
	state := w.inner.lock()
	target := w.inner.request(state, full)
	w.inner.mu.Unlock()
	select {
	case w.inner.notify <- struct{}{}:
	default:
	}
	return target
}

// InertWikiIndexer is Rust's WikiIndexer::inert: an indexer whose sync loop
// never runs.
func InertWikiIndexer() WikiIndexer {
	return WikiIndexer{inner: newIndexer()}
}

func (w WikiIndexer) State() WikiSyncState {
	state := w.inner.lock()
	defer w.inner.mu.Unlock()
	return state.view
}

func (w WikiIndexer) Watch() *core.Watch[WikiSyncState] { return w.inner.updates }

// WatchAfter waits for the next publication, or answers immediately for
// another epoch.
func (w WikiIndexer) WatchAfter(ctx context.Context, epoch string, after uint64) (WikiSyncState, error) {
	for {
		// Subscribing before reading means a publication after the read wakes
		// the wait below.
		changed, stop := w.inner.updates.Subscribe()
		state := w.inner.updates.Borrow()
		if state.Epoch != epoch || state.Version > after {
			stop()
			return state, nil
		}
		select {
		case <-changed:
			stop()
		case <-ctx.Done():
			stop()
			return WikiSyncState{}, ctx.Err()
		}
	}
}

// SyncNow requests a pass and waits for one that covers it. A later
// successful pass also covers everything this request needed.
func (w WikiIndexer) SyncNow(ctx context.Context, full bool) error {
	return w.waitForGeneration(ctx, w.RequestSync(full))
}

// waitForGeneration waits until generation target has finished, reading only
// the newest published state, so a later successful pass that covered the
// target answers for an earlier failed one.
func (w WikiIndexer) waitForGeneration(ctx context.Context, target uint64) error {
	for {
		changed, stop := w.inner.updates.Subscribe()
		state := w.inner.updates.Borrow()
		if state.Finished >= target {
			stop()
			if state.Successful >= target {
				return nil
			}
			if failed, ok := state.Outcome.(client.WikiSyncOutcomeFailed); ok {
				return errors.New(failed.Reason)
			}
			return fmt.Errorf("SessionWiki generation %d finished without covering target %d", state.Finished, target)
		}
		select {
		case <-changed:
			stop()
		case <-ctx.Done():
			stop()
			return ctx.Err()
		}
	}
}

func (w WikiIndexer) Status() client.WikiStatus {
	return client.WikiStatus{State: IndexState()}
}

func (w WikiIndexer) Changes() *core.Watch[uint64] { return w.inner.completed }

// lock takes i.mu and returns the state it guards; release it with
// i.mu.Unlock().
func (i *indexer) lock() *syncState {
	i.mu.Lock()
	return &i.state
}

// publish publishes the view. All publications happen under the owner's
// lock, preserving transition order.
func (i *indexer) publish(state *syncState) {
	state.view.Version++
	i.updates.SendReplace(state.view)
}

// request asks for the pass after the one started last. The caller holds
// i.mu.
func (i *indexer) request(state *syncState, full bool) uint64 {
	target := state.view.Started + 1
	if state.view.Requested < target || (full && !state.fullRequested) {
		state.view.Requested = target
		state.fullRequested = state.fullRequested || full
		i.publish(state)
	}
	return target
}

// startNextPass starts the requested pass, if any: its generation, whether
// it is full, and the epoch second an incremental pass starts from (nil for
// all).
func (i *indexer) startNextPass() (generation uint64, full bool, since *int64, ok bool) {
	state := i.lock()
	defer i.mu.Unlock()
	if state.view.Requested <= state.view.Started {
		return 0, false, nil, false
	}
	state.view.Started++
	state.counts = map[syncCountKey]int{}
	state.view.Running = &client.WikiSyncProgress{}
	full = state.fullRequested
	state.fullRequested = false
	if !full && state.lastSuccess != nil {
		from := *state.lastSuccess - 60
		since = &from
	}
	i.publish(state)
	return state.view.Started, full, since, true
}

// run is the sync loop. Like Rust's task it runs until the process exits.
func (i *indexer) run() {
	for range i.notify {
		for {
			generation, full, since, ok := i.startNextPass()
			if !ok {
				break
			}
			started := time.Now()
			startedEpoch := time.Now().Unix()
			slog.Info("SessionWiki sync started", "generation", generation, "full", full, "since", optionalInt64(since))
			cache := i.nativeScanCache
			_, err := runAbandonable(func() (struct{}, error) {
				return struct{}{}, syncBlocking(since, cache, i)
			})
			state := i.lock()
			outcome := state.finishPass(generation, full, startedEpoch, err)
			if failed, ok := outcome.(client.WikiSyncOutcomeFailed); ok {
				slog.Warn("SessionWiki sync failed", "generation", generation, "error", failed.Reason)
			}
			mjolnirSessions, nativeSessions := 0, 0
			for key, done := range state.counts {
				switch key.phase {
				case "Mjolnir":
					mjolnirSessions += done
				case "Native":
					nativeSessions += done
				}
			}
			slog.Info("SessionWiki sync finished",
				"generation", generation,
				"duration_ms", time.Since(started).Milliseconds(),
				"mjolnir_sessions", mjolnirSessions,
				"native_sessions", nativeSessions,
				"outcome", outcome)
			i.publish(state)
			i.completed.SendReplace(generation)
			i.mu.Unlock()
		}
	}
}

// optionalInt64 is an optional number as a log value: nil for none.
func optionalInt64(value *int64) any {
	if value == nil {
		return nil
	}
	return *value
}

func (i *indexer) progress(phase string, progress *brokksessionwiki.SyncProgress) {
	next := client.WikiSyncProgress{
		Phase:    phase,
		Tool:     progress.Tool,
		Adapter:  progress.Adapter,
		Adapters: progress.Adapters,
		Done:     progress.Done,
	}
	if progress.Total != nil {
		total := *progress.Total
		next.Total = &total
	}
	state := i.lock()
	defer i.mu.Unlock()
	state.counts[syncCountKey{phase: phase, adapter: progress.Adapter}] = progress.Done
	// visible is what a surface shows of a progress report: everything but
	// the exact count, which it shows as a whole percentage.
	type visible struct {
		phase, tool       string
		adapter, adapters int
		hasPercent        bool
		percent           int
	}
	visibleOf := func(p *client.WikiSyncProgress) visible {
		shown := visible{phase: p.Phase, tool: p.Tool, adapter: p.Adapter, adapters: p.Adapters}
		if p.Total != nil {
			shown.hasPercent = true
			shown.percent = 100
			if *p.Total != 0 {
				shown.percent = saturatingMulInt(p.Done, 100) / *p.Total
			}
		}
		return shown
	}
	if state.view.Running == nil || visibleOf(state.view.Running) != visibleOf(&next) {
		state.view.Running = &next
		i.publish(state)
	}
}

// runAbandonable runs one sync pass on a goroutine of its own, outside
// daemon upgrade admission and outside the runtime's blocking work.
//
// A pass walks every native session store and can take minutes. It is safe
// to stop at any point: SessionWiki writes its index in SQLite transactions,
// which roll back when the process exits, and every daemon syncs again when
// it starts. So a daemon handoff must not wait for it. Holding admission made
// the handoff wait for the whole pass, and the daemon process waits for its
// blocking work when it exits, so a pass there would hold up the exit
// instead. On its own goroutine the exiting process abandons the pass.
func runAbandonable[T any](pass func() (T, error)) (T, error) {
	type answer struct {
		value T
		err   error
	}
	answers := make(chan answer, 1)
	go func() {
		defer func() {
			if recovered := recover(); recovered != nil {
				// Rust's sync thread panics with the panic hook's report and
				// drops its sender, which the waiter reads as no answer.
				slog.Error("the SessionWiki sync thread panicked", "panic", recovered, "stack", string(debug.Stack()))
				answers <- answer{err: fmt.Errorf("the SessionWiki sync thread stopped without an answer: %w", errors.New("channel closed"))}
			}
		}()
		value, err := pass()
		answers <- answer{value: value, err: err}
	}()
	result := <-answers
	return result.value, result.err
}

// openForSync opens the index for writing, with tool output kept as
// summaries.
//
// Mjolnir keeps only SessionWiki's one-line tool summaries: full output
// would greatly enlarge an index that holds every session on the machine and
// is shared with the `sessionwiki` command. The mode is set on every open
// rather than once, so an index whose setting is absent stays on summaries if
// SessionWiki's default changes. Setting the mode starts its own transaction,
// so it must run before any sync does.
//
// Opening an index an older SessionWiki built rebuilds its cache. That is
// said out loud: it costs a full pass, and an older `sessionwiki` command
// still in use rebuilds it back the next time it runs.
func openForSync() (*store.Connection, error) {
	if version := currentIndexVersion(); version.kind == indexVersionOlder {
		slog.Warn("an older SessionWiki wrote the session index; rebuilding it at this build's schema. "+
			"If a sessionwiki command did this, install the version the Mjolnir docs name "+
			"so the two stop rebuilding each other",
			"found", version.found, "expected", brokksessionwiki.SchemaVersion)
	}
	connection, err := brokksessionwiki.Open()
	if err != nil {
		return nil, fmt.Errorf("open the SessionWiki index: %w", err)
	}
	if _, err := brokksessionwiki.SetToolOutputMode(connection, brokksessionwiki.ToolOutputModeSummary); err != nil {
		connection.Close()
		return nil, fmt.Errorf("keep tool output as summaries in the SessionWiki index: %w", err)
	}
	return connection, nil
}

// syncBlocking is one synchronous sync pass. It refuses writes when this
// process must not touch the index, so a refused run never records a
// success it did not have. since nil is a full pass.
func syncBlocking(since *int64, cache *mjimport.NativeScanCache, owner *indexer) error {
	if refusal, refused := checkIndexWrite(); refused {
		return errors.New(refusal.String())
	}
	// Isolated tests park a pass here to stand for one that takes minutes.
	if err := core.ReachTestHook("sessionwiki_sync_pass"); err != nil {
		return err
	}
	config, state, err := store.LoadConfigAndState()
	if err != nil {
		return fmt.Errorf("load controller state for the SessionWiki sync: %w", err)
	}
	// Mjolnir's own sessions go first: a cold index walks every other tool's
	// store for many minutes, and a just-closed session should not wait on
	// it. Cleanup and enumeration use one ownership snapshot. Native
	// discovery happens afterwards, so it cannot make this snapshot stale
	// before use.
	mjolnir := MjolnirAdapterFromStateWithConfig(&state, &config)
	owned := []brokksessionwiki.Adapter{mjolnir}
	mjolnir.mu.Lock()
	ownership := mjolnir.sessions.ownership
	mjolnir.mu.Unlock()
	connection, err := openForSync()
	if err != nil {
		return err
	}
	defer connection.Close()
	// An incremental pass skips sessions older than since that the cache
	// does not hold, so it is only sound on the cache the last full pass
	// filled. Opening may itself have rebuilt the cache, and so may another
	// program at any time; the stamp read here, after opening, is what
	// decides.
	marker := fullPassMarker()
	generation, err := indexStampOf(connection)
	if err != nil {
		return fmt.Errorf("read the SessionWiki index header: %w", err)
	}
	if since != nil && !fullPassHolds(marker, generation) {
		since = nil
	}
	if _, err := topLevelPrune(connection, map[string]struct{}{}, &ownership); err != nil {
		return err
	}
	if err := brokksessionwiki.SyncWithProgress(connection, owned, since, func(progress *brokksessionwiki.SyncProgress) {
		owner.progress("Mjolnir", progress)
	}); err != nil {
		return fmt.Errorf("sync Mjolnir sessions into SessionWiki: %w", err)
	}
	adapters := nativeAdapters(&config)
	owner.progress("Native", &brokksessionwiki.SyncProgress{
		Tool:     "native sessions",
		Adapter:  0,
		Adapters: len(adapters),
		Done:     0,
	})
	native, excluded := topLevelPrepare(adapters, cache)
	if _, err := topLevelPrune(connection, excluded, &ownership); err != nil {
		return err
	}
	if err := brokksessionwiki.SyncWithProgress(connection, native, since, func(progress *brokksessionwiki.SyncProgress) {
		owner.progress("Native", progress)
	}); err != nil {
		return fmt.Errorf("sync native sessions into SessionWiki: %w", err)
	}
	if _, err := topLevelPrune(connection, excluded, &ownership); err != nil {
		return err
	}
	if err := writeSessionTags(connection, mjolnir.IndexedTags()); err != nil {
		return fmt.Errorf("store Mjolnir's session metadata in the SessionWiki index: %w", err)
	}
	if err := provenanceBackfill(connection, mjolnir); err != nil {
		return fmt.Errorf("backfill Mjolnir file provenance: %w", err)
	}
	// A full pass has walked every store, so the index is complete enough for
	// a search to be trusted. The marker is what a later daemon reads instead
	// of walking the corpus again to find out. A cache rebuilt during the pass
	// lost what the pass wrote before the rebuild, so it is not recorded.
	if since == nil {
		stamp, err := indexStampOf(connection)
		if err != nil {
			return fmt.Errorf("read the SessionWiki index header: %w", err)
		}
		if stamp == generation {
			recordFullPass(marker, generation)
		}
	}
	return nil
}

// writeSessionTags stores each session's target, profile and harness in the
// index, in one transaction.
//
// Every session is written on every sync rather than only the changed ones:
// the write is a delete and three inserts, which is nothing beside the
// transcript indexing in the same pass, and it is what makes a Move or a
// profile switch show up without tracking which records changed. It is also
// what gives sessions indexed before this existed their metadata, with no
// migration and no re-index.
func writeSessionTags(conn *store.Connection, sessionTags map[string]MjTags) error {
	if len(sessionTags) == 0 {
		return nil
	}
	transaction, err := conn.Begin()
	if err != nil {
		return fmt.Errorf("open a transaction for the session metadata: %w", err)
	}
	defer transaction.Rollback()
	for _, sessionID := range sortedKeys(sessionTags) {
		session := sessionTags[sessionID]
		if session.IsEmpty() {
			continue
		}
		if err := TagsWrite(transaction, sessionID, &session); err != nil {
			return err
		}
	}
	if err := transaction.Commit(); err != nil {
		return fmt.Errorf("commit the session metadata: %w", err)
	}
	return nil
}

// nativeAdapters is the non-Mjolnir adapters this install indexes.
//
// Mjolnir's configured harness profiles decide which harness homes are
// indexed, not the stock `~/.codex` and `~/.claude` locations. A user who
// runs several profile homes expects every session Mjolnir can start to be
// searchable, and a home no profile names is not Mjolnir's to walk. So the
// stock adapters are replaced with one adapter per enabled profile home,
// except OpenCode, which still uses its stock SQLite adapter. Adapters for
// unsupported harnesses (aider, gemini, cline, ...) are dropped, so they can
// neither trigger a home-directory walk nor add unrelated search results.
//
// Kimi Code, Grok Build, Muse, DSH and Antigravity CLI use SessionWiki's
// public adapters directly, one per enabled profile home. DSH histories are
// searchable even though Mjolnir does not yet import them for native resume.
//
// Each per-home adapter reports a reconcile scope covering only its own
// root, so a sync of one install never archives the rows of another.
func nativeAdapters(config *core.Config) []brokksessionwiki.Adapter {
	// Two profiles may share one home, and two harnesses may share one home
	// path without sharing sessions, so the kind is part of the identity.
	// Homes compare by component, as Rust's Path does.
	type home struct {
		kind       core.HarnessKind
		components string
	}
	seen := map[home]struct{}{}
	var adapters []brokksessionwiki.Adapter
	for _, profile := range config.EnabledProfiles() {
		identity := home{kind: profile.Kind, components: strings.Join(pathComponents(profile.Home), "\x00")}
		// A second adapter for the same home would only walk it twice.
		if _, walked := seen[identity]; walked {
			continue
		}
		seen[identity] = struct{}{}
		home := profile.Home
		switch profile.Kind {
		// Codex moves an archived thread's rollout to `archived_sessions`,
		// and may keep a thread only in its history database after the
		// rollout is gone. Each place needs its own adapter.
		case core.HarnessKindCodex:
			adapters = append(adapters,
				brokksessionwiki.CodexInHome(home),
				brokksessionwiki.CodexArchivedIn(home),
				brokksessionwiki.CodexHistoryInHome(home))
		case core.HarnessKindClaude:
			adapters = append(adapters, brokksessionwiki.ClaudeCodeInHome(home))
		case core.HarnessKindKimi:
			adapters = append(adapters, brokksessionwiki.KimiCodeInHome(home))
		// Earlier mj Grok/Muse keys used the configured home spelling,
		// including symlinks. Keep those IDs and reconciliation scopes.
		case core.HarnessKindGrok:
			adapters = append(adapters, brokksessionwiki.GrokBuildInHome(home).KeepConfiguredPaths())
		case core.HarnessKindMuse:
			// Share the import root, including native Windows separators.
			root, err := checkpoint.MuseSessionsRoot(home)
			if err != nil {
				panic(fmt.Sprintf("Muse always has a nested data directory: %v", err))
			}
			adapters = append(adapters, brokksessionwiki.MuseInSessionsRoot(root).KeepConfiguredPaths())
		case core.HarnessKindDsh:
			adapters = append(adapters, brokksessionwiki.DshInHome(home))
		case core.HarnessKindOpenCode:
		case core.HarnessKindAntigravity:
			adapters = append(adapters, brokksessionwiki.AntigravityCLIInHome(home))
		}
	}
	for _, adapter := range brokksessionwiki.All() {
		if kind, ok := harnessForTool(adapter.Name()); ok && kind == core.HarnessKindOpenCode {
			adapters = append(adapters, adapter)
		}
	}
	return adapters
}

// ---------------------------------------------------------------------------
// Which index, and whether it may be touched
// ---------------------------------------------------------------------------

// indexIsIsolatedSaid is index_is_isolated's static SAID.
var indexIsIsolatedSaid atomic.Bool

// indexIsIsolated reports whether this process may open the index at all.
//
// Indexing is always on, so a process that never resolved where its index
// belongs must not reach for one: it would walk the user's real session
// stores and write the user's real index. Only Mjolnir's own startup resolves
// it (see core's apply_instance_flag), so this refuses every unit test that
// builds a daemon runtime directly and every other embedder, unless it names
// an index of its own with `SESSIONWIKI_DATA`.
func indexIsIsolated() bool {
	if core.SessionIndexIsResolved() {
		return true
	}
	if _, ok := os.LookupEnv(core.SessionIndexEnv); ok {
		return true
	}
	if !indexIsIsolatedSaid.Swap(true) {
		slog.Debug("this process did not resolve a session index location; SessionWiki is not used")
	}
	return false
}

// indexStamp is the two numbers in an index file's header that say what its
// cache is.
type indexStamp struct {
	// schema is the schema SessionWiki built the cache at (`user_version`).
	// Zero is an index SessionWiki has not finished creating.
	schema int64
	// cookie is SQLite's schema cookie, which moves whenever a table is
	// created or dropped. A cache rebuilt at the same schema keeps schema and
	// moves this, so together they name one generation of the cache.
	cookie int64
}

// indexStampOf is Rust's IndexStamp::of.
func indexStampOf(conn store.Queryer) (indexStamp, error) {
	var stamp indexStamp
	if err := conn.QueryRow("PRAGMA user_version").Scan(&stamp.schema); err != nil {
		return indexStamp{}, err
	}
	if err := conn.QueryRow("PRAGMA schema_version").Scan(&stamp.cookie); err != nil {
		return indexStamp{}, err
	}
	return stamp, nil
}

func (s indexStamp) version() indexVersion {
	switch {
	case s.schema == 0:
		return indexVersion{kind: indexVersionCurrent}
	case s.schema < brokksessionwiki.SchemaVersion:
		return indexVersion{kind: indexVersionOlder, found: s.schema}
	case s.schema > brokksessionwiki.SchemaVersion:
		return indexVersion{kind: indexVersionNewer, found: s.schema}
	}
	return indexVersion{kind: indexVersionCurrent}
}

// indexVersion is how the cache on disk relates to the SessionWiki this
// build links (Rust's enum IndexVersion, as a comparable kind and value).
type indexVersion struct {
	kind indexVersionKind
	// found is the schema an older or newer SessionWiki built the cache at.
	found int64
}

type indexVersionKind int

const (
	// indexVersionCurrent: this build's schema, or an index not yet stamped
	// with one.
	indexVersionCurrent indexVersionKind = iota
	// indexVersionOlder: an older SessionWiki built it. This build cannot
	// read it, and the next write rebuilds the cache at this build's schema;
	// SessionWiki keeps the durable tables (tags, notes, summaries, archive)
	// across the rebuild.
	indexVersionOlder
	// indexVersionNewer: a newer SessionWiki built it. Rebuilding it down
	// would only make the newer program rebuild it back, so Mjolnir neither
	// reads nor writes it.
	indexVersionNewer
)

// onDiskStamp is the stamp of the index file, or false when there is no file
// yet or its header cannot be read. It opens nothing through SessionWiki,
// whose Open would rebuild the cache and whose DBPath creates directories.
func onDiskStamp() (indexStamp, bool) {
	path, ok := brokksessionwiki.ExistingDBPath()
	if !ok {
		return indexStamp{}, false
	}
	connection, err := store.OpenConnectionReadOnly(path)
	var stamp indexStamp
	if err == nil {
		stamp, err = indexStampOf(connection)
		connection.Close()
	}
	if err != nil {
		slog.Debug("could not read the SessionWiki index header", "error", err, "path", path)
		return indexStamp{}, false
	}
	return stamp, true
}

// currentIndexVersionSaid is index_version's static SAID.
var currentIndexVersionSaid atomic.Bool

// currentIndexVersion is Rust's `index_version` (renamed because Go cannot
// give the function and the indexVersion type one name).
func currentIndexVersion() indexVersion {
	version := indexVersion{kind: indexVersionCurrent}
	if stamp, ok := onDiskStamp(); ok {
		version = stamp.version()
	}
	if version.kind == indexVersionNewer && !currentIndexVersionSaid.Swap(true) {
		slog.Warn("a newer SessionWiki wrote the session index; Mjolnir leaves it alone until Mjolnir is upgraded",
			"found", version.found, "expected", brokksessionwiki.SchemaVersion)
	}
	return version
}

// indexIsWritable reports whether this process may write the index. Only a
// newer index is refused: writing an older one is what brings it to this
// build's schema.
func indexIsWritable() bool {
	_, refused := checkIndexWrite()
	return !refused
}

// readableIndex opens the index for reading, or nil when this process has no
// index of its own or the cache is at another schema. The schema is read on
// the connection the caller then queries, not beside it; a rebuild that
// lands after that read fails the query instead of answering from it.
// unreadableReason says which case it was. The caller closes the connection.
func readableIndex() (*store.Connection, error) {
	if !indexIsIsolated() {
		return nil, nil
	}
	connection, err := openReadonly()
	if err != nil {
		return nil, err
	}
	stamp, err := indexStampOf(connection)
	if err != nil {
		connection.Close()
		return nil, fmt.Errorf("read the SessionWiki index header: %w", err)
	}
	if stamp.version().kind != indexVersionCurrent {
		connection.Close()
		return nil, nil
	}
	return connection, nil
}

// unreadableReason is why readableIndex answered nil, in words an agent can
// act on.
func unreadableReason() string {
	if !indexIsIsolated() {
		return "session index location has not been configured"
	}
	switch currentIndexVersion().kind {
	case indexVersionNewer:
		return "a newer SessionWiki wrote the session index; upgrade Mjolnir to search it"
	case indexVersionOlder:
		return "the session index is being rebuilt for this SessionWiki version; retry in a few minutes"
	}
	return "the session index changed while it was opened; retry"
}

// fullPassMarker is the file recording the cache generation the last
// complete full pass indexed into. A cache rebuilt since, by this process or
// another program, has another generation, so the marker no longer holds and
// the next pass is a full one.
func fullPassMarker() string {
	return filepath.Join(core.DataDir(), "sessionwiki-built")
}

func markerText(stamp indexStamp) string {
	return fmt.Sprintf("%d %d", stamp.schema, stamp.cookie)
}

func fullPassHolds(marker string, stamp indexStamp) bool {
	if stamp.schema != brokksessionwiki.SchemaVersion {
		return false
	}
	held, err := os.ReadFile(marker)
	return err == nil && utf8.Valid(held) && strings.TrimSpace(string(held)) == markerText(stamp)
}

func recordFullPass(marker string, stamp indexStamp) {
	if fullPassHolds(marker, stamp) {
		return
	}
	// std::fs::write creates the file with mode 0o666 before the umask.
	if err := os.WriteFile(marker, []byte(markerText(stamp)), 0o666); err != nil {
		slog.Warn("could not record the full SessionWiki pass", "error", err, "path", marker)
	}
}

// IndexState is what a surface should say about this index right now.
func IndexState() client.WikiIndexState {
	if !indexIsIsolated() {
		return client.WikiIndexStateIndexing
	}
	stamp, ok := onDiskStamp()
	if !ok {
		return client.WikiIndexStateIndexing
	}
	return indexStateFor(stamp)
}

func indexStateFor(stamp indexStamp) client.WikiIndexState {
	switch stamp.version().kind {
	case indexVersionNewer:
		return client.WikiIndexStateVersionMismatch
	case indexVersionOlder:
		return client.WikiIndexStateIndexing
	}
	if fullPassHolds(fullPassMarker(), stamp) {
		return client.WikiIndexStateReady
	}
	return client.WikiIndexStateIndexing
}

// ---------------------------------------------------------------------------
// Indexing a session before it is destroyed
// ---------------------------------------------------------------------------

// DestroySyncWait is how long a destroy waits for a sync pass before it
// indexes the session on its own. A first build can run for many minutes,
// and a destroy must not wait for it.
const DestroySyncWait = 20 * time.Second

// How often rows the index was too busy to take are offered again, and for
// how long. A first build holds the index while it parses one tool's
// sessions and frees it between tools.
const (
	deferredWriteRetry = 5 * time.Second
	deferredWriteLimit = 60 * 60 * time.Second
)

// IndexedBeforeDestroy is how a session about to be destroyed, with its
// sub-agents, reached the index.
type IndexedBeforeDestroy interface {
	isIndexedBeforeDestroy()
}

// IndexedBeforeDestroyCurrent: the index already held every one of them as
// they are now, or none of them had a conversation to index.
type IndexedBeforeDestroyCurrent struct{}

// IndexedBeforeDestroySynced: a sync pass that began after the request
// finished in time.
type IndexedBeforeDestroySynced struct{}

// IndexedBeforeDestroyWrittenDirectly: the sync pass did not finish in time,
// so their rows were written on their own.
type IndexedBeforeDestroyWrittenDirectly struct{}

// IndexedBeforeDestroyDeferred: the index was busy. Their rows were read
// before the destroy and are written in the background once the index is
// free.
type IndexedBeforeDestroyDeferred struct{}

// IndexedBeforeDestroyUnavailable: this process may not write the index, and
// why.
type IndexedBeforeDestroyUnavailable struct {
	Reason string
}

// IndexedBeforeDestroyFailed: indexing failed, and why. The destroy goes
// ahead.
type IndexedBeforeDestroyFailed struct {
	Reason string
}

func (IndexedBeforeDestroyCurrent) isIndexedBeforeDestroy()         {}
func (IndexedBeforeDestroySynced) isIndexedBeforeDestroy()          {}
func (IndexedBeforeDestroyWrittenDirectly) isIndexedBeforeDestroy() {}
func (IndexedBeforeDestroyDeferred) isIndexedBeforeDestroy()        {}
func (IndexedBeforeDestroyUnavailable) isIndexedBeforeDestroy()     {}
func (IndexedBeforeDestroyFailed) isIndexedBeforeDestroy()          {}

// blockingAnswer is the value and error of one piece of blocking work.
type blockingAnswer[T any] struct {
	value T
	err   error
}

// IndexBeforeDestroy puts a top-level session into the index while its
// record and stored conversation still exist. Child cleanup never requires
// an index copy.
//
// Sessions enter the index only on a sync pass, and destroy deletes the
// record and the conversation, so a session created and destroyed between
// two passes was never findable (R2-11). This asks for an incremental pass
// and waits `wait` for it. A pass that does not finish in time, such as a
// first build, is left running, and the sessions are indexed on their own
// from the same rows the pass would write.
func (w WikiIndexer) IndexBeforeDestroy(ctx context.Context, sessionID string, wait time.Duration) IndexedBeforeDestroy {
	if reason, refused := unwritableReason(); refused {
		return IndexedBeforeDestroyUnavailable{Reason: reason}
	}
	root := sessionID
	answer, err := core.SpawnBlocking(func() blockingAnswer[[]string] {
		pending, err := unindexedSession(root)
		return blockingAnswer[[]string]{value: pending, err: err}
	}).Join(ctx)
	if err != nil {
		return IndexedBeforeDestroyFailed{Reason: fmt.Sprintf("checking the index for the session stopped: %v", err)}
	}
	if answer.err != nil {
		return IndexedBeforeDestroyFailed{Reason: fmt.Sprintf("could not tell whether the index holds the session: %v", answer.err)}
	}
	pending := answer.value
	if len(pending) == 0 {
		return IndexedBeforeDestroyCurrent{}
	}
	return indexBeforeDestroyWith(
		ctx,
		func() error { return w.SyncNow(ctx, false) },
		wait,
		func() ([]capturedSession, error) { return captureSessions(pending) },
		deferredWriteRetry,
	)
}

// indexWriteRefusal is why this process may not write the index, if it may
// not (Rust's enum IndexWriteRefusal: NotIsolated, or NewerSchema(found)).
type indexWriteRefusal struct {
	// newerSchema is false for NotIsolated.
	newerSchema bool
	found       int64
}

func (r indexWriteRefusal) summary() string {
	if r.newerSchema {
		return "a newer SessionWiki wrote the SessionWiki index"
	}
	return "this process did not resolve a SessionWiki index of its own"
}

// String is Rust's Display.
func (r indexWriteRefusal) String() string {
	if !r.newerSchema {
		return r.summary()
	}
	return fmt.Sprintf(
		"a newer SessionWiki wrote index schema %d; this build supports schema %d; upgrade Mjolnir to sync it",
		r.found, brokksessionwiki.SchemaVersion,
	)
}

// checkIndexWrite is Rust's `index_write_refusal` (renamed because Go cannot
// give the function and the indexWriteRefusal type one name): why this
// process may not write the index, and true, if it may not.
func checkIndexWrite() (indexWriteRefusal, bool) {
	if !indexIsIsolated() {
		return indexWriteRefusal{}, true
	}
	if version := currentIndexVersion(); version.kind == indexVersionNewer {
		return indexWriteRefusal{newerSchema: true, found: version.found}, true
	}
	return indexWriteRefusal{}, false
}

func unwritableReason() (string, bool) {
	refusal, refused := checkIndexWrite()
	if !refused {
		return "", false
	}
	return refusal.summary(), true
}

// indexBeforeDestroyWith is the bounded wait and its fallback, with the sync
// pass and the reading of the sessions passed in so a test can stand in for
// either. sync runs on its own task and is left running when the wait ends
// first.
func indexBeforeDestroyWith(ctx context.Context, sync func() error, wait time.Duration, capture func() ([]capturedSession, error), retry time.Duration) IndexedBeforeDestroy {
	// A timed-out caller leaves its generation with the owner; the pass
	// finishes even when destroy proceeds with its captured session copy.
	pass := core.Spawn(sync)
	timer := time.NewTimer(wait)
	select {
	case <-pass.Done():
		timer.Stop()
		failure, stopped := pass.Join(context.Background())
		switch {
		case stopped != nil:
			slog.Warn("the SessionWiki sync before a destroy stopped; indexing the session on its own", "error", stopped)
		case failure != nil:
			slog.Warn("the SessionWiki sync before a destroy failed; indexing the session on its own", "error", failure)
		default:
			return IndexedBeforeDestroySynced{}
		}
	case <-timer.C:
		slog.Info("the SessionWiki sync did not finish in time; indexing the session on its own",
			"wait_seconds", wait.Seconds())
	case <-ctx.Done():
		timer.Stop()
		return IndexedBeforeDestroyFailed{Reason: fmt.Sprintf("waiting for the SessionWiki sync stopped: %v", ctx.Err())}
	}
	answer, err := core.SpawnBlocking(func() blockingAnswer[[]capturedSession] {
		captured, err := capture()
		return blockingAnswer[[]capturedSession]{value: captured, err: err}
	}).Join(ctx)
	if err != nil {
		return IndexedBeforeDestroyFailed{Reason: fmt.Sprintf("reading the session to index it stopped: %v", err)}
	}
	if answer.err != nil {
		return IndexedBeforeDestroyFailed{Reason: fmt.Sprintf("could not read the session to index it: %v", answer.err)}
	}
	captured := answer.value
	if len(captured) == 0 {
		return IndexedBeforeDestroyCurrent{}
	}
	written, err := core.SpawnBlocking(func() error { return writeCaptured(captured) }).Join(ctx)
	switch {
	case err != nil:
		return IndexedBeforeDestroyFailed{Reason: fmt.Sprintf("writing the session into the index stopped: %v", err)}
	case written == nil:
		return IndexedBeforeDestroyWrittenDirectly{}
	case store.IsBusyError(written):
		writeCapturedLater(captured, retry)
		return IndexedBeforeDestroyDeferred{}
	}
	return IndexedBeforeDestroyFailed{Reason: fmt.Sprintf("could not write the session into the index: %v", written)}
}

// unindexedSession is the top-level session root when the index does not
// hold its current conversation.
func unindexedSession(root string) ([]string, error) {
	config, state, err := store.LoadConfigAndState()
	if err != nil {
		return nil, fmt.Errorf("load controller state to index a destroyed session: %w", err)
	}
	if !state.Sessions.ContainsKey(root) {
		return nil, nil
	}
	return unindexed(MjolnirAdapterFromStateWithConfig(&state, &config), []string{root})
}

// unindexed is those of sessionIDs that have a conversation the index does
// not hold as it is now: no row, an archived row, or a row with an older
// change token than the adapter lists.
func unindexed(adapter *MjolnirAdapter, sessionIDs []string) ([]string, error) {
	tokens := storeTokens(adapter)
	// No index yet holds nothing, and neither does an older cache: writing
	// these rows is what rebuilds it, so a row it holds now is about to go.
	connection, err := readableIndex()
	if err != nil {
		connection = nil
	}
	if connection != nil {
		defer connection.Close()
	}
	var pending []string
	for _, sessionID := range sessionIDs {
		key := adapter.keyFor(sessionID)
		// Only a session with a stored or checkpointed conversation is
		// listed.
		token, listed := tokens[key]
		if !listed {
			continue
		}
		current := false
		if connection != nil {
			held, ok, err := indexedToken(connection, key)
			if err != nil {
				return nil, err
			}
			current = ok && held == token
		}
		if !current {
			pending = append(pending, sessionID)
		}
	}
	return pending, nil
}

// storeTokens is every key the adapter's store lists, with its change token.
func storeTokens(adapter *MjolnirAdapter) map[string]int64 {
	tokens := map[string]int64{}
	if listed := adapter.Store(); listed != nil {
		for _, key := range listed.Keys {
			tokens[key.Key] = key.Token
		}
	}
	return tokens
}

// indexedToken is the change token the index holds for a live row, and
// whether it holds one. SessionWiki stores a shared-store token in the
// `mtime` column.
func indexedToken(conn store.Queryer, key string) (int64, bool, error) {
	var token int64
	err := conn.QueryRow("SELECT mtime FROM files WHERE path = ?1 AND archived_at IS NULL", key).Scan(&token)
	switch {
	case errors.Is(err, sql.ErrNoRows):
		return 0, false, nil
	case err != nil:
		return 0, false, fmt.Errorf("read a session's change token from the SessionWiki index: %w", err)
	}
	return token, true, nil
}

// capturedSession is one session's index row and metadata, read while its
// record and conversation still exist, so they can be written after both are
// gone.
type capturedSession struct {
	key     string
	token   int64
	session *brokksessionwiki.Session
	tags    MjTags
}

func captureSessions(sessionIDs []string) ([]capturedSession, error) {
	config, state, err := store.LoadConfigAndState()
	if err != nil {
		return nil, fmt.Errorf("load controller state to index a destroyed session: %w", err)
	}
	return captureSessionsFrom(MjolnirAdapterFromStateWithConfig(&state, &config), sessionIDs)
}

// captureSessionsFrom is the rows a sync pass would write for these
// sessions, built by the same adapter. A session with no conversation to
// index is left out.
func captureSessionsFrom(adapter *MjolnirAdapter, sessionIDs []string) ([]capturedSession, error) {
	tokens := storeTokens(adapter)
	sessionTags := adapter.IndexedTags()
	var captured []capturedSession
	for _, sessionID := range sessionIDs {
		key := adapter.keyFor(sessionID)
		token, listed := tokens[key]
		if !listed {
			continue
		}
		session, err := adapter.ParseKey(key)
		if err != nil {
			return nil, err
		}
		tags := sessionTags[sessionID]
		delete(sessionTags, sessionID)
		captured = append(captured, capturedSession{key: key, token: token, session: session, tags: tags})
	}
	return captured, nil
}

// writeCaptured writes captured rows through SessionWiki's own indexing,
// one session at a time, and their metadata beside them. captured is shared
// with retries and not changed.
func writeCaptured(captured []capturedSession) error {
	if !indexIsWritable() {
		return errors.New("this process may not write the SessionWiki index")
	}
	connection, err := openForSync()
	if err != nil {
		return err
	}
	defer connection.Close()
	for index := range captured {
		adapter := &capturedAdapter{captured: captured, index: index}
		if err := brokksessionwiki.SyncWith(connection, []brokksessionwiki.Adapter{adapter}, nil); err != nil {
			return fmt.Errorf("index a session before it is destroyed: %w", err)
		}
	}
	sessionTags := map[string]MjTags{}
	for _, session := range captured {
		sessionTags[session.session.ID] = session.tags
	}
	if err := writeSessionTags(connection, sessionTags); err != nil {
		return fmt.Errorf("store Mjolnir's session metadata in the SessionWiki index: %w", err)
	}
	return nil
}

// writeCapturedLater offers rows the index was too busy to take until it
// takes them, or until deferredWriteLimit passes. The rows live only in this
// task: a daemon that stops before the index is free loses them, and the
// sessions logged here are then not found by id.
func writeCapturedLater(captured []capturedSession, retry time.Duration) {
	sessions := make([]string, len(captured))
	for index := range captured {
		sessions[index] = captured[index].session.ID
	}
	slog.Info("the SessionWiki index is busy; indexing the destroyed sessions once it is free", "sessions", sessions)
	go func() {
		started := time.Now()
		for {
			time.Sleep(retry)
			written, stopped := core.SpawnBlocking(func() error { return writeCaptured(captured) }).Join(context.Background())
			err := written
			if stopped != nil {
				err = stopped
			}
			if err == nil {
				slog.Info("indexed the destroyed sessions", "sessions", sessions)
				return
			}
			if !store.IsBusyError(err) || time.Since(started) >= deferredWriteLimit {
				slog.Warn("gave up indexing destroyed sessions in SessionWiki", "sessions", sessions, "error", err)
				return
			}
		}
	}()
}

// capturedAdapter is one captured session, offered to SessionWiki as a
// shared store that lists only it.
type capturedAdapter struct {
	// AdapterDefaults supplies NativeID, ModifiedAt and PathKeyDepth.
	brokksessionwiki.AdapterDefaults
	captured []capturedSession
	index    int
}

// current is Rust's CapturedAdapter::captured (renamed because the struct
// has a captured field): the one session this adapter lists.
func (a *capturedAdapter) current() *capturedSession {
	return &a.captured[a.index]
}

func (a *capturedAdapter) Name() string { return mjolnirTool }

func (a *capturedAdapter) Root() (string, bool) {
	return pathParent(a.current().key)
}

func (a *capturedAdapter) Discover() brokksessionwiki.Discovered {
	return brokksessionwiki.Discovered{}
}

func (a *capturedAdapter) Parse(string) (*brokksessionwiki.Session, error) {
	return nil, errors.New("Mjolnir sessions are parsed by key, not by file")
}

func (a *capturedAdapter) Store() *brokksessionwiki.Store {
	captured := a.current()
	return &brokksessionwiki.Store{
		Keys: []brokksessionwiki.StoreKey{{Key: captured.key, Token: captured.token}},
	}
}

func (a *capturedAdapter) ParseKey(key string) (*brokksessionwiki.Session, error) {
	captured := a.current()
	if key != captured.key {
		return nil, fmt.Errorf("no captured session for key %s", core.RustDebugString(key))
	}
	return copySession(captured.session), nil
}

// ReconcileScope is a prefix no key starts with, since keys hold no NUL.
// This store lists one session, not every session of this instance, so
// reconciliation must not archive the rows it does not list.
func (a *capturedAdapter) ReconcileScope() (string, bool) {
	return a.current().key + "\x00", true
}

// copySession is a copy of an indexed session, for a write that may be
// retried.
func copySession(session *brokksessionwiki.Session) *brokksessionwiki.Session {
	copied := *session
	copied.RawCwd = copyString(session.RawCwd)
	copied.OperationalEditedPaths = slices.Clone(session.OperationalEditedPaths)
	copied.Started = copyTime(session.Started)
	copied.Ended = copyTime(session.Ended)
	copied.Touched = slices.Clone(session.Touched)
	if session.Messages != nil {
		copied.Messages = make([]brokksessionwiki.Message, len(session.Messages))
		for index, message := range session.Messages {
			message.Ts = copyTime(message.Ts)
			message.Tool = copyToolEvent(message.Tool)
			copied.Messages[index] = message
		}
	}
	if session.Edits != nil {
		copied.Edits = make([]brokksessionwiki.EditEvent, len(session.Edits))
		for index, edit := range session.Edits {
			edit.Ts = copyTime(edit.Ts)
			copied.Edits[index] = edit
		}
	}
	return &copied
}

// copyToolEvent is a deep copy of a message's tool data.
func copyToolEvent(event brokksessionwiki.ToolEvent) brokksessionwiki.ToolEvent {
	switch event := event.(type) {
	case nil:
		return nil
	case *brokksessionwiki.ToolEventSummary:
		summary := event.Summary
		summary.Args = copyJSONValue(summary.Args)
		summary.Output = copyString(summary.Output)
		if summary.IsError != nil {
			isError := *summary.IsError
			summary.IsError = &isError
		}
		if summary.Lines != nil {
			lines := *summary.Lines
			summary.Lines = &lines
		}
		return &brokksessionwiki.ToolEventSummary{Summary: summary}
	case *brokksessionwiki.ToolEventPart:
		switch part := event.Part.(type) {
		case brokksessionwiki.ToolPartCall:
			part.Call.ID = copyString(part.Call.ID)
			part.Call.Args = copyJSONValue(part.Call.Args)
			return &brokksessionwiki.ToolEventPart{Part: part}
		case brokksessionwiki.ToolPartResult:
			part.Result.CallID = copyString(part.Result.CallID)
			return &brokksessionwiki.ToolEventPart{Part: part}
		}
		return &brokksessionwiki.ToolEventPart{Part: event.Part}
	}
	panic(fmt.Sprintf("unknown tool event %T", event))
}

// copyJSONValue is a deep copy of a JSON value in jsonvalue form.
func copyJSONValue(value any) any {
	switch value := value.(type) {
	case jsonvalue.Object:
		return value.Clone()
	case []any:
		if value == nil {
			return value
		}
		copied := make([]any, len(value))
		for index, item := range value {
			copied[index] = copyJSONValue(item)
		}
		return copied
	}
	return value
}

// ---------------------------------------------------------------------------
// Queries and restore
// ---------------------------------------------------------------------------

// MaxWikiLimit is the largest page a caller may ask a wiki query for.
const MaxWikiLimit = 200

// DefaultWikiLimit is the page size a caller that names none gets.
const DefaultWikiLimit = 50

// minFulltextQuery: SessionWiki's full-text index needs three characters;
// shorter queries fall back to a substring scan.
const minFulltextQuery = 3

// QueryRows is one page of the index, newest first or best match first.
//
// live is the set of session ids this daemon still holds, which is what
// decides whether a Mjolnir row names a session the user can simply resume.
// Only top-level sessions are answered. includeToolMatches keeps tool-only
// hits for agent history; the resume list requires conversational matches.
// Runs SQLite work, so callers on an event loop run it as blocking work.
func QueryRows(query string, limit int, live map[string]struct{}, includeToolMatches bool) ([]client.WikiRow, error) {
	limit = min(max(limit, 1), MaxWikiLimit)
	connection, err := readableIndex()
	if err != nil {
		return nil, err
	}
	if connection == nil {
		// Nothing to answer from: either this process has no index of its
		// own or the one on disk is at another version. The status beside the
		// rows says which.
		return []client.WikiRow{}, nil
	}
	defer connection.Close()
	ownership, err := currentSnapshot()
	if err != nil {
		return nil, err
	}
	return queryRowsFrom(connection, query, limit, live, includeToolMatches, &ownership, mjimport.NativeScanCacheShared())
}

// MjolnirSessionIDs is the session ids SessionWiki has indexed under
// Mjolnir's tool name. This is a discovery supplement: the daemon's durable
// session table remains the authority, while the index can reveal a
// checkpointed session that has not been materialized into controller state
// in this daemon run.
func MjolnirSessionIDs() (map[string]struct{}, error) {
	connection, err := readableIndex()
	if err != nil {
		return nil, err
	}
	if connection == nil {
		return map[string]struct{}{}, nil
	}
	defer connection.Close()
	statement, err := connection.Prepare("SELECT DISTINCT session_id FROM files WHERE tool = ?1")
	if err != nil {
		return nil, fmt.Errorf("prepare Mjolnir SessionWiki discovery: %w", err)
	}
	defer statement.Close()
	rows, err := statement.Query(mjolnirTool)
	if err != nil {
		return nil, fmt.Errorf("query Mjolnir SessionWiki discovery: %w", err)
	}
	defer rows.Close()
	ids := map[string]struct{}{}
	for rows.Next() {
		var id string
		if err := rows.Scan(&id); err != nil {
			return nil, fmt.Errorf("read Mjolnir SessionWiki discovery: %w", err)
		}
		ids[id] = struct{}{}
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("read Mjolnir SessionWiki discovery: %w", err)
	}
	return ids, nil
}

func queryRowsFrom(conn store.Queryer, query string, limit int, live map[string]struct{}, includeToolMatches bool, ownership *topLevelSnapshot, cache *mjimport.NativeScanCache) ([]client.WikiRow, error) {
	limit = min(max(limit, 1), MaxWikiLimit)
	query = strings.TrimSpace(query)
	if query == "" {
		recent, err := brokksessionwiki.Recent(conn, MaxWikiLimit, nil, nil, nil, true)
		if err != nil {
			return nil, fmt.Errorf("list recent SessionWiki sessions: %w", err)
		}
		visible := make([]client.WikiRow, 0, limit)
		for index := range recent {
			if len(visible) >= limit {
				break
			}
			child, err := isChild(&recent[index], ownership, cache)
			if err != nil {
				return nil, err
			}
			if child {
				continue
			}
			visible = append(visible, wikiRow(recent[index], nil, live))
		}
		if err := fillSessionTags(conn, visible); err != nil {
			return nil, err
		}
		return visible, nil
	}
	// SessionWiki filters conversational roles before its candidate cap and
	// session grouping. Native-child filtering still happens here, so
	// over-fetch to keep children ranked first from shortening a page.
	searchLimit := MaxWikiLimit
	var roles *[]brokksessionwiki.Role
	if !includeToolMatches {
		conversational := slices.Clone(conversationRoles)
		roles = &conversational
	}
	hits, err := brokksessionwiki.SearchWithRoles(conn, query, searchLimit, nil, nil, roles)
	if err != nil {
		return nil, fmt.Errorf("search the SessionWiki index: %w", err)
	}
	rows := make([]client.WikiRow, 0, min(len(hits), limit))
	found := map[string]struct{}{}
	// A pasted session id names the session itself, so it ranks above the
	// sessions that merely mention that id in their messages.
	pasted, err := idLike(conn, query)
	if err != nil {
		return nil, err
	}
	for index := range pasted {
		if len(rows) >= limit {
			break
		}
		child, err := isChild(&pasted[index], ownership, cache)
		if err != nil {
			return nil, err
		}
		if child {
			continue
		}
		found[pasted[index].SessionID] = struct{}{}
		rows = append(rows, wikiRow(pasted[index], nil, live))
	}
	for index := range hits {
		if len(rows) >= limit {
			break
		}
		if _, seen := found[hits[index].Row.SessionID]; seen {
			continue
		}
		child, err := isChild(&hits[index].Row, ownership, cache)
		if err != nil {
			return nil, err
		}
		if child {
			continue
		}
		found[hits[index].Row.SessionID] = struct{}{}
		snippet := hits[index].Snippet
		rows = append(rows, wikiRow(hits[index].Row, &snippet, live))
	}
	// SessionWiki searches message text alone, so a session known by a title
	// or a project that is never said out loud would be unfindable. Those
	// matches follow the full-text ones rather than displacing them.
	if len(rows) < limit {
		named, err := namedLike(conn, query)
		if err != nil {
			return nil, err
		}
		for index := range named {
			if len(rows) >= limit {
				break
			}
			if _, seen := found[named[index].SessionID]; seen {
				continue
			}
			child, err := isChild(&named[index], ownership, cache)
			if err != nil {
				return nil, err
			}
			if child {
				continue
			}
			found[named[index].SessionID] = struct{}{}
			rows = append(rows, wikiRow(named[index], nil, live))
		}
	}
	if err := fillSessionTags(conn, rows); err != nil {
		return nil, err
	}
	return rows, nil
}

// textSearchMessageLimit is how many message rows one text search reads
// before it stops. A term common enough to pass this can miss sessions whose
// only match ranks past it; the person narrows the query, as with the resume
// dialog's own search.
const textSearchMessageLimit int64 = 20_000

// SessionTextMatches is the live Mjolnir sessions whose user or agent
// messages contain every query term, ignoring case; quotes group phrases.
// User matches take precedence over agent matches. Tool messages never
// match. Runs SQLite work, so callers on an event loop run it as blocking
// work.
func SessionTextMatches(query string, live map[string]struct{}) ([]client.SessionTextMatch, error) {
	query = strings.TrimSpace(query)
	if query == "" {
		return []client.SessionTextMatch{}, nil
	}
	connection, err := readableIndex()
	if err != nil {
		return nil, err
	}
	if connection == nil {
		return []client.SessionTextMatch{}, nil
	}
	defer connection.Close()
	ownership, err := currentSnapshot()
	if err != nil {
		return nil, err
	}
	return textMatchesIn(connection, query, live, &ownership)
}

func textMatchesIn(conn store.Queryer, query string, live map[string]struct{}, ownership *topLevelSnapshot) ([]client.SessionTextMatch, error) {
	terms := queryTerms(query)
	if len(terms) == 0 {
		return []client.SessionTextMatch{}, nil
	}
	var phrases []string
	for _, term := range terms {
		if utf8.RuneCountInString(term) >= minFulltextQuery {
			phrases = append(phrases, `"`+strings.ReplaceAll(term, `"`, `""`)+`"`)
		}
	}
	fulltext := strings.Join(phrases, " ")
	var sqlText strings.Builder
	if fulltext == "" {
		// No term is long enough for the trigram index.
		sqlText.WriteString("SELECT f.session_id, f.path, m.role, f.kind FROM messages m")
	} else {
		sqlText.WriteString("SELECT f.session_id, f.path, m.role, f.kind\n         FROM msgs JOIN messages m ON m.id = msgs.rowid")
	}
	sqlText.WriteString(" JOIN files f ON f.session_id = m.session_id\n          WHERE f.tool = ? AND m.role IN ('user', 'assistant')")
	arguments := []any{mjolnirTool}
	if fulltext != "" {
		sqlText.WriteString(" AND msgs MATCH ?")
		arguments = append(arguments, fulltext)
	}
	for _, term := range terms {
		if utf8.RuneCountInString(term) >= minFulltextQuery {
			continue
		}
		sqlText.WriteString(` AND m.text LIKE ? ESCAPE '\'`)
		escaped := strings.NewReplacer(`\`, `\\`, `%`, `\%`, `_`, `\_`).Replace(term)
		arguments = append(arguments, "%"+escaped+"%")
	}
	if fulltext == "" {
		sqlText.WriteString(" ORDER BY m.id DESC")
	}
	sqlText.WriteString(" LIMIT ?")
	arguments = append(arguments, textSearchMessageLimit)
	statement, err := conn.Prepare(sqlText.String())
	if err != nil {
		return nil, err
	}
	defer statement.Close()
	rows, err := statement.Query(arguments...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	type matched struct{ sessionID, path, role, kind string }
	var read []matched
	for rows.Next() {
		var row matched
		if err := rows.Scan(&row.sessionID, &row.path, &row.role, &row.kind); err != nil {
			return nil, fmt.Errorf("search the indexed messages: %w", err)
		}
		read = append(read, row)
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("search the indexed messages: %w", err)
	}
	found := map[string]client.SessionTextMatchKind{}
	for _, row := range read {
		if _, isLive := live[row.sessionID]; !isLive || ownership.ownsIndexedChild(row.sessionID, row.path, mjolnirTool, row.kind) {
			continue
		}
		kind := client.SessionTextMatchKindAgent
		if row.role == "user" {
			kind = client.SessionTextMatchKindUser
		}
		if existing, seen := found[row.sessionID]; !seen || kind < existing {
			found[row.sessionID] = kind
		}
	}
	matches := make([]client.SessionTextMatch, 0, len(found))
	for _, sessionID := range sortedKeys(found) {
		matches = append(matches, client.SessionTextMatch{SessionID: sessionID, Kind: found[sessionID]})
	}
	return matches, nil
}

// fillSessionTags fills in the target, profile and harness of every Mjolnir
// row on this page from the index's own tags, in one query.
//
// Only Mjolnir writes those tags, so a row from another tool keeps nil and is
// not even asked about.
func fillSessionTags(conn store.Queryer, rows []client.WikiRow) error {
	var ids []string
	for index := range rows {
		if rows[index].Tool == mjolnirTool {
			ids = append(ids, rows[index].ID)
		}
	}
	found, err := TagsRead(conn, ids)
	if err != nil {
		return fmt.Errorf("read the indexed session metadata: %w", err)
	}
	for index := range rows {
		row := &rows[index]
		if row.Tool != mjolnirTool {
			continue
		}
		session, ok := found[row.ID]
		if !ok {
			continue
		}
		row.Target = session.Target
		row.Profile = session.Profile
		row.Harness = session.Harness
	}
	return nil
}

// nameScanLimit is how far back a title or project match looks. Those
// columns have no index of their own, so this is a scan of the most recent
// sessions rather than of the whole corpus. Fetch only metadata here: the
// regular recent-session query also loads a preview, summary and tags for
// every row, none of which title matching needs.
const nameScanLimit = 2_000

// idLike is the indexed sessions whose id starts with the query, when the
// query reads as a pasted id: hexadecimal digits and dashes only (dashes for
// a harness's own UUID), and at least as long as the id prefix Mjolnir
// shows. The length keeps short hex words such as "face" or "added" from
// reading as ids. SessionWiki's Resolve matches Mjolnir and other tools'
// short ids as well as Codex and Claude native ids.
func idLike(conn store.Queryer, query string) ([]brokksessionwiki.SessionRow, error) {
	hexDigits := 0
	onlyIDCharacters := true
	for _, character := range query {
		hex := '0' <= character && character <= '9' || 'a' <= character && character <= 'f' || 'A' <= character && character <= 'F'
		if hex {
			hexDigits++
		} else if character != '-' {
			onlyIDCharacters = false
		}
	}
	if !onlyIDCharacters || hexDigits < core.ShortIDLen {
		return nil, nil
	}
	rows, err := brokksessionwiki.Resolve(conn, query)
	if err != nil {
		return nil, fmt.Errorf("look up a pasted session id: %w", err)
	}
	return rows, nil
}

// namedLike is the indexed sessions whose title or project contains the
// query, ignoring case.
func namedLike(conn store.Queryer, query string) ([]brokksessionwiki.SessionRow, error) {
	needle := grepLowercase(query)
	statement, err := conn.Prepare(fmt.Sprintf(
		`SELECT session_id, tool, path, project, title, started, msg_count, kind,
                archived_at IS NOT NULL
         FROM files ORDER BY started DESC LIMIT %d`, nameScanLimit))
	if err != nil {
		return nil, fmt.Errorf("prepare recent SessionWiki metadata scan: %w", err)
	}
	defer statement.Close()
	rows, err := statement.Query()
	if err != nil {
		return nil, fmt.Errorf("list recent SessionWiki metadata: %w", err)
	}
	defer rows.Close()
	var matched []brokksessionwiki.SessionRow
	for rows.Next() {
		var row brokksessionwiki.SessionRow
		var started sql.NullString
		if err := rows.Scan(&row.SessionID, &row.Tool, &row.Path, &row.Project, &row.Title,
			&started, &row.MsgCount, &row.Kind, &row.Archived); err != nil {
			return nil, fmt.Errorf("read recent SessionWiki metadata: %w", err)
		}
		if started.Valid {
			row.Started = &started.String
		}
		if strings.Contains(grepLowercase(row.Title), needle) || strings.Contains(grepLowercase(row.Project), needle) {
			matched = append(matched, row)
		}
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("read recent SessionWiki metadata: %w", err)
	}
	return matched, nil
}

// Brief is the briefing for one indexed session, or false when the id names
// none.
func Brief(id string, maxChars int) (string, bool, error) {
	connection, err := readableIndex()
	if err != nil || connection == nil {
		return "", false, err
	}
	defer connection.Close()
	ownership, err := currentSnapshot()
	if err != nil {
		return "", false, err
	}
	row, err := visibleRowByID(connection, id, &ownership, mjimport.NativeScanCacheShared())
	if err != nil || row == nil {
		return "", false, err
	}
	session, err := brokksessionwiki.SessionFromIndex(connection, row)
	if err != nil {
		return "", false, fmt.Errorf("read an indexed session: %w", err)
	}
	return brokksessionwiki.BriefMarkdown(session, maxChars, true), true, nil
}

// TranscriptHits is the passages of one indexed session that match query,
// or nil when the id names no indexed session.
//
// Every matching message is returned with contextMessages neighbours on each
// side; overlapping groups are merged and each group's first block says how
// many messages were skipped before it. Each block's text is capped at
// perMessageChars characters, keeping the window around its first match.
func TranscriptHits(id string, query string, contextMessages int, perMessageChars int) (*client.WikiHitTranscript, error) {
	connection, err := readableIndex()
	if err != nil || connection == nil {
		return nil, err
	}
	defer connection.Close()
	ownership, err := currentSnapshot()
	if err != nil {
		return nil, err
	}
	row, err := visibleRowByID(connection, id, &ownership, mjimport.NativeScanCacheShared())
	if err != nil || row == nil {
		return nil, err
	}
	session, err := brokksessionwiki.SessionFromIndex(connection, row)
	if err != nil {
		return nil, fmt.Errorf("read an indexed session: %w", err)
	}
	found := hitTranscript(session, query, contextMessages, perMessageChars)
	return &found, nil
}

// hitTranscript is the matching passages of one loaded session, converted to
// the daemon response shape. Pure, so the conversion can be tested without an
// index.
//
// Tool output never anchors a passage: it is machine chatter the reader did
// not write, a hit buried in it would open the preview on a wall of command
// output, and the preview collapses tool runs anyway. Tool messages still
// appear as context around a real match.
func hitTranscript(session *brokksessionwiki.Session, query string, contextMessages int, perMessageChars int) client.WikiHitTranscript {
	found := grepSession(session, query, &grepOpts{
		contextMessages: contextMessages,
		chars:           perMessageChars,
		anchorRoles:     []brokksessionwiki.Role{brokksessionwiki.RoleUser, brokksessionwiki.RoleAssistant},
	})
	blocks := make([]client.WikiHitBlock, 0, len(found.hits))
	for _, hit := range found.hits {
		hits := hit.matches
		if hits == nil {
			hits = [][2]int{}
		}
		blocks = append(blocks, client.WikiHitBlock{
			Role:          roleName(hit.role),
			Text:          hit.text,
			Hits:          hits,
			OmittedBefore: hit.omittedBefore,
			Truncated:     hit.truncated,
		})
	}
	return client.WikiHitTranscript{Blocks: blocks, OmittedAfter: found.omittedAfter}
}

func roleName(role brokksessionwiki.Role) string {
	switch role {
	case brokksessionwiki.RoleUser:
		return "user"
	case brokksessionwiki.RoleAssistant:
		return "assistant"
	case brokksessionwiki.RoleTool:
		return "tool"
	}
	panic(fmt.Sprintf("unknown role %q", string(role)))
}

// ArchivedSession is what a restore needs from the index: the transcript as
// a snapshot the compaction pipeline accepts, plus the title and project of
// the session it came from.
type ArchivedSession struct {
	Title string
	// ProjectDirectory is the project directory the session ran in, when the
	// row names one that still exists.
	ProjectDirectory *string
	Snapshot         core.CanonicalSessionSnapshot
}

// LoadArchivedSession is Rust's `archived_session` (renamed because Go
// cannot give the function and the ArchivedSession type one name): one
// indexed session for restore, or nil when the id names none.
func LoadArchivedSession(id string) (*ArchivedSession, error) {
	connection, err := readableIndex()
	if err != nil || connection == nil {
		return nil, err
	}
	defer connection.Close()
	ownership, err := currentSnapshot()
	if err != nil {
		return nil, err
	}
	row, err := visibleRowByID(connection, id, &ownership, mjimport.NativeScanCacheShared())
	if err != nil || row == nil {
		return nil, err
	}
	return archivedSessionFromRow(connection, row)
}

func archivedSessionFromRow(conn store.Queryer, row *brokksessionwiki.SessionRow) (*ArchivedSession, error) {
	session, err := brokksessionwiki.SessionFromIndex(conn, row)
	if err != nil {
		return nil, fmt.Errorf("read an indexed session: %w", err)
	}
	snapshot, err := snapshotOf(session)
	if err != nil {
		return nil, err
	}
	archived := &ArchivedSession{Title: session.Title, Snapshot: snapshot}
	if directory, ok := projectDirectoryOf(session.Project); ok {
		archived.ProjectDirectory = &directory
	}
	return archived, nil
}

// ---------------------------------------------------------------------------
// The archive job
// ---------------------------------------------------------------------------

// SessionsReadyToArchive is the stopped sessions that `archive_after_days =
// olderThanDays` has caught, children before their parents.
//
// A session qualifies when its record is Stopped, its last update is at least
// that many days old, and every sub-agent child it still has is being
// archived in the same pass. The child rule is what keeps the pass from
// destroying a session it did not choose: archiving a parent tears its
// children down with it, so a child that is still running, or stopped but not
// yet old enough, holds its parent back until the next pass.
//
// Pure over controller state, so the rule can be tested without a daemon.
func SessionsReadyToArchive(sessions core.SnapshotMap[string, core.SessionRecord], subagents core.SnapshotMap[string, core.SubagentMetadata], now time.Time, olderThanDays uint32) []string {
	state := core.NewStateState()
	state.Sessions = sessions
	state.Subagents = subagents
	return SessionsReadyToArchiveFromState(&state, now, olderThanDays)
}

func checkpointMayBeOriginDelta(metadata *core.CheckpointMetadata) bool {
	usesOriginDelta, err := checkpoint.CheckpointArchiveUsesOriginDelta(metadata.ArchivePath)
	if err == nil {
		return usesOriginDelta
	}
	// Fail closed when an archive file exists on disk and cannot be read;
	// return false only if confirmed absent so synthetic unit tests with
	// non-existent paths continue to work.
	exists, err := pathTryExists(metadata.ArchivePath)
	return err != nil || exists
}

func SessionsReadyToArchiveFromState(state *core.StateState, now time.Time, olderThanDays uint32) []string {
	sessions := &state.Sessions
	subagents := &state.Subagents
	cutoff := now.UTC().AddDate(0, 0, -int(olderThanDays))
	aged := func(sessionID string) bool {
		record := sessions.GetShared(sessionID)
		if record == nil {
			return false
		}
		checkout, err := state.Checkout(sessionID)
		hasCheckout := err == nil
		isManagedWorktree := false
		if hasCheckout {
			if managed, ok := checkout.Effective().(core.CheckoutManagedWorktree); ok {
				isManagedWorktree = managed.Worktree.Kind.Equal(core.ManagedCheckoutKindWorktree)
			}
		}
		isLegacyManagedWorktree := isManagedWorktree &&
			!(record.Checkpoint != nil && checkpointMayBeOriginDelta(record.Checkpoint))
		// Preserve the historical child rule: its copied raw path counted as
		// a raw checkout, even when the parent owns a managed clone.
		hasRawPath := false
		if hasCheckout {
			if _, ok := checkout.ProjectDirectory(); ok {
				switch checkout.(type) {
				case core.CheckoutAttached, core.CheckoutBorrowed:
					hasRawPath = true
				}
			}
		}
		published := record.Checkpoint != nil && record.Publication != nil &&
			record.Publication.CheckpointSha256 == record.Checkpoint.Sha256 &&
			record.Publication.State == core.PublicationStatePublished &&
			!record.Publication.Dirty &&
			!record.Publication.Stashed
		updated, parsed := parseTime(record.UpdatedAt)
		return record.State == core.SessionStateStopped &&
			parsed && !updated.After(cutoff) &&
			(isLegacyManagedWorktree || hasRawPath || published)
	}
	var selected []string
	for sessionID := range sessions.Keys() {
		if !aged(sessionID) {
			continue
		}
		ready := true
		for child := range subagents.Values() {
			// A child whose record is already gone holds nothing open.
			if child.ParentSessionID != sessionID || !sessions.ContainsKey(child.ChildSessionID) {
				continue
			}
			if !aged(child.ChildSessionID) {
				ready = false
				break
			}
		}
		if ready {
			selected = append(selected, sessionID)
		}
	}
	slices.SortStableFunc(selected, func(a, b string) int {
		return ancestorDepth(b, *subagents) - ancestorDepth(a, *subagents)
	})
	return selected
}

// ancestorDepth is how many sub-agent parents a session has above it. Deeper
// sessions are archived first so a parent never tears down a child the pass
// still has to visit.
func ancestorDepth(sessionID string, subagents core.SnapshotMap[string, core.SubagentMetadata]) int {
	depth := 0
	current := sessionID
	// Bounded by the map: a cycle cannot outlive one pass over every entry.
	for {
		child := subagents.GetShared(current)
		if child == nil {
			break
		}
		depth++
		if depth > subagents.Len() {
			break
		}
		current = child.ParentSessionID
	}
	return depth
}

// ArchiveSpacePreview is the space every session uses now and, when
// olderThanDays is set, the space archiving after that many days would
// reclaim.
//
// "Mjolnir's own copy" of a session is the checkpoint archive plus the
// session's image attachments; the conversation itself lives in the
// SessionWiki index and is not counted, because archiving keeps it.
//
// The reclaim figure uses the archive job's own selection rule but not its
// "is it indexed yet" gate: that gate depends on how far the hourly index
// sync has got, so applying it would make the estimate swing between zero
// and the true value while the first index builds. This answers what the
// policy would reclaim, not what the next tick happens to reclaim.
//
// Walks the filesystem, so callers on an event loop run it as blocking work.
func ArchiveSpacePreview(olderThanDays *uint32) (core.ArchiveSpacePreview, error) {
	_, state, err := store.LoadConfigAndState()
	if err != nil {
		return core.ArchiveSpacePreview{}, fmt.Errorf("load the session records to size their storage: %w", err)
	}
	return archiveSpaceOverState(core.SessionsDir(), &state, time.Now().UTC(), olderThanDays), nil
}

// archiveSpaceOverState sizes archives using a State view so borrowed
// checkout owners remain visible.
func archiveSpaceOverState(sessionsRoot string, state *core.StateState, now time.Time, olderThanDays *uint32) core.ArchiveSpacePreview {
	preview := core.ArchiveSpacePreview{Sessions: state.Sessions.Len()}
	for sessionID, record := range state.Sessions.All() {
		preview.Bytes = saturatingAddUint64(preview.Bytes, sessionBytes(sessionsRoot, sessionID, record))
	}
	if olderThanDays != nil {
		aged := SessionsReadyToArchiveFromState(state, now, *olderThanDays)
		preview.ReclaimableSessions = len(aged)
		for _, sessionID := range aged {
			if record := state.Sessions.GetShared(sessionID); record != nil {
				preview.ReclaimableBytes = saturatingAddUint64(preview.ReclaimableBytes, sessionBytes(sessionsRoot, sessionID, record))
			}
		}
	}
	return preview
}

// sessionBytes is what archiving one session would free: its checkpoint
// archive and its attachments. Anything already missing counts as zero.
func sessionBytes(sessionsRoot string, sessionID string, record *core.SessionRecord) uint64 {
	var archived uint64
	if record.Checkpoint != nil {
		if info, err := os.Stat(record.Checkpoint.ArchivePath); err == nil && info.Mode().IsRegular() {
			archived = uint64(info.Size())
		}
	}
	attachments, err := mjimport.DirectorySize(filepath.Join(sessionsRoot, sessionID, core.AttachmentDir))
	if err != nil {
		attachments = 0
	}
	return saturatingAddUint64(archived, attachments)
}

// IndexedWithMessages is which of sessionIDs the index holds under this
// instance's own key, with at least one message and not already archived.
//
// This is the gate the archive job will not cross: Mjolnir only deletes its
// own copy of a conversation SessionWiki has actually stored. Runs SQLite
// work, so callers on an event loop run it as blocking work.
func IndexedWithMessages(sessionIDs []string) (map[string]struct{}, error) {
	connection, err := readableIndex()
	if err != nil {
		return nil, err
	}
	if connection == nil {
		// An index this daemon cannot read holds nothing it may act on, and
		// the archive job deletes data, so it must find nothing here.
		return map[string]struct{}{}, nil
	}
	defer connection.Close()
	sessionsDir := core.SessionsDir()
	indexed := map[string]struct{}{}
	for _, sessionID := range sessionIDs {
		key := sessionsDir + "/" + sessionID
		rows, err := brokksessionwiki.Resolve(connection, sessionID)
		if err != nil {
			return nil, fmt.Errorf("look up a stopped session in the SessionWiki index: %w", err)
		}
		for _, row := range rows {
			if row.Tool == mjolnirTool && row.Path == key && row.MsgCount > 0 && !row.Archived {
				indexed[sessionID] = struct{}{}
				break
			}
		}
	}
	return indexed, nil
}

func openReadonly() (*store.Connection, error) {
	connection, err := brokksessionwiki.OpenReadonly()
	if err != nil {
		return nil, fmt.Errorf("open the SessionWiki index: %w", err)
	}
	return connection, nil
}

// rowByID is the one row an id names exactly, or nil. Resolve matches
// prefixes, which is right for a person typing and wrong for a client
// passing an id back.
func rowByID(conn store.Queryer, id string) (*brokksessionwiki.SessionRow, error) {
	rows, err := brokksessionwiki.Resolve(conn, id)
	if err != nil {
		return nil, fmt.Errorf("look up an indexed session: %w", err)
	}
	for index := range rows {
		if rows[index].SessionID == id {
			return &rows[index], nil
		}
	}
	return nil, nil
}

// visibleRowByID: explicit IDs obey the same visibility rule as search
// results; a child found by standalone SessionWiki sync is hidden as though
// it were absent.
func visibleRowByID(conn store.Queryer, id string, ownership *topLevelSnapshot, cache *mjimport.NativeScanCache) (*brokksessionwiki.SessionRow, error) {
	row, err := rowByID(conn, id)
	if err != nil || row == nil {
		return nil, err
	}
	child, err := isChild(row, ownership, cache)
	if err != nil {
		return nil, err
	}
	if child {
		return nil, nil
	}
	return row, nil
}

func wikiRow(row brokksessionwiki.SessionRow, snippet *string, live map[string]struct{}) client.WikiRow {
	// Only this daemon's own sessions can be live here, and only under the
	// key shape the adapter writes: the checkpoint directory and the session
	// id.
	var helSessionID *string
	if row.Tool == mjolnirTool {
		sessionID := row.Path[strings.LastIndexByte(row.Path, '/')+1:]
		if _, isLive := live[sessionID]; isLive {
			helSessionID = &sessionID
		}
	}
	var nativeID *string
	if id, ok := brokksessionwiki.NativeIDOf(row.Path); ok {
		nativeID = &id
	}
	return client.WikiRow{
		ID:           row.SessionID,
		Tool:         row.Tool,
		Project:      row.Project,
		Title:        row.Title,
		Started:      row.Started,
		Msgs:         row.MsgCount,
		Preview:      row.Preview,
		Archived:     row.Archived,
		NativeID:     nativeID,
		Snippet:      snippet,
		HelSessionID: helSessionID,
		// Filled in by fillSessionTags from the index's own tags; the row
		// itself does not carry them.
	}
}

// projectDirectoryOf is the project a restored session should open.
//
// A Mjolnir session runs in a managed worktree under the repository it was
// started from, and that worktree is gone once the session is archived. The
// repository above it is what the user still has, so a worktree path is
// reduced to it. Any other path is used as it stands, and a path that no
// longer exists is left for the caller to replace.
func projectDirectoryOf(project string) (string, bool) {
	if strings.TrimSpace(project) == "" {
		return "", false
	}
	repository := project
	for ancestor, ok := project, true; ok; ancestor, ok = pathParent(ancestor) {
		if name, named := pathFileName(ancestor); named && name == ".mj" {
			if parent, hasParent := pathParent(ancestor); hasParent {
				repository = parent
			}
			break
		}
	}
	if info, err := os.Stat(repository); err != nil || !info.IsDir() {
		return "", false
	}
	return repository, true
}

// snapshotOf rebuilds an indexed transcript as a canonical snapshot.
//
// The snapshot is only ever read by the compaction pipeline, which wants
// turns: a user message opens a turn and assistant and tool items attach to
// it. Messages before the first user message therefore have nowhere to go
// and are dropped, and a session with no user message at all cannot be
// restored.
func snapshotOf(session *brokksessionwiki.Session) (core.CanonicalSessionSnapshot, error) {
	var startedMs int64
	if session.Started != nil {
		startedMs = session.Started.UnixMilli()
	}
	var items []core.CanonicalTranscriptItem
	for _, message := range session.Messages {
		text := strings.TrimSpace(message.Text)
		if text == "" {
			continue
		}
		// Compaction attaches assistant and tool items to the open turn, so an
		// item before the first user message would be dropped anyway.
		if len(items) == 0 && message.Role != brokksessionwiki.RoleUser {
			continue
		}
		position := uint64(len(items)) + 1
		var body core.CanonicalTranscriptBody
		switch message.Role {
		case brokksessionwiki.RoleUser:
			content, err := jsonvalue.Marshal(jsonvalue.Object{
				{Key: "type", Value: "text"},
				{Key: "text", Value: text},
			})
			if err != nil {
				return core.CanonicalSessionSnapshot{}, err
			}
			body = core.CanonicalTranscriptBodyUser{Content: []json.RawMessage{content}}
		case brokksessionwiki.RoleAssistant:
			chunk, err := jsonvalue.Marshal(jsonvalue.Object{
				{Key: "content", Value: jsonvalue.Object{
					{Key: "type", Value: "text"},
					{Key: "text", Value: text},
				}},
			})
			if err != nil {
				return core.CanonicalSessionSnapshot{}, err
			}
			body = core.CanonicalTranscriptBodyAgent{Chunks: []json.RawMessage{chunk}, Streaming: false}
		// New indexes retain the shared projection; legacy rows contain only
		// a title.
		case brokksessionwiki.RoleTool:
			call, terminalOutputs := transcript.IndexedToolCall(text, fmt.Sprintf("wiki-tool-%d", position))
			body = core.CanonicalTranscriptBodyTool{
				Call:            call,
				TerminalOutputs: terminalOutputs,
				TerminalRefs:    []string{},
			}
		default:
			panic(fmt.Sprintf("unknown role %q", string(message.Role)))
		}
		createdAtMs := startedMs
		if message.Ts != nil {
			createdAtMs = message.Ts.UnixMilli()
		}
		item := core.CanonicalTranscriptItem{
			StableID:        fmt.Sprintf("wiki-%d", position),
			Position:        position,
			CreatedAtMs:     createdAtMs,
			LastChangedAtMs: createdAtMs,
			Body:            body,
		}
		// The validator wants an ordinal on agent messages and on nothing
		// else; one event per item makes the item's own position right.
		if _, agent := body.(core.CanonicalTranscriptBodyAgent); agent {
			ordinal := position
			item.LatestContentEventOrdinal = &ordinal
		}
		items = append(items, item)
	}
	if len(items) == 0 {
		return core.CanonicalSessionSnapshot{}, errors.New("the archived session has no prompt to restore from")
	}

	lastActivityAtMs := items[len(items)-1].LastChangedAtMs
	// Not a relay frontier, so there is no recorded digest to carry. It has
	// to be a well-formed non-genesis digest, and deriving it from the
	// session makes two restores of one session agree.
	digest := sha256.Sum256([]byte("sessionwiki:" + session.ID))
	var title *string
	if strings.TrimSpace(session.Title) != "" {
		copied := session.Title
		title = &copied
	}
	return core.CanonicalSessionSnapshot{
		EventFrontier:       uint64(len(items)),
		EventFrontierDigest: core.LowerHex(digest[:]),
		Session: core.CanonicalSessionState{
			Execution:        core.CanonicalExecutionState{State: core.CanonicalExecutionStateIdle},
			LastActivityAtMs: &lastActivityAtMs,
			SessionTitle:     title,
		},
		Transcript:    items,
		QueuedPrompts: []core.CanonicalQueuedPrompt{},
	}, nil
}

// ---------------------------------------------------------------------------
// Continuing an indexed session
// ---------------------------------------------------------------------------

// WikiContinuation is what continuing one indexed session means.
//
// An agent that found a session with SessionWiki should not have to know
// whose session it was, so the branch lives here and `mj resume --wiki` takes
// it on the agent's behalf.
type WikiContinuation interface {
	isWikiContinuation()
}

// WikiContinuationResume: a Mjolnir session this daemon still has a record
// of; resume it.
type WikiContinuationResume struct {
	SessionID string
}

// WikiContinuationRestore: a Mjolnir session whose record the archive job
// destroyed; start a new session seeded with a compacted hand-off.
type WikiContinuationRestore struct {
	WikiID string
}

// WikiContinuationImport: another tool's session; import it, then resume
// what the import made.
type WikiContinuationImport struct {
	Harness         core.HarnessKind
	NativeSessionID string
}

func (WikiContinuationResume) isWikiContinuation()  {}
func (WikiContinuationRestore) isWikiContinuation() {}
func (WikiContinuationImport) isWikiContinuation()  {}

// WikiContinuationOf is Rust's `wiki_continuation` (renamed because Go
// cannot give the function and the WikiContinuation type one name): how to
// continue the indexed session a row describes.
//
// Pure over the row so the branch can be tested without an index: path is
// the row's stored path and hasRecord says whether controller state still
// holds a session with this id.
func WikiContinuationOf(wikiID string, tool string, path string, hasRecord bool) (WikiContinuation, error) {
	if tool == mjolnirTool {
		// MjolnirAdapter.ParseKey names the session by its own Mjolnir id, so
		// a Mjolnir row's SessionWiki id is the session id.
		if hasRecord {
			return WikiContinuationResume{SessionID: wikiID}, nil
		}
		return WikiContinuationRestore{WikiID: wikiID}, nil
	}
	harness, ok := harnessForTool(tool)
	if !ok {
		return nil, fmt.Errorf("Mjolnir cannot continue a %s session", tool)
	}
	if !mjimport.NativeSessionsEnumerable(harness) {
		return nil, fmt.Errorf("Mjolnir cannot import %s sessions", tool)
	}
	if harness == core.HarnessKindCodex && strings.ContainsRune(path, '\u001f') {
		return nil, errors.New("Mjolnir can only import Codex sessions with a rollout log; this row is from the Codex history database")
	}
	nativeSessionID, ok := mjimport.NativeSessionIDFromPath(harness, path)
	if !ok {
		return nil, fmt.Errorf("no %s session id in the indexed path %s", tool, path)
	}
	return WikiContinuationImport{Harness: harness, NativeSessionID: nativeSessionID}, nil
}

// WikiSession is what one indexed session is, as far as continuing it is
// concerned, or nil when the id names none. The daemon serves it for `mj
// resume --wiki` and for `mj sessions --session` when the id names no
// Mjolnir session.
func WikiSession(wikiID string, knownSessions map[string]struct{}) (*client.WikiSessionInfo, error) {
	connection, err := readableIndex()
	if err != nil || connection == nil {
		return nil, err
	}
	defer connection.Close()
	ownership, err := currentSnapshot()
	if err != nil {
		return nil, err
	}
	return wikiSessionFrom(connection, wikiID, knownSessions, &ownership, mjimport.NativeScanCacheShared())
}

func wikiSessionFrom(conn store.Queryer, wikiID string, knownSessions map[string]struct{}, ownership *topLevelSnapshot, cache *mjimport.NativeScanCache) (*client.WikiSessionInfo, error) {
	row, err := visibleRowByID(conn, wikiID, ownership, cache)
	if err != nil || row == nil {
		return nil, err
	}
	isMjolnir := row.Tool == mjolnirTool
	var mjolnirSessionID *string
	hasRecord := false
	if isMjolnir {
		sessionID := row.SessionID
		mjolnirSessionID = &sessionID
		_, hasRecord = knownSessions[sessionID]
	}
	var status client.WikiSessionStatus
	switch {
	case !isMjolnir:
		status = client.WikiSessionStatusNative
	case hasRecord:
		status = client.WikiSessionStatusMine
	default:
		status = client.WikiSessionStatusArchived
	}
	var tags MjTags
	if isMjolnir {
		found, err := TagsRead(conn, []string{row.SessionID})
		if err != nil {
			return nil, fmt.Errorf("read the indexed session metadata: %w", err)
		}
		tags = found[row.SessionID]
	}
	// Only an archived row is continued by restoring its transcript, and a
	// restore needs a prompt to open the first turn.
	nothingToRestore := false
	if status == client.WikiSessionStatusArchived {
		session, err := brokksessionwiki.SessionFromIndex(conn, row)
		if err != nil {
			return nil, fmt.Errorf("read an indexed session: %w", err)
		}
		nothingToRestore = !hasPrompt(session)
	}
	var harness *core.HarnessKind
	if tags.Harness != nil {
		if kind, err := core.ParseHarnessKind(*tags.Harness); err == nil {
			harness = &kind
		}
	}
	if harness == nil && !isMjolnir {
		if kind, ok := harnessForTool(row.Tool); ok {
			harness = &kind
		}
	}
	return &client.WikiSessionInfo{
		WikiID:           row.SessionID,
		Tool:             row.Tool,
		Path:             row.Path,
		Status:           status,
		MjolnirSessionID: mjolnirSessionID,
		ProfileID:        tags.Profile,
		TargetTemplateID: tags.Target,
		Harness:          harness,
		Title:            row.Title,
		Project:          row.Project,
		NothingToRestore: nothingToRestore,
	}, nil
}

// hasPrompt reports whether an indexed transcript holds a prompt, which is
// what snapshotOf needs to open a turn.
func hasPrompt(session *brokksessionwiki.Session) bool {
	for _, message := range session.Messages {
		if message.Role == brokksessionwiki.RoleUser && strings.TrimSpace(message.Text) != "" {
			return true
		}
	}
	return false
}

// ---------------------------------------------------------------------------
// Small helpers for Rust's library behaviour
// ---------------------------------------------------------------------------

// jsonMember is a JSON object's member key (serde_json's Value::get with a
// key), nil when value is not an object or has no such member.
func jsonMember(value any, key string) any {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil
	}
	member, _ := object.Get(key)
	return member
}

// chronoTimestampMillis is chrono's DateTime::from_timestamp_millis: nil
// outside chrono's years -262143 to 262142.
func chronoTimestampMillis(millis int64) *time.Time {
	at := time.UnixMilli(millis).UTC()
	if year := at.Year(); year < -262143 || year > 262142 {
		return nil
	}
	return &at
}

// chronoJSONTime is a timestamp as chrono's Serialize writes a
// DateTime<Utc>: RFC 3339 with as many fractional digits as it needs and a
// `Z`, or nil for none.
func chronoJSONTime(at *time.Time) any {
	if at == nil {
		return nil
	}
	return strings.TrimSuffix(core.FormatChronoRFC3339(*at), "+00:00") + "Z"
}

func copyTime(at *time.Time) *time.Time {
	if at == nil {
		return nil
	}
	copied := *at
	return &copied
}

func copyString(text *string) *string {
	if text == nil {
		return nil
	}
	copied := *text
	return &copied
}

// firstChars is text's first count characters, as Rust's
// `chars().take(count)` collects them.
func firstChars(text string, count int) string {
	for offset := range text {
		if count <= 0 {
			return text[:offset]
		}
		count--
	}
	return text
}

// sortedKeys is a map's keys in order, as Rust's BTreeMap iterates them.
func sortedKeys[V any](values map[string]V) []string {
	keys := make([]string, 0, len(values))
	for key := range values {
		keys = append(keys, key)
	}
	slices.Sort(keys)
	return keys
}

func saturatingMulInt64(a, b int64) int64 {
	if a == 0 || b == 0 {
		return 0
	}
	product := a * b
	if product/b != a || (a == -1 && b == math.MinInt64) || (b == -1 && a == math.MinInt64) {
		if (a < 0) != (b < 0) {
			return math.MinInt64
		}
		return math.MaxInt64
	}
	return product
}

func saturatingAddInt64(a, b int64) int64 {
	sum := a + b
	switch {
	case b > 0 && sum < a:
		return math.MaxInt64
	case b < 0 && sum > a:
		return math.MinInt64
	}
	return sum
}

func saturatingAddInt(a, b int) int {
	if b > 0 && a > math.MaxInt-b {
		return math.MaxInt
	}
	return a + b
}

func saturatingMulInt(a, b int) int {
	if a != 0 && b != 0 && a > math.MaxInt/b {
		return math.MaxInt
	}
	return a * b
}

func saturatingAddUint64(a, b uint64) uint64 {
	if a > math.MaxUint64-b {
		return math.MaxUint64
	}
	return a + b
}

// pathSeparator reports whether character separates path components here,
// as Rust's Path reads them.
func pathSeparator(character rune) bool {
	return character == '/' || character == filepath.Separator
}

// pathComponents is a path's components as Rust's Path::components yields
// them: the root as "/", "." dropped after the first component, ".." kept.
func pathComponents(path string) []string {
	var components []string
	if path != "" && pathSeparator(rune(path[0])) {
		components = append(components, "/")
	}
	for index, part := range strings.FieldsFunc(path, pathSeparator) {
		if part == "." && !(index == 0 && components == nil) {
			continue
		}
		components = append(components, part)
	}
	return components
}

// pathTrimEnd drops the trailing separators and `.` components Rust's Path
// ignores at the end of a path, keeping a lone root.
func pathTrimEnd(path string) string {
	for {
		trimmed := strings.TrimRightFunc(path, pathSeparator)
		if trimmed == "" {
			if path != "" {
				return path[:1]
			}
			return ""
		}
		if before, ok := strings.CutSuffix(trimmed, "."); ok && before != "" && pathSeparator(rune(before[len(before)-1])) {
			path = before
			continue
		}
		return trimmed
	}
}

// pathParent is Rust's Path::parent: the path without its last component,
// false for a root or an empty path.
func pathParent(path string) (string, bool) {
	trimmed := pathTrimEnd(path)
	if trimmed == "" || (len(trimmed) == 1 && pathSeparator(rune(trimmed[0]))) {
		return "", false
	}
	cut := strings.LastIndexFunc(trimmed, pathSeparator)
	if cut < 0 {
		return "", true
	}
	parent := strings.TrimRightFunc(trimmed[:cut], pathSeparator)
	if parent == "" {
		return trimmed[:1], true
	}
	return pathTrimEnd(parent), true
}

// pathFileName is Rust's Path::file_name: the last component when it is a
// name, false for a root, `.` or `..`.
func pathFileName(path string) (string, bool) {
	trimmed := pathTrimEnd(path)
	name := trimmed[strings.LastIndexFunc(trimmed, pathSeparator)+1:]
	if name == "" || name == "." || name == ".." {
		return "", false
	}
	return name, true
}
