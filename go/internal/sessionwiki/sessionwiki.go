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
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"strings"
	"sync"
	"sync/atomic"
	"time"
	"unicode/utf8"

	acp "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/brokksessionwiki"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/mjimport"
	"github.com/BrokkAi/mjolnir/go/internal/store"
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs session_change_token")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs project_directories_of")
}

// IndexedProjectDirectory is the project path SessionWiki should search for
// this session: the attached or managed raw checkout, or the primary
// repository in a managed workspace. false when there is none.
func IndexedProjectDirectory(state *core.StateState, config *core.Config, sessionID string, record *core.SessionRecord) (string, bool, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs indexed_project_directory")
}

// liveTokens is the change token of every session whose transcript is still
// only in the daemon's database: its activity watermark in whole seconds.
//
// A stopped session keeps being indexed from its checkpoint, which never
// changes again. Everything else is indexed from the projection, so a running
// session is findable before it has ever been closed.
func liveTokens(state *core.StateState) map[string]int64 {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs live_tokens")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs MjolnirAdapter::indexed_tags")
}

// reloadSessions is Rust's MjolnirAdapter::reload (renamed because the
// struct has a reload field).
func (a *MjolnirAdapter) reloadSessions() {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs MjolnirAdapter::reload")
}

// checkpointedTranscript is the conversation of a stopped session, read from
// its newest checkpoint, with the title the checkpoint recorded.
func (a *MjolnirAdapter) checkpointedTranscript(sessionID string) (indexedTranscript, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs MjolnirAdapter::checkpointed_transcript")
}

// projectedTranscript is the conversation of a session that has not
// stopped, read from the daemon's own projection. It is the same
// conversation the checkpoint would hold, minus whatever has not happened
// yet.
func (a *MjolnirAdapter) projectedTranscript(sessionID string) (indexedTranscript, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs MjolnirAdapter::projected_transcript")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs MjolnirAdapter::newest_archives")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs newest_checkpoint_archive")
}

// projectedTranscriptOf is Rust's free function `projected_transcript`: a
// running session's conversation and evidence, as SessionWiki stores them.
func projectedTranscriptOf(projection *core.MaterializedSession) indexedTranscript {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs projected_transcript")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs TranscriptIndex::tool")
}

// finish resolves the legacy tools with legacyOutput and keeps the messages
// after the last context boundary.
func (x *transcriptIndex[R]) finish(title *string, legacyOutput func(records []R, call *acp.ToolCall) (string, bool)) indexedTranscript {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs TranscriptIndex::finish")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs message")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs MjolnirAdapter::store")
}

// ReconcileScope: every Mjolnir instance publishes under one tool name, so
// this instance speaks only for keys under its own checkpoint directory.
// Without the scope, two instances would archive each other's rows on every
// sync.
func (a *MjolnirAdapter) ReconcileScope() (string, bool) {
	return a.sessionsDir + "/", true
}

func (a *MjolnirAdapter) ParseKey(key string) (*brokksessionwiki.Session, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs MjolnirAdapter::parse_key")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs SyncState::finish_pass")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs WikiIndexer::watch_after")
}

// SyncNow requests a pass and waits for one that covers it. A later
// successful pass also covers everything this request needed.
func (w WikiIndexer) SyncNow(ctx context.Context, full bool) error {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs WikiIndexer::sync_now")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs Indexer::request")
}

// startNextPass starts the requested pass, if any: its generation, whether
// it is full, and the epoch second an incremental pass starts from (nil for
// all).
func (i *indexer) startNextPass() (generation uint64, full bool, since *int64, ok bool) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs Indexer::start_next_pass")
}

// run is the sync loop. Like Rust's task it runs until the process exits.
func (i *indexer) run() {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs Indexer::run")
}

func (i *indexer) progress(phase string, progress *brokksessionwiki.SyncProgress) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs Indexer::progress")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs run_abandonable")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs open_for_sync")
}

