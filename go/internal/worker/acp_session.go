// Port of mj-worker/src/acp/session.rs.

package worker

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"slices"
	"strconv"
	"strings"
	"sync/atomic"
	"time"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"
	acpunstable "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// promptAnswer is a `session/prompt` response as the unstable schema reads
// it: with the turn's token usage, which the stable v1 response leaves out
// (Rust's `unstable_end_turn_token_usage`).
type promptAnswer = acpunstable.PromptResponse

// acpUnstableContentBlock is a prompt block as the attachment store reads
// it.
type acpUnstableContentBlock = acpunstable.ContentBlock

// startupRequestError keeps evidence from the ACP response before the
// worker records a text error.
func startupRequestError(err *acp.RPCError, step string) error {
	diagnostic := core.TurnDiagnosticFromACP(err)
	authFailure := !diagnostic.IsUsageLimit() &&
		(err.Code == acp.ErrorCodeAuthRequired || core.TurnDiagnosticReportsAuthFailure(&diagnostic))
	if authFailure {
		return fmt.Errorf("%s (%s): %w", step, core.PromptAuthRequiredMarker, acpDisplayError{err})
	}
	return fmt.Errorf("%s: %w", step, acpDisplayError{err})
}

// clientName is the name the worker gives an agent as its ACP client. It is
// the name of the package that ships the worker program, which this library
// was once part of; agents and their logs have always seen this name.
const clientName = "brokk-mj-worker"

// recordPromptUse records possible native history only when the prompt is
// ready to be sent.
func recordPromptUse(ctx context.Context, opened *Shared[*OpenedSession], events chan<- core.RuntimeEvent) error {
	// Mark before sending: even a failed reply cannot prove the agent did
	// not receive and persist this prompt.
	firstUse := false
	session, unlock := opened.Lock()
	if *session != nil {
		firstUse = !(*session).nativeSessionUsed.Swap(true)
	}
	unlock()
	// Persist the same fact durably, once, so a restart after this prompt
	// never treats the thread as an empty one to replace.
	if firstUse {
		return emitRuntimeEvent(ctx, events, core.RuntimeEventNativeSessionUsed{})
	}
	return nil
}

// acceptLiveHistory marks the boundary between a provider's replay of old
// history and the live history of this connection. Loading suppresses
// conversation replay, while current session metadata always passes. Every
// open path marks this boundary before accepting new conversation content.
func acceptLiveHistory(liveHistoryEnabled *atomic.Bool) {
	liveHistoryEnabled.Store(true)
}

// loadAtReplayBoundary sends `session/load` and decides the replay boundary
// where its response sits in the connection's incoming message order.
//
// The SDK's dispatch loop hands the next notification to its handler as soon
// as the response has been routed. A bridge that sends live updates right
// behind the response (Claude Code cycles that ran before the first prompt,
// for example) would therefore reach the notification handler before this
// session task resumed and flipped a flag. An ordered result callback runs
// inside the dispatch loop and holds it, so `ReplayCommit` and the flag
// change happen before any later notification is handled and every earlier
// one was handled as replay. The notification handler reads the flag; this
// callback is its only writer for `session/load`. A failed load decides
// nothing: the caller owns that error path. boundary is the connection's
// transport tap, which runs the callback in that order (responseTap).
func loadAtReplayBoundary(ctx context.Context, connection *acp.Connection, request schema.LoadSessionRequest, events chan<- core.RuntimeEvent, nativeChildren bool, liveHistoryEnabled *atomic.Bool, boundary *responseTap) (schema.LoadSessionResponse, error) {
	decided := make(chan error, 1)
	boundary.expect("session/load", func(success bool) {
		if success {
			if nativeChildren && emitRuntimeEvent(ctx, events, core.RuntimeEventNativeAgent{Event: core.NativeAgentEventReplayCommit{}}) != nil {
				// Give the session task the real cause; a dropped answer
				// would read as the connection ending.
				decided <- acpDisplayError{relayEventChannelError()}
				return
			}
			acceptLiveHistory(liveHistoryEnabled)
		}
		decided <- nil
	})
	var response schema.LoadSessionResponse
	err := connection.Call(context.WithoutCancel(ctx), "session/load", request, &response)
	var answered *acp.RPCError
	if err == nil || errors.As(err, &answered) {
		// The answer came, so the callback runs before the reader takes the
		// next message.
		select {
		case failure := <-decided:
			if failure != nil {
				return schema.LoadSessionResponse{}, failure
			}
		case <-connection.Done():
			if err == nil {
				return schema.LoadSessionResponse{}, acpDisplayError{acpInternalError(
					"ACP connection ended before the session/load response was delivered")}
			}
		}
	}
	return response, acpCallError(err)
}

// opencodeToolRegistrySettle is how long after a session opens OpenCode
// needs before its tool registry has reloaded for the MCP servers it was
// given: its 100 ms debounce, doubled and rounded up.
const opencodeToolRegistrySettle = 250 * time.Millisecond

// opencodeMayMissFirstPromptTools says whether an OpenCode that named itself
// agentVersion in `initialize` may still start a turn before its MCP tools
// are registered. OpenCode 2.2.6 and later wait for starting MCP servers
// before each model call (anomalyco/opencode#60490); an older one, such as
// the 2.2.1 in a container built from an older image, does not. A missing or
// unparseable version takes the safe side and counts as older.
func opencodeMayMissFirstPromptTools(agentVersion *string) bool {
	fixedIn := [3]uint64{2, 2, 6}
	if agentVersion == nil {
		return true
	}
	parts := strings.SplitN(strings.TrimLeft(*agentVersion, "v"), ".", 3)
	if len(parts) != 3 {
		return true
	}
	var version [3]uint64
	for i, part := range parts {
		if i == 2 {
			// The patch number ends at a pre-release or build suffix.
			if end := strings.IndexAny(part, "-+"); end >= 0 {
				part = part[:end]
			}
		}
		number, ok := rustParseU64(part)
		if !ok {
			return true
		}
		version[i] = number
	}
	return slices.Compare(version[:], fixedIn[:]) < 0
}

// rustParseU64 is Rust's `str::parse::<u64>`: digits with an optional
// leading `+`.
func rustParseU64(text string) (uint64, bool) {
	number, err := strconv.ParseUint(strings.TrimPrefix(text, "+"), 10, 64)
	return number, err == nil
}

// jsonPath is serde_json's chained `get`: the member under keys, through
// nested objects; nil when any is missing.
func jsonPath(value any, keys ...string) any {
	for _, key := range keys {
		object, ok := value.(jsonvalue.Object)
		if !ok {
			return nil
		}
		member, ok := object.Get(key)
		if !ok {
			return nil
		}
		value = member
	}
	return value
}

// jsonArrayContains says whether value is an array holding the string text.
func jsonArrayContains(value any, text string) bool {
	items, ok := value.([]any)
	return ok && slices.ContainsFunc(items, func(item any) bool { return item == text })
}

// cloneConfigOptions is a deep copy of a session's config options, so an
// event keeps the options as they were when it was emitted.
func cloneConfigOptions(options []schema.SessionConfigOption) []schema.SessionConfigOption {
	if options == nil {
		return []schema.SessionConfigOption{}
	}
	encoded, err := json.Marshal(options)
	if err != nil {
		panic(fmt.Sprintf("session config options are serializable: %v", err))
	}
	var copied []schema.SessionConfigOption
	if err := json.Unmarshal(encoded, &copied); err != nil {
		panic(fmt.Sprintf("session config options round-trip: %v", err))
	}
	if copied == nil {
		copied = []schema.SessionConfigOption{}
	}
	return copied
}

// cloneModes is a deep copy of a session's mode state; nil stays nil.
func cloneModes(modes *schema.SessionModeState) *schema.SessionModeState {
	if modes == nil {
		return nil
	}
	encoded, err := json.Marshal(modes)
	if err != nil {
		panic(fmt.Sprintf("session modes are serializable: %v", err))
	}
	var copied schema.SessionModeState
	if err := json.Unmarshal(encoded, &copied); err != nil {
		panic(fmt.Sprintf("session modes round-trip: %v", err))
	}
	return &copied
}

// stopReasonDebug is Rust's `{:?}` of a StopReason: its variant name.
func stopReasonDebug(reason schema.StopReason) string {
	var name strings.Builder
	for _, word := range strings.Split(string(reason), "_") {
		if word == "" {
			continue
		}
		name.WriteString(strings.ToUpper(word[:1]) + word[1:])
	}
	return name.String()
}

// servedSession is the state serveSession's command loop owns.
type servedSession struct {
	ctx                context.Context
	connection         *acp.Connection
	spec               *LaunchSpec
	requests           <-chan CommandRequest
	events             chan<- core.RuntimeEvent
	terminals          *TerminalRegistry
	pending            *PendingElicitations
	planSlot           *PlanImplementationSlot
	opened             *Shared[*OpenedSession]
	agentOutputCount   *AgentOutputCount
	lastAgentMessage   *LastAgentMessage
	grokUsage          *Collector
	sessionID          schema.SessionId
	configOptions      []schema.SessionConfigOption
	modes              *schema.SessionModeState
	grokModels         *core.GrokModelState
	enforcement        *core.ExecutionEnforcement
	steeringSupported  bool
	opencodeWholeTurns bool
	// antigravityWholeTurns: this Antigravity's prompt responses report the
	// whole turn once verified.
	antigravityWholeTurns bool
	firstPromptNotBefore  *time.Time
	goalQuestion          *Question
	goalControls          *PendingControls
	configRecovery        *CommandLoopQuestion
}

func (s *servedSession) emit(event core.RuntimeEvent) error {
	return emitRuntimeEvent(s.ctx, s.events, event)
}

func (s *servedSession) reject(reason core.OutcomeReason, requestID, message string) error {
	return s.emit(core.RuntimeEventCommandRejected{Reason: reason, RequestID: requestID, Message: message})
}

func (s *servedSession) sessionConfigured() error {
	return s.emit(core.RuntimeEventSessionConfigured{ConfigOptions: cloneConfigOptions(s.configOptions)})
}

