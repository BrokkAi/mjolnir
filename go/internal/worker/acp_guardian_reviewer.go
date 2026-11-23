// Port of mj-worker/src/acp/guardian/reviewer.rs.

// The reviewer side of the Mjolnir guardian.
//
// A Guardian belongs to one bridge, and runBridge stops its reviewer after
// that bridge's own teardown on every exit path. On its first review it
// starts a second process from the session's launch, retaining the harness,
// login, and model in a reviewer-specific environment. Each review opens a
// fresh session there, so no review inherits another's history.
//
// The reviewer's launch is narrowed for it (see reviewerLaunch): an OpenCode
// reviewer gets none of the session's Mjolnir plugins, so it can never
// drain the session's mailbox, and its hook and rules let it read and
// nothing else. Every harness's reviewer runs under the session's execution
// enforcement, its connection advertises no client terminal, file system,
// or elicitation, Mjolnir passes it no MCP servers, and every permission
// request it raises is refused once.

package worker

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"sync"
	"sync/atomic"
	"time"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// maxReviewAttempts is Codex's `MAX_REVIEW_ATTEMPTS`: reviewer prompts per
// review, counting a retry on a fresh reviewer and a reminder for a reply
// without a verdict.
const maxReviewAttempts = 3

// reviewerExitGrace is how long a retiring reviewer bridge gets to reap its
// process group after its stdin closes, the grace runBridge gives the
// session's own bridge. Tests shorten it to 100 milliseconds (Rust
// cfg(test)).
var reviewerExitGrace = 5 * time.Second

// reviewerSupervisorSpec is the reviewer's supervisor spec, beside the
// session's (Unix workers only).
const reviewerSupervisorSpec = "acp-supervisor-guardian.json"

// HostSlot is a held lock on a guardian's reviewer host (Rust a tokio
// MutexGuard over `Option<Arc<ReviewerHost>>`). Release it when done.
type HostSlot struct {
	guardian *Guardian
}

// host is the slot's reviewer, nil when there is none.
func (s *HostSlot) host() **ReviewerHost {
	return &s.guardian.host
}

// release unlocks the slot.
func (s *HostSlot) release() {
	<-s.guardian.hostLock
}

// Guardian is the guardian for one bridge: the reviewer host it starts on
// demand and the launch it reviews for.
type Guardian struct {
	spec *LaunchSpec
	// hostLock holds one token while a review owns host; a review keeps it
	// across its awaits (Rust tokio::sync::Mutex).
	hostLock chan struct{}
	host     *ReviewerHost
	// endpoint is the worker's daemon guardian queue; nil reviews over ACP
	// only.
	endpoint *GuardianEndpoint
}

// guardianForLaunch is the guardian for spec, when its harness relies on
// Mjolnir's, its target keeps configured approvals, and its profile has not
// turned the guardian off; nil otherwise. The guardian keeps its own copy
// of the launch. A nil endpoint reviews over ACP only.
func guardianForLaunch(spec *LaunchSpec, endpoint *GuardianEndpoint) *Guardian {
	if !spec.Harness.MjolnirGuardianReviews(spec.ExecutionPolicy, spec.ApprovalChoices) {
		return nil
	}
	clone := spec.Clone()
	return &Guardian{spec: &clone, hostLock: make(chan struct{}, 1), endpoint: endpoint}
}

// lockHost waits for the host slot, or for ctx to end.
func (g *Guardian) lockHost(ctx context.Context) (*HostSlot, error) {
	select {
	case g.hostLock <- struct{}{}:
		return &HostSlot{guardian: g}, nil
	case <-ctx.Done():
		return nil, ctx.Err()
	}
}