// syncBlocking is one synchronous sync pass. It refuses writes when this
// process must not touch the index, so a refused run never records a
// success it did not have. since nil is a full pass.
func syncBlocking(since *int64, cache *mjimport.NativeScanCache, owner *indexer) error {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs sync_blocking")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs write_session_tags")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs native_adapters")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs index_is_isolated")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs IndexStamp::of")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs on_disk_stamp")
}

// currentIndexVersionSaid is index_version's static SAID.
var currentIndexVersionSaid atomic.Bool

// currentIndexVersion is Rust's `index_version` (renamed because Go cannot
// give the function and the indexVersion type one name).
func currentIndexVersion() indexVersion {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs index_version")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs readable_index")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs full_pass_marker")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs WikiIndexer::index_before_destroy")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs index_before_destroy_with")
}

// unindexedSession is the top-level session root when the index does not
// hold its current conversation.
func unindexedSession(root string) ([]string, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs unindexed_session")
}

// unindexed is those of sessionIDs that have a conversation the index does
// not hold as it is now: no row, an archived row, or a row with an older
// change token than the adapter lists.
func unindexed(adapter *MjolnirAdapter, sessionIDs []string) ([]string, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs unindexed")
}

// indexedToken is the change token the index holds for a live row, and
// whether it holds one. SessionWiki stores a shared-store token in the
// `mtime` column.
func indexedToken(conn store.Queryer, key string) (int64, bool, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs indexed_token")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs capture_sessions")
}

// captureSessionsFrom is the rows a sync pass would write for these
// sessions, built by the same adapter. A session with no conversation to
// index is left out.
func captureSessionsFrom(adapter *MjolnirAdapter, sessionIDs []string) ([]capturedSession, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs capture_sessions_from")
}

// writeCaptured writes captured rows through SessionWiki's own indexing,
// one session at a time, and their metadata beside them. captured is shared
// with retries and not changed.
func writeCaptured(captured []capturedSession) error {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs write_captured")
}

// writeCapturedLater offers rows the index was too busy to take until it
// takes them, or until deferredWriteLimit passes. The rows live only in this
// task: a daemon that stops before the index is free loses them, and the
// sessions logged here are then not found by id.
func writeCapturedLater(captured []capturedSession, retry time.Duration) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs write_captured_later")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs CapturedAdapter::root")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs copy_session")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs query_rows")
}

// MjolnirSessionIDs is the session ids SessionWiki has indexed under
// Mjolnir's tool name. This is a discovery supplement: the daemon's durable
// session table remains the authority, while the index can reveal a
// checkpointed session that has not been materialized into controller state
// in this daemon run.
func MjolnirSessionIDs() (map[string]struct{}, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs mjolnir_session_ids")
}

func queryRowsFrom(conn store.Queryer, query string, limit int, live map[string]struct{}, includeToolMatches bool, ownership *topLevelSnapshot, cache *mjimport.NativeScanCache) ([]client.WikiRow, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs query_rows_from")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs session_text_matches")
}

func textMatchesIn(conn store.Queryer, query string, live map[string]struct{}, ownership *topLevelSnapshot) ([]client.SessionTextMatch, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs text_matches_in")
}

// fillSessionTags fills in the target, profile and harness of every Mjolnir
// row on this page from the index's own tags, in one query.
//
// Only Mjolnir writes those tags, so a row from another tool keeps nil and is
// not even asked about.
func fillSessionTags(conn store.Queryer, rows []client.WikiRow) error {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs fill_session_tags")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs id_like")
}

// namedLike is the indexed sessions whose title or project contains the
// query, ignoring case.
func namedLike(conn store.Queryer, query string) ([]brokksessionwiki.SessionRow, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs named_like")
}

// Brief is the briefing for one indexed session, or false when the id names
// none.
func Brief(id string, maxChars int) (string, bool, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs brief")
}

// TranscriptHits is the passages of one indexed session that match query,
// or nil when the id names no indexed session.
//
// Every matching message is returned with contextMessages neighbours on each
// side; overlapping groups are merged and each group's first block says how
// many messages were skipped before it. Each block's text is capped at
// perMessageChars characters, keeping the window around its first match.
func TranscriptHits(id string, query string, contextMessages int, perMessageChars int) (*client.WikiHitTranscript, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs transcript_hits")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs hit_transcript")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs archived_session")
}