func (s *servedSession) sessionModesConfigured() error {
	return s.emit(core.RuntimeEventSessionModesConfigured{Modes: cloneModes(s.modes)})
}

func serveSession(ctx context.Context, connection *acp.Connection, spec *LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, terminals *TerminalRegistry, pendingElicitations *PendingElicitations, planImplementationSlot *PlanImplementationSlot, opened *Shared[*OpenedSession], agentOutputCount *AgentOutputCount, lastAgentMessage *LastAgentMessage, liveHistoryEnabled *atomic.Bool, nativeSessionUsed *atomic.Bool, sessionOpen BridgeSessionOpen, grokUsage *Collector, boundary *responseTap) (SessionRestart, error) {
	emit := func(event core.RuntimeEvent) error { return emitRuntimeEvent(ctx, events, event) }
	harness := spec.Harness
	meta := jsonvalue.Object{
		{Key: "terminal_output", Value: true},
		{Key: "terminal_output_delta", Value: true},
	}
	if harness == core.HarnessKindCodex {
		meta.Set("execution", jsonvalue.Object{{Key: "version", Value: int64(1)}})
	}
	airHarness := harness == core.HarnessKindClaude || harness == core.HarnessKindCodex
	if airHarness {
		airCapabilities := []any{"asyncTasks", "nativeSubagentSessions", "sessionFailure"}
		if harness == core.HarnessKindClaude {
			airCapabilities = append(airCapabilities, "backgroundSubagents")
		}
		meta.Set("jetbrains", jsonvalue.Object{{Key: "air", Value: jsonvalue.Object{
			{Key: "version", Value: int64(1)},
			// `sessionFailure` makes adapters report a failed turn with a
			// typed record instead of an ordinary end of turn whose error is
			// agent text (issue 1217).
			{Key: "capabilities", Value: airCapabilities},
		}}})
	}
	// Kimi routes every shell call through the client's terminal surface and
	// has no local fallback, so this capability is what makes Bash work.
	terminal, readFiles, writeFiles := true, false, false
	capabilities := schema.ClientCapabilities{
		Fs:          &schema.FileSystemCapabilities{ReadTextFile: &readFiles, WriteTextFile: &writeFiles},
		Terminal:    &terminal,
		Elicitation: &schema.ElicitationCapabilities{Form: &schema.ElicitationFormCapabilities{}},
		Meta:        meta,
	}
	title := "Mjolnir"
	var initialized schema.InitializeResponse
	initializeErr := connection.Call(context.WithoutCancel(ctx), "initialize", schema.InitializeRequest{
		ProtocolVersion:    schema.ProtocolVersion(1),
		ClientCapabilities: &capabilities,
		ClientInfo:         &schema.Implementation{Name: clientName, Title: &title, Version: core.PackageVersion},
	}, &initialized)
	spec.ACPActivity.Mark()
	if initializeErr != nil {
		return nil, startupRequestError(acpRPCError(initializeErr), "initialize ACP bridge")
	}
	if initialized.ProtocolVersion != schema.ProtocolVersion(1) {
		return nil, fmt.Errorf("ACP bridge negotiated unsupported protocol ProtocolVersion(%d)", initialized.ProtocolVersion)
	}
	agentCapabilities := initialized.AgentCapabilities
	if agentCapabilities == nil {
		agentCapabilities = &schema.AgentCapabilities{}
	}
	var agentVersion *string
	if initialized.AgentInfo != nil {
		version := initialized.AgentInfo.Version
		agentVersion = &version
	}
	advertisedAirCapabilities := jsonPath(initialized.Meta, "jetbrains", "air", "capabilities")
	nativeChildrenSupported := jsonArrayContains(advertisedAirCapabilities, "nativeSubagentSessions")
	backgroundSubagentsSupported := jsonArrayContains(advertisedAirCapabilities, "backgroundSubagents")
	steeringSupported := steeringSupportedFromMeta(initialized.Meta)
	availabilitySupported := jsonPath(initialized.Meta, "nativeSubagentAvailability", "supported") == true

	opencodeWholeTurns := harness == core.HarnessKindOpenCode && ReportsWholeTurns(agentVersion)
	antigravityWholeTurns := harness == core.HarnessKindAntigravity && AntigravityUsageReportsWholeTurns(agentVersion)

	// Grok Build publishes its catalogue here rather than as `configOptions`.
	var grokModels *core.GrokModelState
	if harness == core.HarnessKindGrok {
		grokModels = core.GrokModelStateOf(initialized.Meta)
	}
	connected := core.RuntimeEventConnected{
		ProtocolVersion:   &initialized.ProtocolVersion,
		Capabilities:      agentCapabilities,
		AgentInfo:         initialized.AgentInfo,
		SteeringSupported: &steeringSupported,
	}
	if initialized.AgentInfo != nil {
		name := initialized.AgentInfo.Name
		connected.AgentName = &name
		connected.AgentVersion = agentVersion
	}
	if err := emit(connected); err != nil {
		return nil, err
	}

	mcpHTTP := agentCapabilities.MCPCapabilities != nil && agentCapabilities.MCPCapabilities.HTTP != nil &&
		*agentCapabilities.MCPCapabilities.HTTP
	mcpAvailable := !harness.MCPServersNeedAdvertisedHTTP() || mcpHTTP
	instructions, err := compose(ctx, spec, mcpAvailable, events)
	if err != nil {
		return nil, err
	}
	systemPrompt, err := deliver(ctx, spec, agentCapabilities, agentVersion, instructions, events)
	if err != nil {
		return nil, err
	}

	directories := sessionDirectoriesNegotiate(spec, agentCapabilities.SessionCapabilities)
	if directories.Notice != nil {
		if err := emit(core.RuntimeEventWarning{Message: *directories.Notice}); err != nil {
			return nil, err
		}
	}

	if harness == core.HarnessKindClaude && !backgroundSubagentsSupported {
		if err := emit(core.RuntimeEventWarning{
			Message: "This Claude adapter does not advertise backgroundSubagents. Foreground completion requires claude-agent-acp 0.95.0 or later; upgrade the container or worker to let prompts finish while background agents run.",
		}); err != nil {
			return nil, err
		}
	}

	if airHarness && !nativeChildrenSupported {
		if err := emit(core.RuntimeEventWarning{
			Message: "This adapter does not advertise native agent sessions; native agent details are unavailable.",
		}); err != nil {
			return nil, err
		}
	}

	airGoal := jsonPath(initialized.Meta, "jetbrains", "air", "goal")
	var capability *core.GoalCapability
	if airGoal != nil {
		encoded, err := jsonvalue.Marshal(airGoal)
		var decoded core.GoalCapability
		if err == nil {
			err = json.Unmarshal(encoded, &decoded)
		}
		if err != nil {
			slog.Warn("adapter advertised malformed goal controls", "error", err)
		} else {
			capability = &decoded
		}
	}
	goalRecoverySupported := capability != nil &&
		capability.Supports(core.GoalControlActionPause) && capability.Supports(core.GoalControlActionResume) &&
		jsonArrayContains(jsonPath(airGoal, "resumePolicies"), "pause")
	if capability != nil && capability.Supports(core.GoalControlActionResume) {
		goal, unlock := spec.GoalRecovery.Lock()
		asking := goal.Asking()
		unlock()
		if asking && !goalRecoverySupported {
			return nil, errors.New("the installed adapter cannot pause a goal before explicit resume; update the adapter")
		}
	}
	var capabilityValue any
	if capability != nil {
		encoded, err := json.Marshal(capability)
		if err != nil {
			return nil, err
		}
		if capabilityValue, err = jsonvalue.Parse(encoded); err != nil {
			return nil, err
		}
	}
	if err := publish(ctx, spec, events, jsonvalue.Object{{Key: "mjGoalCapability", Value: capabilityValue}}); err != nil {
		return nil, err
	}

	// Kept on the wire for older workers; no harness can lose native
	// continuity this way any more.
	nativeContinuityLost := false
	// The MCP servers the request that opened the session carried.
	offeredMCPServers := 0
	hasGoal := codexHasGoal(spec)
	type loadedSession struct {
		sessionID     schema.SessionId
		configOptions []schema.SessionConfigOption
		modes         *schema.SessionModeState
	}
	var loaded *loadedSession
	if spec.ResumeSession != nil && sessionOpen.replaceUnusedCodexThread {
		existing := *spec.ResumeSession
		// This bridge starts a new app-server. Codex keeps an unused thread in
		// the old process until its first turn writes a rollout. The history
		// evidence proves no turn reached this thread, so there is no history
		// to recover and no need to ask Codex to load it.
		if err := emit(core.RuntimeEventWarning{
			Message: fmt.Sprintf("Codex session %s was never used; continuing in a new empty session", existing),
		}); err != nil {
			return nil, err
		}
		if nativeChildrenSupported {
			// Publish the empty replay the missing-thread fallback publishes,
			// so staging left by an abandoned load cannot hide the
			// replacement's native children.
			for _, event := range []core.NativeAgentEvent{core.NativeAgentEventReplayBegin{}, core.NativeAgentEventReplayCommit{}} {
				if err := emit(core.RuntimeEventNativeAgent{Event: event}); err != nil {
					return nil, err
				}
			}
		}
	} else if spec.ResumeSession != nil {
		existing := *spec.ResumeSession
		sessionID := schema.SessionId(existing)
		// Native children require replay to recover identity and
		// transcripts. Other adapters can resume without replaying the
		// parent's history.
		nativeChildren := nativeChildrenSupported && airHarness
		if nativeChildren {
			if err := emit(core.RuntimeEventNativeAgent{Event: core.NativeAgentEventReplayBegin{}}); err != nil {
				return nil, err
			}
		}
		var reloadedMeta schema.Meta
		var reloadedOptions []schema.SessionConfigOption
		var reloadedModes *schema.SessionModeState
		var reloadErr error
		if !nativeChildren && agentCapabilities.SessionCapabilities != nil && agentCapabilities.SessionCapabilities.Resume != nil {
			// `session/resume` does not replay, so everything it sends is
			// live.
			acceptLiveHistory(liveHistoryEnabled)
			request := resumeSessionRequest(spec, directories.Accepted, sessionID, systemPrompt)
			offeredMCPServers = len(request.MCPServers)
			var resumed schema.ResumeSessionResponse
			err := acpCall(ctx, connection, "session/resume", request, &resumed)
			spec.ACPActivity.Mark()
			if err != nil {
				reloadErr = fmt.Errorf("resume ACP session %s: %w", existing, err)
			} else {
				reloadedMeta, reloadedOptions, reloadedModes = resumed.Meta, resumed.ConfigOptions, resumed.Modes
			}
		} else {
			request := loadSessionRequest(spec, directories.Accepted, sessionID, systemPrompt)
			offeredMCPServers = len(request.MCPServers)
			loadedResponse, err := loadAtReplayBoundary(ctx, connection, request, events, nativeChildren, liveHistoryEnabled, boundary)
			spec.ACPActivity.Mark()
			if err != nil {
				reloadErr = fmt.Errorf("load ACP session %s: %w", existing, err)
			} else {
				reloadedMeta, reloadedOptions, reloadedModes = loadedResponse.Meta, loadedResponse.ConfigOptions, loadedResponse.Modes
			}
		}
		// For threads that must be loaded first, a typed missing-session
		// error can confirm that the harness has no record of an unused
		// thread.
		if reloadErr != nil {
			if !harnessReportsMissingNativeSession(spec, reloadErr) {
				return nil, reloadErr
			}
			name := harness.DisplayName()
			if spec.NativeSessionHistory == NativeSessionHistoryMayHaveHistory || hasGoal {
				return nil, fmt.Errorf("%s has no native history for session %s; the existing "+
					"conversation or goal cannot be resumed: %w", name, existing, reloadErr)
			}
			if err := emit(core.RuntimeEventWarning{Message: fmt.Sprintf(
				"%s has no native session %s and this session never used it; continuing in a new empty session",
				name, existing)}); err != nil {
				return nil, err
			}
			if nativeChildren {
				// The missing session was proven unused. Publish the empty
				// replay before routing children of its replacement live.
				if err := emit(core.RuntimeEventNativeAgent{Event: core.NativeAgentEventReplayCommit{}}); err != nil {
					return nil, err
				}
			}
		} else {
			// The replay boundary, including its `ReplayCommit`, was already
			// decided where the load response arrived.
			if grokModels != nil {
				if fresh := core.GrokModelStateOf(reloadedMeta); fresh != nil {
					*grokModels = *fresh
				}
			}
			if capability != nil && reloadedMeta != nil {
				if err := publish(ctx, spec, events, reloadedMeta); err != nil {
					return nil, err
				}
			}
			loaded = &loadedSession{sessionID: sessionID, configOptions: reloadedOptions, modes: reloadedModes}
		}
	}
	var sessionID schema.SessionId
	var configOptions []schema.SessionConfigOption
	var modes *schema.SessionModeState
	resumed := false
	if loaded != nil {
		sessionID, configOptions, modes, resumed = loaded.sessionID, loaded.configOptions, loaded.modes, true
	} else {
		// A new native session has no history to replay, whether this is a
		// first launch or a resume that replaced an unused session. Without
		// this, a fallback from a failed reload dropped every reply the new
		// session sent for the rest of the worker's life (R8-1).
		acceptLiveHistory(liveHistoryEnabled)
		request := newSessionRequest(spec, directories.Accepted, true, systemPrompt)
		offeredMCPServers = len(request.MCPServers)
		var created schema.NewSessionResponse
		createErr := connection.Call(context.WithoutCancel(ctx), "session/new", request, &created)
		spec.ACPActivity.Mark()
		if createErr != nil {
			return nil, startupRequestError(acpRPCError(createErr), "create ACP session")
		}
		if capability != nil && created.Meta != nil {
			if err := publish(ctx, spec, events, created.Meta); err != nil {
				return nil, err
			}
		}
		// A session may open on a different model than the agent-wide
		// default, so a fresher catalogue on the session wins.
		if grokModels != nil {
			if fresh := core.GrokModelStateOf(created.Meta); fresh != nil {
				*grokModels = *fresh
			}
		}
		sessionID, configOptions, modes = created.SessionID, created.ConfigOptions, created.Modes
	}
	// OpenCode before 2.2.6 reloads its tool registry on a 100 ms debounce
	// after an MCP server connects, and signals nobody when it is done. A
	// prompt that starts a turn inside that window misses the `mj-agents` and
	// `mj-memory` tools (measured on 2.2.1: prompts 0-80 ms after
	// `tools/list` missed them almost always, prompts at 120 ms or later had
	// them). So the first prompt to such a bridge waits until
	// opencodeToolRegistrySettle has passed since the open returned.
	var firstPromptNotBefore *time.Time
	if harness == core.HarnessKindOpenCode && opencodeMayMissFirstPromptTools(agentVersion) {
		notBefore := time.Now().Add(opencodeToolRegistrySettle)
		firstPromptNotBefore = &notBefore
	}

	// An adapter that withholds the servers it was offered says so only in
	// its log. Checked after the open, because a Muse host that could not
	// start also withholds the grant, and the open's error carries that
	// host's own diagnostic. A container keeps the adapter it was created
	// with, so a session from an older image continues without the tools and
	// says so. A reviewer does not: it is told to inspect the change with its
	// analyzer tools, and reviewing without them is the degraded review the
	// review design refuses.
	if offeredMCPServers > 0 && !mcpAvailable {
		remedy := fmt.Sprintf("Managed targets install muse-acp %s; on a container target, suspend and resume the session once its agent-dev image has updated",
			core.MuseACPVersion)
		if len(spec.ExtraMCPServers) > 0 {
			return nil, fmt.Errorf("this Muse runtime does not accept MCP servers, so a Muse reviewer here would run without its analyzer tools. %s.", remedy)
		}
		if err := emit(core.RuntimeEventWarning{Message: fmt.Sprintf(
			"This session's Muse runtime does not accept MCP servers, so Mjolnir's session-history and other MCP tools are unavailable in it. %s.",
			remedy)}); err != nil {
			return nil, err
		}
	}

	if availabilitySupported {
		if err := refreshAvailability(ctx, connection, sessionID, events); err != nil {
			return nil, err
		}
	}

	// Launch flags and environment are applied before the bridge starts. ACP
	// modes are selected after the session exists, before any prompt can
	// run.
	enforcement := sessionEnforcement(spec)
	if configOptions == nil {
		configOptions = []schema.SessionConfigOption{}
	}
	// Grok Build publishes model selection through its legacy catalogue.
	// Keep any standard selectors it also returns while projecting
	// model/effort into the shape the rest of Hel reads.
	if grokModels != nil {
		core.GrokMergeConfigOptions(&configOptions, grokModels)
	}
	// Model selection can replace the effort catalogue. Both must be restored
	// before SessionConfigured releases queued prompts. They are restored
	// before the execution mode, because a harness judges a mode against the
	// model the session runs: the Claude adapter answers Auto on a model
	// without it with Accept edits. On `session/new` that adapter describes
	// its default model until a model is selected, whatever model Claude Code
	// started on, so a mode asked for first was judged against the wrong
	// model and Claude Code refused it (R8-2).
	var droppedSelectors []sessionConfigSelector
	var selectorFailures []string
	var normalizedSelectors []sessionConfigSelector
	{
		acceptedConfig, unlock := spec.AcceptedConfig.Lock()
		accepted := *acceptedConfig
		unlock()
		for _, selector := range []struct {
			key   string
			value *string
		}{{"model", accepted.Model}, {"effort", accepted.Effort}} {
			if selector.value == nil {
				continue
			}
			key, value := selector.key, *selector.value
			applied, err := applySessionSelector(ctx, connection, sessionID, &configOptions, grokModels, harness, key, value)
			if err == nil && applied != value {
				normalizedSelectors = append(normalizedSelectors, sessionConfigSelector{key: key, value: applied})
			}
			if err != nil {
				// Model selection is recoverable even when the catalogue
				// lists it: API validation can refuse a listed model. Keep the
				// worker reachable so the operator can choose another model
				// below.
				if key != "model" && (spec.ClearContextRequest != nil || selectorValueIsOffered(configOptions, key, value)) {
					return nil, fmt.Errorf("restore this session's accepted %s %s: %w", key, sessionConfigDebugString(value), err)
				}
				slog.Warn("could not restore saved session selector", "selector", key, "value", value, "error", err.Error())
				selectorFailures = append(selectorFailures, fmt.Sprintf("%s %s: %s", key, sessionConfigDebugString(value), err))
				droppedSelectors = append(droppedSelectors, sessionConfigSelector{key: key, value: value})
			}
		}
	}
	reset := spec.ClearContextRequest
	if reset == nil {
		reset = spec.ContextRestore
	}
	if reset != nil {
		for _, selector := range reset.Selectors {
			key, value := selector[0], selector[1]
			_, err := applySessionSelector(ctx, connection, sessionID, &configOptions, grokModels, harness, key, value)
			if err == nil {
				continue
			}
			// These values are what the previous bridge reported, not what the
			// user chose (the accepted configuration above carries that). A
			// resumed Claude bridge reports its model as a raw id it does not
			// list, and it refuses that id when it is sent back. Such a value
			// cannot be restored, so the new conversation keeps the bridge's
			// own value. The rollback after a failed clear never fails on a
			// selector: it must leave a usable session.
			if spec.ClearContextRequest != nil && key != "model" && selectorValueIsOffered(configOptions, key, value) {
				return nil, fmt.Errorf("restore %s after clear: %w", key, err)
			}
			slog.Warn("kept the bridge's value after clear because the reported value could not be restored",
				"selector", key, "value", value, "error", err.Error())
			if key == "model" {
				if err := emit(core.RuntimeEventWarning{Message: fmt.Sprintf(
					"Could not restore model %s after clear (%s). You can change the model while this worker is running.",
					sessionConfigDebugString(value), err)}); err != nil {
					return nil, err
				}
			}
		}
	}
	if reset != nil && reset.Mode != nil {
		mode := *reset.Mode
		_, policyHasMode := "", false
		if enforcement != nil {
			_, policyHasMode = enforcement.ACPMode()
		}
		// A saved mode counts only as evidence that Plan was on when policy
		// owns it.
		if mode == "plan" || !policyHasMode {
			if _, err := enforceExecutionMode(ctx, connection, sessionID, harness, nil, mode, &configOptions, modes); err != nil {
				return nil, err
			}
		}
	}
	// Every open selects the policy, so a loaded session cannot keep an
	// earlier one. Only Plan carried over from a resumed session or a saved
	// context stays on. Muse's auto-review is off in every session muse-acp
	// opens, so the policy setting is selected on every open too. A cleared
	// or restored context put Plan back above, through its saved mode or its
	// saved selectors; a new session does not start in it.
	sync := PolicySyncAlways
	if current := currentSessionMode(configOptions, modes); (resumed || reset != nil) && current != nil && *current == "plan" {
		sync = PolicySyncIfDrifted
	}
	refusal, err := reassertExecutionPolicy(ctx, connection, sessionID, harness, enforcement, &configOptions, modes, sync)
	if err != nil {
		return nil, err
	}
	if refusal != nil {
		// A session the harness refuses the mode for stays usable rather than
		// failing the worker, which left a resumed session suspended on every
		// retry (R8-2). Claude takes its default mode, new or reloaded, so a
		// restored mode such as bypassPermissions never survives. Another
		// harness keeps its own mode for a new session that refused the
		// request; a reloaded one fails, since it opened in this mode before,
		// and so does one whose bridge does not offer the mode at all (a Muse
		// listing only allowAll would run without approvals).
		message, ok, err := takeSafeMode(ctx, connection, sessionID, harness, enforcement, &configOptions, modes, refusal)
		if err != nil {
			return nil, err
		}
		if !ok {
			if resumed || refusal.unoffered {
				return nil, refusal.error
			}
			message = refusedPolicyWarning(harness, enforcement, refusal, modes, configOptions)
		}
		if err := emit(core.RuntimeEventWarning{Message: message}); err != nil {
			return nil, err
		}
	}
	// Startup failures must retain their cause rather than being classified
	// as a dead running bridge and retried with the same invalid settings.
	openedSession, unlockOpened := opened.Lock()
	*openedSession = &OpenedSession{
		nativeSessionID:   string(sessionID),
		startedAt:         time.Now(),
		resumed:           resumed,
		nativeSessionUsed: nativeSessionUsed,
	}
	unlockOpened()
	if resolved := takePendingReloadAck(sessionOpen.pendingReloadAck); resolved != nil {
		sendResolution(resolved, nil)
	}
	// Drop anything the worker queued for the bridge this one replaced. The
	// worker dispatches only while it believes the session is configured; it
	// clears that flag on `HarnessRestarting` and sets it again only after
	// this bridge's `SessionConfigured`, which has not been sent yet. So every
	// request still in the channel was dispatched before the worker saw the
	// restart and is already in the set the worker interrupted. Emitting a
	// runtime event for one would interrupt it twice and fail the
	// coordinator's `require_in_flight`; delivering it would run it
	// untracked on the fresh session. A first start drains nothing:
	// out-of-band senders such as compaction are not gated on the session
	// being configured, so a request that arrives while the very first
	// bridge is still handshaking is a live request for this session, not a
	// leftover.
	if sessionOpen.replacingPreviousBridge {
		drainRequestsFromThePreviousBridge(requests)
	}

	if spec.ClearContextRequest != nil {
		if err := emit(core.RuntimeEventContextCleared{
			RequestID:       spec.ClearContextRequest.RequestID,
			NativeSessionID: string(sessionID),
		}); err != nil {
			return nil, err
		}
	} else {
		// A failed load or the Codex shortcut can replace a recorded thread.
		// Each replacement reports the first ID so restore can match its
		// archive.
		var replacedUnusedNativeSessionID *string
		if spec.ResumeSession != nil && !resumed {
			first := sessionOpen.firstReplacedUnusedNativeSessionID
			if first == nil {
				first = new(*string)
			}
			if *first == nil {
				existing := *spec.ResumeSession
				*first = &existing
			}
			replaced := **first
			replacedUnusedNativeSessionID = &replaced
		}
		var executionMode *string
		if enforcement != nil {
			label := enforcement.Label()
			executionMode = &label
		}
		if err := emit(core.RuntimeEventSessionStarted{
			NativeSessionID:               string(sessionID),
			Resumed:                       resumed,
			ExecutionMode:                 executionMode,
			NativeContinuityLost:          nativeContinuityLost,
			ReplacedUnusedNativeSessionID: replacedUnusedNativeSessionID,
		}); err != nil {
			return nil, err
		}
	}
	for _, selector := range normalizedSelectors {
		accepted, unlock := spec.AcceptedConfig.Lock()
		accepted.Remember(selector.key, selector.value, configOptions)
		unlock()
	}
	session := &servedSession{
		ctx:                   ctx,
		connection:            connection,
		spec:                  spec,
		requests:              requests,
		events:                events,
		terminals:             terminals,
		pending:               pendingElicitations,
		planSlot:              planImplementationSlot,
		opened:                opened,
		agentOutputCount:      agentOutputCount,
		lastAgentMessage:      lastAgentMessage,
		grokUsage:             grokUsage,
		sessionID:             sessionID,
		configOptions:         configOptions,
		modes:                 modes,
		grokModels:            grokModels,
		enforcement:           enforcement,
		steeringSupported:     steeringSupported,
		opencodeWholeTurns:    opencodeWholeTurns,
		antigravityWholeTurns: antigravityWholeTurns,
		firstPromptNotBefore:  firstPromptNotBefore,
		goalControls:          &PendingControls{},
	}
	defer func() {
		// The open questions this loop owns end with it (Rust drops them).
		if session.configRecovery != nil {
			session.configRecovery.Close()
		}
		if session.goalQuestion != nil && session.goalQuestion.request != nil {
			session.goalQuestion.request.Close()
		}
	}()
	if err := session.sessionConfigured(); err != nil {
		return nil, err
	}
	if err := session.sessionModesConfigured(); err != nil {
		return nil, err
	}

	// A successful restore can map a saved transcript ID onto the current
	// picker alias. Persist that accepted spelling only after startup is
	// ready; ConfigApplied also publishes a configured session to the relay.
	for _, selector := range normalizedSelectors {
		if err := emit(core.RuntimeEventConfigApplied{
			Key:           selector.key,
			Value:         selector.value,
			ConfigOptions: cloneConfigOptions(session.configOptions),
		}); err != nil {
			return nil, err
		}
	}

	// The transcript keeps the evidence that this session changed selector
	// even after the question below is answered and gone.
	if len(droppedSelectors) > 0 {
		if err := emit(core.RuntimeEventWarning{Message: fmt.Sprintf(
			"%s You can change the selection while this worker is running. %s",
			droppedSelectorWarning(droppedSelectors, session.configOptions), strings.Join(selectorFailures, "; "))}); err != nil {
			return nil, err
		}
		if request := sessionConfigRecoveryRequest(droppedSelectors, session.configOptions); request != nil {
			question := pendingElicitations.registerCommandLoop(*request)
			// Its local wrapper registers liveness with the shared owner; the
			// command loop below still owns the recovery answer path.
			session.configRecovery = question
			if err := emit(core.RuntimeEventElicitationRequested{Request: question.ElicitationRequest}); err != nil {
				return nil, err
			}
		}
	}

	goalQuestion, err := goalRecover(ctx, goalRecoverySupported, connection, sessionID, spec, events,
		*pendingElicitations, session.configRecovery != nil)
	if err != nil {
		return nil, err
	}
	session.goalQuestion = goalQuestion
	return session.serve()
}