// review reviews one permission request. A nil input is no input.
func (g *Guardian) review(ctx context.Context, request *schema.RequestPermissionRequest, input any, nativeChild bool) GuardianDecision {
	deadline := time.Now().Add(core.GuardianReviewTimeout)
	toolCallID := string(request.ToolCall.ToolCallID)
	if g.spec.TurnContext.GuardianTripped() {
		slog.Info("guardian denied several actions this turn; asking the person",
			"target", "mj_guardian", "tool_call_id", toolCallID)
		return GuardianDecisionAskPerson{Reason: "it denied several actions in this turn"}
	}
	action := core.PlannedAction(g.spec.Harness, request, input, g.spec.Cwd, nativeChild)
	cwd := g.spec.Cwd
	type collected struct {
		instructions *string
		failed       bool
	}
	result := make(chan collected, 1)
	go func() {
		defer func() {
			if recovered := recover(); recovered != nil {
				slog.Warn("could not collect project instructions", "target", "mj_guardian",
					"tool_call_id", toolCallID, "error", fmt.Sprint(recovered))
				result <- collected{failed: true}
			}
		}()
		if instructions, ok := core.CollectProjectInstructions(cwd); ok {
			result <- collected{instructions: &instructions}
		} else {
			result <- collected{}
		}
	}()
	var projectInstructions *string
	timer := time.NewTimer(time.Until(deadline))
	select {
	case outcome := <-result:
		timer.Stop()
		if outcome.failed {
			return GuardianDecisionAskPerson{Reason: "it could not collect project instructions"}
		}
		projectInstructions = outcome.instructions
	case <-timer.C:
		slog.Warn("collecting project instructions timed out", "target", "mj_guardian", "tool_call_id", toolCallID)
		return GuardianDecisionAskPerson{Reason: "collecting project instructions timed out"}
	case <-ctx.Done():
		timer.Stop()
		return GuardianDecisionAskPerson{Reason: "collecting project instructions timed out"}
	}
	if g.endpoint != nil {
		outcome, err := g.reviewRemotely(ctx, g.endpoint, toolCallID, action, projectInstructions, deadline)
		if err != nil {
			slog.Warn("guardian relay failed", "target", "mj_guardian", "tool_call_id", toolCallID, "error", err.Error())
			return GuardianDecisionAskPerson{Reason: "its review failed: " + err.Error()}
		}
		switch outcome := outcome.(type) {
		case core.GuardianReviewOutcomeUseAcpFallback:
		case core.GuardianReviewOutcomeAllow:
			return g.recordVerdict(nil)
		case core.GuardianReviewOutcomeDeny:
			return g.recordVerdict(&outcome.Rationale)
		case core.GuardianReviewOutcomeAskPerson:
			if outcome.Reason == core.GuardianDisabledReason {
				return GuardianDecisionAskPerson{Reason: outcome.Reason}
			}
			reason := core.SafeDaemonGuardianFailureReason(outcome.Reason)
			slog.Warn("daemon guardian review failed", "target", "mj_guardian", "tool_call_id", toolCallID, "reason", reason)
			return GuardianDecisionAskPerson{Reason: "its review failed: " + reason}
		}
	}
	return g.reviewOverACP(ctx, toolCallID, action, projectInstructions)
}

func (g *Guardian) reviewRemotely(
	ctx context.Context,
	endpoint *GuardianEndpoint,
	toolCallID string,
	action any,
	projectInstructions *string,
	deadline time.Time,
) (core.GuardianReviewOutcome, error) {
	evidence := g.collectEvidence(toolCallID, action, projectInstructions)
	for {
		result, err := endpoint.review(ctx, evidence.request, deadline)
		if err != nil {
			return nil, err
		}
		if _, fallback := result.Decision.(core.GuardianReviewOutcomeUseAcpFallback); fallback {
			return result.Decision, nil
		}
		current := g.collectEvidence(toolCallID, action, projectInstructions)
		if !current.personWroteSince(&evidence) {
			return result.Decision, nil
		}
		evidence = current
		slog.Info("the person wrote during a guardian review; reviewing again", "target", "mj_guardian")
	}
}

func (g *Guardian) reviewOverACP(ctx context.Context, toolCallID string, action any, projectInstructions *string) GuardianDecision {
	slot, err := g.lockHost(ctx)
	if err != nil {
		return GuardianDecisionAskPerson{Reason: "its review failed: the ACP reviewer could not complete the review"}
	}
	// Time spent waiting for the shared reviewer is outside the review
	// deadline.
	started := time.Now()
	reviewCtx, cancel := context.WithTimeout(ctx, core.GuardianReviewTimeout)
	assessment, err := g.decide(reviewCtx, slot, toolCallID, action, projectInstructions)
	timedOut := errors.Is(reviewCtx.Err(), context.DeadlineExceeded) && ctx.Err() == nil
	cancel()
	if err != nil && timedOut {
		slog.Warn("ACP guardian review timed out", "target", "mj_guardian",
			"harness", g.spec.Harness.DisplayName(), "tool_call_id", toolCallID)
		return GuardianDecisionAskPerson{Reason: fmt.Sprintf("the ACP review did not finish within %d seconds",
			int(core.GuardianReviewTimeout/time.Second))}
	}
	if err != nil {
		slog.Warn("ACP guardian review failed", "target", "mj_guardian",
			"harness", g.spec.Harness.DisplayName(), "tool_call_id", toolCallID,
			"elapsed_ms", time.Since(started).Milliseconds(), "error", err.Error())
		return GuardianDecisionAskPerson{Reason: "its review failed: the ACP reviewer could not complete the review"}
	}
	outcome := assessment.EnforcedOutcome()
	// Never the rationale or the reviewer's text: both can quote workspace
	// content. The transcript carries a denial's rationale.
	slog.Info("guardian reviewed a permission request", "target", "mj_guardian",
		"harness", g.spec.Harness.DisplayName(), "tool_call_id", toolCallID,
		"outcome", string(outcome), "reviewer_outcome", string(assessment.Outcome),
		"risk_level", string(assessment.RiskLevel), "user_authorization", string(assessment.UserAuthorization),
		"elapsed_ms", time.Since(started).Milliseconds())
	if outcome == core.OutcomeDeny {
		return g.recordVerdict(&assessment.Rationale)
	}
	return g.recordVerdict(nil)
}