func archivedSessionFromRow(conn store.Queryer, row *brokksessionwiki.SessionRow) (*ArchivedSession, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs archived_session_from_row")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs sessions_ready_to_archive")
}

func checkpointMayBeOriginDelta(checkpoint *core.CheckpointMetadata) bool {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs checkpoint_may_be_origin_delta")
}

func SessionsReadyToArchiveFromState(state *core.StateState, now time.Time, olderThanDays uint32) []string {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs sessions_ready_to_archive_from_state")
}

// ancestorDepth is how many sub-agent parents a session has above it. Deeper
// sessions are archived first so a parent never tears down a child the pass
// still has to visit.
func ancestorDepth(sessionID string, subagents core.SnapshotMap[string, core.SubagentMetadata]) int {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs ancestor_depth")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs archive_space_preview")
}

// archiveSpaceOverState sizes archives using a State view so borrowed
// checkout owners remain visible.
func archiveSpaceOverState(sessionsRoot string, state *core.StateState, now time.Time, olderThanDays *uint32) core.ArchiveSpacePreview {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs archive_space_over_state")
}

// sessionBytes is what archiving one session would free: its checkpoint
// archive and its attachments. Anything already missing counts as zero.
func sessionBytes(sessionsRoot string, sessionID string, record *core.SessionRecord) uint64 {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs session_bytes")
}

// IndexedWithMessages is which of sessionIDs the index holds under this
// instance's own key, with at least one message and not already archived.
//
// This is the gate the archive job will not cross: Mjolnir only deletes its
// own copy of a conversation SessionWiki has actually stored. Runs SQLite
// work, so callers on an event loop run it as blocking work.
func IndexedWithMessages(sessionIDs []string) (map[string]struct{}, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs indexed_with_messages")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs row_by_id")
}

// visibleRowByID: explicit IDs obey the same visibility rule as search
// results; a child found by standalone SessionWiki sync is hidden as though
// it were absent.
func visibleRowByID(conn store.Queryer, id string, ownership *topLevelSnapshot, cache *mjimport.NativeScanCache) (*brokksessionwiki.SessionRow, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs visible_row_by_id")
}

func wikiRow(row brokksessionwiki.SessionRow, snippet *string, live map[string]struct{}) client.WikiRow {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs wiki_row")
}

// projectDirectoryOf is the project a restored session should open.
//
// A Mjolnir session runs in a managed worktree under the repository it was
// started from, and that worktree is gone once the session is archived. The
// repository above it is what the user still has, so a worktree path is
// reduced to it. Any other path is used as it stands, and a path that no
// longer exists is left for the caller to replace.
func projectDirectoryOf(project string) (string, bool) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs project_directory_of")
}

// snapshotOf rebuilds an indexed transcript as a canonical snapshot.
//
// The snapshot is only ever read by the compaction pipeline, which wants
// turns: a user message opens a turn and assistant and tool items attach to
// it. Messages before the first user message therefore have nowhere to go
// and are dropped, and a session with no user message at all cannot be
// restored.
func snapshotOf(session *brokksessionwiki.Session) (core.CanonicalSessionSnapshot, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs snapshot_of")
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
	panic("unported: mj-sessionwiki/src/sessionwiki.rs wiki_continuation")
}

// WikiSession is what one indexed session is, as far as continuing it is
// concerned, or nil when the id names none. The daemon serves it for `mj
// resume --wiki` and for `mj sessions --session` when the id names no
// Mjolnir session.
func WikiSession(wikiID string, knownSessions map[string]struct{}) (*client.WikiSessionInfo, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs wiki_session")
}

func wikiSessionFrom(conn store.Queryer, wikiID string, knownSessions map[string]struct{}, ownership *topLevelSnapshot, cache *mjimport.NativeScanCache) (*client.WikiSessionInfo, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki.rs wiki_session_from")
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