// serve is the command loop. An idle OpenCode session reloads to import
// changed logins. Only here, between commands, and only with nothing
// waiting on an answer, the same conditions /clear needs: a reload never
// interrupts a turn, and a command that arrives meanwhile waits for the
// fresh bridge.
func (s *servedSession) serve() (SessionRestart, error) {
	spec := s.spec
	// tokio's interval ticks at once, then every checkInterval; a tick it
	// missed fires as soon as it is polled again (MissedTickBehavior::Delay).
	loginsCheck := time.NewTimer(0)
	defer loginsCheck.Stop()
	for {
		// This task owns these; only the open questions change beside it.
		mayReload := spec.ImportedLogins != nil && s.goalControls.isEmpty() &&
			s.configRecovery == nil && s.goalQuestion == nil
		var goalEvents <-chan core.RuntimeEvent
		if !s.goalControls.isEmpty() {
			goalEvents = s.goalControls.next()
		}
		var loginsTick <-chan time.Time
		if mayReload {
			loginsTick = loginsCheck.C
		}
		var request CommandRequest
		select {
		case event := <-goalEvents:
			if err := s.emit(event); err != nil {
				return nil, err
			}
			continue
		case <-loginsTick:
			loginsCheck.Reset(checkInterval)
			changed, err := spec.ImportedLogins.changed(s.ctx)
			if err != nil {
				return nil, err
			}
			// Decided under the lock that registers questions, with nothing
			// awaited in between: a question registered first holds the
			// reload back, and one registered later belongs to a bridge being
			// replaced, which cancels it as every replacement does.
			if changed && s.pending.isEmpty() {
				return SessionRestartReloadLogins{Value: string(s.sessionID)}, nil
			}
			continue
		case received, ok := <-s.requests:
			if !ok {
				return nil, nil
			}
			request = received
		case <-s.ctx.Done():
			return nil, s.ctx.Err()
		}
		if attachments, ok := request.(CommandRequestPromptAttachments); ok {
			prompt, err := resolveAttachments(attachments.Root, attachments.Prompt)
			if err != nil {
				if err := s.reject(core.OutcomeReasonCommandFailed, attachments.RequestID,
					"could not load attached images: "+err.Error()); err != nil {
					return nil, err
				}
				continue
			}
			request = CommandRequestPrompt{RequestID: attachments.RequestID, Prompt: prompt}
		}
		restart, done, err := s.command(request)
		if err != nil || done {
			return restart, err
		}
	}
}