// recordVerdict records the review on the turn's denial breaker; a nil
// rationale is an allow.
func (g *Guardian) recordVerdict(rationale *string) GuardianDecision {
	tripped := g.spec.TurnContext.RecordGuardianReview(rationale != nil)
	if rationale == nil {
		return GuardianDecisionAllow{}
	}
	text := *rationale
	if tripped {
		text += " The guardian has denied several actions in this turn, so it will ask you about the rest of the turn's requests."
	}
	return GuardianDecisionDeny{Rationale: text}
}

func (g *Guardian) collectEvidence(toolCallID string, action any, projectInstructions *string) ReviewEvidence {
	return reviewEvidenceCollect(g.spec.Harness, g.spec.TurnContext, toolCallID, action, projectInstructions)
}

// decide reviews action against the evidence as it stands. If the person
// wrote while it was reviewed, the verdict may rest on stale authorization,
// so it is reviewed again on what they now say, as Codex discards a stale
// verdict (`review_request.rs`). Each repeat follows a new message; the
// review deadline bounds them.
func (g *Guardian) decide(ctx context.Context, first *HostSlot, toolCallID string, action any, projectInstructions *string) (core.Assessment, error) {
	slot := first
	defer func() {
		if slot != nil {
			slot.release()
		}
	}()
	attempts := 0
	for {
		evidence := g.collectEvidence(toolCallID, action, projectInstructions)
		prompt := []schema.ContentBlock{
			reviewerText(core.ReviewerInstructionsACP()),
			reviewerText(evidence.request.RequestText()),
		}
		assessment, err := g.assess(ctx, &slot, prompt, &attempts)
		if err != nil {
			return core.Assessment{}, err
		}
		current := g.collectEvidence(toolCallID, action, projectInstructions)
		if !current.personWroteSince(&evidence) {
			return assessment, nil
		}
		slog.Info("the person wrote during a guardian review; reviewing again", "target", "mj_guardian")
	}
}

// reviewerText is a text content block.
func reviewerText(text string) schema.ContentBlock {
	return schema.ContentBlock{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: text}}
}

// assess runs the review's attempts and returns the first verdict. Every
// retry follows a change: a reviewer whose process died is replaced, and a
// reply without a verdict is asked for only the verdict. Any other failure
// would repeat, so it ends the review. *slot is the held host slot, or nil
// once released.
func (g *Guardian) assess(ctx context.Context, slot **HostSlot, prompt []schema.ContentBlock, attempts *int) (core.Assessment, error) {
	for {
		guard := *slot
		*slot = nil
		if guard == nil {
			var err error
			if guard, err = g.lockHost(ctx); err != nil {
				return core.Assessment{}, err
			}
		}
		host, err := g.currentHost(ctx, guard)
		guard.release()
		if err != nil {
			return core.Assessment{}, err
		}
		assessment, failure := reviewerHostUntilExit(ctx, host, func(ctx context.Context) (core.Assessment, error) {
			return g.assessOn(ctx, host, prompt, attempts)
		})
		if failure == nil {
			return assessment, nil
		}
		if host.alive() || *attempts >= maxReviewAttempts {
			return core.Assessment{}, failure
		}
		slog.Debug("guardian reviewer exited; retrying on a fresh one", "target", "mj_guardian", "error", failure.Error())
		g.discard(ctx, host)
	}
}

// assessOn is one reviewer session's attempts: the prompt, then the verdict
// reminder while replies carry none and attempts remain.
func (g *Guardian) assessOn(ctx context.Context, host *ReviewerHost, prompt []schema.ContentBlock, attempts *int) (core.Assessment, error) {
	*attempts++
	session, err := host.openSession(ctx, g.spec)
	if err != nil {
		return core.Assessment{}, err
	}
	defer session.Close()
	blocks := slices.Clone(prompt)
	for {
		reply, err := session.prompt(ctx, blocks)
		if err != nil {
			return core.Assessment{}, err
		}
		assessment, err := core.ParseAssessment(reply)
		if err == nil {
			return assessment, nil
		}
		if *attempts >= maxReviewAttempts {
			return core.Assessment{}, fmt.Errorf("the reviewer replied %d bytes without a verdict: %w", len(reply), err)
		}
		*attempts++
		slog.Debug("guardian reply carried no verdict; asking again", "target", "mj_guardian",
			"reply_bytes", len(reply), "error", err.Error())
		blocks = []schema.ContentBlock{reviewerText(verdictReminder)}
	}
}

