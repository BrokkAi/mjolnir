// Port of mj-worker/src/worker_runtime/subagents.rs.

// Parent-worker queue behind the Mjolnir sub-agent MCP server.

//go:build unix

package worker

import (
	"context"
	"errors"
	"io/fs"
	"maps"
	"net"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SubagentSocket is the worker root's sub-agent MCP socket.
const SubagentSocket = "subagents.sock"

// opencodeMailboxPlugin is the worker root directory holding the OpenCode
// mailbox plugin.
const opencodeMailboxPlugin = "opencode-mailbox"

// configureOpencodeMailboxHook registers a worker-owned OpenCode 2 plugin
// without editing the user's home.
func configureOpencodeMailboxHook(root string, environment map[string]string) error {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs configure_opencode_mailbox_hook")
}

// subagentsCodexConfigEnv is subagents.rs's own CODEX_CONFIG_ENV; it has the
// same value as codexConfigEnv.
const subagentsCodexConfigEnv = "CODEX_CONFIG"
const codexMailboxHookMarker = "MJOLNIR_MAILBOX_HOOK=1 "
const claudeMailboxHookMarker = "MJOLNIR_MAILBOX_HOOK=1 "

// configureClaudeSubagentMCP registers this launch's `mj-agents` server, and
// the `mj-review` server when the review gate is on, in the session's
// private Claude profile, and allows every tool they list, before Claude
// starts.
//
// `mj-agents` is always loaded. `mj-review` has no `alwaysLoad`, so Claude
// defers its tools until they are needed at push time. A review server the
// launch no longer offers is removed.
//
// The worker owns this registration, as it owns Codex's, and writes it on
// every start. A profile is staged once, but a worker upgrade or recovery
// replaces the launch configuration and binary without restaging it, so a
// registration written only at staging went stale: a profile staged before
// top-level sessions could message each other never got `send_message`.
//
// Paths are absolute on the target, so Claude can launch the server from the
// checkout. Without a role the server is removed. Rules the person already
// has are kept. A home that is not a private staged directory may be the
// person's own, so it is never edited: a delegating parent or a child cannot
// start without its tools there, while a message-only session starts without
// `send_message`. Returns whether the role servers were registered.
func configureClaudeSubagentMCP(root string, sessionID string, home string, role *core.SubagentMcpRole, agentMailboxesEnabled bool, subtleSubagents bool, reviewTools bool) (bool, error) {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs configure_claude_subagent_mcp")
}

// configureKimiSubagentMCP binds stdio servers to a runtime through Kimi's
// private MCP profile.
func configureKimiSubagentMCP(root string, sessionID string, home string, role core.SubagentMcpRole, reviewTools bool) error {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs configure_kimi_subagent_mcp")
}

// editProfileJSON rewrites a staged profile's JSON file, writing only when
// its content changes. A missing file starts as an empty object. The object
// keeps its member order, as serde_json's preserve_order map does.
func editProfileJSON(path string, label string, edit func(object *jsonvalue.Object) error) error {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs edit_profile_json")
}

// privateStagedHome reports whether `home` is a profile this session staged
// for itself: `<root>/profile` on this machine, `profiles/<session>` in a
// container or under a remote login (the controller's
// `removable_profile_root`). An earlier local session's `<root>/profile` may
// link to the person's own home, so each directory is checked without
// following links. Only the local profile's parent and root are resolved for
// comparison, allowing symlinked ancestors without accepting a linked
// profile, profiles, or root directory.
func privateStagedHome(root string, sessionID string, home string) bool {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs private_staged_home")
}

// regularFileOrAbsent reports whether a profile file may be edited in place:
// only when it is a regular file or does not exist yet; a link may lead out
// of the private profile.
func regularFileOrAbsent(path string) bool {
	info, err := os.Lstat(path)
	if err != nil {
		return errors.Is(err, fs.ErrNotExist)
	}
	return info.Mode().IsRegular()
}

// configureClaudeMailboxHook installs the mailbox command hook in the
// session-private Claude settings and resolves staged remote paths on the
// worker target before Claude reads them.
//
// The Claude home is the staged profile directory itself: `<root>/profile`
// on this machine, `profiles/<session>` in a container or under a remote
// login (the controller's `removable_profile_root`).
func configureClaudeMailboxHook(root string, sessionID string, home string) (bool, error) {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs configure_claude_mailbox_hook")
}

// configureClaudeSubagentSettings turns off Claude Code behavior that costs a
// managed child inference, tools or network work while doing nothing for a
// headless coding task (audited against Claude Code 2.1.326 with
// claude-agent-acp 0.108.0). The bridge asks for predicted next prompts,
// which can spend an extra model request on a turn; claude.ai connectors are
// enumerated, connected and listed at startup on an eligible login. The
// built-in Concise output style replaces the default prompt's opening
// narration and closing recaps, and any inherited Explanatory or Learning
// style, while keeping outcomes and decisions in the child's reports.
// Project instructions, skills, plugins and configured MCP servers do not
// depend on either. A child overrides the inherited user settings, which
// still apply to top-level sessions; an explicitly exported
// `CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION` is a deliberate choice and still
// wins.
func configureClaudeSubagentSettings(root string, sessionID string, home string) error {
	path := filepath.Join(home, "settings.json")
	if !(privateStagedHome(root, sessionID, home) && regularFileOrAbsent(path)) {
		return nil
	}
	return editProfileJSON(path, "staged Claude settings", func(settings *jsonvalue.Object) error {
		settings.Set("promptSuggestionEnabled", false)
		settings.Set("disableClaudeAiConnectors", true)
		settings.Set("outputStyle", "Concise")
		return nil
	})
}

// excludeDefaultClaudeInstructions keeps the person's `~/.claude/CLAUDE.md`
// out of the session's project instructions. The session's configuration
// lives in the staged profile, so Claude Code finds that file only as an
// ancestor of any checkout under the home directory: it loads the person's
// instructions a second time as project files, and the project then counts
// as having a CLAUDE.md, so its AGENTS.md is not loaded
// (anthropics/claude-code#80580). `userHome` is the HOME the harness runs
// with. Exclusions the person already has are kept.
func excludeDefaultClaudeInstructions(root string, sessionID string, home string, userHome *string) error {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs exclude_default_claude_instructions")
}

func configureCodexMCPWithMailboxes(root string, sessionID string, home string, role *core.SubagentMcpRole, policy core.ExecutionPolicy, mailboxesEnabled bool, subtleSubagents bool, reviewTools bool, managedSubagent bool) (bool, error) {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs configure_codex_mcp_with_mailboxes")
}

// codexSubagentDisabledFeatures are the Codex features that cost a managed
// child context, tools, network work or startup latency while doing nothing
// for a headless coding task (audited against Codex 0.170.0). `apps` adds
// hosted connector tools and guidance and can hold the first tool catalog on
// a cold connector fetch; `plugins` adds guidance and a background
// marketplace sync, though the staged home carries no plugin cache;
// `remote_plugin` and `tool_suggest` fetch plugin suggestions on the first
// turn and add an install tool; `image_generation` adds a ~500-token tool
// that spends image quota. AGENTS.md injection, staged skills and configured
// MCP servers do not depend on any of these. Flags that add nothing to a
// headless session (browser and computer use, skill search) are left alone.
// A child overrides the inherited user configuration, which still applies to
// top-level sessions; a project configuration or profile session override
// that enables a feature is a deliberate choice for that scope and still
// wins.
var codexSubagentDisabledFeatures = [5]string{
	"apps",
	"plugins",
	"remote_plugin",
	"tool_suggest",
	"image_generation",
}

// disableCodexSubagentFeatures edits a decoded TOML table; it reports
// whether the configuration changed.
func disableCodexSubagentFeatures(config map[string]any, path string) (bool, error) {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs disable_codex_subagent_features")
}

// configureCodexMCPServers returns whether the configuration changed.
func configureCodexMCPServers(config map[string]any, path string, root string, role *core.SubagentMcpRole, policy core.ExecutionPolicy, mailboxesEnabled bool, subtleSubagents bool, reviewTools bool) (bool, error) {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs configure_codex_mcp_servers")
}

func configureCodexProfile(root string, sessionID string, home string, environment map[string]string, role *core.SubagentMcpRole, policy core.ExecutionPolicy, options ParentTools, reviewTools bool, managedSubagent bool) (bool, error) {
	if err := configureCodexMailboxHook(root, environment); err != nil {
		return false, err
	}
	return configureCodexMCPWithMailboxes(root, sessionID, home, role, policy,
		options.AgentMailboxesEnabled, options.SubtleSubagents, reviewTools, managedSubagent)
}

// configureCodexMailboxHook puts the mailbox hook in codex-acp's per-thread
// config override. Its state trusts only this generated handler; using
// Codex's global trust bypass would also enable untrusted handlers from the
// user's and project's config layers.
func configureCodexMailboxHook(root string, environment map[string]string) error {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs configure_codex_mailbox_hook")
}

// codexMailboxHookHash matches Codex 0.159.1's hook identity hash for the
// generated command hook.
func codexMailboxHookHash(command string) (string, error) {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs codex_mailbox_hook_hash")
}

func canonicalizeJSON(value any) any {
	switch value := value.(type) {
	case jsonvalue.Object:
		entries := slices.Clone(value)
		slices.SortStableFunc(entries, func(left, right jsonvalue.Member) int { return strings.Compare(left.Key, right.Key) })
		canonical := jsonvalue.Object{}
		for _, entry := range entries {
			canonical.Set(entry.Key, canonicalizeJSON(entry.Value))
		}
		return canonical
	case []any:
		items := make([]any, len(value))
		for i, item := range value {
			items[i] = canonicalizeJSON(item)
		}
		return items
	}
	return value
}

const subagentQueue = "subagents.json"

const maxSocketTasks = 96
const longSocketTasks = 32
const controlSocketTasks = 32
const socketIODeadline = 2 * time.Second

// socketAdmission is two tokio semaphores: a buffered channel holds one
// token per admitted socket task.
type socketAdmission struct {
	long    chan struct{}
	control chan struct{}
}

func defaultSocketAdmission() *socketAdmission {
	return &socketAdmission{
		long:    make(chan struct{}, longSocketTasks),
		control: make(chan struct{}, controlSocketTasks),
	}
}

// workerReplyMargin is how much sooner than the MCP server gives up this
// worker answers a request. The MCP server waits replyTimeout; answering
// first lets it tell the model what this worker knows (whether the daemon
// has picked the request up) instead of only that nothing came back (#1197).
const workerReplyMargin = 10 * time.Second

// queueState is the durable sub-agent queue (serde deny_unknown_fields; its
// wire form is queueStateWire).
type queueState struct {
	requests           map[string]core.SubagentToolRequest
	results            map[string]core.SubagentToolResult
	completions        map[string]core.SubagentCompletion
	callerAcknowledged map[string]struct{}
	daemonAcknowledged map[string]struct{}
	// legacyDeliveredToWaiter decodes queue files from older workers without
	// using their delivery decisions; it is never written.
	legacyDeliveredToWaiter map[string]bool
	// mutatingAdmissionOpen is protected by the same lock as `requests`,
	// making gate closure atomic with queue admission. Old queue files
	// default to the open state.
	mutatingAdmissionOpen bool
	// collections are the daemon's reads of this queue since this worker
	// started. It lives under the queue's lock so a request is either in a
	// read or after it. Not serialized.
	collections subagentsCollections
	// generation is not serialized.
	generation uint64
	// liveWaiters are waiters currently registered in this worker process.
	// Registration and completion share the queue lock to decide the
	// fallback race once. Not serialized.
	liveWaiters map[string]int
}

// queueStateWire is queueState's serde form, fields in declaration order.
type queueStateWire struct {
	Requests                map[string]core.SubagentToolRequest `json:"requests"`
	Results                 map[string]core.SubagentToolResult  `json:"results"`
	Completions             map[string]core.SubagentCompletion  `json:"completions"`
	CallerAcknowledged      []string                            `json:"caller_acknowledged"`
	DaemonAcknowledged      []string                            `json:"daemon_acknowledged"`
	LegacyDeliveredToWaiter map[string]bool                     `json:"delivered_to_waiter,omitempty"`
	MutatingAdmissionOpen   bool                                `json:"mutating_admission_open"`
}

func defaultQueueState() queueState {
	return queueState{
		requests:                map[string]core.SubagentToolRequest{},
		results:                 map[string]core.SubagentToolResult{},
		completions:             map[string]core.SubagentCompletion{},
		callerAcknowledged:      map[string]struct{}{},
		daemonAcknowledged:      map[string]struct{}{},
		legacyDeliveredToWaiter: map[string]bool{},
		mutatingAdmissionOpen:   true,
		liveWaiters:             map[string]int{},
	}
}

func defaultMutatingAdmission() bool { return true }

// clone is Rust's derive(Clone): every map is copied.
func (s *queueState) clone() queueState {
	return queueState{
		requests:                maps.Clone(s.requests),
		results:                 maps.Clone(s.results),
		completions:             maps.Clone(s.completions),
		callerAcknowledged:      maps.Clone(s.callerAcknowledged),
		daemonAcknowledged:      maps.Clone(s.daemonAcknowledged),
		legacyDeliveredToWaiter: maps.Clone(s.legacyDeliveredToWaiter),
		mutatingAdmissionOpen:   s.mutatingAdmissionOpen,
		collections:             s.collections,
		generation:              s.generation,
		liveWaiters:             maps.Clone(s.liveWaiters),
	}
}

// MarshalJSON writes the serialized fields; delivered_to_waiter is never
// written.
func (s queueState) MarshalJSON() ([]byte, error) {
	wire := queueStateWire{
		Requests:              s.requests,
		Results:               s.results,
		Completions:           s.completions,
		CallerAcknowledged:    subagentsSortedSet(s.callerAcknowledged),
		DaemonAcknowledged:    subagentsSortedSet(s.daemonAcknowledged),
		MutatingAdmissionOpen: s.mutatingAdmissionOpen,
	}
	if wire.Requests == nil {
		wire.Requests = map[string]core.SubagentToolRequest{}
	}
	if wire.Results == nil {
		wire.Results = map[string]core.SubagentToolResult{}
	}
	if wire.Completions == nil {
		wire.Completions = map[string]core.SubagentCompletion{}
	}
	return workerRuntimeMarshalJSON(wire)
}

// UnmarshalJSON decodes a queue file: every field defaults, unknown fields
// are refused, and mutating admission defaults to open.
func (s *queueState) UnmarshalJSON(data []byte) error {
	wire := queueStateWire{MutatingAdmissionOpen: defaultMutatingAdmission()}
	if err := workerRuntimeDecodeStrict(data, &wire); err != nil {
		return err
	}
	state := defaultQueueState()
	maps.Copy(state.requests, wire.Requests)
	maps.Copy(state.results, wire.Results)
	maps.Copy(state.completions, wire.Completions)
	for _, id := range wire.CallerAcknowledged {
		state.callerAcknowledged[id] = struct{}{}
	}
	for _, id := range wire.DaemonAcknowledged {
		state.daemonAcknowledged[id] = struct{}{}
	}
	maps.Copy(state.legacyDeliveredToWaiter, wire.LegacyDeliveredToWaiter)
	state.mutatingAdmissionOpen = wire.MutatingAdmissionOpen
	*s = state
	return nil
}

// subagentsSortedSet is a BTreeSet<String> as serde writes it: a sorted
// array, `[]` when empty.
func subagentsSortedSet(set map[string]struct{}) []string {
	return slices.Sorted(maps.Keys(set))
}

// subagentsCollections is how often and how recently the daemon has read
// the queue.
type subagentsCollections struct {
	count uint64
	last  *time.Time
}

// queueStateCell is Rust's Arc<Mutex<QueueState>>.
type queueStateCell struct {
	mu    sync.Mutex
	state queueState
}

// SubagentEndpoint is the worker side of the sub-agent queue. Copies share
// the queue, as Rust's Clone does.
type SubagentEndpoint struct {
	path         string
	admission    *socketAdmission
	relay        *RelayCell
	subagentKind *core.SubagentKind
	// handbackGate is tokio's Mutex<()>: a channel with room for one holder.
	handbackGate chan struct{}
	state        *queueStateCell
	// completed is woken whenever a result lands, so a socket call awaiting
	// its own request returns the moment the daemon completes it (tokio's
	// Notify::notify_waiters).
	completed *core.Watch[struct{}]
	// generationWatch is the queue's generation, published by every write so
	// a connection waiting for a change (`WaitForChange`) wakes.
	generationWatch *core.Watch[uint64]
}

// SubagentEndpointOpen opens the queue in a worker root, importing results
// an older worker kept inline.
func SubagentEndpointOpen(root string) (*SubagentEndpoint, error) {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs SubagentEndpoint::open")
}

func (e *SubagentEndpoint) checkHandback(ctx context.Context, request *core.SubagentToolRequest) (*core.SubagentToolResult, error) {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs SubagentEndpoint::check_handback")
}

func (e *SubagentEndpoint) cachedResult(requestID string) (*core.SubagentToolResult, error) {
	e.state.mu.Lock()
	defer e.state.mu.Unlock()
	return e.readResult(&e.state.state, requestID)
}

func (e *SubagentEndpoint) registerWaiter(requestID string) *liveWaiter {
	e.state.mu.Lock()
	defer e.state.mu.Unlock()
	if e.state.state.liveWaiters == nil {
		e.state.state.liveWaiters = map[string]int{}
	}
	e.state.state.liveWaiters[requestID]++
	return &liveWaiter{cell: e.state, requestID: requestID, active: true}
}

func (e *SubagentEndpoint) awaitResultRegistered(ctx context.Context, requestID string, deadline time.Time, waiter *liveWaiter) (*core.SubagentToolResult, error) {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs SubagentEndpoint::await_result_registered")
}

// SetMutatingAdmission serializes an admission change with every enqueue
// and persists it before reporting success. A request that raced the close
// is either already in `requests` for the controller to drain, or is
// rejected by that enqueue.
func (e *SubagentEndpoint) SetMutatingAdmission(open bool) error {
	e.state.mu.Lock()
	defer e.state.mu.Unlock()
	if e.state.state.mutatingAdmissionOpen == open {
		return nil
	}
	next := e.state.state.clone()
	next.mutatingAdmissionOpen = open
	if err := e.persist(&next); err != nil {
		return err
	}
	e.state.state = next
	return nil
}

// daemonContact reports whether the daemon has read the queue after the read
// numbered `mark`, and how long ago it last read it.
func (e *SubagentEndpoint) daemonContact(mark uint64) DaemonContact {
	e.state.mu.Lock()
	collections := e.state.state.collections
	e.state.mu.Unlock()
	contact := DaemonContact{PickedUp: collections.count > mark}
	if collections.last != nil {
		seconds := uint64(time.Since(*collections.last) / time.Second)
		contact.LastCollectedSecondsAgo = &seconds
	}
	return contact
}

// enqueueMarkedAtOrigin queues `request`. It also returns whether the
// request was accepted, any result already recorded for it, and the number
// of daemon reads that happened before it joined the queue.
func (e *SubagentEndpoint) enqueueMarkedAtOrigin(request core.SubagentToolRequest, capturedOrigin *string) (bool, *core.SubagentToolResult, uint64, error) {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs SubagentEndpoint::enqueue_marked_at_origin")
}

// Complete records the daemon's result for a request; it reports whether a
// live waiter in this worker receives it.
func (e *SubagentEndpoint) Complete(result core.SubagentToolResult) (bool, error) {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs SubagentEndpoint::complete")
}

func (e *SubagentEndpoint) storeResult(state *queueState, result core.SubagentToolResult) error {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs SubagentEndpoint::store_result")
}

func (e *SubagentEndpoint) persist(state *queueState) error {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs SubagentEndpoint::persist")
}

// generations changes after every write to the queue.
func (e *SubagentEndpoint) generations() *core.Watch[uint64] {
	return e.generationWatch
}

// liveWaiter is one waiter registered in queueState.liveWaiters. Rust's Drop
// is finish; callers defer it.
type liveWaiter struct {
	cell      *queueStateCell
	requestID string
	active    bool
}

// finishAndTakeResult ends the waiter and inspects its result under the same
// lock used by Complete, so timeout and completion have one serialized
// winner.
func (w *liveWaiter) finishAndTakeResult(endpoint *SubagentEndpoint) (*core.SubagentToolResult, error) {
	w.cell.mu.Lock()
	defer w.cell.mu.Unlock()
	liveWaiterRemoveFrom(&w.active, w.requestID, &w.cell.state)
	return endpoint.readResult(&w.cell.state, w.requestID)
}

func (w *liveWaiter) finish() {
	w.cell.mu.Lock()
	defer w.cell.mu.Unlock()
	liveWaiterRemoveFrom(&w.active, w.requestID, &w.cell.state)
}

func liveWaiterRemoveFrom(active *bool, requestID string, state *queueState) {
	if !*active {
		return
	}
	if waiters, ok := state.liveWaiters[requestID]; ok {
		if waiters <= 1 {
			delete(state.liveWaiters, requestID)
		} else {
			state.liveWaiters[requestID] = waiters - 1
		}
	}
	*active = false
}

// socketReply is the worker's answer on the sub-agent socket (serde
// deny_unknown_fields).
type socketReply struct {
	Accepted               bool                     `json:"accepted"`
	AcknowledgmentRequired bool                     `json:"acknowledgment_required"`
	Result                 *core.SubagentToolResult `json:"result,omitempty"`
	// Daemon is set only when Result is absent.
	Daemon *DaemonContact `json:"daemon,omitempty"`
}

// UnmarshalJSON requires `accepted` and refuses unknown fields.
func (r *socketReply) UnmarshalJSON(data []byte) error {
	type plain socketReply
	var value plain
	if err := workerRuntimeDecodeStrict(data, &value, "accepted"); err != nil {
		return err
	}
	*r = socketReply(value)
	return nil
}

// subagentsServe binds the sub-agent socket in `root` and serves it until
// the returned guard is released.
func subagentsServe(runtime *core.Runtime, root string, relay *RelayCell, harness core.HarnessKind, subagentKind *core.SubagentKind) (*SubagentEndpoint, *subagentSocketGuard, error) {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs serve")
}

// subagentSocketGuard stops the socket's accept loop and removes the socket
// when released (Rust's Drop).
type subagentSocketGuard struct {
	cancel context.CancelFunc
	task   *core.JoinHandle[struct{}]
	socket *socketGuard
}

// Release is Rust's Drop: it aborts the accept loop and removes the socket.
func (g *subagentSocketGuard) Release() {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs SubagentSocketGuard::drop")
}

func subagentsServeOne(ctx context.Context, stream net.Conn, endpoint *SubagentEndpoint) error {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs serve_one")
}

func subagentsWriteReply(ctx context.Context, write net.Conn, reply socketReply) error {
	panic("unported: mj-worker/src/worker_runtime/subagents.rs write_reply")
}