// command handles one command while no prompt runs. done says the session
// ends with restart.
func (s *servedSession) command(request CommandRequest) (restart SessionRestart, done bool, err error) {
	spec := s.spec
	ctx := s.ctx
	connection := s.connection
	switch request := request.(type) {
	case CommandRequestPromptAttachments:
		panic("resolved before ACP dispatch")
	case CommandRequestClearContext:
		if !s.pending.isEmpty() || s.configRecovery != nil || s.goalQuestion != nil {
			message := "/clear requires all pending interactions to be resolved"
			if s.pending.onlyReviews() {
				message = "/clear waits for the Mjolnir guardian to finish reviewing a tool request, which takes at most 90 seconds; send it again then"
			}
			return nil, false, s.reject(core.OutcomeReasonAdmissionRejected, request.RequestID, message)
		}
		options := slices.Clone(s.configOptions)
		rank := func(option schema.SessionConfigOption) int {
			if option.Category == nil {
				return 2
			}
			switch *option.Category {
			case schema.SessionConfigOptionCategoryModel:
				return 0
			case schema.SessionConfigOptionCategoryThoughtLevel:
				return 1
			}
			return 2
		}
		slices.SortStableFunc(options, func(a, b schema.SessionConfigOption) int { return rank(a) - rank(b) })
		var selectors [][2]string
		for _, option := range options {
			if option.Select == nil {
				continue
			}
			selectors = append(selectors, [2]string{string(option.ID), string(option.Select.CurrentValue)})
		}
		reset := ContextReset{RequestID: request.RequestID, Selectors: selectors}
		if s.modes != nil {
			mode := string(s.modes.CurrentModeID)
			reset.Mode = &mode
		}
		return SessionRestartClear{Reset: reset, Previous: string(s.sessionID)}, true, nil
	case CommandRequestPrompt:
		return s.prompt(request.RequestID, request.Prompt)
	case CommandRequestGoalControl:
		prepared, err := prepareControl(ctx, spec, s.events, &s.goalQuestion, s.configRecovery != nil, request.RequestID, request.Action)
		if err != nil {
			return nil, true, err
		}
		if prepared {
			expected := controlIdentity(spec, request.RequestID)
			s.goalControls.start(ctx, spec, s.events, connection, s.sessionID, request.RequestID, request.Action, expected)
		}
		return nil, false, nil
	case CommandRequestSetConfig:
		return nil, false, s.setConfig(request)
	case CommandRequestRestoreExecutionMode:
		warning, hasWarning, err := leavePlanForPolicy(ctx, connection, s.sessionID, spec.Harness, s.enforcement, &s.configOptions, s.modes)
		var value *string
		if err == nil {
			if value = currentSessionMode(s.configOptions, s.modes); value == nil {
				err = errors.New("the policy mode was not reported")
			}
		}
		if err != nil {
			return nil, false, s.reject(core.OutcomeReasonCommandFailed, request.RequestID, "restore execution mode: "+err.Error())
		}
		if hasWarning {
			if err := s.emit(core.RuntimeEventWarning{Message: warning}); err != nil {
				return nil, true, err
			}
		}
		if err := s.sessionModesConfigured(); err != nil {
			return nil, true, err
		}
		return nil, false, s.emit(core.RuntimeEventConfigApplied{
			RequestID: request.RequestID, Key: "mode", Value: *value, ConfigOptions: cloneConfigOptions(s.configOptions),
		})
	case CommandRequestSetSessionMode:
		return nil, false, s.setSessionMode(request)
	case CommandRequestCancelTurnFor:
		return nil, false, s.reject(core.OutcomeReasonAdmissionRejected, request.RequestID, "The requested turn is no longer running")
	case CommandRequestSteer:
		return nil, false, s.reject(core.OutcomeReasonAdmissionRejected, request.RequestID, "The requested turn is no longer running")
	case CommandRequestCancel:
		if err := applyCancel(ctx, connection, s.sessionID, request.RequestID, s.events, s.terminals, s.pending); err != nil {
			return nil, true, err
		}
		// No prompt runs, so no turn stays withdrawn.
		s.pending.endWithdrawal()
		return nil, false, nil
	case CommandRequestResolveElicitation:
		outcome, err := s.resolveIdle(request)
		if err != nil {
			return nil, true, err
		}
		if !sendResolution(request.Resolved, outcome) {
			slog.Debug("elicitation resolution receiver was already closed", "session_id", string(s.sessionID),
				"operation", "resolve_elicitation", "elicitation_id", request.ElicitationID)
		}
		return nil, false, nil
	case CommandRequestStopBackgroundTask:
		resolveBackgroundTaskStop(ctx, connection, s.sessionID, s.terminals, request.Target, request.Resolved)
		return nil, false, nil
	case CommandRequestReloadLaunchConfig:
		var resumeSession *string
		if request.LaunchConfig.Harness == spec.Harness {
			nativeSessionID := string(s.sessionID)
			resumeSession = &nativeSessionID
		}
		return SessionRestartReloadLaunchConfig{
			LaunchConfig:          request.LaunchConfig,
			Environment:           request.Environment,
			Command:               request.Command,
			Args:                  request.Args,
			AdditionalDirectories: request.AdditionalDirectories,
			SupervisorSpec:        request.SupervisorSpec,
			Leases:                request.Leases,
			ResumeSession:         resumeSession,
			Resolved:              request.Resolved,
		}, true, nil
	case CommandRequestClose:
		outcome := acpCall(ctx, connection, "session/close", schema.CloseSessionRequest{SessionID: s.sessionID}, nil)
		if err := emitCloseOutcome(ctx, s.events, request.RequestID, outcome); err != nil {
			return nil, true, err
		}
		return nil, true, nil
	}
	panic(fmt.Sprintf("unknown command request %T", request))
}