// currentHost is the live reviewer in slot, started now if there is none.
func (g *Guardian) currentHost(ctx context.Context, slot *HostSlot) (*ReviewerHost, error) {
	current := slot.host()
	if *current != nil && (*current).alive() {
		return *current, nil
	}
	if dead := *current; dead != nil {
		*current = nil
		dead.shutdown(ctx)
	}
	host, err := reviewerHostStart(ctx, g.spec)
	if err != nil {
		return nil, err
	}
	*current = host
	return host, nil
}

// discard forgets host if it is still the current reviewer, and stops it.
func (g *Guardian) discard(ctx context.Context, host *ReviewerHost) {
	if slot, err := g.lockHost(context.WithoutCancel(ctx)); err == nil {
		if current := slot.host(); *current == host {
			*current = nil
		}
		slot.release()
	}
	host.shutdown(ctx)
}

// shutdown stops the reviewer with the bridge it belongs to.
func (g *Guardian) shutdown(ctx context.Context) {
	slot, err := g.lockHost(context.WithoutCancel(ctx))
	if err != nil {
		return
	}
	host := *slot.host()
	*slot.host() = nil
	slot.release()
	if host != nil {
		host.shutdown(ctx)
	}
}

// ReviewerLaunch is how the reviewer's bridge is started.
type ReviewerLaunch struct {
	command     string
	args        []string
	environment map[string]string
	// profile is the reviewer's private home, removed once the reviewer is
	// gone (Rust a TempDir); "" for none.
	profile string
}

// reviewerStagesProfile: Rust stages the reviewer's home on Unix only.
var reviewerStagesProfile = runtime.GOOS != "windows"

// reviewerLaunch is the session's launch, narrowed for a reviewer. A
// production bridge is the worker's ACP supervisor, which takes the harness
// environment from its spec file, so the reviewer gets a spec of its own
// beside the session's.
func reviewerLaunch(spec *LaunchSpec) (ReviewerLaunch, error) {
	if reviewerStagesProfile && spec.BridgeSpecPath != nil {
		path := *spec.BridgeSpecPath
		supervisor, err := AcpSupervisorSpecRead(path)
		if err != nil {
			return ReviewerLaunch{}, err
		}
		if supervisor.Environment, err = reviewerEnvironment(spec.Harness, supervisor.Environment); err != nil {
			return ReviewerLaunch{}, err
		}
		supervisor.ExcludedEnvironment = append(supervisor.ExcludedEnvironment, reviewerSessionEnvironment...)
		profile, err := reviewerProfileStage(spec.Harness, supervisor.Environment, supervisor.Args)
		if err != nil {
			return ReviewerLaunch{}, err
		}
		reviewerPath := filepath.Join(filepath.Dir(path), reviewerSupervisorSpec)
		if err := supervisor.writeSpec(reviewerPath); err != nil {
			reviewerRemoveProfile(profile)
			return ReviewerLaunch{}, err
		}
		args := slices.Clone(spec.Args)
		value := slices.Index(args, "--spec") + 1
		if value == 0 || value >= len(args) {
			reviewerRemoveProfile(profile)
			return ReviewerLaunch{}, errors.New("the session's bridge command names no supervisor spec")
		}
		args[value] = reviewerPath
		environment := maps.Clone(spec.Environment)
		if environment == nil {
			environment = map[string]string{}
		}
		reviewerRemoveSessionEnvironment(environment)
		return ReviewerLaunch{command: spec.Command, args: args, environment: environment, profile: profile}, nil
	}
	environment, err := reviewerEnvironment(spec.Harness, spec.Environment)
	if err != nil {
		return ReviewerLaunch{}, err
	}
	args := slices.Clone(spec.Args)
	profile := ""
	if reviewerStagesProfile {
		if profile, err = reviewerProfileStage(spec.Harness, environment, args); err != nil {
			return ReviewerLaunch{}, err
		}
	}
	return ReviewerLaunch{command: spec.Command, args: args, environment: environment, profile: profile}, nil
}

func reviewerRemoveProfile(profile string) {
	if profile != "" {
		if err := os.RemoveAll(profile); err != nil {
			slog.Warn("could not remove the guardian reviewer's profile", "target", "mj_guardian", "error", err.Error())
		}
	}
}

