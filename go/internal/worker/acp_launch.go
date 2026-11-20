// Port of mj-worker/src/acp/launch.rs.

package worker

import (
	"context"
	"errors"
	"maps"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"unicode/utf8"
	"weak"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// ClaudeLoginIdentity is one observed identity shared by credential
// installation and prompt dispatch. Sampling supplemental recovery evidence
// never performs filesystem I/O. Share it by pointer: the zero value is ready
// (Rust's Default), and copies of the pointer share the state as Rust's
// clones share their Arc.
type ClaudeLoginIdentity struct {
	state ClaudeLoginState
}

// ClaudeLoginState is the state a ClaudeLoginIdentity shares.
type ClaudeLoginState struct {
	// installation serializes credential installation (Rust's
	// tokio::sync::Mutex<()>).
	installation sync.Mutex
	// observedMu guards the observed (revision, fingerprint) pair.
	observedMu          sync.Mutex
	observedRevision    uint64
	observedFingerprint *string
	fileFingerprintMu   sync.Mutex
	fileFingerprint     *string
}

func (c *ClaudeLoginIdentity) snapshot() (uint64, *string) {
	c.state.observedMu.Lock()
	defer c.state.observedMu.Unlock()
	return c.state.observedRevision, c.state.observedFingerprint
}

func (c *ClaudeLoginIdentity) observe(expected uint64, fingerprint *string) {
	c.state.observedMu.Lock()
	defer c.state.observedMu.Unlock()
	if c.state.observedRevision == expected {
		c.state.observedFingerprint = fingerprint
	}
}

func (c *ClaudeLoginIdentity) installed(fingerprint *string) {
	c.state.observedMu.Lock()
	defer c.state.observedMu.Unlock()
	if c.state.observedRevision == ^uint64(0) {
		panic("Claude login revision exhausted")
	}
	c.state.observedRevision++
	c.state.observedFingerprint = fingerprint
}

func (c *ClaudeLoginIdentity) fileFingerprint() *string {
	c.state.fileFingerprintMu.Lock()
	defer c.state.fileFingerprintMu.Unlock()
	return c.state.fileFingerprint
}

func (c *ClaudeLoginIdentity) observeFile(fingerprint *string) {
	c.state.fileFingerprintMu.Lock()
	defer c.state.fileFingerprintMu.Unlock()
	c.state.fileFingerprint = fingerprint
}

// installation takes the credential installation lock and returns the
// function that releases it (Rust's guard).
func (c *ClaudeLoginIdentity) installation() func() {
	c.state.installation.Lock()
	return c.state.installation.Unlock
}

// fileReadOffRuntime runs a filesystem read on its own goroutine, so a
// canceled caller is not held by a stalled filesystem. Results and panics are
// reported even if the caller has stopped waiting.
func fileReadOffRuntime[T any](ctx context.Context, name string, read func() (T, error)) (T, error) {
	panic("unported: mj-worker/src/acp/launch.rs file_read_off_runtime")
}

func readClaudeLoginFingerprint(ctx context.Context, path string) (*string, error) {
	return fileReadOffRuntime(ctx, "claude-login-read", func() (*string, error) {
		_, bytes, err := core.ReadCredentialFile(core.HarnessKindClaude, path)
		if err != nil {
			return nil, err
		}
		fingerprint, ok := core.ClaudeAccessTokenFingerprint(bytes)
		if !ok {
			return nil, nil
		}
		return &fingerprint, nil
	})
}

// SubagentMcpSocket is the worker's sub-agent socket and the `mj-agents`
// role it backs: a parent delegates through it, a child only hands its
// report back.
type SubagentMcpSocket struct {
	Path                  string
	Role                  core.SubagentMcpRole
	AgentMailboxesEnabled bool
	// SubtleSubagents says whether a parent's spawn tool offers the subtle
	// task type.
	SubtleSubagents bool
	// ReviewTools says whether this launch offers the `mj-review` server
	// beside `mj-agents`.
	ReviewTools bool
	// ProfileRegistration is the actual profile registration outcome.
	// Legacy shared Codex homes use ACP; shared Claude homes skip these
	// tools entirely.
	ProfileRegistration bool
}

// ReviewerMcpServer is one configured MCP server exposed to the reviewer
// harness.
type ReviewerMcpServer struct {
	server core.ReviewMcpServer
}

func NewReviewerMcpServer(server core.ReviewMcpServer) ReviewerMcpServer {
	return ReviewerMcpServer{server: server}
}

// LaunchSpec is everything one ACP runtime needs to start and restart its
// bridge.
type LaunchSpec struct {
	LoginIdentity *ClaudeLoginIdentity
	// OperatorAway is live availability shared with the worker control
	// connection.
	OperatorAway        *OperatorAway
	ClearContextRequest *ContextReset
	// ContextRestore holds the settings to restore when a failed clear
	// reloads the old conversation.
	ContextRestore        *ContextReset
	GoalRecovery          *Shared[core.GoalRecoveryContext]
	Command               string
	Args                  []string
	Environment           map[string]string
	Cwd                   string
	AdditionalDirectories []string
	ProjectMemory         *core.ProjectMemoryLaunchConfig

	// ExtraMCPServers are extra stdio MCP servers this session gets beyond
	// project memory. Reviewers can receive configured servers here; the
	// primary gets none.
	ExtraMCPServers []ReviewerMcpServer
	// SubagentPolicy independently controls native tool suppression.
	SubagentPolicy core.SubagentPolicy
	// ManagedSubagent: this session belongs to another Mjolnir session,
	// independently of its MCP role.
	ManagedSubagent bool
	InstructionRole *core.InstructionRole
	// SubagentMCPSocket is the private Mjolnir socket: parent delegation or
	// child handback.
	SubagentMCPSocket *SubagentMcpSocket
	// BridgeSpecPath is the supervisor spec the bridge command reads, when
	// this launch has one. It is rewritten from AcceptedConfig before every
	// bridge start, so a harness that can only take a selector before it
	// opens a session gets the value this session accepted at *this* launch
	// rather than the one it held when the worker started.
	BridgeSpecPath *string
	ResumeSession  *string
	// NativeSessionHistory is the history evidence for the native thread
	// named by ResumeSession.
	NativeSessionHistory NativeSessionHistory
	// AcceptedConfig holds the accepted selectors for this logical session,
	// shared across native bridge replacements. Workers seed this from their
	// durable relay.
	AcceptedConfig *Shared[core.AcceptedSessionConfig]
	// InitialModel is the model this session was created for, from the
	// launch config. See StartupModel.
	InitialModel *string
	Harness      core.HarnessKind
	// ImportedLogins is, for OpenCode, the staged logins this bridge's launch
	// imported; the idle session reloads once they change. Set when the
	// bridge starts.
	ImportedLogins  *ImportedLogins
	ExecutionPolicy core.ExecutionPolicy
	// ApprovalChoices are the profile's approval choices from the launch
	// config: the Claude permission mode, read only through
	// sessionEnforcement, and whether the Mjolnir guardian reviews, read only
	// through guardianForLaunch.
	ApprovalChoices core.ApprovalChoices
	ACPActivity     core.AcpActivityClock
	// StepClock is when the step the agent is on began. Marked from the same
	// handlers as ACPActivity, but only where a new step actually starts.
	StepClock *core.StepClock
	// ToolsInFlight are the tool calls the agent has open, shared with the
	// durable relay that records them. The turn stall watchdog reads this: a
	// turn blocked in a long tool call is working, however silent the
	// protocol is (#1020).
	ToolsInFlight *core.ToolsInFlight
	// TerminalOwner: primary workers admit terminals and native resumes
	// through the cut owner. Scratch ACP connections have no relay and leave
	// this nil; a non-nil pointer whose value is gone means the owner went
	// away.
	TerminalOwner *weak.Pointer[RelayCell]
	TurnContext   *transcript.TurnContext
	// Verdict is retained for source compatibility. Active ACP turns never
	// consult Jev; completed-turn classification belongs to the durable relay
	// coordinator.
	Verdict     VerdictSource
	StallPolicy *core.StallPolicy
}

// Clone is LaunchSpec's derived Clone: the copy shares the handles Rust
// shares through an Arc and owns its own collections.
func (s *LaunchSpec) Clone() LaunchSpec {
	clone := *s
	clone.Args = slices.Clone(s.Args)
	clone.Environment = maps.Clone(s.Environment)
	clone.AdditionalDirectories = slices.Clone(s.AdditionalDirectories)
	clone.ExtraMCPServers = slices.Clone(s.ExtraMCPServers)
	return clone
}

// StartupModel is the model the harness opens its session on: the one this
// session accepted, else the one it was created for.
//
// A harness that can only take its model before the session opens (Codex
// through `CODEX_CONFIG`, Claude through `session/new`) otherwise starts a
// new session on the profile default and is switched before its first turn.
// Codex prewarms its Responses connection with the default, and a
// sub-agent's first request for another model over that connection was
// refused (issue 1217). The accepted model wins as soon as there is one, so
// a model chosen later is never replaced by the one the session was created
// with.
func (s *LaunchSpec) StartupModel() *string {
	accepted, unlock := s.AcceptedConfig.Lock()
	defer unlock()
	model := accepted.Model
	if model == nil {
		model = s.InitialModel
	}
	if model == nil {
		return nil
	}
	copied := *model
	return &copied
}

// memoryMCPServer is the project-history server's name.
const memoryMCPServer = "mj-memory"

func projectHistoryMCP(spec *LaunchSpec) []schema.McpServer {
	panic("unported: mj-worker/src/acp/launch.rs project_history_mcp")
}

func sessionRequestMeta(spec *LaunchSpec, includeHistoryTools bool) schema.Meta {
	panic("unported: mj-worker/src/acp/launch.rs session_request_meta")
}

// extraMCP is the reviewer's configured MCP servers, for harnesses that
// accept a server over ACP. Claude and Kimi read their staged profile
// instead, which the controller writes while staging the reviewer.
func extraMCP(spec *LaunchSpec) []schema.McpServer {
	panic("unported: mj-worker/src/acp/launch.rs extra_mcp")
}

// subagentACPMCP is the `mj-agents` server, when this launch delivers it
// over ACP. The worker decides delivery after checking profile ownership.
// Legacy shared Codex homes keep ACP delivery until the controller restages
// them.
func subagentACPMCP(spec *LaunchSpec) *schema.McpServer {
	panic("unported: mj-worker/src/acp/launch.rs subagent_acp_mcp")
}

// reviewACPMCP is the `mj-review` server, when this launch offers it and
// delivers it over ACP, on the same terms as `mj-agents`.
func reviewACPMCP(spec *LaunchSpec) *schema.McpServer {
	panic("unported: mj-worker/src/acp/launch.rs review_acp_mcp")
}

// ownedMcpServerTools is one Mjolnir-owned MCP server and the tools it lists.
type ownedMcpServerTools struct {
	server string
	tools  []string
}

// ownedACPMCPTools is the Mjolnir-owned MCP servers this launch registers
// over ACP, each with the tools it lists: project history, delegation and
// review. Reviewer servers a person configured are not Mjolnir's, so they
// are never in this list.
func ownedACPMCPTools(spec *LaunchSpec) []ownedMcpServerTools {
	panic("unported: mj-worker/src/acp/launch.rs owned_acp_mcp_tools")
}

// approveOwnedMCP: only owned registrations opt in; yolo and other
// harnesses stay unchanged.
func approveOwnedMCP(spec *LaunchSpec, server schema.McpServerStdio) schema.McpServerStdio {
	if spec.Harness == core.HarnessKindCodex && spec.ExecutionPolicy == core.ExecutionPolicyConfiguredApprovals {
		server.Meta = jsonvalue.Object{{
			Key:   "codex",
			Value: jsonvalue.Object{{Key: "defaultToolsApprovalMode", Value: "approve"}},
		}}
	}
	return server
}

// antigravityProjectID is the project agy runs every ACP conversation in
// (its log says "Conversation using project ID: default-cli-project"). agy
// reads that project's permission grants from `config/projects/<id>.json`
// under its `--gemini-dir`.
const antigravityProjectID = "default-cli-project"

// ownedMCPServers is every server name Mjolnir may register, so a grant left
// by an earlier launch for a server this launch no longer registers is
// removed.
var ownedMCPServers = [3]string{
	memoryMCPServer,
	core.SubagentMCPServer,
	core.ReviewMCPServer,
}

// antigravityGhGrant is agy's rule for commands whose first word is `gh`. Its
// Auto mode sends a `gh` call that leaves the sandbox to its reviewer, and
// the worker grants the command so an agent's GitHub work does not interrupt
// a person. agy keeps a person's own `deny` rules above this one.
const antigravityGhGrant = "command(gh)"

// stageAntigravityGrants trusts the Mjolnir-owned MCP servers this launch
// registered over ACP, for agy in its guardian (Auto) mode, which asks a
// person before every MCP tool call. agy names a call `mcp_<server>_<tool>`
// and the request says nothing more, so the title cannot tell
// `mj-agents`/`send_message` from a foreign server `mj-agents_send`/`message`.
// agy's own grant rule `mcp(<server>/*)` names the server, and agy matches it
// against the server that owns the tool, so the lookalike still asks (checked
// against agy 1.5.11; `tests/e2e/antigravity_owned_mcp_grants.py` repeats the
// check). That is the whole mechanism: the worker answers none of agy's
// permission requests for these servers. Claude's allowed tools and Codex's
// server approval mode do the same for those harnesses.
//
// Also grant reads below the home directory agy sees in its launch
// environment. A trailing directory separator allows nested files and keeps
// sibling paths outside the grant (checked against agy 1.5.11). This grants
// no writes or commands; agy keeps applying its Auto-mode policy to those
// tools. One command grant is staged: `command(gh)`, Mjolnir's GitHub CLI
// approval, so an agent's issue and pull-request work reaches no person. A
// person's own `deny` rules stay above it.
//
// Unconstrained targets run agy with `--dangerously-skip-permissions`, which
// already approves every call.
func stageAntigravityGrants(spec *LaunchSpec) error {
	panic("unported: mj-worker/src/acp/launch.rs stage_antigravity_grants")
}

// homeReadRule is agy's rule for reads below userHome. The trailing
// separator keeps same-prefix siblings such as `/home/al2` outside a grant
// for `/home/al`.
func homeReadRule(userHome string) (string, error) {
	directory := userHome
	if !strings.HasSuffix(directory, string(filepath.Separator)) {
		directory += string(filepath.Separator)
	}
	if !utf8.ValidString(directory) {
		return "", errors.New("Antigravity home is not UTF-8")
	}
	return "read_file(" + directory + ")", nil
}

// withAntigravityGrants is agy's project file with `mcp(<server>/*)` allowed
// for servers and for no other Mjolnir server, plus readRule and none of the
// earlier stagedReads. Other project entries and grants are preserved.
// existing is the decoded project file, nil when there is none (a non-nil
// pointer to nil is a file holding JSON null).
func withAntigravityGrants(existing *any, servers []string, readRule string, stagedReads []string) ([]byte, error) {
	panic("unported: mj-worker/src/acp/launch.rs with_antigravity_grants")
}

func sessionMCP(spec *LaunchSpec, includeHistoryTools bool) []schema.McpServer {
	servers := extraMCP(spec)
	if includeHistoryTools {
		servers = append(servers, projectHistoryMCP(spec)...)
	}
	return servers
}

// toolCacheDirectoryVariables are the environment variables that name
// Mjolnir's build-tool cache directories. The worker adds these to the
// session's additional directories, and the notice for directories a harness
// cannot take uses them to say what was dropped.
var toolCacheDirectoryVariables = [3]string{"MJ_TOOL_CACHE_DIR", "MJ_NX_HOME_DIR", "MJ_TOOL_CACHE_HOME"}

// SessionDirectories is the additional directories one ACP connection can
// take, and what could not be passed.
//
// An agent that accepts `additionalDirectories` says so in
// `sessionCapabilities`. One that does not rejects a non-empty list and
// fails session creation (Grok Build: "a session works only in its cwd",
// #1433), so the list is withheld from it and the session starts with its
// working directory alone.
type SessionDirectories struct {
	Accepted []string
	// Notice is set when directories were withheld: what the person should
	// know.
	Notice *string
}

func sessionDirectoriesNegotiate(spec *LaunchSpec, capabilities *schema.SessionCapabilities) SessionDirectories {
	panic("unported: mj-worker/src/acp/launch.rs SessionDirectories::negotiate")
}

// SystemPromptAppend keeps vendor prompt shapes at the ACP request boundary.
type SystemPromptAppend interface {
	isSystemPromptAppend()
}

type SystemPromptAppendStandard struct {
	Value schema.SystemPrompt
}

type SystemPromptAppendAntigravity struct {
	Value string
}

func (SystemPromptAppendStandard) isSystemPromptAppend()    {}
func (SystemPromptAppendAntigravity) isSystemPromptAppend() {}

func systemPromptParameters(meta schema.Meta, prompt SystemPromptAppend) (schema.Meta, *schema.SystemPrompt) {
	panic("unported: mj-worker/src/acp/launch.rs system_prompt_parameters")
}

func newSessionRequest(spec *LaunchSpec, directories []string, includeHistoryTools bool, systemPrompt SystemPromptAppend) schema.NewSessionRequest {
	meta, prompt := systemPromptParameters(sessionRequestMeta(spec, includeHistoryTools), systemPrompt)
	return schema.NewSessionRequest{
		Cwd:                   spec.Cwd,
		AdditionalDirectories: slices.Clone(directories),
		MCPServers:            sessionMCP(spec, includeHistoryTools),
		SystemPrompt:          prompt,
		Meta:                  meta,
	}
}

func loadSessionRequest(spec *LaunchSpec, directories []string, sessionID schema.SessionId, systemPrompt SystemPromptAppend) schema.LoadSessionRequest {
	meta, prompt := systemPromptParameters(sessionRequestMeta(spec, true), systemPrompt)
	return schema.LoadSessionRequest{
		SessionID:             sessionID,
		Cwd:                   spec.Cwd,
		AdditionalDirectories: slices.Clone(directories),
		// The servers Mjolnir owns travel on every launch, because a bridge
		// that opens the session again is a new harness process: Codex builds
		// the resumed thread's MCP set from this request and recovers nothing
		// it was not given (#1085). Replay filtering belongs to the
		// notification handler; omitting history here removes its tools after
		// restart.
		MCPServers:   sessionMCP(spec, true),
		SystemPrompt: prompt,
		Meta:         meta,
	}
}

func resumeSessionRequest(spec *LaunchSpec, directories []string, sessionID schema.SessionId, systemPrompt SystemPromptAppend) schema.ResumeSessionRequest {
	meta, prompt := systemPromptParameters(sessionRequestMeta(spec, true), systemPrompt)
	return schema.ResumeSessionRequest{
		SessionID:             sessionID,
		Cwd:                   spec.Cwd,
		AdditionalDirectories: slices.Clone(directories),
		MCPServers:            sessionMCP(spec, true),
		SystemPrompt:          prompt,
		Meta:                  meta,
	}
}

// ContextReset is the settings captured from the old native conversation
// before retiring it.
type ContextReset struct {
	RequestID string
	Selectors [][2]string
	Mode      *string
}