// resolveIdle answers a question while no prompt runs: the goal question,
// the config recovery question, or a form the harness is waiting on.
func (s *servedSession) resolveIdle(request CommandRequestResolveElicitation) (error, error) {
	spec := s.spec
	answer, handled, err := resolve(s.ctx, s.connection, s.sessionID, spec, s.events, &s.goalQuestion,
		s.configRecovery != nil, request.ElicitationID, &request.Response)
	if err != nil {
		return nil, err
	}
	if handled {
		return answer, nil
	}
	previousModel, hadModel := sessionModel(s.configOptions)
	answer, handled, err = resolveSessionConfigRecovery(s.ctx, s.connection, s.sessionID, spec, s.events,
		&s.configOptions, s.grokModels, &s.configRecovery, true, request.ElicitationID, &request.Response)
	if err != nil {
		return nil, err
	}
	// The answer can change the model like SetConfig does.
	if model, hasModel := sessionModel(s.configOptions); model != previousModel || hasModel != hadModel {
		if err := reconcileExecutionPolicy(s.ctx, s.connection, s.sessionID, spec.Harness, s.enforcement,
			&s.configOptions, s.modes, s.events); err != nil {
			return nil, err
		}
		// The answer published its own configuration before the policy was
		// restored.
		if err := s.sessionConfigured(); err != nil {
			return nil, err
		}
		if err := s.sessionModesConfigured(); err != nil {
			return nil, err
		}
	}
	if handled {
		return answer, nil
	}
	return resolvePendingElicitation(s.pending, request.ElicitationID, request.Response), nil
}

func (s *servedSession) setConfig(request CommandRequestSetConfig) error {
	spec := s.spec
	key, value := request.Key, request.Value
	if !policyPermits(s.enforcement, s.configOptions, &key, value) {
		return s.reject(core.OutcomeReasonCommandFailed, request.RequestID, policyOwnedMode)
	}
	previousModel, hadModel := sessionModel(s.configOptions)
	grokModelChange := s.grokModels != nil && core.GrokHandlesConfigKey(key)
	// A model change, even one the bridge then refuses and rolls back, can
	// move the bridge's mode, so the policy follows it.
	option := core.FindSessionConfigOption(s.configOptions, key)
	modelChange := (option != nil && option.Category != nil && *option.Category == schema.SessionConfigOptionCategoryModel) ||
		key == "model"
	applied, err := applySessionSelector(s.ctx, s.connection, s.sessionID, &s.configOptions, s.grokModels, spec.Harness, key, value)
	if err == nil {
		model, hasModel := sessionModel(s.configOptions)
		if modelChange || model != previousModel || hasModel != hadModel {
			if err := reconcileExecutionPolicy(s.ctx, s.connection, s.sessionID, spec.Harness, s.enforcement,
				&s.configOptions, s.modes, s.events); err != nil {
				return err
			}
		}
		accepted, unlock := spec.AcceptedConfig.Lock()
		accepted.Remember(key, applied, s.configOptions)
		unlock()
		return s.emit(core.RuntimeEventConfigApplied{
			RequestID: request.RequestID, Key: key, Value: applied, ConfigOptions: cloneConfigOptions(s.configOptions),
		})
	}
	if grokModelChange && core.GrokResponseWasLost(err) {
		return fmt.Errorf("Grok model change response was lost; reload the session to reconcile its model state: %w", err)
	}
	if modelChange {
		if err := reconcileExecutionPolicy(s.ctx, s.connection, s.sessionID, spec.Harness, s.enforcement,
			&s.configOptions, s.modes, s.events); err != nil {
			return err
		}
		if err := s.sessionConfigured(); err != nil {
			return err
		}
		if err := s.sessionModesConfigured(); err != nil {
			return err
		}
	}
	return s.reject(core.OutcomeReasonCommandFailed, request.RequestID, err.Error())
}