// reviewerEnvironment is the harness environment a reviewer runs with.
func reviewerEnvironment(harness core.HarnessKind, environment map[string]string) (map[string]string, error) {
	environment = maps.Clone(environment)
	if environment == nil {
		environment = map[string]string{}
	}
	reviewerRemoveSessionEnvironment(environment)
	switch harness {
	case core.HarnessKindOpenCode:
		if !reviewerStagesProfile {
			return nil, errors.New("an OpenCode reviewer needs a Unix worker")
		}
		return opencodeReviewerEnvironment(environment)
	case core.HarnessKindCodex:
		config := jsonvalue.Object{}
		if text, ok := environment["CODEX_CONFIG"]; ok {
			value, err := jsonvalue.Parse([]byte(text))
			object, isObject := value.(jsonvalue.Object)
			if err != nil || !isObject {
				if err == nil {
					err = errors.New("invalid type: expected a map")
				}
				return nil, fmt.Errorf("parse the Codex reviewer configuration: %w", err)
			}
			config = object
		}
		// serde_json's Map::remove with preserve_order.
		config.SwapDelete("hooks")
		config.SwapDelete("mcp_servers")
		config.SwapDelete("sandbox_mode")
		config.Set("default_permissions", ":read-only")
		config.Set("web_search", "disabled")
		featuresValue, present := config.Get("features")
		if !present {
			featuresValue = jsonvalue.Object{}
		}
		features, isObject := featuresValue.(jsonvalue.Object)
		if !isObject {
			return nil, errors.New("Codex reviewer features must be an object")
		}
		features = features.Clone()
		if features == nil {
			features = jsonvalue.Object{}
		}
		features.SwapDelete("codex_hooks")
		for _, key := range []string{"hooks", "apps", "plugins", "remote_plugin"} {
			features.Set(key, false)
		}
		config.Set("features", features)
		encoded, err := jsonvalue.Marshal(config)
		if err != nil {
			return nil, err
		}
		environment["CODEX_CONFIG"] = string(encoded)
		return environment, nil
	}
	return environment, nil
}

// Replies holds the text each reviewer session has sent as its latest agent
// message. The zero value is ready to use.
type Replies struct {
	mu      sync.Mutex
	replies map[string]*ReviewerReply
}

type ReviewerReply struct {
	messageID *string
	text      string
}

// observe records a session update (a JSON value) of a reviewer session.
func (r *Replies) observe(sessionID string, update any) {
	r.mu.Lock()
	defer r.mu.Unlock()
	reply, ok := r.replies[sessionID]
	if !ok {
		return
	}
	kind, _ := acpFeaturesString(update, "sessionUpdate")
	switch kind {
	case "agent_message_chunk":
		var messageID *string
		if id, ok := acpFeaturesString(update, "messageId"); ok {
			messageID = &id
		}
		// Only the latest message is the reply (muse-acp does the same).
		if (messageID == nil) != (reply.messageID == nil) || (messageID != nil && *messageID != *reply.messageID) {
			reply.text = ""
			reply.messageID = messageID
		}
		content, _ := acpFeaturesGet(update, "content")
		if contentType, _ := acpFeaturesString(content, "type"); contentType == "text" {
			if text, ok := acpFeaturesString(content, "text"); ok {
				reply.text += text
			}
		}
	case "tool_call":
		// A tool call ends the message before it; the verdict follows.
		reply.text = ""
		reply.messageID = nil
	}
}

func (r *Replies) open(sessionID string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.replies == nil {
		r.replies = map[string]*ReviewerReply{}
	}
	r.replies[sessionID] = &ReviewerReply{}
}

func (r *Replies) take(sessionID string) string {
	r.mu.Lock()
	defer r.mu.Unlock()
	reply, ok := r.replies[sessionID]
	if !ok {
		return ""
	}
	reply.messageID = nil
	text := reply.text
	reply.text = ""
	return text
}

func (r *Replies) close(sessionID string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	delete(r.replies, sessionID)
}

// ReviewerHost is a running reviewer bridge and its ACP connection. Close
// stops a host dropped without shutdown: its task closes the transport and
// reaps the bridge, logging what fails (Rust Drop).
type ReviewerHost struct {
	connection    *acp.Connection
	supportsClose bool
	replies       *Replies
	// stopped is done once the host is told to stop; stop tells it.
	stopped context.Context
	stop    context.CancelFunc
	// taskMu guards taskDone, closed once the bridge's supervising task has
	// ended, until shutdown takes it.
	taskMu   sync.Mutex
	taskDone chan struct{}
	// gone is done once the bridge or its connection is gone, by whichever
	// observer sees it first (see reviewerHostUntilExit); markGone records
	// it. The one owner of whether this reviewer is alive: a request to a
	// dead bridge may never be answered, so every request races it.
	gone     context.Context
	markGone context.CancelFunc
}