func (s *servedSession) setSessionMode(request CommandRequestSetSessionMode) error {
	modeID := request.ModeID
	if !policyPermits(s.enforcement, s.configOptions, nil, modeID) {
		return s.reject(core.OutcomeReasonCommandFailed, request.RequestID, policyOwnedMode)
	}
	advertised := s.modes != nil && slices.ContainsFunc(s.modes.AvailableModes, func(mode schema.SessionMode) bool {
		return string(mode.ID) == modeID
	})
	grokPlanFallback := core.GrokPermitsUnadvertisedPlanMode(s.spec.Harness, modeID)
	var err error
	if advertised || grokPlanFallback {
		if err = acpCall(s.ctx, s.connection, "session/set_mode",
			schema.SetSessionModeRequest{SessionID: s.sessionID, ModeID: schema.SessionModeId(modeID)}, nil); err != nil {
			err = fmt.Errorf("set session mode to %s: %w", modeID, err)
		}
	} else {
		err = fmt.Errorf("%s is not an available session mode", sessionConfigDebugString(modeID))
	}
	if err != nil {
		return s.reject(core.OutcomeReasonCommandFailed, request.RequestID, err.Error())
	}
	if s.modes != nil {
		s.modes.CurrentModeID = schema.SessionModeId(modeID)
	}
	return s.emit(core.RuntimeEventSessionModeApplied{
		RequestID:     request.RequestID,
		ModeID:        modeID,
		ConfigOptions: cloneConfigOptions(s.configOptions),
		Modes:         cloneModes(s.modes),
	})
}

// turnEvent is what woke a running prompt's loop.
type turnEvent int

const (
	turnGoalControl turnEvent = iota
	turnPlanApproved
	turnPromptAnswered
	turnStallCheck
	turnImplementationTimeout
	turnCancelTimeout
	turnSteeringTimeout
	turnSteerAnswered
	turnCommand
	turnModeRestored
	turnShutdown
)

// prompt runs one prompt until its turn ends. The pending ACP request owns
// this turn. Jev may assess unfinished work after the harness replies,
// never complete an active request based on silence or historical text.
func (s *servedSession) prompt(requestID string, prompt []schema.ContentBlock) (SessionRestart, bool, error) {
	spec := s.spec
	ctx := s.ctx
	connection := s.connection
	harness := spec.Harness
	if len(prompt) == 0 {
		return nil, false, s.reject(core.OutcomeReasonAdmissionRejected, requestID, "ACP prompt has no content blocks")
	}
	if notBefore := s.firstPromptNotBefore; notBefore != nil {
		s.firstPromptNotBefore = nil
		timer := time.NewTimer(time.Until(*notBefore))
		select {
		case <-timer.C:
		case <-ctx.Done():
			timer.Stop()
			return nil, true, ctx.Err()
		}
	}
	prompt = promptForHarness(harness, prompt)
	updatesBefore := s.agentOutputCount.get()
	s.lastAgentMessage.clear()
	// A prompt asking the harness to compact its context is answered by
	// compacting, and the bridges report that with banners rather than with
	// agent output. Judging such a turn as unanswered would report the
	// compaction that was asked for as a failure (#970).
	askedToCompact := core.PromptRequestsCompaction(prompt)
	var texts []string
	for _, block := range prompt {
		if block.Text != nil {
			texts = append(texts, block.Text.Text)
		}
	}
	commandID := requestID
	spec.TurnContext.Reset(&commandID, strings.Join(texts, "\n"))
	s.pending.endWithdrawal()
	spec.StepClock.BeginTurn()
	if harness == core.HarnessKindGrok {
		s.grokUsage.Begin(string(s.sessionID))
	}
	// Start the stall clock at send time so the watchdog measures silence
	// within this turn, not idle time carried from before.
	spec.ACPActivity.Mark()
	// One policy for every harness, off unless the operator set a bound. No
	// harness ends the turn Mjolnir reports without the `session/prompt`
	// reply, so there is no harness that is safe to exempt and nothing left
	// to special-case.
	stallPolicy := turnStallPolicy()
	if spec.StallPolicy != nil {
		stallPolicy = *spec.StallPolicy
	}
	_, loginFingerprint := spec.LoginIdentity.snapshot()
	if err := recordPromptUse(ctx, s.opened, s.events); err != nil {
		return nil, true, err
	}
	active, dropActive := acpStartCall[promptAnswer](ctx, connection, "session/prompt",
		schema.PromptRequest{SessionID: s.sessionID, Prompt: prompt})
	// Whatever ends this loop drops the requests it still waits for, as Rust
	// drops their futures.
	defer func() { dropActive() }()
	implementations := newPlanImplementationQueue()
	slot, unlock := s.planSlot.Lock()
	*slot = implementations
	unlock()
	defer ActivePlanImplementation{slot: s.planSlot}.release()
	var approvedPlan *PlanImplementation
	var implementationDeadline *time.Timer
	var modeRestoration PlanModeRestoration
	stopRestoration := func() {}
	defer func() { stopRestoration() }()
	promptRunning := true
	var cancelDeadline *time.Timer
	var pendingSteer *PendingSteer
	defer func() {
		if pendingSteer != nil {
			pendingSteer.drop()
		}
	}()
	var steeringDeadline *time.Timer
	timerC := func(timer *time.Timer) <-chan time.Time {
		if timer == nil {
			return nil
		}
		return timer.C
	}
	stopTimer := func(timer **time.Timer) {
		if *timer != nil {
			(*timer).Stop()
			*timer = nil
		}
	}
	defer stopTimer(&implementationDeadline)
	defer stopTimer(&cancelDeadline)
	defer stopTimer(&steeringDeadline)
	clearImplementation := func() {
		implementations.close()
		approvedPlan = nil
		stopTimer(&implementationDeadline)
	}
	for {
		var goalEvents <-chan core.RuntimeEvent
		if !s.goalControls.isEmpty() {
			goalEvents = s.goalControls.next()
		}
		var planReady <-chan struct{}
		if cancelDeadline == nil && approvedPlan == nil && modeRestoration == nil {
			planReady = implementations.ready()
		}
		var answered ActivePrompt
		if promptRunning {
			answered = active
		}
		// The watchdog keeps waiting as long as the harness shows a sign of
		// life. A tool call is a sign of life: a turn blocked in a
		// twenty-minute build sends nothing at all, and failing it for that
		// lost real work (#1020). Only the much longer tool-call bound can
		// end such a turn.
		var stalled core.StallVerdict
		var stallCheck <-chan time.Time
		var stallTimer *time.Timer
		if promptRunning && stallPolicy.Enabled() {
			facts := turnStallFacts(spec)
			nowMs := core.EpochMillis()
			verdict := core.StallVerdictOf(&facts, stallPolicy, nowMs)
			// At most one line a second, and the only record of why a turn
			// was or was not failed for going quiet. A turn that is wrongly
			// failed is diagnosed from exactly these facts (#1020).
			slog.Debug("turn stall check", "session_id", string(s.sessionID),
				"tools_in_flight", fmt.Sprint(facts.ToolsInFlight), "verdict", fmt.Sprintf("%#v", verdict),
				"policy", fmt.Sprintf("%#v", stallPolicy))
			if _, live := verdict.(core.StallVerdictLive); live {
				stallTimer = time.NewTimer(stallPolicy.NextCheck(&facts, nowMs))
				stallCheck = stallTimer.C
			} else {
				stalled = verdict
			}
		}
		var steerAnswered <-chan asyncResult[any]
		var steeringTimeout <-chan time.Time
		if pendingSteer != nil {
			steerAnswered = pendingSteer.response
			steeringTimeout = timerC(steeringDeadline)
		}
		var restored PlanModeRestoration
		if modeRestoration != nil && len(s.requests) == 0 {
			restored = modeRestoration
		}
		var event turnEvent
		var goalEvent core.RuntimeEvent
		var answer asyncResult[promptAnswer]
		var steerOutcome asyncResult[any]
		var command CommandRequest
		commandOpen := true
		var restoredMode asyncResult[RestoredPlanMode]
		// Biased as Rust's select: what is ready is taken in this order;
		// only when nothing is does the loop wait for whichever comes first.
		ready := func(wait bool) bool {
			if !wait {
				select {
				case goalEvent = <-goalEvents:
					event = turnGoalControl
					return true
				default:
				}
				select {
				case <-planReady:
					event = turnPlanApproved
					return true
				default:
				}
				select {
				case answer = <-answered:
					event = turnPromptAnswered
					return true
				default:
				}
				if stalled != nil {
					event = turnStallCheck
					return true
				}
				return false
			}
			select {
			case goalEvent = <-goalEvents:
				event = turnGoalControl
			case <-planReady:
				event = turnPlanApproved
			case answer = <-answered:
				event = turnPromptAnswered
			case <-stallCheck:
				event = turnStallCheck
			case <-timerC(implementationDeadline):
				event = turnImplementationTimeout
			case <-timerC(cancelDeadline):
				event = turnCancelTimeout
			case <-steeringTimeout:
				event = turnSteeringTimeout
			case steerOutcome = <-steerAnswered:
				event = turnSteerAnswered
			case command, commandOpen = <-s.requests:
				event = turnCommand
			case restoredMode = <-restored:
				event = turnModeRestored
			case <-ctx.Done():
				event = turnShutdown
			}
			return true
		}
		if !ready(false) {
			ready(true)
		}
		if stallTimer != nil {
			stallTimer.Stop()
		}
		switch event {
		case turnShutdown:
			return nil, true, ctx.Err()
		case turnGoalControl:
			if err := s.emit(goalEvent); err != nil {
				return nil, true, err
			}
		case turnPlanApproved:
			plan, ok := implementations.take()
			if !ok {
				continue
			}
			approvedPlan = &plan
			implementationDeadline = time.NewTimer(cancelAckTimeout)
			if err := s.emit(core.RuntimeEventWarning{
				Message: "Plan approved; waiting for Claude to finish planning before restoring the policy mode.",
			}); err != nil {
				return nil, true, err
			}
		case turnPromptAnswered:
			spec.ACPActivity.Mark()
			spec.StepClock.EndTurn()
			// A cancelled turn is over once its prompt answers.
			s.pending.endWithdrawal()
			if approvedPlan != nil && cancelDeadline == nil {
				if answer.err == nil && (schema.StopReason(answer.value.StopReason) == schema.StopReasonEndTurn ||
					schema.StopReason(answer.value.StopReason) == schema.StopReasonCancelled) {
					promptRunning = false
					implementation := *approvedPlan
					approvedPlan = nil
					restoreCtx, stop := context.WithCancel(ctx)
					stopRestoration = stop
					state := RestoredPlanMode{
						configOptions: cloneConfigOptions(s.configOptions),
						modes:         cloneModes(s.modes),
						plan:          implementation.plan,
					}
					restoration := make(chan asyncResult[RestoredPlanMode], 1)
					go func() {
						restoredState, err := restorePlanExecutionMode(restoreCtx, connection, s.sessionID, state,
							implementation.permissionSent, s.enforcement)
						restoration <- asyncResult[RestoredPlanMode]{value: restoredState, err: err}
					}()
					modeRestoration = restoration
					continue
				}
				if err := s.emit(core.RuntimeEventWarning{
					Message: "Plan implementation stopped because Claude did not finish the planning turn successfully.",
				}); err != nil {
					return nil, true, err
				}
			}
			if err := settleSteerAtTurnEnd(ctx, s.events, &pendingSteer); err != nil {
				return nil, true, err
			}
			stopTimer(&steeringDeadline)
			// A rejected prompt fails the turn, not the worker: the bridge can
			// still serve later prompts. A JSON-RPC error stays on this
			// connection; a dead transport is recovered by runBridge via child
			// exit or a protocol error after the session is open.
			finished, err := s.promptFinished(requestID, answer, askedToCompact, updatesBefore, loginFingerprint)
			if err != nil {
				return nil, true, err
			}
			// An acknowledged cancel leaves the bridge in place; the next
			// prompt goes to the same session.
			if err := s.emit(finished); err != nil {
				return nil, true, err
			}
			return nil, false, nil
		case turnStallCheck:
			if stalled == nil {
				continue
			}
			slog.Warn("turn stopped responding; failing the turn", "session_id", string(s.sessionID),
				"harness", harness.DisplayName(), "verdict", fmt.Sprintf("%#v", stalled))
			message := turnStallMessage(harness, stalled)
			if err := s.emit(core.RuntimeEventWarning{Message: message}); err != nil {
				return nil, true, err
			}
			// Fail the turn so it leaves Running and `mj wait` returns; leave
			// the session serving so a resend or a late recovery still works.
			// The reason travels with the outcome, not only in the
			// transcript, so `mj wait` and the session summary can say why.
			code := turnStalledStopReason
			return nil, false, s.emit(core.RuntimeEventPromptFinished{
				RequestID:  requestID,
				StopReason: turnStalledStopReason,
				Diagnostic: &core.TurnDiagnostic{Message: message, Code: &code},
			})
		case turnImplementationTimeout:
			message := "Plan implementation timed out while finishing planning or restoring the policy mode; restarting the harness without submitting the continuation."
			if err := s.emit(core.RuntimeEventWarning{Message: message}); err != nil {
				return nil, true, err
			}
			if err := s.emit(core.RuntimeEventCommandInterrupted{
				RequestID: requestID, Message: message, Reason: core.OutcomeReasonTimedOut,
			}); err != nil {
				return nil, true, err
			}
			return SessionRestartResume{Value: string(s.sessionID)}, true, nil
		case turnCancelTimeout:
			if err := s.emit(core.RuntimeEventWarning{Message: cancelUnackedWarning}); err != nil {
				return nil, true, err
			}
			if err := s.emit(core.RuntimeEventCommandInterrupted{
				Reason: core.OutcomeReasonRequestedCancellation, RequestID: requestID, Message: cancelUnackedWarning,
			}); err != nil {
				return nil, true, err
			}
			return SessionRestartResume{Value: string(s.sessionID)}, true, nil
		case turnSteeringTimeout:
			stopTimer(&steeringDeadline)
			if err := s.emit(core.RuntimeEventSteeringUnconfirmed{
				RequestID: pendingSteer.requestID,
				Message:   "No steering acknowledgment after 30s. Delivery is unconfirmed; the prompt remains held.",
			}); err != nil {
				return nil, true, err
			}
		case turnSteerAnswered:
			stopTimer(&steeringDeadline)
			pending := *pendingSteer
			pendingSteer = nil
			if err := settleSteer(ctx, s.events, pending, steerOutcome); err != nil {
				return nil, true, err
			}
		case turnCommand:
			if !commandOpen {
				cancellation := connection.Notify(context.WithoutCancel(ctx), "session/cancel",
					schema.CancelNotification{SessionID: s.sessionID})
				if err := s.emit(core.RuntimeEventCommandInterrupted{
					Reason:    core.OutcomeReasonRuntimeStopped,
					RequestID: requestID,
					Message:   "ACP command channel closed while the prompt was running",
				}); err != nil {
					return nil, true, err
				}
				if cancellation != nil {
					return nil, true, fmt.Errorf("cancel ACP prompt during runtime shutdown: %w", cancellation)
				}
				return nil, true, nil
			}
			switch command := command.(type) {
			case CommandRequestCancelTurnFor:
				if command.ActivePromptID != requestID || !promptRunning || cancelDeadline != nil {
					if err := s.reject(core.OutcomeReasonAdmissionRejected, command.RequestID,
						"The requested turn is no longer available for cancellation"); err != nil {
						return nil, true, err
					}
					continue
				}
				clearImplementation()
				if err := applyCancel(ctx, connection, s.sessionID, command.RequestID, s.events, s.terminals, s.pending); err != nil {
					return nil, true, err
				}
				cancelDeadline = time.NewTimer(cancelAckTimeout)
			case CommandRequestSteer:
				if command.ActivePromptID != requestID || !promptRunning || cancelDeadline != nil || pendingSteer != nil || !s.steeringSupported {
					message := "The requested turn is no longer available for steering"
					if !s.steeringSupported {
						message = "This harness does not support steering"
					}
					if err := s.reject(core.OutcomeReasonAdmissionRejected, command.RequestID, message); err != nil {
						return nil, true, err
					}
					continue
				}
				steer := startSteer(ctx, connection, s.sessionID, command.RequestID, command.SteeringPrompt)
				pendingSteer = &steer
				stopTimer(&steeringDeadline)
				steeringDeadline = time.NewTimer(30 * time.Second)
			case CommandRequestCancel:
				if command.SteeringPrompt != nil && pendingSteer != nil {
					if err := s.reject(core.OutcomeReasonAdmissionRejected, command.RequestID, "Steering is already pending"); err != nil {
						return nil, true, err
					}
					continue
				}
				clearImplementation()
				if !promptRunning {
					if err := applyCancel(ctx, connection, s.sessionID, command.RequestID, s.events, s.terminals, s.pending); err != nil {
						return nil, true, err
					}
					// No turn runs, so none stays withdrawn.
					s.pending.endWithdrawal()
					return nil, false, s.emit(core.RuntimeEventPromptFinished{RequestID: requestID, StopReason: "Cancelled"})
				}
				if command.SteeringPrompt != nil {
					if !s.steeringSupported || cancelDeadline != nil {
						if err := s.reject(core.OutcomeReasonAdmissionRejected, command.RequestID,
							"Steering is not available. The prompt remains queued; cancel the turn explicitly to apply it next."); err != nil {
							return nil, true, err
						}
					} else {
						steer := startSteer(ctx, connection, s.sessionID, command.RequestID, *command.SteeringPrompt)
						pendingSteer = &steer
					}
				} else {
					if err := applyCancel(ctx, connection, s.sessionID, command.RequestID, s.events, s.terminals, s.pending); err != nil {
						return nil, true, err
					}
					if cancelDeadline == nil {
						cancelDeadline = time.NewTimer(cancelAckTimeout)
					}
				}
			case CommandRequestClose:
				if err := connection.Notify(context.WithoutCancel(ctx), "session/cancel",
					schema.CancelNotification{SessionID: s.sessionID}); err != nil {
					if err := s.emit(core.RuntimeEventWarning{Message: "cancel ACP prompt before close: " + acpErrorDisplay(err)}); err != nil {
						return nil, true, err
					}
				}
				if err := s.emit(core.RuntimeEventCommandInterrupted{
					Reason:    core.OutcomeReasonRequestedCancellation,
					RequestID: requestID,
					Message:   "prompt interrupted because the session was closed",
				}); err != nil {
					return nil, true, err
				}
				outcome := acpCall(ctx, connection, "session/close", schema.CloseSessionRequest{SessionID: s.sessionID}, nil)
				if err := emitCloseOutcome(ctx, s.events, command.RequestID, outcome); err != nil {
					return nil, true, err
				}
				return nil, true, nil
			case CommandRequestPrompt:
				if err := s.reject(core.OutcomeReasonAdmissionRejected, command.RequestID, "a prompt is already running"); err != nil {
					return nil, true, err
				}
			case CommandRequestPromptAttachments:
				if err := s.reject(core.OutcomeReasonAdmissionRejected, command.RequestID, "a prompt is already running"); err != nil {
					return nil, true, err
				}
			case CommandRequestGoalControl:
				prepared, err := prepareControl(ctx, spec, s.events, &s.goalQuestion, s.configRecovery != nil, command.RequestID, command.Action)
				if err != nil {
					return nil, true, err
				}
				if prepared {
					expected := controlIdentity(spec, command.RequestID)
					s.goalControls.start(ctx, spec, s.events, connection, s.sessionID, command.RequestID, command.Action, expected)
				}
			case CommandRequestClearContext, CommandRequestRestoreExecutionMode, CommandRequestSetConfig:
				var id string
				switch command := command.(type) {
				case CommandRequestClearContext:
					id = command.RequestID
				case CommandRequestRestoreExecutionMode:
					id = command.RequestID
				case CommandRequestSetConfig:
					id = command.RequestID
				}
				if err := s.reject(core.OutcomeReasonAdmissionRejected, id,
					"configuration can only be changed while the agent is idle"); err != nil {
					return nil, true, err
				}
			case CommandRequestSetSessionMode:
				if err := s.reject(core.OutcomeReasonAdmissionRejected, command.RequestID,
					"the session mode can only be changed while the agent is idle"); err != nil {
					return nil, true, err
				}
			case CommandRequestResolveElicitation:
				var permission *PendingElicitation
				if promptRunning {
					permission = s.pending.takeTurnCancel(command.ElicitationID, &command.Response)
				}
				if permission != nil {
					// Cancel on a tool permission stops the turn, as an
					// interrupt does. Cancelling it answers the request
					// `cancelled`. With no turn running, the request alone is
					// cancelled.
					clearImplementation()
					outcome := cancelPromptTurn(ctx, connection, s.sessionID, s.terminals, s.pending)
					if outcome != nil {
						outcome = errors.New("cancel ACP prompt: " + acpErrorDisplay(outcome))
					}
					permission.cancel()
					if outcome == nil && cancelDeadline == nil {
						cancelDeadline = time.NewTimer(cancelAckTimeout)
					}
					if !sendResolution(command.Resolved, outcome) {
						slog.Debug("elicitation resolution receiver was already closed", "session_id", string(s.sessionID),
							"operation", "resolve_elicitation", "elicitation_id", command.ElicitationID)
					}
					continue
				}
				outcome, err := s.resolveRunning(command)
				if err != nil {
					return nil, true, err
				}
				if !sendResolution(command.Resolved, outcome) {
					slog.Debug("elicitation resolution receiver was already closed", "session_id", string(s.sessionID),
						"operation", "resolve_elicitation", "elicitation_id", command.ElicitationID)
				}
			case CommandRequestStopBackgroundTask:
				resolveBackgroundTaskStop(ctx, connection, s.sessionID, s.terminals, command.Target, command.Resolved)
			case CommandRequestReloadLaunchConfig:
				sendResolution(command.Resolved, errors.New("cannot reload launch configuration while a turn is active"))
				releaseReloadLeases(command.Leases)
			}
		case turnModeRestored:
			modeRestoration = nil
			stopTimer(&implementationDeadline)
			if restoredMode.err != nil {
				if err := s.emit(core.RuntimeEventWarning{
					Message: "Plan implementation stopped: could not restore the policy mode: " + restoredMode.err.Error(),
				}); err != nil {
					return nil, true, err
				}
				return nil, false, s.emit(core.RuntimeEventPromptFinished{RequestID: requestID, StopReason: promptErrorStopReason})
			}
			state := restoredMode.value
			s.configOptions = state.configOptions
			s.modes = state.modes
			if state.warning != nil {
				if err := s.emit(core.RuntimeEventWarning{Message: *state.warning}); err != nil {
					return nil, true, err
				}
			}
			if err := s.sessionConfigured(); err != nil {
				return nil, true, err
			}
			if err := s.sessionModesConfigured(); err != nil {
				return nil, true, err
			}
			continuation := "The user approved the following plan. Implement it now; the preceding permission cancellation was mj's mode-transition handling.\n\n" + state.plan
			updatesBefore = s.agentOutputCount.get()
			s.lastAgentMessage.clear()
			askedToCompact = false
			spec.StepClock.BeginTurn()
			dropActive()
			active, dropActive = acpStartCall[promptAnswer](ctx, connection, "session/prompt", schema.PromptRequest{
				SessionID: s.sessionID,
				Prompt:    []schema.ContentBlock{{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: continuation}}},
			})
			promptRunning = true
		}
	}
}