// reviewerHostStart starts the reviewer bridge for spec and initializes its
// connection.
func reviewerHostStart(ctx context.Context, spec *LaunchSpec) (*ReviewerHost, error) {
	launch, err := reviewerLaunch(spec)
	if err != nil {
		return nil, err
	}
	command := exec.Command(launch.command, launch.args...)
	command.Env = make([]string, 0, len(launch.environment))
	for _, name := range slices.Sorted(maps.Keys(launch.environment)) {
		command.Env = append(command.Env, name+"="+launch.environment[name])
	}
	command.Dir = spec.Cwd
	// The worker keeps its own ends of the pipes, so reaping the child never
	// closes a stream the connection still reads.
	stdinRead, stdinWrite, err := os.Pipe()
	if err != nil {
		reviewerRemoveProfile(launch.profile)
		return nil, err
	}
	stdoutRead, stdoutWrite, err := os.Pipe()
	if err != nil {
		stdinRead.Close()
		stdinWrite.Close()
		reviewerRemoveProfile(launch.profile)
		return nil, err
	}
	stderrRead, stderrWrite, err := os.Pipe()
	if err != nil {
		stdinRead.Close()
		stdinWrite.Close()
		stdoutRead.Close()
		stdoutWrite.Close()
		reviewerRemoveProfile(launch.profile)
		return nil, err
	}
	command.Stdin, command.Stdout, command.Stderr = stdinRead, stdoutWrite, stderrWrite
	// Register the bridge with the worker's orphan reaper as it spawns, so
	// the reaper never takes the exit status its owner awaits.
	err = core.SpawnChild(command)
	stdinRead.Close()
	stdoutWrite.Close()
	stderrWrite.Close()
	if err != nil {
		stdinWrite.Close()
		stdoutRead.Close()
		stderrRead.Close()
		reviewerRemoveProfile(launch.profile)
		return nil, fmt.Errorf("start the reviewer's %s bridge %s: %w", spec.Harness.DisplayName(), launch.command, err)
	}
	waited := make(chan error, 1)
	go func() { waited <- command.Wait() }()

	replies := &Replies{}
	stopped, stop := context.WithCancel(context.Background())
	gone, markGone := context.WithCancel(context.Background())
	connection := acp.Connect(skipStdoutPreamble(stdoutRead), stdinWrite,
		func(ctx context.Context, method string, params json.RawMessage) (any, error) {
			if method == schema.SessionRequestPermissionMethodName {
				var request schema.RequestPermissionRequest
				if err := json.Unmarshal(params, &request); err != nil {
					return nil, &acp.RPCError{Code: rpcCodeInvalidParams, Message: "Invalid params"}
				}
				slog.Debug("refused a guardian reviewer's tool request", "target", "mj_guardian",
					"tool_call_id", string(request.ToolCall.ToolCallID))
				return rejectionResponse(&request), nil
			}
			// The reviewer gets no client terminal, file system, or
			// elicitation. Every request is answered, because an unhandled
			// one that carries a session id would be parked.
			return nil, rpcErrorWithData(rpcCodeMethodNotFound, "Method not found", method)
		},
		func(method string, params json.RawMessage) error {
			if method != schema.SessionUpdateMethodName {
				return nil
			}
			var notification struct {
				SessionID string          `json:"sessionId"`
				Update    json.RawMessage `json:"update"`
			}
			if err := json.Unmarshal(params, &notification); err != nil {
				return nil
			}
			update, err := jsonvalue.Parse(notification.Update)
			if err != nil {
				return nil
			}
			replies.observe(notification.SessionID, update)
			return nil
		})
	harness := spec.Harness
	taskDone := make(chan struct{})
	go func() {
		defer close(taskDone)
		defer reviewerRemoveProfile(launch.profile)
		stderrTail := make(chan string, 1)
		go func() {
			tail, err := readStderrTail(stderrRead)
			stderrRead.Close()
			if err != nil {
				tail = fmt.Sprintf("(stderr unreadable: %s)", err)
			}
			stderrTail <- tail
		}()
		reaped := false
		ended := "connection closed"
		select {
		case <-connection.Done():
			if err := connection.Err(); err != nil {
				ended = err.Error()
			}
		case err := <-waited:
			reaped = true
			if err != nil {
				ended = "exited with " + err.Error()
			} else {
				ended = "exited with exit status: 0"
			}
		case <-stopped.Done():
		}
		markGone()
		// Drop the transport: the bridge's stdin closes and the connection's
		// reader ends. The transport is gone, so the supervisor's stdin is
		// closed: give it the session bridge's grace to reap its process
		// group, then kill it.
		_ = stdinWrite.Close()
		_ = stdoutRead.Close()
		_ = connection.Close()
		if !reaped {
			select {
			case <-waited:
			case <-time.After(reviewerExitGrace):
				if err := command.Process.Kill(); err != nil {
					slog.Warn("could not kill the guardian reviewer bridge", "target", "mj_guardian", "error", err.Error())
				}
				<-waited
			}
		}
		tail := <-stderrTail
		// An exit nobody asked for is a reviewer failure; its stderr says why
		// (a login that lapsed, a harness that would not start).
		if stopped.Err() == nil {
			actionable, _ := actionableStderrTail(tail)
			slog.Warn("guardian reviewer bridge stopped unexpectedly", "target", "mj_guardian",
				"harness", harness.DisplayName(), "ended", ended, "stderr", actionable)
		}
	}()
	type initialization struct {
		supportsClose bool
		err           error
	}
	initialized := make(chan initialization, 1)
	go func() {
		supportsClose, err := initializeReviewer(ctx, connection, spec)
		initialized <- initialization{supportsClose, err}
	}()
	var outcome initialization
	select {
	case outcome = <-initialized:
	case <-gone.Done():
		outcome.err = errors.New("the reviewer's bridge exited during initialization")
	case <-ctx.Done():
		// A start that is dropped (its review was cancelled or timed out)
		// must still stop the bridge it spawned.
		outcome.err = ctx.Err()
	}
	if outcome.err != nil {
		stop()
		<-taskDone
		return nil, outcome.err
	}
	return &ReviewerHost{
		connection:    connection,
		supportsClose: outcome.supportsClose,
		replies:       replies,
		stopped:       stopped,
		stop:          stop,
		taskDone:      taskDone,
		gone:          gone,
		markGone:      markGone,
	}, nil
}

func (h *ReviewerHost) alive() bool {
	return h.gone.Err() == nil
}

// reviewerHostUntilExit runs work against host, failing it if the bridge
// exits.
//
// A request can learn that the bridge is gone before this host's task does:
// the connection fails it once its transport closed. That failure is
// recorded in gone before it is returned, so the decision to retry, which
// reads only gone, sees the death whichever observer saw it first.
func reviewerHostUntilExit[T any](ctx context.Context, host *ReviewerHost, work func(ctx context.Context) (T, error)) (T, error) {
	workCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	type outcome struct {
		value T
		err   error
	}
	done := make(chan outcome, 1)
	go func() {
		value, err := work(workCtx)
		done <- outcome{value, err}
	}()
	var result outcome
	select {
	case result = <-done:
	case <-host.gone.Done():
		// Rust drops the work; it ends here before the retry decision.
		cancel()
		<-done
		var zero T
		result = outcome{zero, errors.New("the reviewer's bridge exited")}
	}
	if result.err != nil {
		select {
		case <-host.connection.Done():
			host.markGone()
		default:
		}
	}
	return result.value, result.err
}

// openSession opens a reviewer session configured like the reviewed one.
func (h *ReviewerHost) openSession(ctx context.Context, spec *LaunchSpec) (*ReviewerSession, error) {
	request := schema.NewSessionRequest{Cwd: spec.Cwd, MCPServers: []schema.McpServer{}}
	if spec.Harness == core.HarnessKindClaude {
		request.Meta = jsonvalue.Object{{Key: "claudeCode", Value: jsonvalue.Object{{Key: "options", Value: jsonvalue.Object{
			{Key: "settingSources", Value: []any{"user"}},
			{Key: "settings", Value: jsonvalue.Object{{Key: "disableAllHooks", Value: true}}},
			{Key: "strictMcpConfig", Value: true},
			{Key: "tools", Value: []any{"Read", "Glob", "Grep"}},
		}}}}}
	}
	var created schema.NewSessionResponse
	if err := h.connection.Call(ctx, schema.SessionNewMethodName, request, &created); err != nil {
		return nil, fmt.Errorf("open a reviewer session: %w", sessionConfigCallError(err))
	}
	session := &ReviewerSession{host: h, id: created.SessionID}
	h.replies.open(string(session.id))
	options := created.ConfigOptions
	modes := created.Modes
	// The reviewed session's policy, so whatever would ask there asks here
	// and is refused.
	refusal, err := reassertExecutionPolicy(ctx, h.connection, session.id, spec.Harness,
		sessionEnforcement(spec), &options, modes, PolicySyncAlways)
	if err != nil {
		session.Close()
		return nil, err
	}
	if refusal != nil {
		session.Close()
		return nil, fmt.Errorf("the reviewer session refused the session's execution policy: %w", refusal.error)
	}
	// The session's model reviews, never silently another one: a session
	// whose harness accepted a model selection must find that model here.
	selectable := core.FindSessionConfigOption(options, "model") != nil
	if model := spec.StartupModel(); model != nil {
		if selectable {
			if current, ok := sessionModel(options); !ok || current != *model {
				if _, err := setSessionConfig(ctx, h.connection, session.id, &options, spec.Harness, "model", *model); err != nil {
					session.Close()
					return nil, fmt.Errorf("select the session's model %s for the reviewer: %w", sessionConfigDebugString(*model), err)
				}
			}
		} else {
			accepted, unlock := spec.AcceptedConfig.Lock()
			hasAccepted := accepted.Model != nil
			unlock()
			if hasAccepted {
				session.Close()
				return nil, fmt.Errorf("the reviewer's %s session offers no model selector, so it cannot run on the session's model %s",
					spec.Harness.DisplayName(), sessionConfigDebugString(*model))
			}
		}
	}
	// A review is a bounded judgement: Codex and muse-acp both run their
	// reviewer at low effort where the model offers it.
	if selectorValueIsOffered(options, "effort", "low") {
		if _, err := setSessionConfig(ctx, h.connection, session.id, &options, spec.Harness, "effort", "low"); err != nil {
			session.Close()
			return nil, fmt.Errorf("select low effort for the reviewer: %w", err)
		}
	}
	return session, nil
}