// resolveRunning answers a question while a prompt runs and the answer does
// not cancel its turn.
func (s *servedSession) resolveRunning(request CommandRequestResolveElicitation) (error, error) {
	answer, handled, err := resolve(s.ctx, s.connection, s.sessionID, s.spec, s.events, &s.goalQuestion,
		s.configRecovery != nil, request.ElicitationID, &request.Response)
	if err != nil {
		return nil, err
	}
	if handled {
		return answer, nil
	}
	answer, handled, err = resolveSessionConfigRecovery(s.ctx, s.connection, s.sessionID, s.spec, s.events,
		&s.configOptions, s.grokModels, &s.configRecovery, false, request.ElicitationID, &request.Response)
	if err != nil {
		return nil, err
	}
	if handled {
		return answer, nil
	}
	return resolvePendingElicitation(s.pending, request.ElicitationID, request.Response), nil
}

// promptFinished is the PromptFinished event of a prompt the harness
// answered, with the warnings its answer calls for emitted first.
func (s *servedSession) promptFinished(requestID string, answer asyncResult[promptAnswer], askedToCompact bool, updatesBefore uint64, loginFingerprint *string) (core.RuntimeEvent, error) {
	spec := s.spec
	harness := spec.Harness
	var usage *core.TokenUsage
	var diagnostic *core.TurnDiagnostic
	var stopReason string
	if answer.err == nil {
		response := answer.value
		var usageMeta schema.Meta
		if response.Usage != nil {
			usageMeta = response.Usage.Meta
			converted := core.TokenUsageFromACP(harness, *response.Usage)
			usage = &converted
		}
		if harness == core.HarnessKindClaude && usage != nil {
			if err := attachProviderDetails(usage, response.Meta); err != nil {
				usage.Scope = core.UsageScopeUnspecified
				if err := s.emit(core.RuntimeEventWarning{
					Message: "Claude per-model usage could not be recorded; turn accounting is incomplete: " + err.Error(),
				}); err != nil {
					return nil, err
				}
			}
		}
		if harness == core.HarnessKindMuse && usage != nil {
			attached := MuseUsageAttachProviderDetails(*usage, usageMeta)
			usage = &attached
		}
		if s.opencodeWholeTurns && usage != nil {
			if err := OpencodeUsageNormalize(usage, usageMeta); err != nil {
				if err := s.emit(core.RuntimeEventWarning{
					Message: "OpenCode usage could not be reconciled; turn accounting is incomplete: " + err.Error(),
				}); err != nil {
					return nil, err
				}
			}
		}
		if s.antigravityWholeTurns && usage != nil {
			if err := AntigravityUsageNormalize(usage, usageMeta); err != nil {
				if err := s.emit(core.RuntimeEventWarning{
					Message: "Antigravity usage could not be reconciled; turn accounting is incomplete: " + err.Error(),
				}); err != nil {
					return nil, err
				}
			}
		}
		if harness == core.HarnessKindGrok {
			reported, err := s.grokUsage.Complete(s.ctx, response.Meta, usage)
			if err == nil {
				usage = reported
			} else if err := s.emit(core.RuntimeEventWarning{Message: "Grok usage report was not recorded: " + err.Error()}); err != nil {
				return nil, err
			}
		}
		failure := core.SessionFailureFromMeta(response.Meta)
		if failure != nil && !failure.IsError() {
			failure = nil
		}
		stop := schema.StopReason(response.StopReason)
		switch {
		case failure != nil:
			// The bridge failed the turn; its stop reason says end_turn only
			// because ACP has no failed one.
			slog.Warn("prompt failed", "harness", harness.DisplayName(), "category", failure.Category, "title", failure.Title)
			failed := core.TurnDiagnosticFromSessionFailure(failure)
			reason, message := sessionFailureOutcome(&failed)
			if err := s.emit(core.RuntimeEventWarning{Message: message}); err != nil {
				return nil, err
			}
			diagnostic = &failed
			stopReason = reason
		case !askedToCompact && promptReturnedWithoutUpdates(stop, updatesBefore, s.agentOutputCount.get()):
			// The harness called this a successful turn but produced nothing
			// at all, so reporting it as finished would tell the person and
			// every waiting script that work happened (#970). Fail the turn
			// under its own stop reason, and leave the session serving so the
			// prompt can be resent by hand.
			message := promptUnansweredMessage(harness)
			if err := s.emit(core.RuntimeEventWarning{Message: message}); err != nil {
				return nil, err
			}
			code := core.PromptUnansweredStopReason
			diagnostic = &core.TurnDiagnostic{Message: message, Code: &code}
			stopReason = core.PromptUnansweredStopReason
		default:
			stopReason = stopReasonDebug(stop)
		}
	} else {
		s.grokUsage.Clear()
		rpc := acpRPCError(answer.err)
		// The raw error, JSON data and all, goes to the log; the
		// conversation gets one line.
		slog.Warn("prompt failed", "harness", harness.DisplayName(), "error", acpErrorText(rpc))
		failed := core.TurnDiagnosticFromACP(rpc)
		reason, warning := promptErrorOutcome(harness, rpc, &failed, s.lastAgentMessage.text())
		diagnostic = &failed
		if warning != nil {
			if err := s.emit(core.RuntimeEventWarning{Message: *warning}); err != nil {
				return nil, err
			}
		}
		stopReason = reason
	}
	if diagnostic != nil && core.TurnDiagnosticReportsAuthFailure(diagnostic) {
		diagnostic.LoginFingerprint = loginFingerprint
	}
	return core.RuntimeEventPromptFinished{
		RequestID:  requestID,
		StopReason: stopReason,
		Usage:      usage,
		Diagnostic: diagnostic,
	}, nil
}

func shouldReplaceUnusedCodexThread(spec *LaunchSpec) bool {
	return spec.Harness == core.HarnessKindCodex &&
		spec.ResumeSession != nil &&
		spec.NativeSessionHistory == NativeSessionHistoryCreatedHereUnused &&
		!codexHasGoal(spec)
}

func codexHasGoal(spec *LaunchSpec) bool {
	if spec.Harness != core.HarnessKindCodex {
		return false
	}
	goal, unlock := spec.GoalRecovery.Lock()
	defer unlock()
	return goal.State.Snapshot != nil
}