func (h *ReviewerHost) shutdown(ctx context.Context) {
	h.stop()
	h.taskMu.Lock()
	done := h.taskDone
	h.taskDone = nil
	h.taskMu.Unlock()
	if done != nil {
		<-done
	}
}

// initializeReviewer initializes ACP without client terminal, file system,
// or elicitation, and reports whether the bridge can close sessions.
func initializeReviewer(ctx context.Context, connection *acp.Connection, spec *LaunchSpec) (bool, error) {
	disabled := false
	title := "Mjolnir guardian"
	var initialized schema.InitializeResponse
	if err := connection.Call(ctx, schema.InitializeMethodName, schema.InitializeRequest{
		ProtocolVersion: 1,
		ClientInfo:      &schema.Implementation{Name: clientName, Title: &title, Version: core.PackageVersion},
		ClientCapabilities: &schema.ClientCapabilities{
			Fs:       &schema.FileSystemCapabilities{ReadTextFile: &disabled, WriteTextFile: &disabled},
			Terminal: &disabled,
		},
	}, &initialized); err != nil {
		return false, fmt.Errorf("initialize the reviewer's %s bridge: %w", spec.Harness.DisplayName(), sessionConfigCallError(err))
	}
	if initialized.ProtocolVersion != 1 {
		return false, fmt.Errorf("the reviewer's bridge negotiated unsupported protocol %d", initialized.ProtocolVersion)
	}
	capabilities := initialized.AgentCapabilities
	return capabilities != nil && capabilities.SessionCapabilities != nil && capabilities.SessionCapabilities.Close != nil, nil
}

// Close stops the host (Rust Drop).
func (h *ReviewerHost) Close() {
	h.stop()
}

// ReviewerSession is one review's session on the reviewer bridge. Closing
// it cancels a turn still running, as a cancelled or timed-out review leaves
// one, and closes the session in the background, outside any review's
// deadline (Rust Drop).
type ReviewerSession struct {
	host      *ReviewerHost
	id        schema.SessionId
	prompting atomic.Bool
}

// prompt sends one prompt and returns the reviewer's latest message.
func (s *ReviewerSession) prompt(ctx context.Context, blocks []schema.ContentBlock) (string, error) {
	s.prompting.Store(true)
	var response schema.PromptResponse
	if err := s.host.connection.Call(ctx, schema.SessionPromptMethodName, schema.PromptRequest{SessionID: s.id, Prompt: blocks}, &response); err != nil {
		return "", fmt.Errorf("run the reviewer's turn: %w", sessionConfigCallError(err))
	}
	s.prompting.Store(false)
	reply := s.host.replies.take(string(s.id))
	if response.StopReason != schema.StopReasonEndTurn {
		return "", fmt.Errorf("the reviewer's turn ended with %s", verdictDebugVariant(string(response.StopReason)))
	}
	return reply, nil
}

func (s *ReviewerSession) Close() {
	s.host.replies.close(string(s.id))
	if !s.host.alive() {
		return
	}
	if s.prompting.Load() {
		if err := s.host.connection.Notify(context.Background(), schema.SessionCancelMethodName, schema.CancelNotification{SessionID: s.id}); err != nil {
			slog.Debug("could not cancel an abandoned guardian review", "target", "mj_guardian", "error", err.Error())
		}
	}
	if s.host.supportsClose {
		connection, id := s.host.connection, s.id
		go func() {
			var closed json.RawMessage
			if err := connection.Call(context.Background(), schema.SessionCloseMethodName, schema.CloseSessionRequest{SessionID: id}, &closed); err != nil {
				slog.Debug("could not close a guardian reviewer session", "target", "mj_guardian", "error", err.Error())
			}
		}()
	}
}
