// Port of mj-worker/src/worker_runtime/unix.rs.
//
// The Unix worker daemon: it recovers the durable relay, publishes the
// control socket, prepares and starts the harness under its own supervisor,
// and serves relay connections until the session closes or the worker stops.
//
// Rust's tokio primitives map onto Go as follows in this module and its
// submodules: a CancellationToken is a context.Context (observed) together
// with its context.CancelFunc (cancelled); a JoinHandle is a
// *core.JoinHandle, aborted through the cancel function of the context its
// task runs under; a watch channel is a *core.Watch; a TaskTracker is a
// *sync.WaitGroup. The ACP command channel and the dispatch wake channel are
// commandChannel and dispatchWakeChannel below, because their senders must
// see a receiver that is gone, and dispatch must reserve command capacity
// before it claims durable work.

//go:build unix

package worker

import (
	"bufio"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"maps"
	"net"
	"os"
	"os/signal"
	"path/filepath"
	"slices"
	"strconv"
	"sync"
	"syscall"
	"time"
	"weak"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

// acpEventChannelCapacity is the ACP runtime's event channel capacity.
const acpEventChannelCapacity = 256

// localPreparationStepTimeout bounds one local preparation step. Cold local
// filesystems can take minutes to traverse under host pressure. Subagent
// profile, project memory, ACP setup, and bridge start each get three
// minutes, with no local preparation stage exceeding five minutes.
const localPreparationStepTimeout = 3 * time.Minute

// acpCommandChannelCapacity is the ACP command channel's capacity (Rust:
// mpsc::channel(32) in build_acp_setup).
const acpCommandChannelCapacity = 32

// preparationSnapshot is what the preparation supervisor publishes to the
// connection handlers: where preparation is, and once Started, the services
// the prepared harness offers.
type preparationSnapshot struct {
	// state is nil (Rust's None) only for handlers with no primary
	// preparation supervisor.
	state    core.HarnessPreparation
	services *preparedConnectionServices
	// stepDeadline is the current step's own bound, which also bounds the
	// threads it starts; zero when there is none.
	stepDeadline time.Time
}

// isPending is whether preparation has not yet published Started.
func (s *preparationSnapshot) isPending() bool {
	_, started := s.state.(core.HarnessPreparationStarted)
	return !started
}

// preparedConnectionServices are the live services a prepared harness offers
// each relay connection.
type preparedConnectionServices struct {
	commands  *commandChannel
	reviewer  *reviewCapture
	subagents *SubagentEndpoint
}

// runningHarness is a started bridge and its relay coordinator.
type runningHarness struct {
	acpTask *core.JoinHandle[error]
	// eventTask is the relay coordinator; eventAbort aborts it.
	eventTask           *core.JoinHandle[error]
	eventAbort          context.CancelFunc
	acpShutdown         context.CancelFunc
	commands            *commandChannel
	reviewer            *reviewCapture
	subagentSocketGuard *subagentSocketGuard
	subagents           *SubagentEndpoint
	// harnessGC aborts the managed harness garbage collector; nil when none
	// runs.
	harnessGC    context.CancelFunc
	shellCleanup *sync.WaitGroup
	// managedHarness and managedNode are runtime leases kept in long-lived
	// ACP state so they cover supervisor replacement gaps as well as the
	// lifetime of any individual supervisor.
	managedHarness *harnessinstall.ManagedHarness
	managedNode    *harnessinstall.ManagedNodeRuntime
	// projectTools is stopped when the harness ends, before a closed session
	// is checkpointed.
	projectTools *backgroundInstall
}

// releaseLeases drops the runtime leases the running harness holds (Rust:
// the fields' Drop).
func (h *runningHarness) releaseLeases() {
	if h.managedHarness != nil {
		h.managedHarness.Release()
		h.managedHarness = nil
	}
	if h.managedNode != nil {
		h.managedNode.Release()
		h.managedNode = nil
	}
}

// preparationOutcome is how a harness preparation ended. There is no failed
// state to serve: a harness that cannot start ends the worker.
type preparationOutcome interface{ isPreparationOutcome() }

// preparationOutcomeStarted is a started harness.
type preparationOutcomeStarted struct{ Value *runningHarness }

// preparationOutcomeCancelled is a Close that sealed the relay, or a worker
// stopping, before the bridge started. Value keeps request-side dispatch
// wakes deliverable while the sealed relay is served without a harness; nil
// when preparation no longer owned them.
type preparationOutcomeCancelled struct{ Value *dispatchWakeChannel }

// preparationOutcomeFailed is a harness that cannot start. The error carries
// a core.WorkerHarnessStartupFailure for the exit record.
type preparationOutcomeFailed struct{ Value error }

func (preparationOutcomeStarted) isPreparationOutcome()   {}
func (preparationOutcomeCancelled) isPreparationOutcome() {}
func (preparationOutcomeFailed) isPreparationOutcome()    {}

// preparationStop is why supervisePreparation did not return a prepared
// value; nil means it did.
type preparationStop interface{ isPreparationStop() }

type preparationStopCancelled struct{}

type preparationStopFailed struct{ Value error }

func (preparationStopCancelled) isPreparationStop() {}
func (preparationStopFailed) isPreparationStop()    {}

// harnessPreparationSetup is everything the harness preparation owns.
type harnessPreparationSetup struct {
	root                    string
	config                  core.WorkerLaunchConfig
	relay                   *RelayCell
	restarting              bool
	sessionGitConfigInclude *string
	credentials             credentialEndpointResult
	kimiTaskHome            *kimiHome
	antigravityTaskHome     *kimiHome
	resumeSession           *string
	nativeSessionHistory    NativeSessionHistory
	// capture is what a sub-agent's worker observes at the end of each turn.
	capture *WorkObserver
}

// credentialEndpointResult is Rust's Result<CredentialEndpoint, String>: the
// endpoint this relay's credential and skills requests use, or why the launch
// config cannot locate harness credentials.
type credentialEndpointResult struct {
	endpoint CredentialEndpoint
	// err is set when there is no endpoint; its text is the reason.
	err error
}

// kimiHome is Rust's Result<PathBuf, String> for the Kimi home the task
// monitor reads: the path, or why it cannot be located.
type kimiHome struct {
	path string
	// err is set when there is no path; its text is the reason.
	err error
}

// preparationStepBudget is one preparation step's deadline.
type preparationStepBudget struct {
	step     string
	deadline time.Time
	timeout  time.Duration
}

func newPreparationStepBudget(step string, timeout time.Duration) preparationStepBudget {
	return preparationStepBudget{step: step, deadline: time.Now().Add(timeout), timeout: timeout}
}

func (b *preparationStepBudget) remaining() time.Duration {
	return max(time.Until(b.deadline), 0)
}

// deadlineError reports the step exceeding its deadline, with the timeout in
// Rust's Duration Debug form.
func (b *preparationStepBudget) deadlineError() error {
	return fmt.Errorf("worker preparation step %s exceeded its %s deadline", b.step, preparationDurationDebug(b.timeout))
}

// preparationDurationDebug formats a duration as Rust's `{:?}` does: "180s",
// "200ms", "1.5s", "20µs".
func preparationDurationDebug(duration time.Duration) string {
	nanos := uint64(max(duration, 0))
	format := func(integer, fractional, divisor uint64, suffix string) string {
		text := strconv.FormatUint(integer, 10)
		if fractional > 0 {
			digits := ""
			for fractional > 0 {
				digits += strconv.FormatUint(fractional/divisor, 10)
				fractional %= divisor
				divisor /= 10
			}
			text += "." + digits
		}
		return text + suffix
	}
	switch {
	case nanos >= 1_000_000_000:
		return format(nanos/1_000_000_000, nanos%1_000_000_000, 100_000_000, "s")
	case nanos >= 1_000_000:
		return format(nanos/1_000_000, nanos%1_000_000, 100_000, "ms")
	case nanos >= 1_000:
		return format(nanos/1_000, nanos%1_000, 100, "µs")
	default:
		return strconv.FormatUint(nanos, 10) + "ns"
	}
}

// acpPreparationSetup is what building the ACP setup consumes.
type acpPreparationSetup struct {
	root                 string
	config               core.WorkerLaunchConfig
	sessionEnvironment   map[string]string
	supervisorSpec       AcpSupervisorSpec
	relay                *RelayCell
	resumeSession        *string
	nativeSessionHistory NativeSessionHistory
	profileRegistration  bool
	// managedSubagent is read from the launch before preparation strips the
	// marker from the harness environment, so the ACP runtime knows the
	// session is a child.
	managedSubagent bool
	subagentRole    *core.SubagentMcpRole
	// runtime is Rust's tokio runtime Handle the bridge tasks are spawned on.
	runtime          *core.Runtime
	managedCacheRoot *string
}

// preparedAcpSetup is a built ACP setup, ready for its bridge to start.
type preparedAcpSetup struct {
	commandsTx          *commandChannel
	commandsRx          <-chan CommandRequest
	eventsTx            chan<- core.RuntimeEvent
	eventsRx            <-chan core.RuntimeEvent
	userShells          *UserShellRegistry
	reviewer            *reviewCapture
	subagents           *SubagentEndpoint
	subagentSocketGuard *subagentSocketGuard
	acpSpec             LaunchSpec
	shellCleanup        *sync.WaitGroup
	managedCacheRoot    *string
	harness             core.HarnessKind
}

// startedBridgeTasks holds a bridge's tasks while it starts. Close (Rust's
// Drop) stops whatever intoRunning has not taken.
type startedBridgeTasks struct {
	acpShutdown         context.CancelFunc
	acpTask             *core.JoinHandle[error]
	eventTask           *core.JoinHandle[error]
	eventAbort          context.CancelFunc
	harnessGC           context.CancelFunc
	commands            *commandChannel
	reviewer            *reviewCapture
	subagentSocketGuard *subagentSocketGuard
	subagents           *SubagentEndpoint
	shellCleanup        *sync.WaitGroup
}

// intoRunning hands the started tasks to a runningHarness.
func (t *startedBridgeTasks) intoRunning() *runningHarness {
	if t.acpShutdown == nil || t.acpTask == nil {
		panic("ACP task was started")
	}
	if t.eventTask == nil {
		panic("relay coordinator was started")
	}
	if t.commands == nil {
		panic("ACP command sender was prepared")
	}
	if t.reviewer == nil {
		panic("reviewer was prepared")
	}
	if t.shellCleanup == nil {
		panic("shell tracker was prepared")
	}
	running := &runningHarness{
		acpShutdown: t.acpShutdown, acpTask: t.acpTask,
		eventTask: t.eventTask, eventAbort: t.eventAbort,
		harnessGC: t.harnessGC, commands: t.commands, reviewer: t.reviewer,
		subagentSocketGuard: t.subagentSocketGuard, subagents: t.subagents,
		shellCleanup: t.shellCleanup,
	}
	*t = startedBridgeTasks{}
	return running
}

// Close is the Drop of startedBridgeTasks: it stops every task not yet handed
// to a runningHarness.
func (t *startedBridgeTasks) Close() {
	if t.acpShutdown != nil {
		t.acpShutdown()
	}
	if t.eventAbort != nil {
		t.eventAbort()
	}
	if t.harnessGC != nil {
		t.harnessGC()
	}
	t.acpTask, t.eventTask, t.eventAbort, t.harnessGC = nil, nil, nil, nil
}

// buildACPSetup writes the ACP supervisor spec and builds the launch the
// bridge starts from.
func buildACPSetup(setup acpPreparationSetup) (*preparedAcpSetup, error) {
	config := setup.config
	relay := setup.relay
	root := setup.root
	commands := newCommandChannel(acpCommandChannelCapacity)
	events := make(chan core.RuntimeEvent, acpEventChannelCapacity)
	userShells := NewUserShellRegistry(config.Cwd, maps.Clone(setup.sessionEnvironment), events)
	guard := relay.Lock()
	state := guard.OperationalState()
	guard.Unlock()
	acceptedConfig := core.AcceptedSessionConfigFromConfiguration(state.Config, state.ConfigOptions)
	supervisorPath := filepath.Join(root, "acp-supervisor.json")
	if err := setup.supervisorSpec.writeSpec(supervisorPath); err != nil {
		return nil, err
	}
	reviewTools := config.ReviewTools()
	workerExecutable, err := os.Executable()
	if err != nil {
		return nil, fmt.Errorf("locate Hel worker executable: %w", err)
	}
	reviewer := &reviewCapture{
		SessionID:             config.SessionID,
		Cwd:                   config.Cwd,
		AdditionalDirectories: reviewerWorkspaceDirectories(&config),
		PrimaryRelay:          relay,
	}
	var subagents *SubagentEndpoint
	var subagentGuard *subagentSocketGuard
	if setup.subagentRole != nil {
		subagents, subagentGuard, err = subagentsServe(setup.runtime, root, relay, config.Harness, config.SubagentKind)
		if err != nil {
			return nil, err
		}
	}

	guard = relay.Lock()
	acpActivity := guard.AcpActivityClock()
	stepClock := guard.StepClock()
	toolsInFlight := guard.ToolsInFlight()
	turnContext := guard.TurnContext()
	loginIdentity := guard.loginIdentity
	operatorAway := guard.operatorAway
	guard.Unlock()
	accepted := NewShared(acceptedConfig)
	shellCleanup := userShells.completionTracker()
	if request := config.GoalResumeRequest; request != nil {
		if err := func() error {
			guard := relay.Lock()
			defer guard.Unlock()
			if answered := guard.OperationalState().Goal.AnsweredResume; answered != nil && *answered == *request {
				return nil
			}
			encoded, err := json.Marshal(map[string]any{
				"sessionUpdate": "session_info_update",
				"_meta":         map[string]any{"mjGoalResumePending": *request},
			})
			if err != nil {
				return err
			}
			var update acpschema.SessionUpdate
			if err := json.Unmarshal(encoded, &update); err != nil {
				return err
			}
			_, err = guard.RecordSessionUpdate(update)
			return err
		}(); err != nil {
			return nil, err
		}
	}
	guard = relay.Lock()
	goal := guard.OperationalState().Goal
	guard.Unlock()
	goalRecovery := NewShared(core.GoalRecoveryContext{
		State:   goal,
		Request: config.GoalResumeRequest,
		Journal: func(update acpschema.SessionUpdate) error {
			guard := relay.Lock()
			defer guard.Unlock()
			_, err := guard.RecordSessionUpdate(update)
			return err
		},
	})
	additionalDirectories := acpAdditionalDirectories(&config)
	extendWritableDirectories(config.Harness, setup.sessionEnvironment, &additionalDirectories)
	var extraMCPServers []ReviewerMcpServer
	if config.ReviewerLaunch != nil {
		for _, server := range config.ReviewerLaunch.MCPServers {
			extraMCPServers = append(extraMCPServers, NewReviewerMcpServer(server))
		}
	}
	subagentPolicy := config.Subagents
	if config.HandbackTool {
		subagentPolicy = core.SubagentPolicy{Mode: core.SubagentPolicyNone}
	}
	var subagentSocket *SubagentMcpSocket
	if setup.subagentRole != nil {
		subagentSocket = &SubagentMcpSocket{
			Path:                 filepath.Join(root, SubagentSocket),
			Role:                 *setup.subagentRole,
			Oracle:               config.Oracle,
			OrchestratorsEnabled: config.OrchestratorsEnabled,
			SubtleSubagents:      config.SubtleSubagents,
			ReviewTools:          reviewTools,
			ProfileRegistration:  setup.profileRegistration,
		}
	}
	terminalOwner := weak.Make(relay)
	acpSpec := LaunchSpec{
		OperatorAway:  operatorAway,
		LoginIdentity: loginIdentity,
		GoalRecovery:  goalRecovery,
		Command:       workerExecutable,
		Args: []string{
			"--login-environment-ready",
			"worker",
			"acp-supervisor",
			"--spec",
			supervisorPath,
		},
		Environment:           setup.sessionEnvironment,
		BridgeSpecPath:        &supervisorPath,
		Cwd:                   config.Cwd,
		AdditionalDirectories: additionalDirectories,
		ExtraMCPServers:       extraMCPServers,
		SubagentPolicy:        subagentPolicy,
		ManagedSubagent:       setup.managedSubagent,
		InstructionRole:       core.InstructionRoleFromLaunch(&config),
		SubagentMCPSocket:     subagentSocket,
		ProjectMemory:         config.ProjectMemory,
		ResumeSession:         setup.resumeSession,
		NativeSessionHistory:  setup.nativeSessionHistory,
		AcceptedConfig:        accepted,
		InitialModel:          config.InitialModel,
		Harness:               config.Harness,
		ExecutionPolicy:       config.ExecutionPolicy,
		// The profile's approval choices the controller resolved into this
		// launch (Rust: WorkerLaunchConfig::approval_choices).
		ApprovalChoices: core.ApprovalChoices{
			ClaudePermissionMode: config.ClaudePermissionMode,
			MjolnirGuardian:      config.MjolnirGuardian,
		},
		ACPActivity:   acpActivity,
		StepClock:     stepClock,
		ToolsInFlight: toolsInFlight,
		TerminalOwner: &terminalOwner,
		TurnContext:   turnContext,
	}
	return &preparedAcpSetup{
		commandsTx:          commands,
		commandsRx:          commands.receiver(),
		eventsTx:            events,
		eventsRx:            events,
		userShells:          userShells,
		reviewer:            reviewer,
		subagents:           subagents,
		subagentSocketGuard: subagentGuard,
		acpSpec:             acpSpec,
		shellCleanup:        shellCleanup,
		managedCacheRoot:    setup.managedCacheRoot,
		harness:             config.Harness,
	}, nil
}

// preparationHandoff is the one decision between a preparation handing its
// bridge off (publishStarted) and its supervisor abandoning it on
// cancellation. In Rust both happen in one task, so a cancelled preparation
// never starts a bridge; here the preparation runs on its own goroutine, and
// this owner keeps that true.
type preparationHandoff struct {
	mu        sync.Mutex
	started   bool
	abandoned bool
}

// start claims the handoff; false once the supervisor abandoned it.
func (h *preparationHandoff) start() bool {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.abandoned {
		return false
	}
	h.started = true
	return true
}

// abandon gives the preparation up; false once it handed its bridge off.
func (h *preparationHandoff) abandon() bool {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.started {
		return false
	}
	h.abandoned = true
	return true
}

type preparationHandoffKey struct{}

// preparationThreadsKey carries the preparation's blocking-work tracker
// (Rust: track_blocking_work's task-local TaskTracker).
type preparationThreadsKey struct{}

// publishStarted hands relay-local lifecycle work from the request handler
// to the coordinator about to start, or refuses because a Close sealed the
// relay first. Returns the dispatch wakes the coordinator will own.
//
// The relay lock is the one decision point between this and a Close the
// request handler accepts during preparation (see serveClientWithMemory).
// Started is published inside it, and only while preparation is not
// cancelled and the relay is not sealed. A handler that saw Preparing
// therefore completed the Close and cancelled a preparation that never
// starts a bridge, and one that sees Started leaves the Close to the
// coordinator.
//
// A seal found here cancels the preparation itself, under the same lock: the
// handler that accepted the Close may not have cancelled it yet, and a worker
// that booted with a closing relay has no handler to do it. The cancelled
// token (ctx, cancelled through cancel) is then the one fact that makes this
// refusal a cancellation rather than a startup failure. On refusal the wakes
// stay with the preparation for the sealed relay.
func publishStarted(ctx context.Context, cancel context.CancelFunc, relay *RelayCell, status *core.Watch[preparationSnapshot], dispatchWakes **dispatchWakeChannel, services preparedConnectionServices) (*dispatchWakeChannel, error) {
	guard := relay.Lock()
	defer guard.Unlock()
	if guard.SealedByClose() {
		cancel()
	}
	if ctx.Err() != nil {
		return nil, errors.New("preparation cancelled before the bridge started")
	}
	if handoff, ok := ctx.Value(preparationHandoffKey{}).(*preparationHandoff); ok && !handoff.start() {
		return nil, errors.New("preparation cancelled before the bridge started")
	}
	wakes := *dispatchWakes
	if wakes == nil {
		return nil, errors.New("preparation lost ownership of relay dispatch wake channel")
	}
	*dispatchWakes = nil
	status.SendReplace(preparationSnapshot{
		state:    core.HarnessPreparationStarted{},
		services: &services,
	})
	return wakes, nil
}

// startBridge starts the bridge and its coordinator once publishStarted
// accepts the handoff. Started is published before any bridge process
// exists, and nothing after the publication can fail, so a Started worker
// always has a coordinator. ctx and cancel are the preparation's
// cancellation token.
func startBridge(ctx context.Context, cancel context.CancelFunc, setup *preparedAcpSetup, relay *RelayCell, dispatchWakes **dispatchWakeChannel, kimiTaskHome *kimiHome, antigravityTaskHome *kimiHome, capture *WorkObserver, runtime *core.Runtime, status *core.Watch[preparationSnapshot]) (*startedBridgeTasks, error) {
	wakes, err := publishStarted(ctx, cancel, relay, status, dispatchWakes, preparedConnectionServices{
		commands:  setup.commandsTx,
		reviewer:  setup.reviewer,
		subagents: setup.subagents,
	})
	if err != nil {
		return nil, err
	}
	// The bridge and its coordinator outlive the preparation that started
	// them, so neither runs under the preparation's token.
	acpContext, acpShutdown := context.WithCancel(context.Background())
	started := &startedBridgeTasks{
		acpShutdown:         acpShutdown,
		commands:            setup.commandsTx,
		reviewer:            setup.reviewer,
		subagentSocketGuard: setup.subagentSocketGuard,
		subagents:           setup.subagents,
		shellCleanup:        setup.shellCleanup,
	}
	if setup.managedCacheRoot != nil {
		gcContext, stopGC := context.WithCancel(context.Background())
		harnessinstall.SpawnGCOn(gcContext, *setup.managedCacheRoot, setup.harness)
		started.harnessGC = stopGC
	}
	commands := setup.commandsTx
	started.acpTask = core.Spawn(func() error {
		// The runtime owns the receiving side: once it stops, a sender sees
		// the closed channel, as when Rust drops the receiver.
		defer commands.closeReceiver()
		return runWithGuardian(acpContext, setup.acpSpec, setup.commandsRx, setup.eventsTx, relay.guardian)
	})
	var kimiTasks *KimiTaskMonitor
	if kimiTaskHome != nil {
		kimiTasks = newKimiTaskMonitor(*kimiTaskHome)
	}
	var antigravityTasks *AntigravityTaskMonitor
	if antigravityTaskHome != nil {
		antigravityTasks = newAntigravityTaskMonitor(*antigravityTaskHome)
	}
	eventContext, eventAbort := context.WithCancel(context.Background())
	started.eventAbort = eventAbort
	started.eventTask = core.Spawn(func() error {
		return runRelayCoordinatorWithShells(eventContext, relay, setup.eventsRx, wakes, commands,
			setup.userShells, kimiTasks, antigravityTasks, capture, newHarnessStartup())
	})
	return started, nil
}

// abortTaskOnDrop aborts a task when closed (Rust's Drop) and waits for it to
// return. An aborted tokio task drops its future, and with it the guards the
// task owns; a Go task releases them only by returning, so Close waits for
// that and the caller never returns while the task still holds them (the
// accept loop's control socket).
type abortTaskOnDrop struct {
	abort context.CancelFunc
	done  <-chan struct{}
}

func (a abortTaskOnDrop) Close() {
	a.abort()
	<-a.done
}

// cancelPreparationOnDrop cancels a preparation when closed (Rust's Drop).
type cancelPreparationOnDrop context.CancelFunc

func (c cancelPreparationOnDrop) Close() { c() }

// preparationStep publishes a new preparation step and records its startup
// breadcrumb within the step's budget. ctx is the preparation's cancellation
// token.
func preparationStep(ctx context.Context, root string, status *core.Watch[preparationSnapshot], step string, timeout time.Duration) (preparationStepBudget, error) {
	budget := newPreparationStepBudget(step, timeout)
	snapshot := status.Borrow()
	snapshot.state = core.HarnessPreparationPreparing{Step: step, SinceMs: time.Now().UnixMilli()}
	snapshot.stepDeadline = budget.deadline
	status.SendReplace(snapshot)
	_, err := boundedBlockingPreparationStep(ctx, &budget, func(stepCtx context.Context) (struct{}, error) {
		if stepCtx.Err() != nil {
			return struct{}{}, errors.New("preparation cancelled before startup breadcrumb")
		}
		RecordStartupStep(root, step)
		return struct{}{}, nil
	})
	if err != nil {
		return preparationStepBudget{}, err
	}
	return budget, nil
}

// preparationError is why operation is unavailable while the harness
// prepares; false when it is available.
func preparationError(snapshot *preparationSnapshot, operation string) (string, bool) {
	// Standalone/reviewer socket handlers have no primary preparation
	// supervisor (nil); their explicit services are already available. This
	// worker never reports Failed: a harness that cannot start ends the
	// worker instead.
	preparing, ok := snapshot.state.(core.HarnessPreparationPreparing)
	if !ok {
		return "", false
	}
	return fmt.Sprintf("harness is preparing at %s; %s is not available yet", preparing.Step, operation), true
}

// waitForChange holds a WaitForChange until the relay's latest event or
// operational state, or this connection's sub-agent queue, differs from what
// the controller names, or until core.RelayChangeWaitBound passes. The state
// is built as an attach builds it, preparation included, so its digest is
// the one the controller holds.
func waitForChange(ctx context.Context, relay *RelayCell, preparation *core.Watch[preparationSnapshot], subagents *SubagentEndpoint, feed *subagentFeed, guardianGeneration *uint64, afterOrdinal uint64, stateDigest *string) (core.RelayResponseBody, error) {
	releases := relay.Releases()
	guardian := relay.guardian.Changes()
	jev := core.RateLimitChanges()
	var generations *core.Watch[uint64]
	if subagents != nil {
		generations = subagents.generations()
	}
	deadline := time.NewTimer(core.RelayChangeWaitBound)
	defer deadline.Stop()
	for {
		// Mark each signal seen before looking, so a change while looking
		// still wakes the wait below. Looking only observes the relay, which
		// is not counted as a change.
		releases.BorrowAndUpdate()
		var generationsChanged, preparationChanged <-chan struct{}
		if generations != nil {
			generationsChanged = generations.Changed()
		}
		// The rate limit is process-wide, so this wait subscribes with a
		// channel of its own; stopJev withdraws it on every way out of this
		// pass.
		jevChanged, stopJev := jev.Subscribe()
		guardianChanged := guardian.Changed()
		var harnessPreparation core.HarnessPreparation
		if preparation != nil {
			// A finished preparation keeps its last value, which an attach
			// still reads.
			preparationChanged = preparation.Changed()
			harnessPreparation = preparation.Borrow().state
		}
		observer := relay.Observe()
		state := observer.OperationalState()
		observer.Unlock()
		state.HarnessPreparation = harnessPreparation
		latestOrdinal := state.LatestOrdinal
		digest, err := core.RelayStateDigest(&state)
		if err != nil {
			stopJev()
			return nil, err
		}
		subagentQueueChanged := subagents != nil && feed.HasUnpolled(subagents)
		generation := guardian.Borrow()
		guardianQueueChanged := generation != *guardianGeneration
		*guardianGeneration = generation
		changed := guardianQueueChanged ||
			latestOrdinal != afterOrdinal ||
			stateDigest == nil || *stateDigest != digest ||
			subagentQueueChanged
		observed := core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadChangeObserved{
			LatestOrdinal:        latestOrdinal,
			StateDigest:          digest,
			SubagentQueueChanged: subagentQueueChanged,
			GuardianQueueChanged: guardianQueueChanged,
		}}
		if changed {
			stopJev()
			return observed, nil
		}
		waitCtx, stopWait := context.WithCancel(ctx)
		released := make(chan error, 1)
		go func() { released <- releases.Changed(waitCtx) }()
		timedOut := false
		select {
		case <-released:
			stopWait()
			stopJev()
			continue
		case <-generationsChanged:
		case <-preparationChanged:
		case <-jevChanged:
		case <-guardianChanged:
		case <-deadline.C:
			timedOut = true
		case <-ctx.Done():
			stopWait()
			stopJev()
			<-released
			return nil, ctx.Err()
		}
		// The release watcher reads the receiver's mark; it ends before the
		// next look marks it again.
		stopWait()
		stopJev()
		<-released
		if timedOut {
			return observed, nil
		}
	}
}

// overlayPreparationState puts the preparation into a status reply. Attach
// replies carry the preparation already; see handleRequest.
func overlayPreparationState(response *core.RelayResponseEnvelope, snapshot *preparationSnapshot) {
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	if !isOk {
		return
	}
	status, isStatus := ok.Payload.(core.RelayResponsePayloadStatus)
	if !isStatus {
		return
	}
	status.State.HarnessPreparation = snapshot.state
	response.Body = core.RelayResponseBodyOk{Payload: status}
}

func unavailableRequest(envelope core.RelayRequestEnvelope, message string) core.RelayResponseEnvelope {
	return core.RelayResponseEnvelope{
		RequestID:       envelope.RequestID,
		ProtocolVersion: envelope.ProtocolVersion,
		Body:            compactionError(core.RelayErrorCodeInvalidState, message),
	}
}

// projectMemoryEndpoint is a connection's project memory and history
// service.
type projectMemoryEndpoint struct {
	config *core.ProjectMemoryLaunchConfig
	// io serializes project memory I/O: a one-slot semaphore, held by
	// sending into it.
	io chan struct{}
	// ioRequested, ioDisconnected and ioBlock are test instrumentation
	// (Rust: cfg(test) fields). The two notifications hold one pending
	// signal each, as tokio's Notify::notify_one does.
	ioRequested    chan struct{}
	ioDisconnected chan struct{}
	ioBlock        *projectMemoryIoBlock
	history        *historyEndpoint
	cwd            string
}

// projectMemoryIoBlock holds a project memory operation in tests (Rust:
// cfg(test)).
type projectMemoryIoBlock struct {
	// started holds one pending signal once the blocked operation starts.
	started chan struct{}
	// release lets the blocked operation go on, or fail with the error sent.
	release           <-chan error
	panicAfterRelease bool
}

func newProjectMemoryEndpoint(config *core.ProjectMemoryLaunchConfig) projectMemoryEndpoint {
	return projectMemoryEndpoint{
		config:         config,
		io:             make(chan struct{}, 1),
		ioRequested:    make(chan struct{}, 1),
		ioDisconnected: make(chan struct{}, 1),
		history:        defaultHistoryEndpoint(),
	}
}

func defaultProjectMemoryEndpoint() projectMemoryEndpoint {
	return newProjectMemoryEndpoint(nil)
}

// socketGuard removes the control socket it names when closed (Rust's Drop).
type socketGuard string

func (g socketGuard) Close() {
	if err := removeControlSocket(string(g), "shutdown"); err != nil {
		slog.Warn("control socket shutdown cleanup failed", "error", err, "socket", string(g))
	}
}

// logControlSocket logs a control socket operation with both sides of it:
// overlay filesystems can report a successful operation while subsequent
// path lookup still exposes an old inode.
func logControlSocket(path, operation string, err error) {
	var errno, lookupErrno *int
	var syscallErr syscall.Errno
	if err != nil && errors.As(err, &syscallErr) {
		value := int(syscallErr)
		errno = &value
	}
	attributes := []any{"socket", path, "operation", operation, "success", err == nil}
	info, lookup := os.Lstat(path)
	if lookup != nil {
		if errors.As(lookup, &syscallErr) {
			value := int(syscallErr)
			lookupErrno = &value
		}
	}
	attributes = append(attributes, "errno", controlSocketOptionalDebug(errno))
	if err != nil {
		attributes = append(attributes, "error", fmt.Sprintf("Some(%s)", err))
	} else {
		attributes = append(attributes, "error", "None")
	}
	if lookup == nil {
		if stat, ok := info.Sys().(*syscall.Stat_t); ok {
			attributes = append(attributes,
				"inode", fmt.Sprintf("Some(%d)", stat.Ino),
				"links", fmt.Sprintf("Some(%d)", stat.Nlink),
				"mode", fmt.Sprintf("Some(%d)", stat.Mode),
				"device", fmt.Sprintf("Some(%d)", stat.Dev))
		}
		attributes = append(attributes, "file_type", fmt.Sprintf("Some(%s)", info.Mode().Type()))
	} else {
		attributes = append(attributes, "inode", "None", "links", "None", "mode", "None", "device", "None", "file_type", "None")
	}
	attributes = append(attributes, "lookup_errno", controlSocketOptionalDebug(lookupErrno), "pid", os.Getpid())
	slog.Info("control socket operation", attributes...)
}

// controlSocketOptionalDebug is an Option<i32>'s Debug form.
func controlSocketOptionalDebug(value *int) string {
	if value == nil {
		return "None"
	}
	return fmt.Sprintf("Some(%d)", *value)
}

func removeControlSocket(path, phase string) error {
	logControlSocket(path, phase+": before unlink", nil)
	err := os.Remove(path)
	logControlSocket(path, phase+": unlink", err)
	return err
}

// publishControlSocket binds `control.sock`, restricts it to the owner, and
// records the `bind-socket` and `serving` startup steps. The caller must
// accept on the listener promptly: the daemon reads the socket's existence
// as "this worker answers".
func publishControlSocket(root, socket string) (*net.UnixListener, socketGuard, error) {
	RecordStartupStep(root, "bind-socket")
	logControlSocket(socket, "before bind", nil)
	listener, err := core.BindUnixListener(socket)
	logControlSocket(socket, "bind", err)
	if err != nil {
		return nil, "", fmt.Errorf("bind worker socket %s: %w", socket, err)
	}
	guard := socketGuard(socket)
	if err := os.Chmod(socket, 0o600); err != nil {
		_ = listener.Close()
		guard.Close()
		return nil, "", err
	}
	RecordStartupStep(root, "serving")
	return listener, guard, nil
}

// writeWorkerPidfile records this daemon's PID where session teardown can
// find it. Teardown must stop the daemon before deleting the worker root;
// without this file it can only guess from process command lines.
func writeWorkerPidfile(root string, pid uint32) error {
	path := filepath.Join(root, core.WorkerPIDFile)
	if err := os.WriteFile(path, []byte(fmt.Sprintf("%d\n", pid)), 0o666); err != nil {
		return fmt.Errorf("write worker pidfile %s: %w", path, err)
	}
	return nil
}

// RunDaemon acquires the worker root and runs the worker daemon in it.
func RunDaemon(ctx context.Context, root string, config core.WorkerLaunchConfig) error {
	owner, err := WorkerRootOwnerAcquire(root)
	if err != nil {
		return err
	}
	defer owner.Release()
	return RunDaemonOwned(ctx, owner, config)
}

// acceptStopped turns the accept task's end into the daemon's error.
func acceptStopped(result error, joinErr error) error {
	if joinErr != nil {
		return fmt.Errorf("worker proxy accept task stopped: %w", joinErr)
	}
	if result != nil {
		return result
	}
	return errors.New("worker proxy accept task stopped")
}

// RunDaemonOwned runs the worker daemon in a root it owns.
func RunDaemonOwned(ctx context.Context, owner *WorkerRootOwner, config core.WorkerLaunchConfig) error {
	root := owner.Root()
	// The daemon owns its launch config, as Rust's takes it by value; the
	// caller's maps stay untouched.
	config.TargetEnvironment = maps.Clone(config.TargetEnvironment)
	var sessionGitConfigInclude *string
	if include, ok := config.TargetEnvironment[core.SessionGitConfigIncludePath]; ok {
		sessionGitConfigInclude = &include
	}
	delete(config.TargetEnvironment, core.SessionNitroMbxEnv)
	if key, ok := config.TargetEnvironment[core.FastinoKeyEnvironment]; ok {
		delete(config.TargetEnvironment, core.FastinoKeyEnvironment)
		if !fastinoKey.Set(key) {
			return errors.New("the worker's Fastino key was already initialized")
		}
	}
	targetEnvironment := maps.Clone(config.TargetEnvironment)
	if targetEnvironment == nil {
		targetEnvironment = map[string]string{}
	}
	maps.Copy(targetEnvironment, config.Environment)
	delete(targetEnvironment, core.FastinoKeyEnvironment)
	delete(targetEnvironment, core.SessionGitConfigIncludePath)
	config.Environment = targetEnvironment
	startupDirectory, err := os.Getwd()
	if err != nil {
		return err
	}
	root = resolveRelativeWorkerRoot(root, startupDirectory)
	resolveRelativeHarnessHome(&config, startupDirectory)
	if config.NoGithubCredentials {
		core.RemoveGithubAuthenticationEnvironment(config.Environment)
		core.RemoveGithubAuthenticationEnvironment(config.TargetEnvironment)
	}
	checkpointOnly := config.RunMode == core.WorkerRunModeCheckpointOnly
	RecordStartupStep(root, "policy")
	if !checkpointOnly {
		if err := enforceExecutionPolicy(&config); err != nil {
			return err
		}
	}
	// Resolve this before the launch config's environment is consumed by the
	// ACP supervisor specification below.
	endpoint, endpointErr := credentialEndpoint(&config)
	credentials := credentialEndpointResult{endpoint: endpoint, err: endpointErr}
	var kimiTaskHome *kimiHome
	if config.Harness == core.HarnessKindKimi {
		kimiTaskHome = &kimiHome{path: endpoint.Home, err: endpointErr}
	}
	var antigravityTaskHome *kimiHome
	if config.Harness == core.HarnessKindAntigravity {
		antigravityTaskHome = &kimiHome{path: endpoint.Home, err: endpointErr}
	}
	if err := core.CreateOwnerOnlyDirectory(root); err != nil {
		return fmt.Errorf("create worker root %s: %w", root, err)
	}
	socket := filepath.Join(root, "control.sock")
	// Refuse a second daemon before touching durable state: opening the
	// relay recovers the journal in place, so getting that far would corrupt
	// the files a live worker is still writing.
	_, statErr := os.Stat(socket)
	exists := statErr == nil
	if exists {
		logControlSocket(socket, "startup: exists", nil)
		connection, connectErr := core.ConnectUnixStream(socket)
		logControlSocket(socket, "startup: connect", connectErr)
		if connectErr == nil {
			_ = connection.Close()
			return fmt.Errorf("a worker is already running at %s", socket)
		}
	} else {
		logControlSocket(socket, "startup: absent", nil)
	}
	// A dead daemon can leave its socket inode behind. Remove it before
	// journal recovery so controller liveness can distinguish a worker that
	// is still starting from one that has published its endpoint.
	if _, err := os.Stat(socket); err == nil {
		if err := removeControlSocket(socket, "startup"); err != nil {
			return fmt.Errorf("remove stale socket %s: %w", socket, err)
		}
	}
	// Check the target directory before serving any relay requests.
	// Otherwise a child process can fail to spawn with ENOENT for its cwd,
	// and a caller that already attached can see only the ensuing relay
	// disconnect.
	if !checkpointOnly {
		const step = "working-directory"
		RecordStartupStep(root, step)
		// Only a directory that is gone or is not a directory is permanent.
		// Other errors, such as ESTALE or EIO from a network filesystem, can
		// clear up. They exit with an ordinary error and no startup-failure
		// marker, because every reader of that marker (unpark and recovery)
		// takes it to mean that starting again would only repeat the
		// failure.
		problem, permanent := "", false
		if info, err := os.Stat(config.Cwd); err == nil && !info.IsDir() {
			problem, permanent = "path is not a directory", true
		} else if err != nil {
			problem = core.IOErrorDisplay(err)
			permanent = errors.Is(err, fs.ErrNotExist) || errors.Is(err, syscall.ENOTDIR)
		}
		if problem != "" {
			message := fmt.Sprintf("worker working directory %s is unavailable: %s", config.Cwd, problem)
			RecordStartupFailure(root, step, message)
			if !permanent {
				return errors.New(message)
			}
			kind := core.WorkerHarnessStartupFailureKindWorkingDirectoryUnavailable
			return core.WorkerHarnessStartupFailure{Message: message, Kind: &kind}
		}
	}
	_, relayStateErr := os.Stat(filepath.Join(root, core.RelayStateFile))
	_, seedErr := os.Stat(filepath.Join(root, core.RestoredRelaySeedFile))
	restarting := relayStateErr == nil || seedErr == nil
	// Validate and recover durable state before publishing a socket. A
	// failed startup must never leave a fresh endpoint that looks live.
	RecordStartupStep(root, "durable-relay")
	var durableRelay *DurableRelay
	if checkpointOnly {
		durableRelay, err = DurableRelayOpenForCheckpointWithInitialCommands(root, config.SessionID, core.PackageVersion, config.InitialCommands)
	} else {
		durableRelay, err = DurableRelayOpenForHarness(root, config.SessionID, core.PackageVersion, config.Harness, config.InitialCommands)
	}
	if err != nil {
		return err
	}
	// Deferred first, so it runs after every deferred stop below: the Jev
	// records queued until then are written before the daemon returns, and
	// a task that outlives it can write none.
	defer durableRelay.Shutdown()
	// Only Claude Code's adapter marks the end of a turn it started on its
	// own, so only it can model those turns without leaving a session stuck
	// Running. See `.agents/docs/claude-autonomous-turns.md`.
	if config.Environment[core.DisabledEnvironment] == "1" {
		durableRelay.jevDisabled = true
	}
	durableRelay.recoversChildQuota = config.SubagentKind != nil && *config.SubagentKind == core.SubagentKindWorker &&
		!durableRelay.jevDisabled
	if err := durableRelay.setManagedSubagent(config.IsManagedSubagent()); err != nil {
		return err
	}
	durableRelay.SetTurnVerdictHarness(config.Harness)
	if config.Harness == core.HarnessKindClaude {
		if path, ok := core.ClaudeOauthLoginPath(config.Environment); ok {
			durableRelay.authenticationLoginPath = &path
		} else {
			durableRelay.authenticationLoginPath = nil
		}
	}
	// Worker admission opens before profile preparation finishes. Claude
	// must already be protected from ACP steering during that window.
	durableRelay.SetSteeringDeliveryMode(config.Harness, false)
	durableRelay.SetHarnessTurnPolicy(harnessTurnPolicyFor(config.Harness))
	// Both adapters publish their background command lifecycle through AIR.
	switch config.Harness {
	case core.HarnessKindClaude, core.HarnessKindCodex:
		durableRelay.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
	case core.HarnessKindKimi:
		durableRelay.SetBackgroundWorkPolicy(BackgroundWorkPolicyKimiTasks)
	case core.HarnessKindAntigravity:
		durableRelay.SetBackgroundWorkPolicy(BackgroundWorkPolicyAntigravityTasks)
	default:
		durableRelay.SetBackgroundWorkPolicy(BackgroundWorkPolicyHostedTerminals)
	}
	if err := recordImportedNativeIdentity(&config, durableRelay); err != nil {
		return err
	}
	resumeSession := selectResumeSession(&config, durableRelay)
	nativeSessionHistory := durableRelay.NativeSessionHistory()
	projectMemory := newProjectMemoryEndpoint(config.ProjectMemory)
	projectMemory.cwd = config.Cwd
	// Startup succeeded far enough to own this root, so claim it. A failed
	// open leaves any previous pidfile alone rather than pointing teardown
	// at a process that never took over.
	if err := writeWorkerPidfile(root, uint32(os.Getpid())); err != nil {
		return err
	}
	// Durable state recovered, so any exit record belongs to a previous life
	// of this worker. Leaving it would make the controller read this startup
	// as another death. Replacing it with a reservation also keeps room for
	// this life's record: a worker that stops on a full disk can still say
	// so.
	if err := Reserve(root); err != nil {
		core.DiagnosticStderr("Mjolnir: could not reserve worker exit record: %s", err)
	}

	if restarting && durableRelay.OperationalState().Execution != core.RelayExecutionStateClosed {
		if _, err := durableRelay.RecordObservation(core.RelayObservationSessionRestarted{}); err != nil {
			return err
		}
	}

	// Only a managed sub-agent that is not a reviewer observes its agent's
	// work when a turn ends (SessionManagedSubagentEnv, set by the controller
	// for every sub-agent, and no ReviewerLaunch): an ordinary session has
	// no parent to report to, and a reviewer's verdict names no working tree.
	var capture *WorkObserver
	if config.IsManagedSubagent() && config.ReviewerLaunch == nil {
		capture = &WorkObserver{
			SessionID:      config.SessionID,
			WorkerPID:      uint32(os.Getpid()),
			Directory:      config.Cwd,
			Git:            "git",
			HarnessReadyMs: durableRelay.HarnessReadyClock(),
			Children:       &ObserverChildren{},
		}
	}
	relay := NewRelayCell(durableRelay)
	if credentials.err == nil && credentials.endpoint.Harness != core.HarnessKindClaude {
		endpoint := credentials.endpoint
		go func() {
			observer := relay.Observe()
			identity := observer.loginIdentity
			observer.Unlock()
			if err := observeInitialLogin(context.Background(), &endpoint, identity); err != nil {
				slog.Warn("could not read the session's staged login", "error", err)
			}
			// Make the new observation visible to a relay change waiter.
			relay.Lock().Unlock()
		}()
	}
	// Client tasks are detached, so a durable failure they cannot recover
	// from has to travel back here to stop the daemon.
	fatal := make(chan error, 1)
	// A managed child's worker keeps work_liveness true while the processes
	// its last turn left running exist. The watcher resumes from the
	// snapshot after a restart and only ends on a durable relay failure.
	if capture != nil {
		observer := *capture
		go func() {
			err := WatchLiveness(context.Background(), relay, observer, LivenessInterval)
			fatal <- err
		}()
	}
	cpu := core.NewWatch(cpuRead{})
	cpuContext, stopCPU := context.WithCancel(context.Background())
	defer stopCPU()
	cpuTask := core.Spawn(func() struct{} {
		sampleCpu(cpuContext, cpu, config.CPUAccounting)
		return struct{}{}
	})
	sessionID := config.SessionID
	go func() {
		_, joinErr := cpuTask.Join(context.Background())
		if joinErr == nil && cpuContext.Err() != nil {
			return
		}
		result := "Ok(())"
		if joinErr != nil {
			result = fmt.Sprintf("Err(%s)", joinErr)
		}
		slog.Error("worker CPU sampler stopped", "session_id", sessionID, "result", result)
		fatal <- fmt.Errorf("worker CPU sampler stopped: %s", result)
	}()

	relayFatal := func(err error) error {
		return fmt.Errorf("relay durable state became unwritable: %w", err)
	}

	executionClosed := func() bool {
		guard := relay.Lock()
		defer guard.Unlock()
		return guard.OperationalState().Execution == core.RelayExecutionStateClosed
	}
	if checkpointOnly || executionClosed() {
		// A durable close is a seal, including across target or daemon
		// restarts. Keep the relay attachable so the controller can catch up
		// and complete its checkpoint, but never reopen the ACP session.
		if checkpointOnly {
			guard := relay.Lock()
			err := guard.DispatchCheckpointOnly()
			guard.Unlock()
			if err != nil {
				return err
			}
		}
		wakes := newDispatchWakeChannel()
		wakes.closeReceiver()
		// Nothing here waits on a harness, so the relay can answer at once.
		listener, guard, err := publishControlSocket(root, socket)
		if err != nil {
			return err
		}
		defer guard.Close()
		runtime := defaultConnectionRuntime()
		runtime.projectMemory = projectMemory
		runtime.cpu = cpu
		served := make(chan error, 1)
		serveContext, stopServing := context.WithCancel(context.Background())
		defer stopServing()
		go func() {
			served <- serveTerminalRelay(serveContext, listener, relay, wakes, credentials, runtime, fatal, fatal)
		}()
		// No signal handler is installed for a sealed relay, as in Rust: a
		// signal ends the process. The caller's context still stops it.
		select {
		case err := <-served:
			return err
		case <-ctx.Done():
			stopServing()
			return nil
		}
	}

	suspendSignal, err := InstallSuspendSignal(root)
	if err != nil {
		return err
	}
	type suspendWait struct {
		request core.SuspendRequest
		err     error
	}
	suspendRequests := make(chan suspendWait, 1)
	suspendContext, stopSuspendWait := context.WithCancel(context.Background())
	defer stopSuspendWait()
	go func() {
		request, err := waitForSuspend(suspendContext, suspendSignal)
		if suspendContext.Err() == nil {
			suspendRequests <- suspendWait{request: request, err: err}
		}
	}()
	suspendSessionID := config.SessionID

	initialStep := "login-resolve"
	preparationStatus := core.NewWatch(preparationSnapshot{
		state: core.HarnessPreparationPreparing{Step: initialStep, SinceMs: time.Now().UnixMilli()},
	})
	preparationContext, preparationCancel := context.WithCancel(context.Background())
	defer cancelPreparationOnDrop(preparationCancel).Close()
	dispatchWake := newDispatchWakeChannel()
	stopSignals := make(chan os.Signal, 2)
	signal.Notify(stopSignals, os.Interrupt, syscall.SIGTERM)
	defer signal.Stop(stopSignals)
	// Interrupt and terminate stop the worker gracefully; so does the
	// caller's context.
	stopping := func() <-chan struct{} {
		stop := make(chan struct{})
		go func() {
			select {
			case <-stopSignals:
			case <-ctx.Done():
			}
			close(stop)
		}()
		return stop
	}()
	// The adoption regression loses only its controller before this worker
	// accepts any ACK, so the ordinary genesis path is exercised end to end.
	if err := core.ReachTestHook("adoption_orphan_before_serving"); err != nil {
		return err
	}
	// The relay is recovered; accept immediately while harness work runs
	// under its own supervisor.
	listener, guard, err := publishControlSocket(root, socket)
	if err != nil {
		return err
	}
	_, hasSubagentRole := config.SubagentMCPRole()
	runtime := defaultConnectionRuntime()
	runtime.projectMemory = projectMemory
	runtime.cpu = cpu
	runtime.preparation = preparationStatus
	runtime.preparationCancel = preparationCancel
	if antigravityTaskHome != nil && antigravityTaskHome.err == nil {
		home := antigravityTaskHome.path
		runtime.antigravityHome = &home
	}
	runtime.hasSubagentTools = hasSubagentRole
	sharedCredentials := newSharedCredentials(credentials)
	clientContext, abortClients := context.WithCancel(context.Background())
	clientTask := core.Spawn(func() error {
		return acceptWorkerClients(clientContext, listener, guard, relay, dispatchWake, sharedCredentials, runtime, fatal)
	})
	// Every return below removes control.sock before the daemon's caller
	// sees the result: the accept loop's socket guard ends with it.
	defer abortTaskOnDrop{abort: abortClients, done: clientTask.Done()}.Close()
	clientStopped := func() error {
		result, joinErr := clientTask.Join(context.Background())
		return acceptStopped(result, joinErr)
	}
	finishSuspended := func(request core.SuspendRequest) error {
		abortClients()
		<-clientTask.Done()
		return finishSuspend(context.Background(), root, suspendSessionID, request)
	}

	preparationSetup := harnessPreparationSetup{
		root:                    root,
		config:                  config,
		relay:                   relay,
		restarting:              restarting,
		sessionGitConfigInclude: sessionGitConfigInclude,
		credentials:             credentials,
		kimiTaskHome:            kimiTaskHome,
		antigravityTaskHome:     antigravityTaskHome,
		resumeSession:           resumeSession,
		nativeSessionHistory:    nativeSessionHistory,
		capture:                 capture,
	}
	prepTask := core.Spawn(func() preparationOutcome {
		return superviseHarnessPreparation(preparationContext, preparationCancel, preparationSetup, dispatchWake, preparationStatus)
	})

	// A Close during preparation cancels it from the request handler; the
	// supervisor then returns Cancelled once its threads have returned.
	var outcome preparationOutcome
	select {
	case <-prepTask.Done():
		var joinErr error
		outcome, joinErr = prepTask.Join(context.Background())
		if joinErr != nil {
			abortClients()
			return fmt.Errorf("harness preparation task stopped: %s", joinErr)
		}
	case report := <-fatal:
		stopPreparation(ctx, preparationCancel, prepTask)
		abortClients()
		return relayFatal(report)
	case suspended := <-suspendRequests:
		if suspended.err != nil {
			return suspended.err
		}
		stopPreparation(ctx, preparationCancel, prepTask)
		return finishSuspended(suspended.request)
	case <-clientTask.Done():
		stopPreparation(ctx, preparationCancel, prepTask)
		return clientStopped()
	case <-stopping:
		stopPreparation(ctx, preparationCancel, prepTask)
		abortClients()
		return nil
	}

	var running *runningHarness
	switch outcome := outcome.(type) {
	case preparationOutcomeStarted:
		running = outcome.Value
	case preparationOutcomeFailed:
		// The exit record carries the typed failure, so the daemon stops
		// restarting this harness and reports why it could not start.
		abortClients()
		return outcome.Value
	case preparationOutcomeCancelled:
		// A Close sealed the relay before the bridge started. Serve it
		// without a harness so the controller can finish its checkpoint. The
		// wake receiver stays open: a request that releases the close's
		// barrier still wakes dispatch, and the handler completes the Close
		// locally.
		select {
		case suspended := <-suspendRequests:
			if suspended.err != nil {
				abortClients()
				return suspended.err
			}
			return finishSuspended(suspended.request)
		case report := <-fatal:
			abortClients()
			return relayFatal(report)
		case <-clientTask.Done():
			return clientStopped()
		case <-stopping:
			abortClients()
			return nil
		}
	default:
		abortClients()
		return fmt.Errorf("harness preparation task stopped: unknown outcome %T", outcome)
	}
	defer running.releaseLeases()

	var suspendRequest *core.SuspendRequest
	var suspendErr error
	acpResult := func() error {
		var acpResult error
		var acpJoinErr error
		select {
		case suspended := <-suspendRequests:
			running.acpShutdown()
			stopFailedCoordinator(running.eventAbort, running.eventTask)
			_, _ = running.acpTask.Join(context.Background())
			if suspended.err != nil {
				suspendErr = suspended.err
				return nil
			}
			suspendRequest = &suspended.request
			return nil
		case report := <-fatal:
			stopFailedCoordinator(running.eventAbort, running.eventTask)
			return stopPeerAndReturn(running.acpTask, running.acpShutdown, report, "relay durable state became unwritable")
		case <-clientTask.Done():
			result, joinErr := clientTask.Join(context.Background())
			if joinErr != nil {
				return fmt.Errorf("worker proxy accept task stopped: %w", joinErr)
			}
			if result == nil {
				result = errors.New("worker proxy accept task stopped")
			}
			stopFailedCoordinator(running.eventAbort, running.eventTask)
			return stopPeerAndReturn(running.acpTask, running.acpShutdown, result, "accept worker proxy")
		case <-running.eventTask.Done():
			result, joinErr := running.eventTask.Join(context.Background())
			switch {
			case joinErr != nil:
				return stopPeerAndReturn(running.acpTask, running.acpShutdown, joinErr, "relay coordinator task stopped")
			case result != nil:
				return stopPeerAndReturn(running.acpTask, running.acpShutdown, result, "relay coordinator failed")
			}
			running.acpShutdown()
			acpResult, acpJoinErr = running.acpTask.Join(context.Background())
		case <-running.acpTask.Done():
			acpResult, acpJoinErr = running.acpTask.Join(context.Background())
			eventResult, eventJoinErr := running.eventTask.Join(context.Background())
			if eventJoinErr != nil {
				return fmt.Errorf("relay event task stopped: %w", eventJoinErr)
			}
			if eventResult != nil {
				return eventResult
			}
		case <-stopping:
			running.acpShutdown()
			stopFailedCoordinator(running.eventAbort, running.eventTask)
			_, _ = running.acpTask.Join(context.Background())
			return nil
		}
		if acpJoinErr == nil {
			return acpResult
		}
		failure := fmt.Errorf("ACP runtime task stopped: %s", acpJoinErr)
		guard := relay.Lock()
		_, err := guard.RecordObservation(core.RelayObservationWarning{Message: failure.Error()})
		guard.Unlock()
		if err != nil {
			return err
		}
		return failure
	}()

	if suspendErr != nil {
		abortClients()
		return suspendErr
	}
	closed := executionClosed()
	// The install belongs to the harness: a closed relay stays attachable
	// for checkpointing, which must not see an installer still writing
	// files.
	if running.projectTools != nil {
		running.projectTools.stop(context.Background())
	}
	running.shellCleanup.Wait()
	if running.subagentSocketGuard != nil {
		running.subagentSocketGuard.Release()
	}
	if running.harnessGC != nil {
		running.harnessGC()
	}
	if suspendRequest != nil {
		return finishSuspended(*suspendRequest)
	}
	if !closed {
		// Includes a harness that did not open its session: the
		// coordinator's typed startup failure ends the worker and reaches
		// the exit record.
		abortClients()
		return acpResult
	}
	if acpResult != nil {
		slog.Warn("ACP stopped with an error after the relay closed", "error", acpResult)
	}
	// A closed relay remains attachable so the controller can finish its
	// checkpoint. Harness-only operations are rejected.
	select {
	case report := <-fatal:
		abortClients()
		return relayFatal(report)
	case <-clientTask.Done():
		return clientStopped()
	case <-stopping:
		abortClients()
		return nil
	}
}

// acceptControlConnections accepts on listener until it fails, closing it when ctx
// ends (Rust: dropping the accept future drops the listener).
func acceptControlConnections(ctx context.Context, listener *net.UnixListener, accepted func(*net.UnixConn)) error {
	stop := context.AfterFunc(ctx, func() { _ = listener.Close() })
	defer stop()
	defer listener.Close()
	for {
		stream, err := listener.AcceptUnix()
		if err != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			return err
		}
		accepted(stream)
	}
}

// acceptWorkerClients serves every relay connection on listener until
// accepting fails.
func acceptWorkerClients(ctx context.Context, listener *net.UnixListener, socketGuard socketGuard, relay *RelayCell, dispatchWake *dispatchWakeChannel, credentials *sharedCredentials, runtime connectionRuntime, fatal chan<- error) error {
	defer socketGuard.Close()
	// Connections are detached: aborting the accept loop leaves served
	// connections to end by themselves. A client that stops by panicking is
	// reported, as Rust's JoinSet reports a failed client task.
	connections := context.WithoutCancel(ctx)
	err := acceptControlConnections(ctx, listener, func(stream *net.UnixConn) {
		go func() {
			defer func() {
				if payload := recover(); payload != nil {
					slog.Warn("worker proxy client task stopped", "error", fmt.Sprintf("task panicked: %v", payload))
				}
			}()
			if err := serveClientWithSharedCredentials(connections, stream, relay, dispatchWake, credentials, runtime, fatal); err != nil {
				slog.Warn("relay proxy client disconnected", "error", err)
			}
		}()
	})
	return fmt.Errorf("accept worker proxy: %w", err)
}

// boundedPreparationStep runs operation within the step's remaining budget.
func boundedPreparationStep[T any](ctx context.Context, budget *preparationStepBudget, operation func(ctx context.Context) (T, error)) (T, error) {
	stepCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	type result struct {
		value T
		err   error
	}
	results := make(chan result, 1)
	go func() {
		value, err := operation(stepCtx)
		results <- result{value, err}
	}()
	timer := time.NewTimer(budget.remaining())
	defer timer.Stop()
	select {
	case outcome := <-results:
		return outcome.value, outcome.err
	case <-timer.C:
		var zero T
		return zero, budget.deadlineError()
	}
}

// boundedBlockingPreparationStep runs blocking operation on its own
// goroutine within the step's budget. ctx is the preparation's cancellation
// token; operation gets a child of it that is cancelled when the step is
// cancelled or times out.
//
// Synchronous filesystem/socket calls cannot be forcibly interrupted. On
// timeout or cancellation this stops waiting for the goroutine; the
// preparation's thread tracker (see supervisePreparation) still counts it,
// so a cancelled preparation waits for it and a failed one ends the worker,
// which ends the goroutine.
func boundedBlockingPreparationStep[T any](ctx context.Context, budget *preparationStepBudget, operation func(stepCtx context.Context) (T, error)) (T, error) {
	var zero T
	stepCtx, stepCancel := context.WithCancel(ctx)
	threads, _ := ctx.Value(preparationThreadsKey{}).(*sync.WaitGroup)
	if threads != nil {
		threads.Add(1)
	}
	type result struct {
		value T
		err   error
	}
	task := core.SpawnBlocking(func() result {
		if threads != nil {
			defer threads.Done()
		}
		value, err := operation(stepCtx)
		return result{value, err}
	})
	cancelled := func() (T, error) {
		stepCancel()
		return zero, fmt.Errorf("worker preparation step %s cancelled", budget.step)
	}
	// Cancellation is checked first, as Rust's biased select does.
	if ctx.Err() != nil {
		return cancelled()
	}
	timer := time.NewTimer(time.Until(budget.deadline))
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return cancelled()
	case <-task.Done():
		outcome, joinErr := task.Join(context.Background())
		stepCancel()
		if joinErr != nil {
			return zero, fmt.Errorf("worker preparation step %s stopped: %w", budget.step, joinErr)
		}
		return outcome.value, outcome.err
	case <-timer.C:
		stepCancel()
		return zero, budget.deadlineError()
	}
}

// superviseHarnessPreparation prepares and starts the harness, owning the
// dispatch wakes until a coordinator takes them. ctx and cancel are the
// preparation's cancellation token.
func superviseHarnessPreparation(ctx context.Context, cancel context.CancelFunc, setup harnessPreparationSetup, dispatchWakes *dispatchWakeChannel, status *core.Watch[preparationSnapshot]) preparationOutcome {
	wakes := dispatchWakes
	running, stop := supervisePreparation(ctx, setup.root, status, func(prepCtx context.Context) (*runningHarness, error) {
		return prepareAndStartHarness(prepCtx, cancel, setup, &wakes, status)
	})
	switch stop := stop.(type) {
	case nil:
		return preparationOutcomeStarted{Value: running}
	case preparationStopFailed:
		return preparationOutcomeFailed{Value: stop.Value}
	default:
		// The supervisor abandoned the preparation before it handed its
		// bridge off, so the wakes are still the preparation's.
		return preparationOutcomeCancelled{Value: wakes}
	}
}

// supervisePreparation runs one harness preparation to an end it owns:
// prepared, cancelled, or failed with the typed startup failure the worker
// exits with. ctx is the preparation's cancellation token.
//
// Commands the preparation runs through the shared subprocess helpers stop
// when it is cancelled or ends, including those on blocking threads, and
// every thread it hands to the blocking pool is counted until it returns. A
// failed preparation lets those threads stop their commands, within the
// failed step's own bound, before the worker exits; the exit ends any thread
// still blocked. A cancelled one returns at once: either the worker is
// stopping, or it keeps serving a sealed relay while the cancelled threads
// finish under their step's bound, and the controller stops the worker
// before it removes any of its files.
func supervisePreparation[T any](ctx context.Context, root string, status *core.Watch[preparationSnapshot], preparation func(ctx context.Context) (T, error)) (T, preparationStop) {
	var zero T
	threads := &sync.WaitGroup{}
	handoff := &preparationHandoff{}
	// The preparation's commands run under a child token, which ends when
	// the preparation does (Rust: the drop guard of
	// with_preparation_cancellation).
	token, stopToken := context.WithCancel(ctx)
	prepCtx := core.WithPreparationCancellation(token, token)
	prepCtx = context.WithValue(prepCtx, preparationThreadsKey{}, threads)
	prepCtx = context.WithValue(prepCtx, preparationHandoffKey{}, handoff)
	type result struct {
		value T
		err   error
	}
	results := make(chan result, 1)
	go func() {
		defer stopToken()
		type prepared struct {
			value T
			err   error
		}
		outcome, panicked := catchPreparationPanic(func() prepared {
			value, err := preparation(prepCtx)
			return prepared{value, err}
		})
		if panicked != nil {
			results <- result{err: panicked}
			return
		}
		results <- result{outcome.value, outcome.err}
	}()
	var finished *result
	// Cancellation is checked first, as Rust's biased select does.
	if ctx.Err() == nil {
		select {
		case <-ctx.Done():
		case outcome := <-results:
			finished = &outcome
		}
	}
	if finished == nil && !handoff.abandon() {
		// The bridge was handed off before the cancellation; nothing after
		// the handoff blocks, so the preparation returns at once.
		outcome := <-results
		finished = &outcome
	}
	snapshot := status.Borrow()
	step := "bridge-start"
	if preparing, ok := snapshot.state.(core.HarnessPreparationPreparing); ok {
		step = preparing.Step
	}
	// Threads are bounded by the step that started them.
	stepDeadline := snapshot.stepDeadline
	if stepDeadline.IsZero() {
		stepDeadline = time.Now().Add(localPreparationStepTimeout)
	}
	if finished != nil && finished.err == nil {
		return finished.value, nil
	}
	// A step that saw the cancellation first fails with it; that is still a
	// cancellation.
	if finished == nil || ctx.Err() != nil {
		return zero, preparationStopCancelled{}
	}
	failure := finished.err
	message := fmt.Sprintf("harness preparation failed at %s: %s", step, failure)
	record := core.SpawnBlocking(func() struct{} {
		RecordStartupFailure(root, step, failure.Error())
		return struct{}{}
	})
	recordTimer := time.NewTimer(localPreparationStepTimeout)
	select {
	case <-record.Done():
		recordTimer.Stop()
		if _, err := record.Join(context.Background()); err != nil {
			slog.Warn("failed to record worker preparation failure", "error", err, "step", step)
		}
	case <-recordTimer.C:
		slog.Warn("recording worker preparation failure exceeded its deadline", "step", step)
	}
	// Let commands the failed step left running stop before the worker
	// exits, within that step's bound (already spent when the step timed
	// out); the exit ends any thread that does not.
	waited := make(chan struct{})
	go func() {
		threads.Wait()
		close(waited)
	}()
	threadTimer := time.NewTimer(time.Until(stepDeadline))
	select {
	case <-waited:
		threadTimer.Stop()
	case <-threadTimer.C:
		slog.Warn("a preparation thread was still running when preparation failed", "step", step)
	}
	return zero, preparationStopFailed{Value: core.WorkerHarnessStartupFailure{Message: message}}
}

// catchPreparationPanic runs preparation, reporting a panic as an error.
func catchPreparationPanic[T any](preparation func() T) (value T, err error) {
	defer func() {
		if payload := recover(); payload != nil {
			err = fmt.Errorf("harness preparation panicked: %s", panicPayloadMessage(payload))
		}
	}()
	return preparation(), nil
}

func panicPayloadMessage(payload any) string {
	switch message := payload.(type) {
	case string:
		return message
	case error:
		return message.Error()
	default:
		return "non-string panic payload"
	}
}

// prepareProjectTools is the fast part of project tools. Whatever mise
// reports missing is installed by the returned projectToolsInstall in the background;
// the agent has already been told, so the harness starts at once. Returns
// whether the project tools environment was applied.
func prepareProjectTools(ctx context.Context, root string, roots []string, config *core.WorkerLaunchConfig, baseEnvironment map[string]string, status *core.Watch[preparationSnapshot], relay *RelayCell) (bool, *projectToolsInstall, error) {
	environment := maps.Clone(baseEnvironment)
	if environment == nil {
		environment = map[string]string{}
	}
	maps.Copy(environment, config.Environment)
	applied := false
	install, err := func() (*projectToolsInstall, error) {
		recipes, err := projectToolsRecipeRoots(ctx, roots)
		if err != nil {
			return nil, err
		}
		if len(recipes) == 0 {
			// Without a recipe the image's own environment (its Rust homes,
			// PATH and tools) is left exactly as it was.
			return nil, nil
		}
		if err := projectToolsApplyEnvironment(environment, roots, nil); err != nil {
			return nil, err
		}
		config.Environment = maps.Clone(environment)
		applied = true
		snapshot := status.Borrow()
		snapshot.state = core.HarnessPreparationPreparing{Step: "Install project tools", SinceMs: time.Now().UnixMilli()}
		// Planning has no bound of its own; its threads get a local step's.
		snapshot.stepDeadline = time.Now().Add(localPreparationStepTimeout)
		status.SendReplace(snapshot)
		if _, err := core.SpawnBlocking(func() struct{} {
			RecordStartupStep(root, "Install project tools")
			return struct{}{}
		}).Join(ctx); err != nil {
			return nil, fmt.Errorf("record project tools startup step: %w", err)
		}
		// Installing mise itself (when the image has none) and asking what is
		// missing are short; the installation proper runs after the harness
		// starts. The preparation owner cancels this on close.
		install, err := projectToolsPlan(ctx, root, recipes, environment)
		if err != nil {
			return nil, err
		}
		config.Environment = maps.Clone(environment)
		if install != nil {
			if err := install.tellAgentStarted(relay); err != nil {
				return nil, err
			}
		}
		return install, nil
	}()
	if err != nil {
		logPath := filepath.Join(root, projectToolsLogFile)
		message := fmt.Sprintf("Project tools preparation failed: %s. See %s. The session will continue.", err, logPath)
		log, openErr := os.OpenFile(logPath, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o666)
		if openErr != nil {
			slog.Warn("open project tools failure log", "error", openErr)
		} else {
			if _, err := log.WriteString(message + "\n"); err != nil {
				slog.Warn("write project tools failure log", "error", err)
			}
			_ = log.Close()
		}
		guard := relay.Lock()
		_, err := guard.RecordObservation(core.RelayObservationNotice{Message: message})
		guard.Unlock()
		if err != nil {
			return false, nil, err
		}
		install = nil
	}
	return applied, install, nil
}

// retainChangedEnvironment keeps the explicit entries and those that differ
// from the base environment: only explicit and Mjolnir-generated overrides
// are persisted, never shell exports.
func retainChangedEnvironment(environment, explicit, base map[string]string) {
	for name, value := range environment {
		if _, ok := explicit[name]; ok {
			continue
		}
		if baseValue, ok := base[name]; ok && baseValue == value {
			delete(environment, name)
		}
	}
}

// prepareAndStartHarness prepares the session's harness and starts its
// bridge. ctx and cancel are the preparation's cancellation token.
func prepareAndStartHarness(ctx context.Context, cancel context.CancelFunc, setup harnessPreparationSetup, dispatchWakes **dispatchWakeChannel, status *core.Watch[preparationSnapshot]) (*runningHarness, error) {
	root := setup.root
	config := setup.config
	config.Environment = maps.Clone(config.Environment)
	if config.Environment == nil {
		config.Environment = map[string]string{}
	}
	relay := setup.relay
	credentials := setup.credentials
	projectToolsRoots, err := projectToolsTakeRoots(config.Environment)
	if err != nil {
		return nil, err
	}
	managedSubagent := config.IsManagedSubagent()
	reviewTools := config.ReviewTools()
	var subagentRole *core.SubagentMcpRole
	if role, ok := config.SubagentMCPRole(); ok {
		subagentRole = &role
	}
	credentialHome := func() (string, error) {
		if credentials.err != nil {
			return "", errors.New(credentials.err.Error())
		}
		return credentials.endpoint.Home, nil
	}
	loginBudget, err := preparationStep(ctx, root, status, "login-resolve", localPreparationStepTimeout)
	if err != nil {
		return nil, err
	}
	// Keep non-Podman Git includes pinned to the worker's original home.
	// Podman supplies its provisioned config path separately because root
	// worker processes can have HOME=/root.
	var workerHome *string
	if home, ok := os.LookupEnv("HOME"); ok {
		workerHome = &home
	}
	baseEnvironment, err := boundedPreparationStep(ctx, &loginBudget, func(ctx context.Context) (map[string]string, error) {
		resolveCtx, stop := context.WithTimeout(ctx, 10*time.Second)
		defer stop()
		environment, err := harnessinstall.Resolve(resolveCtx)
		if err != nil && errors.Is(resolveCtx.Err(), context.DeadlineExceeded) && ctx.Err() == nil {
			return nil, errors.New("login environment resolution exceeded its 10-second bound: deadline has elapsed")
		}
		return environment, err
	})
	if err != nil {
		return nil, err
	}
	if config.Harness == core.HarnessKindOpenCode {
		// Retained launch files can predate the explicit OpenCode config pin.
		endpoint, err := credentialEndpoint(&config)
		if err != nil {
			return nil, errors.New(err.Error())
		}
		config.Harness.ConfigureStagedHomeEnvironment(endpoint.Home, config.Environment)
	}
	sessionEnvironment := maps.Clone(baseEnvironment)
	if sessionEnvironment == nil {
		sessionEnvironment = map[string]string{}
	}
	recordRawLaunch := config.Environment[core.SessionRawLaunchRecordEnv] == "1"
	maps.Copy(sessionEnvironment, config.Environment)
	for _, name := range []string{core.SessionGitConfigIncludePath, core.SessionManagedSubagentEnv, core.SessionRawLaunchRecordEnv, core.SessionMessageMCPEnv, core.SessionProjectToolsRoots} {
		delete(sessionEnvironment, name)
	}
	if err := restoreMbxLauncher(root, workerHome, sessionEnvironment); err != nil {
		return nil, err
	}
	noGithubCredentials := config.NoGithubCredentials
	gitConfigInclude := setup.sessionGitConfigInclude
	explicitEnvironment := maps.Clone(config.Environment)
	delete(explicitEnvironment, core.SessionManagedSubagentEnv)
	delete(explicitEnvironment, core.SessionRawLaunchRecordEnv)
	delete(explicitEnvironment, core.SessionMessageMCPEnv)
	// Credential sync installs the session's slopcop login in its harness
	// home.
	slopcopEndpoint, slopcopErr := credentialEndpoint(&config)
	sessionEnvironment, err = boundedBlockingPreparationStep(ctx, &loginBudget, func(stepCtx context.Context) (map[string]string, error) {
		if stepCtx.Err() != nil {
			return nil, errors.New("preparation cancelled before resolving Git credentials")
		}
		if err := ConfigureGithubCLI(root, sessionEnvironment, workerHome, gitConfigInclude, noGithubCredentials); err != nil {
			return nil, err
		}
		if slopcopErr != nil {
			slog.Warn("slopcop wrapper not installed: " + slopcopErr.Error())
		} else if err := ConfigureSlopcopCLI(root, filepath.Join(slopcopEndpoint.Home, core.LoginFile)); err != nil {
			return nil, err
		}
		retainChangedEnvironment(sessionEnvironment, explicitEnvironment, baseEnvironment)
		return sessionEnvironment, nil
	})
	if err != nil {
		return nil, err
	}
	// Persist only explicit and Mjolnir-generated overrides, never shell
	// exports.
	config.Environment = maps.Clone(sessionEnvironment)

	// Every launch records its `gh` approval where the selected harness
	// reads it, and OpenCode's denials apply in every execution policy.
	{
		budget, err := preparationStep(ctx, root, status, "command-permissions", localPreparationStepTimeout)
		if err != nil {
			return nil, err
		}
		home, err := credentialHome()
		if err != nil {
			return nil, err
		}
		sessionID := config.SessionID
		harness := config.Harness
		policy := config.ExecutionPolicy
		// A child, a None choice and a Mjolnir choice all leave delegation to
		// Mjolnir (or to nobody), so OpenCode's own `subagent` tool goes.
		denials := openCodeDenials{nativeSubagent: config.Subagents.SuppressesNative()}
		nativeExplicitEnvironment := maps.Clone(config.Environment)
		environment := maps.Clone(baseEnvironment)
		if environment == nil {
			environment = map[string]string{}
		}
		maps.Copy(environment, nativeExplicitEnvironment)
		config.Environment, err = boundedBlockingPreparationStep(ctx, &budget, func(stepCtx context.Context) (map[string]string, error) {
			if stepCtx.Err() != nil {
				return nil, errors.New("preparation cancelled before configuring command permissions")
			}
			if err := configureNativeRules(root, sessionID, home, harness, policy, denials, environment); err != nil {
				return nil, err
			}
			retainChangedEnvironment(environment, nativeExplicitEnvironment, baseEnvironment)
			return environment, nil
		})
		if err != nil {
			return nil, err
		}
	}

	if recordRawLaunch && !setup.restarting {
		budget, err := preparationStep(ctx, root, status, "raw-launch-record", localPreparationStepTimeout)
		if err != nil {
			return nil, err
		}
		cwd, sessionID := config.Cwd, config.SessionID
		if _, err := boundedBlockingPreparationStep(ctx, &budget, func(stepCtx context.Context) (struct{}, error) {
			if stepCtx.Err() != nil {
				return struct{}{}, errors.New("preparation cancelled before recording raw launch baseline")
			}
			if err := checkpoint.RecordLaunchIfAbsent(checkpoint.SystemGit{}, cwd, sessionID); err != nil {
				// Capture falls back to keeping all local refs and stashes
				// when the launch record is absent, so a read-only Git
				// directory must not block starting the session.
				slog.Warn("could not record raw launch baseline; continuing without baseline",
					"session_id", sessionID, "cwd", cwd, "error", err)
			}
			return struct{}{}, nil
		}); err != nil {
			return nil, err
		}
	}

	boundaryMailboxHookAvailable := false
	var mailboxDeliveryMode *core.MailboxDeliveryMode
	profileRegistration := false
	switch {
	case config.Harness == core.HarnessKindCodex:
		budget, err := preparationStep(ctx, root, status, "subagent-profile", localPreparationStepTimeout)
		if err != nil {
			return nil, err
		}
		home, err := credentialHome()
		if err != nil {
			return nil, err
		}
		sessionID := config.SessionID
		environment := maps.Clone(config.Environment)
		policy := config.ExecutionPolicy
		subtleSubagents := config.SubtleSubagents
		type codexProfile struct {
			registered  bool
			environment map[string]string
			mode        core.MailboxDeliveryMode
		}
		configured, err := boundedBlockingPreparationStep(ctx, &budget, func(stepCtx context.Context) (codexProfile, error) {
			if stepCtx.Err() != nil {
				return codexProfile{}, errors.New("preparation cancelled before configuring the Codex profile")
			}
			registered, err := configureCodexProfile(root, sessionID, home, environment, subagentRole, policy, subtleSubagents, reviewTools, managedSubagent)
			if err != nil {
				return codexProfile{}, err
			}
			// The staged home holds the configuration Codex runs with,
			// overlay included. An unreadable one fails startup here rather
			// than guessing a delivery mode.
			provider, err := core.ReadCodexProvider(home, nil)
			if err != nil {
				return codexProfile{}, fmt.Errorf("resolve the Codex provider for mailbox delivery: %w", err)
			}
			return codexProfile{registered, environment, core.MailboxDeliveryModeForCodexProvider(provider)}, nil
		})
		if err != nil {
			return nil, err
		}
		config.Environment = configured.environment
		mailboxDeliveryMode = &configured.mode
		profileRegistration = configured.registered
	case config.Harness == core.HarnessKindClaude:
		budget, err := preparationStep(ctx, root, status, "harness-profile", localPreparationStepTimeout)
		if err != nil {
			return nil, err
		}
		home, err := credentialHome()
		if err != nil {
			return nil, err
		}
		sessionID := config.SessionID
		oracle, orchestratorsEnabled := config.Oracle, config.OrchestratorsEnabled
		subtleSubagents := config.SubtleSubagents
		if managedSubagent {
			// Marketplace refreshes on every child start fetch from Git or
			// HTTP sources; the child's staged plugins are copied from the
			// person's profile, which keeps refreshing them. The same switch
			// stops Claude Code's own updater, which the pinned managed
			// install does not use.
			if _, ok := config.Environment["DISABLE_AUTOUPDATER"]; !ok {
				config.Environment["DISABLE_AUTOUPDATER"] = "1"
			}
			// Claude Code otherwise reminds a child that has gone several
			// steps without text that "the user hasn't heard from you",
			// prompting status messages nobody reads; the parent sees only
			// the child's report.
			if _, ok := config.Environment["CLAUDE_CODE_SILENT_TURN_REMINDER"]; !ok {
				config.Environment["CLAUDE_CODE_SILENT_TURN_REMINDER"] = "false"
			}
		}
		// config.Environment keeps only overrides; the login shell's HOME,
		// which the harness inherits, is in the base environment.
		var userHome *string
		if home, ok := config.Environment["HOME"]; ok {
			userHome = &home
		} else if home, ok := baseEnvironment["HOME"]; ok {
			userHome = &home
		}
		type claudeProfile struct {
			registered           bool
			mailboxHookAvailable bool
		}
		configured, err := boundedBlockingPreparationStep(ctx, &budget, func(stepCtx context.Context) (claudeProfile, error) {
			if stepCtx.Err() != nil {
				return claudeProfile{}, errors.New("preparation cancelled before configuring the Claude profile")
			}
			registered, err := configureClaudeSubagentMCP(root, sessionID, home, subagentRole, oracle, orchestratorsEnabled, subtleSubagents, reviewTools)
			if err != nil {
				return claudeProfile{}, err
			}
			mailboxHookAvailable, err := configureClaudeMailboxHook(root, sessionID, home)
			if err != nil {
				return claudeProfile{}, err
			}
			if err := excludeDefaultClaudeInstructions(root, sessionID, home, userHome); err != nil {
				return claudeProfile{}, err
			}
			if managedSubagent {
				if err := configureClaudeSubagentSettings(root, sessionID, home); err != nil {
					return claudeProfile{}, err
				}
			}
			return claudeProfile{registered, mailboxHookAvailable}, nil
		})
		if err != nil {
			return nil, err
		}
		boundaryMailboxHookAvailable = configured.mailboxHookAvailable
		profileRegistration = configured.registered
	case config.Harness == core.HarnessKindAntigravity:
		budget, err := preparationStep(ctx, root, status, "harness-profile", localPreparationStepTimeout)
		if err != nil {
			return nil, err
		}
		home, err := credentialHome()
		if err != nil {
			return nil, err
		}
		sessionID := config.SessionID
		available, err := boundedBlockingPreparationStep(ctx, &budget, func(stepCtx context.Context) (bool, error) {
			if stepCtx.Err() != nil {
				return false, errors.New("preparation cancelled before configuring the Antigravity profile")
			}
			return configureAntigravityMailboxHook(root, sessionID, home)
		})
		if err != nil {
			return nil, err
		}
		boundaryMailboxHookAvailable = available
		mode := core.MailboxDeliveryModeTurnBoundary
		if available {
			mode = core.MailboxDeliveryModeToolBoundary
		}
		mailboxDeliveryMode = &mode
		// Antigravity receives its MCP servers through ACP.
		profileRegistration = false
	case config.Harness == core.HarnessKindOpenCode:
		budget, err := preparationStep(ctx, root, status, "mailbox-plugin", localPreparationStepTimeout)
		if err != nil {
			return nil, err
		}
		environment := maps.Clone(config.Environment)
		// Under configured approvals the Mjolnir guardian answers OpenCode's
		// permission requests, and its hook makes OpenCode ask.
		var guardianRoots []string
		if config.ExecutionPolicy == core.ExecutionPolicyConfiguredApprovals {
			guardianRoots = append([]string{config.Cwd}, config.AdditionalDirectories...)
		}
		config.Environment, err = boundedBlockingPreparationStep(ctx, &budget, func(stepCtx context.Context) (map[string]string, error) {
			if stepCtx.Err() != nil {
				return nil, errors.New("preparation cancelled before configuring the OpenCode mailbox plugin")
			}
			if err := configureOpencodeMailboxHook(root, environment); err != nil {
				return nil, err
			}
			if guardianRoots != nil {
				if err := configureOpencodeGuardian(root, guardianRoots, environment); err != nil {
					return nil, err
				}
			}
			return environment, nil
		})
		if err != nil {
			return nil, err
		}
		// OpenCode receives its MCP servers through ACP.
		profileRegistration = false
	case config.Harness == core.HarnessKindKimi && subagentRole != nil:
		budget, err := preparationStep(ctx, root, status, "harness-profile", localPreparationStepTimeout)
		if err != nil {
			return nil, err
		}
		home, err := credentialHome()
		if err != nil {
			return nil, err
		}
		sessionID := config.SessionID
		role := *subagentRole
		if _, err := boundedBlockingPreparationStep(ctx, &budget, func(stepCtx context.Context) (struct{}, error) {
			if stepCtx.Err() != nil {
				return struct{}{}, errors.New("preparation cancelled before configuring the Kimi profile")
			}
			return struct{}{}, configureKimiSubagentMCP(root, sessionID, home, role, reviewTools)
		}); err != nil {
			return nil, err
		}
		profileRegistration = true
	}
	{
		guard := relay.Lock()
		guard.SetSteeringDeliveryMode(config.Harness, boundaryMailboxHookAvailable)
		// Resolved before the harness starts, so no hook can drain earlier.
		if mailboxDeliveryMode != nil {
			guard.SetMailboxDeliveryMode(*mailboxDeliveryMode)
		}
		guard.Unlock()
	}
	if config.PreparesReview() {
		budget, err := preparationStep(ctx, root, status, "review-gate", localPreparationStepTimeout)
		if err != nil {
			return nil, err
		}
		launch := config
		if _, err := boundedBlockingPreparationStep(ctx, &budget, func(stepCtx context.Context) (struct{}, error) {
			if stepCtx.Err() != nil {
				return struct{}{}, errors.New("review gate preparation cancelled")
			}
			notices, err := prepareReviewGate(&launch)
			if err != nil {
				return struct{}{}, err
			}
			guard := relay.Lock()
			defer guard.Unlock()
			for _, message := range notices {
				if _, err := guard.RecordObservation(core.RelayObservationWarning{Message: message}); err != nil {
					return struct{}{}, err
				}
			}
			return struct{}{}, nil
		}); err != nil {
			return nil, err
		}
	}

	projectToolsApplied := false
	var projectToolsPlanned *projectToolsInstall
	if projectToolsRoots != nil {
		projectToolsApplied, projectToolsPlanned, err = prepareProjectTools(ctx, root, *projectToolsRoots, &config, baseEnvironment, status, relay)
		if err != nil {
			return nil, err
		}
	}

	keepBudget, err := preparationStep(ctx, root, status, "session-keep", localPreparationStepTimeout)
	if err != nil {
		return nil, err
	}
	keepCwd, keepSessionID := config.Cwd, config.SessionID
	keepEnvironment := maps.Clone(config.Environment)
	config.Environment, err = boundedBlockingPreparationStep(ctx, &keepBudget, func(stepCtx context.Context) (map[string]string, error) {
		if stepCtx.Err() != nil {
			return nil, errors.New("preparation cancelled before preparing the session keep directory")
		}
		if _, err := core.InstallSessionKeepDir(keepCwd, keepSessionID, keepEnvironment); err != nil {
			// A read-only checkout, or a symlinked `.mj` path, must not
			// block the session. The harness then has no MJ_KEEP_DIR.
			slog.Warn("could not prepare the session keep directory; continuing without MJ_KEEP_DIR",
				"session_id", keepSessionID, "cwd", keepCwd, "error", err)
		}
		return keepEnvironment, nil
	})
	if err != nil {
		return nil, err
	}

	harnessBudget, err := preparationStep(ctx, root, status, "harness-resolve", 20*time.Minute)
	if err != nil {
		return nil, err
	}
	preparedHarness, err := boundedPreparationStep(ctx, &harnessBudget, func(ctx context.Context) (*PreparedHarnessLaunch, error) {
		prepared, err := PrepareHarnessLaunch(ctx, config.Harness, config.HarnessRuntime, config.ExecutionPolicy, primarySupervisorSpec(&config))
		if err != nil {
			return nil, err
		}
		if err := freezeNodeEntrypoints(ctx, root, prepared); err != nil {
			prepared.Release()
			return nil, err
		}
		return prepared, nil
	})
	if err != nil {
		return nil, err
	}
	// The preparation owns the runtime leases until the running harness
	// takes them; every failure below releases them.
	leasesTaken := false
	defer func() {
		if !leasesTaken {
			preparedHarness.Release()
		}
	}()
	if projectToolsRoots != nil && projectToolsApplied {
		if err := projectToolsApplyEnvironment(preparedHarness.environment, *projectToolsRoots, nil); err != nil {
			guard := relay.Lock()
			_, recordErr := guard.RecordObservation(core.RelayObservationNotice{
				Message: fmt.Sprintf("Project tools environment could not be applied: %s. The session will continue.", err),
			})
			guard.Unlock()
			if recordErr != nil {
				return nil, recordErr
			}
		}
		// Managed bridge preparation can prepend its own Node bin directory.
		// Restore the same session tool environment before launching the
		// bridge.
		if preparedHarness.Spec.Environment == nil {
			preparedHarness.Spec.Environment = map[string]string{}
		}
		maps.Copy(preparedHarness.Spec.Environment, preparedHarness.environment)
	}
	var managedCacheRoot *string
	if preparedHarness.managed != nil {
		cacheRoot := preparedHarness.managed.CacheRoot
		managedCacheRoot = &cacheRoot
	}

	acpBudget, err := preparationStep(ctx, root, status, "acp-setup", localPreparationStepTimeout)
	if err != nil {
		return nil, err
	}
	acpSetup := acpPreparationSetup{
		root:                 root,
		config:               config,
		sessionEnvironment:   preparedHarness.environment,
		supervisorSpec:       preparedHarness.Spec,
		relay:                relay,
		resumeSession:        setup.resumeSession,
		nativeSessionHistory: setup.nativeSessionHistory,
		profileRegistration:  profileRegistration,
		managedSubagent:      managedSubagent,
		subagentRole:         subagentRole,
		runtime:              core.CurrentRuntime(),
		managedCacheRoot:     managedCacheRoot,
	}
	preparedACP, err := boundedBlockingPreparationStep(ctx, &acpBudget, func(stepCtx context.Context) (*preparedAcpSetup, error) {
		if stepCtx.Err() != nil {
			return nil, errors.New("preparation cancelled before ACP setup")
		}
		return buildACPSetup(acpSetup)
	})
	if err != nil {
		return nil, err
	}

	if _, err := preparationStep(ctx, root, status, "bridge-start", localPreparationStepTimeout); err != nil {
		if preparedACP.subagentSocketGuard != nil {
			preparedACP.subagentSocketGuard.Release()
		}
		return nil, err
	}
	// This stage only schedules prepared tasks; it performs no filesystem or
	// process I/O and cannot block, so it needs no bound of its own.
	started, err := startBridge(ctx, cancel, preparedACP, relay, dispatchWakes, setup.kimiTaskHome, setup.antigravityTaskHome, setup.capture, core.CurrentRuntime(), status)
	if err != nil {
		if preparedACP.subagentSocketGuard != nil {
			preparedACP.subagentSocketGuard.Release()
		}
		return nil, err
	}
	running := started.intoRunning()
	leasesTaken = true
	running.managedHarness = preparedHarness.managed
	running.managedNode = preparedHarness.managedNode
	if projectToolsPlanned != nil {
		running.projectTools = backgroundInstallSpawn(*projectToolsPlanned, root, relay)
	}
	return running, nil
}

// stopPreparation stops a preparation the worker no longer wants, including
// a harness it started just before the stop.
func stopPreparation(ctx context.Context, cancel context.CancelFunc, task *core.JoinHandle[preparationOutcome]) {
	cancel()
	outcome, err := task.Join(context.Background())
	if err != nil {
		return
	}
	if started, ok := outcome.(preparationOutcomeStarted); ok {
		shutdownRunningHarness(ctx, started.Value)
	}
}

func shutdownRunningHarness(ctx context.Context, running *runningHarness) {
	running.acpShutdown()
	stopFailedCoordinator(running.eventAbort, running.eventTask)
	if _, err := running.acpTask.Join(context.Background()); err != nil {
		slog.Warn("ACP runtime stopped while cancelling harness preparation", "error", err)
	}
	running.shellCleanup.Wait()
	if running.subagentSocketGuard != nil {
		running.subagentSocketGuard.Release()
	}
	if running.harnessGC != nil {
		running.harnessGC()
	}
	running.releaseLeases()
}

// PrepareManagedHarness installs or validates the managed harness named by a
// proposed launch config without starting, stopping, or otherwise touching
// the session worker.
//
// The preparation owns the runtime lease for the whole call, so preparation
// and the supervisor that later uses the same runtime never overlap.
func PrepareManagedHarness(ctx context.Context, config core.WorkerLaunchConfig) error {
	environment := maps.Clone(config.TargetEnvironment)
	if environment == nil {
		environment = map[string]string{}
	}
	delete(environment, core.SessionNitroMbxEnv)
	delete(environment, core.SessionGitConfigIncludePath)
	maps.Copy(environment, config.Environment)
	delete(environment, core.SessionGitConfigIncludePath)
	config.Environment = environment
	if config.RequiresHarnessPreparation() {
		prepared, err := PrepareHarnessLaunch(ctx, config.Harness, config.HarnessRuntime, config.ExecutionPolicy, primarySupervisorSpec(&config))
		if err != nil {
			return err
		}
		prepared.Release()
	}
	return nil
}

// serveTerminalRelay serves a sealed or checkpoint-only relay, which has no
// harness, until a durable failure is reported.
func serveTerminalRelay(ctx context.Context, listener *net.UnixListener, relay *RelayCell, dispatchWake *dispatchWakeChannel, credentials credentialEndpointResult, runtime connectionRuntime, fatal chan<- error, fatalReports <-chan error) error {
	acceptContext, stopAccepting := context.WithCancel(ctx)
	defer stopAccepting()
	connections := context.WithoutCancel(ctx)
	accepted := make(chan error, 1)
	go func() {
		accepted <- acceptControlConnections(acceptContext, listener, func(stream *net.UnixConn) {
			go func() {
				// A sealed session has no ACP runtime left, so compaction
				// cannot be served here.
				if err := serveClientWithMemory(connections, stream, relay, dispatchWake, credentials, runtime, fatal); err != nil {
					slog.Warn("closed relay proxy client disconnected", "error", err)
				}
			}()
		})
	}()
	select {
	case err := <-accepted:
		if ctx.Err() != nil {
			return ctx.Err()
		}
		return fmt.Errorf("accept closed relay proxy: %w", err)
	case report := <-fatalReports:
		return fmt.Errorf("relay durable state became unwritable: %w", report)
	}
}

// connectionRuntime is the live services one relay connection can reach
// beyond the durable relay: everything answered on the connection instead of
// being journaled.
type connectionRuntime struct {
	projectMemory projectMemoryEndpoint
	// commands is the ACP coordinator's command channel, or nil once the
	// session is sealed and no ACP runtime is left to serve scratch prompts.
	commands *commandChannel
	// reviewer is capture state prepared with the primary harness. A sealed
	// session has none.
	reviewer    *reviewCapture
	subagents   *SubagentEndpoint
	cpu         *core.Watch[cpuRead]
	preparation *core.Watch[preparationSnapshot]
	// preparationCancel cancels the harness preparation; nil without one.
	preparationCancel context.CancelFunc
	// antigravityHome is the Antigravity home an Antigravity Stop samples.
	antigravityHome  *string
	hasSubagentTools bool
}

func defaultConnectionRuntime() connectionRuntime {
	return connectionRuntime{projectMemory: defaultProjectMemoryEndpoint()}
}

// serveClient serves one connection with only an ACP command channel (Rust:
// cfg(test)).
func serveClient(ctx context.Context, stream *net.UnixConn, relay *RelayCell, dispatchWake *dispatchWakeChannel, credentials credentialEndpointResult, commands *commandChannel, fatal chan<- error) error {
	runtime := defaultConnectionRuntime()
	runtime.commands = commands
	return serveClientWithMemory(ctx, stream, relay, dispatchWake, credentials, runtime, fatal)
}

// connectionFrame is one request line read from a connection, the end of
// the stream (ok false, err nil), or why reading failed.
type connectionFrame struct {
	line string
	ok   bool
	err  error
}

// disconnected is Rust's fill_buf outcome for a frame that arrived while a
// request was in progress: an error, or whether the stream ended (true) or
// sent more (false).
func (f connectionFrame) disconnected() (bool, error) {
	if f.err != nil {
		return false, f.err
	}
	return !f.ok, nil
}

// serveClientWithMemory serves one relay connection until it closes.
func serveClientWithMemory(ctx context.Context, stream *net.UnixConn, relay *RelayCell, dispatchWake *dispatchWakeChannel, credentials credentialEndpointResult, runtime connectionRuntime, fatal chan<- error) error {
	return serveClientWithSharedCredentials(ctx, stream, relay, dispatchWake, newSharedCredentials(credentials), runtime, fatal)
}

// serveClientWithSharedCredentials serves one relay connection with the
// worker-wide credential endpoint until it closes.
func serveClientWithSharedCredentials(ctx context.Context, stream *net.UnixConn, relay *RelayCell, dispatchWake *dispatchWakeChannel, credentials *sharedCredentials, runtime connectionRuntime, fatal chan<- error) error {
	defer stream.Close()
	projectMemory := runtime.projectMemory
	commands := runtime.commands
	reviewer := runtime.reviewer
	subagents := runtime.subagents
	cpu := runtime.cpu
	preparation := runtime.preparation
	preparationCancel := runtime.preparationCancel
	hasSubagentTools := runtime.hasSubagentTools
	antigravityHome := runtime.antigravityHome

	guard := relay.Lock()
	relayRoot := guard.Root()
	guard.Unlock()
	guard = relay.Lock()
	sessionID := guard.OperationalState().SessionID
	guard.Unlock()

	// One goroutine reads the connection's frames. A frame that arrives
	// while a request is in progress is what Rust's fill_buf observes: the
	// peer went away or broke the request-then-response protocol. The end of
	// the stream, or a read error, stays observable however often it is
	// asked for, as it does for a buffered reader.
	frames := make(chan connectionFrame)
	readerDone := make(chan struct{})
	defer close(readerDone)
	go func() {
		reader := bufio.NewReader(stream)
		for {
			line, ok, err := readBoundedLine(reader, core.MaxFrameBytes)
			frame := connectionFrame{line: line, ok: ok, err: err}
			for {
				select {
				case frames <- frame:
				case <-readerDone:
					return
				}
				if ok && err == nil {
					break
				}
			}
		}
	}()

	var writer io.Writer = stream
	checkpointBarriers := map[string]struct{}{}
	var feed subagentFeed
	var guardianGeneration uint64
	var transfer connectionProjectMemoryTransfer
	var liveBaselineDownload *core.FrozenSnapshotDownload
	servingResult := func() error {
		for {
			var frame connectionFrame
			select {
			case frame = <-frames:
			case <-ctx.Done():
				return ctx.Err()
			}
			if frame.err != nil {
				return frame.err
			}
			if !frame.ok {
				return nil
			}
			// One decoder owns this boundary: an unknown method is a
			// protocol error the controller can act on, and an unreadable
			// frame is answered rather than dropping the connection.
			var envelope core.RelayRequestEnvelope
			switch decoded := core.DecodeRelayRequest([]byte(frame.line)).(type) {
			case core.DecodedRelayRequestKnown:
				envelope = *decoded.Envelope
			case core.DecodedRelayRequestUnknown:
				response := core.UnsupportedRelayMethodResponse(decoded.RequestID, decoded.ProtocolVersion, decoded.Method)
				if err := writeLoggedResponse(writer, &response, sessionID, "unknown"); err != nil {
					return err
				}
				continue
			case core.DecodedRelayRequestInvalid:
				response := core.InvalidRelayRequestResponse(decoded.RequestID, decoded.ProtocolVersion, decoded.Message)
				if err := writeLoggedResponse(writer, &response, sessionID, "invalid"); err != nil {
					return err
				}
				continue
			}
			if body := core.RelayProtocolRejection(&envelope); body != nil {
				response := core.RelayResponseEnvelope{
					RequestID:       envelope.RequestID,
					ProtocolVersion: envelope.ProtocolVersion,
					Body:            body,
				}
				if err := writeLoggedResponse(writer, &response, sessionID, "incompatible"); err != nil {
					return err
				}
				continue
			}
			var prepSnapshot preparationSnapshot
			if preparation != nil {
				prepSnapshot = preparation.Borrow()
			}
			requestCommands, requestReviewer, requestSubagents := commands, reviewer, subagents
			if services := prepSnapshot.services; services != nil {
				requestCommands, requestReviewer = services.commands, services.reviewer
				if services.subagents != nil {
					requestSubagents = services.subagents
				}
			}
			respond := func(body core.RelayResponseBody, operation string) error {
				response := core.RelayResponseEnvelope{
					RequestID:       envelope.RequestID,
					ProtocolVersion: envelope.ProtocolVersion,
					Body:            body,
				}
				return writeLoggedResponse(writer, &response, sessionID, operation)
			}
			switch request := envelope.Request.(type) {
			case core.RelayRequestWaitForChange:
				body, err := waitForChange(ctx, relay, preparation, requestSubagents, &feed, &guardianGeneration, request.AfterOrdinal, request.StateDigest)
				if err != nil {
					body = compactionError(core.RelayErrorCodeInternal, "wait for a relay change: "+err.Error())
				}
				if err := respond(body, "wait_for_change"); err != nil {
					return err
				}
				continue
			case core.RelayRequestSetOperatorAway:
				guard := relay.Lock()
				guard.operatorAway.set(core.OperatorPresence{Away: request.Away, Revision: request.Revision})
				state := guard.OperationalState()
				guard.Unlock()
				if err := respond(core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadStatus{State: state}}, "set_operator_away"); err != nil {
					return err
				}
				continue
			case core.RelayRequestBeginLiveBaseline, core.RelayRequestReadLiveBaselinePage, core.RelayRequestAbortLiveBaseline:
				var cut *core.LiveSessionBaseline
				if _, begin := request.(core.RelayRequestBeginLiveBaseline); begin {
					outcome, ended, err := awaitLiveBaseline(relay, frames, envelope, writer, sessionID)
					if err != nil {
						return err
					}
					if err := func() error {
						guard := relay.Lock()
						defer guard.Unlock()
						return guard.resumeAfterAdoptionWait()
					}(); err != nil {
						return err
					}
					_ = dispatchWake.trySend()
					if ended {
						return nil
					}
					if outcome.Err != nil {
						if err := respond(compactionError(core.RelayErrorCodeInvalidState, outcome.Err.Error()), "live_baseline_refused"); err != nil {
							return err
						}
						continue
					}
					cut = &outcome.Baseline
				}
				download := liveBaselineDownload
				liveBaselineDownload = nil
				type reply struct {
					payload  core.RelayResponsePayload
					err      error
					download *core.FrozenSnapshotDownload
				}
				replied, joinErr := core.SpawnBlocking(func() reply {
					payload, err := core.LiveBaselineReply(envelope.Request, cut, &download, envelope.RequestID, envelope.ProtocolVersion)
					return reply{payload, err, download}
				}).Join(context.Background())
				if joinErr != nil {
					return fmt.Errorf("live recovery baseline download task failed: %w", joinErr)
				}
				liveBaselineDownload = replied.download
				var body core.RelayResponseBody = core.RelayResponseBodyOk{Payload: replied.payload}
				if replied.err != nil {
					body = compactionError(core.RelayErrorCodeInvalidRequest, replied.err.Error())
				}
				if err := respond(body, "live_baseline"); err != nil {
					return err
				}
				continue
			case core.RelayRequestAttachmentPresent, core.RelayRequestInstallAttachment, core.RelayRequestReadAttachment:
				operation := core.RelayRequestMethodName(envelope.Request)
				store := core.AttachmentStoreWorker(relayRoot)
				type answered struct {
					payload core.RelayResponsePayload
					err     error
				}
				result, joinErr := core.SpawnBlocking(func() answered {
					switch request := request.(type) {
					case core.RelayRequestAttachmentPresent:
						present, err := store.Contains(&request.Reference)
						return answered{core.RelayResponsePayloadAttachmentPresent{Present: present}, err}
					case core.RelayRequestInstallAttachment:
						if len(request.Data) > (core.MaxImageBytes+2)/3*4 {
							return answered{err: errors.New("image upload exceeds 700 KiB")}
						}
						data, err := base64.StdEncoding.DecodeString(request.Data)
						if err != nil {
							return answered{err: err}
						}
						if err := store.Install(&request.Reference, data); err != nil {
							return answered{err: err}
						}
						return answered{payload: core.RelayResponsePayloadAttachmentInstalled{}}
					case core.RelayRequestReadAttachment:
						data, err := store.Read(&request.Reference)
						if err != nil {
							return answered{err: err}
						}
						return answered{payload: core.RelayResponsePayloadAttachmentData{Data: base64.StdEncoding.EncodeToString(data)}}
					}
					panic("unreachable")
				}).Join(context.Background())
				var body core.RelayResponseBody
				switch {
				case joinErr != nil:
					body = compactionError(core.RelayErrorCodeInternal, "image attachment task failed: "+joinErr.Error())
				case result.err != nil:
					body = compactionError(core.RelayErrorCodeInvalidRequest, result.err.Error())
				default:
					body = core.RelayResponseBodyOk{Payload: result.payload}
				}
				if err := respond(body, operation); err != nil {
					return err
				}
				continue
			case core.RelayRequestCpuUsage:
				var value cpuRead
				if cpu != nil {
					value = cpu.Borrow()
				}
				var body core.RelayResponseBody
				if value.Err != nil {
					body = core.RelayResponseBodyError{Error: core.RelayProtocolError{
						Code:    core.RelayErrorCodeInternal,
						Message: value.Err.Error(),
					}}
				} else {
					body = core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadCpuUsage{Usage: value.Usage}}
				}
				if err := respond(body, "cpu_usage"); err != nil {
					return err
				}
				continue
			case core.RelayRequestCredentialState, core.RelayRequestAwsCredentialsState,
				core.RelayRequestInstallAwsCredentials, core.RelayRequestRemoveAwsCredentials,
				core.RelayRequestAwsCredentialsScope, core.RelayRequestSlopcopLoginState,
				core.RelayRequestInstallSlopcopLogin, core.RelayRequestRemoveSlopcopLogin,
				core.RelayRequestReadCredentials, core.RelayRequestInstallCredentials,
				core.RelayRequestSkillsState, core.RelayRequestInstallSkills,
				core.RelayRequestGithubTokenState, core.RelayRequestInstallGithubToken,
				core.RelayRequestRemoveGithubToken:
				// Credential, token, and skills bytes stay on this
				// connection. They never reach DurableRelay, its journal, or
				// its command ledger.
				operation := core.RelayRequestMethodName(envelope.Request)
				_, loginInstalled := request.(core.RelayRequestInstallCredentials)
				_, credentialState := request.(core.RelayRequestCredentialState)
				loginObserved := loginInstalled || credentialState
				// Take the read guard before checking the in-place move hold
				// and keep it through credentialResponse. Because
				// HoldInPlaceMoveDispatch sets the hold before draining via
				// credentials.drain, every pre-hold request finishes before
				// InPlaceMoveDispatchHeld replies, and every post-hold
				// request observes the hold and refuses.
				currentCredentials, releaseCredentials := credentials.read()
				observer := relay.Observe()
				moveHeld := observer.inPlaceMoveHoldActive()
				loginIdentity := observer.loginIdentity
				observer.Unlock()
				var response core.RelayResponseEnvelope
				if moveHeld {
					response = core.RelayResponseEnvelope{
						RequestID:       envelope.RequestID,
						ProtocolVersion: envelope.ProtocolVersion,
						Body: core.RelayResponseBodyError{Error: core.RelayProtocolError{
							Code:      core.RelayErrorCodeInvalidState,
							Message:   "cannot access credentials while an in-place move dispatch hold is active",
							Retryable: true,
						}},
					}
				} else {
					response = credentialResponse(ctx, envelope, currentCredentials, relayRoot, loginIdentity)
				}
				releaseCredentials()
				_, ok := response.Body.(core.RelayResponseBodyOk)
				if loginInstalled && ok {
					if err := wakeDispatch(relay, dispatchWake); err != nil {
						return err
					}
				} else if loginObserved {
					// A failed read can withdraw the previous observation.
					relay.Lock().Unlock()
				}
				if err := writeLoggedResponse(writer, &response, sessionID, operation); err != nil {
					return err
				}
				continue
			case core.RelayRequestProjectMemorySnapshot, core.RelayRequestInstallProjectMemorySnapshot,
				core.RelayRequestReplaceProjectMemoryTree, core.RelayRequestProjectMemoryTransfer:
				operation := core.RelayRequestMethodName(envelope.Request)
				memoryCtx, cancelMemory := context.WithCancel(ctx)
				responses := make(chan *core.RelayResponseEnvelope, 1)
				go func() {
					responses <- projectMemoryResponse(memoryCtx, envelope, &projectMemory, &transfer)
				}()
				// The relay protocol is strictly request-then-response per
				// connection.
				var response *core.RelayResponseEnvelope
				select {
				case response = <-responses:
					cancelMemory()
				case frame := <-frames:
					// Cancel queued work and reads, but keep ownership of an
					// admitted write until its filesystem mutation ends.
					cancelMemory()
					unixNotifyOne(projectMemory.ioDisconnected)
					if late := <-responses; late != nil {
						if failure, failed := late.Body.(core.RelayResponseBodyError); failed {
							return fmt.Errorf("project-memory %s failed after disconnect: %s", operation, failure.Error.Message)
						}
						slog.Debug("project-memory operation completed after disconnect", "session_id", sessionID, "operation", operation)
					}
					ended, err := frame.disconnected()
					if err != nil {
						return err
					}
					if !ended {
						return errors.New("protocol error: received a request before the project-memory response")
					}
					return nil
				}
				if response == nil {
					return nil
				}
				if err := writeLoggedResponse(writer, response, sessionID, operation); err != nil {
					return err
				}
				continue
			case core.RelayRequestGuardianRequests:
				body := core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadGuardianRequests{Requests: relay.guardian.Requests()}}
				if err := respond(body, core.RelayRequestMethodName(envelope.Request)); err != nil {
					return err
				}
				continue
			case core.RelayRequestCompleteGuardianRequest:
				relay.guardian.Complete(request.Result)
				body := core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadGuardianRequestCompleted{}}
				if err := respond(body, core.RelayRequestMethodName(envelope.Request)); err != nil {
					return err
				}
				continue
			case core.RelayRequestHistoryQuery, core.RelayRequestHistoryRequests, core.RelayRequestCompleteHistoryRequest:
				operation := core.RelayRequestMethodName(envelope.Request)
				var payload core.RelayResponsePayload
				var err error
				switch request := request.(type) {
				case core.RelayRequestHistoryRequests:
					payload = core.RelayResponsePayloadHistoryRequests{Requests: projectMemory.history.Requests()}
				case core.RelayRequestCompleteHistoryRequest:
					projectMemory.history.Complete(request.Result)
					payload = core.RelayResponsePayloadHistoryRequestCompleted{}
				case core.RelayRequestHistoryQuery:
					if projectMemory.config == nil || projectMemory.config.HistorySocket == nil {
						err = errors.New("history tools are unavailable in this worker; resume with a current controller")
						break
					}
					queryCtx, cancelQuery := context.WithCancel(ctx)
					type queried struct {
						result core.HistoryResult
						err    error
					}
					results := make(chan queried, 1)
					go func() {
						result, err := projectMemory.history.Query(queryCtx, request.Query, projectMemory.cwd)
						results <- queried{result, err}
					}()
					select {
					case outcome := <-results:
						cancelQuery()
						if outcome.err != nil {
							err = outcome.err
						} else {
							payload = core.RelayResponsePayloadHistoryResult{Result: outcome.result}
						}
					case frame := <-frames:
						// Dropping the query withdraws it, as Rust drops the
						// future.
						cancelQuery()
						<-results
						if frame.err != nil {
							return frame.err
						}
						return nil
					}
				}
				var body core.RelayResponseBody = core.RelayResponseBodyOk{Payload: payload}
				if err != nil {
					body = compactionError(core.RelayErrorCodeInternal, err.Error())
				}
				if err := respond(body, operation); err != nil {
					return err
				}
				continue
			case core.RelayRequestReviewer:
				// Capture operations never enter the conversation journal.
				operation := core.RelayRequestMethodName(envelope.Request)
				var response core.RelayResponseEnvelope
				if message, unavailable := preparationError(&prepSnapshot, "reviewer requests"); unavailable {
					response = unavailableRequest(envelope, message)
				} else if func() bool {
					guard := relay.Lock()
					defer guard.Unlock()
					return guard.OperationalState().Execution == core.RelayExecutionStateClosed
				}() {
					response = unavailableRequest(envelope, "session is closed; review capture is unavailable")
				} else {
					response = reviewCaptureResponse(ctx, envelope, requestReviewer)
				}
				if err := writeLoggedResponse(writer, &response, sessionID, operation); err != nil {
					return err
				}
				continue
			case core.RelayRequestSubagentRequests, core.RelayRequestPollSubagentQueue,
				core.RelayRequestCompleteSubagentRequest, core.RelayRequestSetSubagentAdmission:
				operation := core.RelayRequestMethodName(envelope.Request)
				var body core.RelayResponseBody
				if message, unavailable := preparationError(&prepSnapshot, "sub-agent requests"); unavailable && hasSubagentTools {
					body = compactionError(core.RelayErrorCodeInvalidState, message)
				} else {
					switch request := request.(type) {
					case core.RelayRequestPollSubagentQueue:
						page, err := feed.Poll(requestSubagents, request.Acknowledged)
						if err != nil {
							body = compactionError(core.RelayErrorCodeInternal, "poll sub-agent queue: "+err.Error())
						} else {
							body = core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadSubagentQueue{Page: page}}
						}
					case core.RelayRequestSubagentRequests:
						body = compactionError(core.RelayErrorCodeInvalidRequest,
							"this worker uses poll_subagent_queue; historical reply snapshots are no longer served")
					case core.RelayRequestCompleteSubagentRequest:
						if requestSubagents == nil {
							body = compactionError(core.RelayErrorCodeInvalidRequest, "this session has no Mjolnir sub-agent tools")
							break
						}
						delivered, err := requestSubagents.Complete(request.Result)
						switch {
						case err != nil:
							body = compactionError(core.RelayErrorCodeInternal, "persist sub-agent result: "+err.Error())
						case envelope.ProtocolVersion >= core.RelaySubagentDeliveryProtocol:
							body = core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadSubagentRequestCompletedWithDelivery{
								DeliveredToWaiter: delivered,
							}}
						default:
							body = core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadSubagentRequestCompleted{}}
						}
					case core.RelayRequestSetSubagentAdmission:
						if requestSubagents == nil {
							body = compactionError(core.RelayErrorCodeInvalidRequest, "this session has no Mjolnir sub-agent tools")
							break
						}
						if err := requestSubagents.SetMutatingAdmission(request.Open); err != nil {
							body = compactionError(core.RelayErrorCodeInternal, "change sub-agent admission: "+err.Error())
						} else {
							body = core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadSubagentAdmissionChanged{Open: request.Open}}
						}
					}
				}
				if err := respond(body, operation); err != nil {
					return err
				}
				continue
			case core.RelayRequestRespondElicitation:
				// Form answers can contain private user input. They travel
				// directly to the ACP runtime and never touch relay state.
				var response core.RelayResponseEnvelope
				if message, unavailable := preparationError(&prepSnapshot, "elicitation responses"); unavailable {
					response = unavailableRequest(envelope, message)
				} else {
					response = elicitationResponse(ctx, envelope, requestCommands)
				}
				if err := writeLoggedResponse(writer, &response, sessionID, "respond_elicitation"); err != nil {
					return err
				}
				continue
			case core.RelayRequestStopBackgroundTask:
				var response core.RelayResponseEnvelope
				if message, unavailable := preparationError(&prepSnapshot, "background-task controls"); unavailable {
					response = unavailableRequest(envelope, message)
				} else {
					response = backgroundTaskStopResponse(ctx, envelope, requestCommands, relay)
				}
				if err := writeLoggedResponse(writer, &response, sessionID, "stop_background_task"); err != nil {
					return err
				}
				continue
			case core.RelayRequestRestartHarnessInPlace:
				var response core.RelayResponseEnvelope
				if message, unavailable := preparationError(&prepSnapshot, "in-place harness restart"); unavailable {
					response = unavailableRequest(envelope, message)
				} else {
					response = restartHarnessInPlaceResponse(ctx, envelope, requestCommands, relay, credentials, relayRoot)
				}
				if err := writeLoggedResponse(writer, &response, sessionID, "restart_harness_in_place"); err != nil {
					return err
				}
				continue
			}
			wakesDispatch, hookActivityChanged := false, false
			switch envelope.Request.(type) {
			case core.RelayRequestSubmit, core.RelayRequestReserveIdle, core.RelayRequestReleaseInPlaceMoveDispatch:
				wakesDispatch = true
			case core.RelayRequestDrainMailbox, core.RelayRequestAckMailbox, core.RelayRequestAntigravityStop:
				hookActivityChanged = true
			}
			change := checkpointChange(envelope.Request)
			operation := core.RelayRequestMethodName(envelope.Request)
			// An attach's state carries the preparation before the relay
			// digests it, so a changed preparation is never mistaken for a
			// state the controller already holds.
			var attachPreparation core.HarnessPreparation
			if preparation != nil {
				attachPreparation = preparation.Borrow().state
			}
			var response core.RelayResponseEnvelope
			var err error
			if _, stop := envelope.Request.(core.RelayRequestAntigravityStop); stop {
				response = handleAntigravityStop(ctx, relay, envelope, antigravityHome)
			} else {
				response, err = handleRequest(ctx, relay, envelope, attachPreparation)
			}
			if err != nil {
				slog.Error("relay request handling failed: "+err.Error(), "session_id", sessionID, "operation", operation)
				return err
			}
			if ok, isOk := response.Body.(core.RelayResponseBodyOk); isOk {
				if _, held := ok.Payload.(core.RelayResponsePayloadInPlaceMoveDispatchHeld); held {
					// Drain every credential RPC that took a read guard
					// before the hold was recorded, before telling the
					// controller it may harvest or overwrite staged profile
					// files.
					credentials.drain()
				}
			}
			// One relay lock decides who owns the relay-local lifecycle work
			// this request may have queued. Until preparation publishes
			// Started, which startBridge does under this same lock, this
			// handler completes checkpoint and Close commands itself, and a
			// Close that sealed the relay cancels preparation so no bridge
			// starts. Once Started is visible here, the coordinator owns them.
			currentPreparation, err := func() (preparationSnapshot, error) {
				guard := relay.Lock()
				defer guard.Unlock()
				var current preparationSnapshot
				if preparation != nil {
					current = preparation.Borrow()
				}
				if preparation != nil && current.isPending() {
					if err := guard.DispatchPreparationLifecycle(); err != nil {
						return current, err
					}
					if guard.SealedByClose() && preparationCancel != nil {
						preparationCancel()
					}
				}
				return current, nil
			}()
			if err != nil {
				return err
			}
			if workerRootWasRemoved(response.Body, relayRoot) {
				// One report is enough; the daemon is already winding down.
				reportFatal(fatal, fmt.Errorf("worker root %s was removed while the relay was serving", relayRoot),
					sessionID, "worker root removed")
			}
			accepted := false
			if ok, isOk := response.Body.(core.RelayResponseBodyOk); isOk {
				switch payload := ok.Payload.(type) {
				case core.RelayResponsePayloadAccepted:
					accepted = true
				case core.RelayResponsePayloadIdleReservation:
					accepted = payload.Ordinal != nil
				case core.RelayResponsePayloadInPlaceMoveDispatchReleased:
					accepted = true
				}
			}
			if accepted {
				switch change := change.(type) {
				case CheckpointChangeBegin:
					checkpointBarriers[change.Value] = struct{}{}
				case CheckpointChangeEnded:
					delete(checkpointBarriers, change.Value)
				}
			}
			if (wakesDispatch && accepted) || hookActivityChanged {
				if err := wakeDispatch(relay, dispatchWake); err != nil {
					return err
				}
			}
			overlayPreparationState(&response, &currentPreparation)
			// Dispatch is driven from durable state, not from delivery of the
			// acknowledgement. A controller can disappear after its request
			// reaches the relay but before the response write.
			if err := writeLoggedResponse(writer, &response, sessionID, operation); err != nil {
				return err
			}
		}
	}()

	cleanupResult := releaseCheckpointBarriers(relay, dispatchWake, checkpointBarriers)
	var outcome error
	switch {
	case servingResult == nil:
		outcome = cleanupResult
	case cleanupResult == nil:
		outcome = servingResult
	default:
		outcome = fmt.Errorf("also failed to release checkpoint barriers: %s: %w", cleanupResult, servingResult)
	}
	if outcome != nil {
		if info, err := os.Stat(relayRoot); err != nil || !info.IsDir() {
			reportFatal(fatal, fmt.Errorf("worker root %s was removed while the relay was serving", relayRoot),
				sessionID, "worker root removed")
		}
	}
	return outcome
}

// awaitLiveBaseline holds a BeginLiveBaseline until the worker reaches a
// quiescent cut, telling the controller what it waits for. ended is true
// when the connection went away first (Rust: the biased fill_buf branch).
// The reservation is withdrawn on every exit.
func awaitLiveBaseline(relay *RelayCell, frames <-chan connectionFrame, envelope core.RelayRequestEnvelope, writer io.Writer, sessionID string) (LiveBaselineResult, bool, error) {
	releases := relay.Releases()
	guard := relay.Lock()
	receiver := guard.ReserveLiveBaseline()
	guard.Unlock()
	defer receiver.Close()
	// A harness that cannot start ends this worker, which ends the wait with
	// the connection.
	var previous []string
	seen := false
	for {
		releases.BorrowAndUpdate()
		observer := relay.Observe()
		blockers := observer.adoptionBlockers()
		observer.Unlock()
		if !seen || !slices.Equal(previous, blockers) {
			if len(blockers) > 0 {
				response := core.RelayResponseEnvelope{
					RequestID:       envelope.RequestID,
					ProtocolVersion: envelope.ProtocolVersion,
					Body: core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadLiveBaselineWaiting{
						Blockers: slices.Clone(blockers),
					}},
				}
				if err := writeLoggedResponse(writer, &response, sessionID, "live_baseline_waiting"); err != nil {
					return LiveBaselineResult{Err: err}, false, nil
				}
			}
			previous, seen = blockers, true
		}
		// The disconnect is checked first, then the cut, then a change. A
		// read error refuses the cut, as Rust's failed fill_buf does; the
		// next read reports it again and ends the connection.
		select {
		case frame := <-frames:
			if _, err := frame.disconnected(); err != nil {
				return LiveBaselineResult{Err: err}, false, nil
			}
			return LiveBaselineResult{}, true, nil
		default:
		}
		select {
		case cut, ok := <-receiver.C():
			if !ok {
				return LiveBaselineResult{Err: errors.New("worker dropped its adoption cut: channel closed")}, false, nil
			}
			return cut, false, nil
		default:
		}
		waitCtx, stopWait := context.WithCancel(context.Background())
		released := make(chan error, 1)
		go func() { released <- releases.Changed(waitCtx) }()
		var outcome *LiveBaselineResult
		var ended bool
		var frameErr error
		// The watcher goroutine sends exactly once. When its branch wins, that
		// send is already consumed; otherwise cancel it and take the send, so
		// the next iteration owns releases alone.
		var changedErr error
		changedDone := false
		select {
		case frame := <-frames:
			_, frameErr = frame.disconnected()
			ended = frameErr == nil
		case cut, ok := <-receiver.C():
			if !ok {
				cut = LiveBaselineResult{Err: errors.New("worker dropped its adoption cut: channel closed")}
			}
			outcome = &cut
		case changedErr = <-released:
			changedDone = true
		}
		stopWait()
		if !changedDone {
			<-released
		}
		switch {
		case frameErr != nil:
			return LiveBaselineResult{Err: frameErr}, false, nil
		case ended:
			return LiveBaselineResult{}, true, nil
		case outcome != nil:
			return *outcome, false, nil
		case changedErr != nil:
			return LiveBaselineResult{Err: fmt.Errorf("worker adoption observation stopped: %w", changedErr)}, false, nil
		}
	}
}

// dispatchWakeChannel is the relay's dispatch wake channel (Rust:
// mpsc::channel::<()>(1)). A wake only says durable work may now be
// available, so a full channel already carries it. The coordinator owns the
// receiving side and closes it when it stops, which senders observe.
type dispatchWakeChannel struct {
	wakes     chan struct{}
	closed    chan struct{}
	closeOnce sync.Once
}

// errDispatchWakeClosed is a wake sent after the receiver closed (Rust:
// TrySendError::Closed).
var errDispatchWakeClosed = errors.New("channel closed")

func newDispatchWakeChannel() *dispatchWakeChannel {
	return &dispatchWakeChannel{wakes: make(chan struct{}, 1), closed: make(chan struct{})}
}

// trySend wakes the receiver without waiting. A full channel is success: the
// pending wake says the same thing.
func (w *dispatchWakeChannel) trySend() error {
	select {
	case <-w.closed:
		return errDispatchWakeClosed
	default:
	}
	select {
	case w.wakes <- struct{}{}:
	default:
	}
	return nil
}

// receive is the receiving side.
func (w *dispatchWakeChannel) receive() <-chan struct{} { return w.wakes }

// closeReceiver is Rust's drop of the receiver: later sends fail.
func (w *dispatchWakeChannel) closeReceiver() {
	w.closeOnce.Do(func() { close(w.closed) })
}

// commandChannel is the ACP command channel (Rust: mpsc::channel of
// CommandRequest with capacity acpCommandChannelCapacity). Every sender is in
// this runtime and the ACP runtime receives from receiver(). Dispatch
// reserves capacity before it claims durable work, so out-of-band senders
// (compaction, elicitation answers) compete for the same slots instead of
// stealing one a claim already counted on. A command counts against the
// capacity until the receiver takes it.
//
// out is a Go channel buffered to the capacity, so the receiver frees a
// slot in the same step that takes a command. Every sender decides under mu,
// keeping len(out)+reserved within the capacity, so a reserved slot is
// always free when its permit sends.
type commandChannel struct {
	mu       sync.Mutex
	capacity int
	// reserved counts outstanding permits.
	reserved int
	// changed is closed and replaced whenever a permit is released or the
	// receiver closes.
	changed chan struct{}
	// out holds the accepted commands not yet taken by the receiver.
	out chan CommandRequest
	// closed is closed when the receiver is gone.
	closed    chan struct{}
	closeOnce sync.Once
}

// commandPermit is one reserved slot of a commandChannel (Rust:
// mpsc::Permit). Exactly one of send or release must be called.
type commandPermit struct {
	channel *commandChannel
}

var (
	// errCommandChannelFull is Rust's TrySendError::Full.
	errCommandChannelFull = errors.New("no available capacity")
	// errCommandChannelClosed is Rust's TrySendError::Closed and SendError.
	errCommandChannelClosed = errors.New("channel closed")
)

// commandChannelRecheck is how often a sender waiting for room looks again.
// The receiver takes commands straight from the Go channel and tells no one,
// so a full channel is looked at again on this interval as well as on every
// permit release.
const commandChannelRecheck = 10 * time.Millisecond

func newCommandChannel(capacity int) *commandChannel {
	if capacity <= 0 {
		panic("mpsc bounded channel requires buffer > 0")
	}
	return &commandChannel{
		capacity: capacity,
		changed:  make(chan struct{}),
		out:      make(chan CommandRequest, capacity),
		closed:   make(chan struct{}),
	}
}

// receiver is the receiving side, which the ACP runtime drains.
func (c *commandChannel) receiver() <-chan CommandRequest { return c.out }

// isClosedLocked is whether the receiver is gone.
func (c *commandChannel) isClosedLocked() bool {
	select {
	case <-c.closed:
		return true
	default:
		return false
	}
}

// notifyLocked wakes everyone waiting for a change.
func (c *commandChannel) notifyLocked() {
	close(c.changed)
	c.changed = make(chan struct{})
}

// availableLocked is the capacity neither queued nor reserved.
func (c *commandChannel) availableLocked() int {
	return c.capacity - len(c.out) - c.reserved
}

// closeReceiver is Rust's drop of the receiver: queued commands are dropped
// and later sends fail.
func (c *commandChannel) closeReceiver() {
	c.closeOnce.Do(func() {
		c.mu.Lock()
		defer c.mu.Unlock()
		close(c.closed)
		for {
			select {
			case <-c.out:
				continue
			default:
			}
			break
		}
		c.notifyLocked()
	})
}

// availableCapacity is how many commands can be sent without waiting (Rust:
// Sender::capacity).
func (c *commandChannel) availableCapacity() int {
	c.mu.Lock()
	defer c.mu.Unlock()
	return max(c.availableLocked(), 0)
}

// tryReserve reserves one slot; false when the channel is full or closed.
func (c *commandChannel) tryReserve() (commandPermit, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.isClosedLocked() || c.availableLocked() <= 0 {
		return commandPermit{}, false
	}
	c.reserved++
	return commandPermit{channel: c}, true
}

// trySend sends without waiting: errCommandChannelFull or
// errCommandChannelClosed when it cannot.
func (c *commandChannel) trySend(request CommandRequest) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.isClosedLocked() {
		return errCommandChannelClosed
	}
	if c.availableLocked() <= 0 {
		return errCommandChannelFull
	}
	c.out <- request
	return nil
}

// send waits for a slot; errCommandChannelClosed once the receiver is gone.
func (c *commandChannel) send(ctx context.Context, request CommandRequest) error {
	for {
		c.mu.Lock()
		if c.isClosedLocked() {
			c.mu.Unlock()
			return errCommandChannelClosed
		}
		if c.availableLocked() > 0 {
			c.out <- request
			c.mu.Unlock()
			return nil
		}
		changed := c.changed
		c.mu.Unlock()
		recheck := time.NewTimer(commandChannelRecheck)
		select {
		case <-changed:
		case <-c.closed:
		case <-recheck.C:
		case <-ctx.Done():
			recheck.Stop()
			return ctx.Err()
		}
		recheck.Stop()
	}
}

// send queues request in the reserved slot.
func (p commandPermit) send(request CommandRequest) {
	c := p.channel
	c.mu.Lock()
	defer c.mu.Unlock()
	c.reserved--
	if c.isClosedLocked() {
		// Rust's Permit::send to a dropped receiver drops the value.
		c.notifyLocked()
		return
	}
	// The reservation kept this slot free, so the send cannot wait.
	c.out <- request
}

// release gives the reserved slot back unused.
func (p commandPermit) release() {
	c := p.channel
	c.mu.Lock()
	defer c.mu.Unlock()
	c.reserved--
	c.notifyLocked()
}

// inPlaceHarnessRestartGuard holds the relay's in-place restart reservation
// for one operation. Close releases it unless complete marked the restart
// durably (Rust's Drop).
type inPlaceHarnessRestartGuard struct {
	relay           *RelayCell
	operationID     string
	expectedStoreID string
	completed       bool
}

func (g *inPlaceHarnessRestartGuard) complete() error {
	guard := g.relay.Lock()
	err := guard.markInPlaceHarnessRestarted(g.operationID, g.expectedStoreID)
	guard.Unlock()
	if err == nil {
		g.completed = true
	}
	return err
}

func (g *inPlaceHarnessRestartGuard) Close() {
	if !g.completed {
		guard := g.relay.Lock()
		guard.cancelInPlaceHarnessRestart(g.operationID)
		guard.Unlock()
	}
}

// sharedCredentials is the worker-wide credential endpoint shared across all
// active and future proxy connections. Credential requests hold a read guard
// across both the in-place move hold check and the filesystem operation,
// while HoldInPlaceMoveDispatch drains in-flight requests and
// RestartHarnessInPlace swaps the endpoint under the write guard.
type sharedCredentials struct {
	mu    sync.RWMutex
	value credentialEndpointResult
}

func newSharedCredentials(initial credentialEndpointResult) *sharedCredentials {
	return &sharedCredentials{value: initial}
}

// read takes the read guard; call release when the request is done with the
// endpoint.
func (c *sharedCredentials) read() (credentialEndpointResult, func()) {
	c.mu.RLock()
	return c.value, c.mu.RUnlock
}

// drain waits for every request holding a read guard.
func (c *sharedCredentials) drain() {
	c.mu.Lock()
	c.mu.Unlock()
}

func (c *sharedCredentials) set(updated credentialEndpointResult) {
	c.mu.Lock()
	c.value = updated
	c.mu.Unlock()
}

func (c *sharedCredentials) snapshot() credentialEndpointResult {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return c.value
}

func harnessTurnPolicyFor(harness core.HarnessKind) HarnessTurnPolicy {
	switch harness {
	case core.HarnessKindClaude:
		return HarnessTurnPolicyClaudeAdapter
	case core.HarnessKindCodex:
		return HarnessTurnPolicyCodexAdapter
	case core.HarnessKindAntigravity:
		return HarnessTurnPolicyAntigravity
	}
	return HarnessTurnPolicyDisabled
}

// inPlaceRestartStep runs a blocking preparation step for an in-place
// harness restart within localPreparationStepTimeout; timedOut reports a
// step that did not finish in time.
func inPlaceRestartStep[T any](ctx context.Context, work func() (T, error)) (value T, err error, timedOut bool) {
	stepCtx, stop := context.WithTimeout(ctx, localPreparationStepTimeout)
	defer stop()
	type outcome struct {
		value T
		err   error
	}
	result, joinErr := core.SpawnBlocking(func() outcome {
		value, err := work()
		return outcome{value: value, err: err}
	}).Join(stepCtx)
	if joinErr != nil {
		if stepCtx.Err() != nil && ctx.Err() == nil {
			return value, nil, true
		}
		return value, joinErr, false
	}
	return result.value, result.err, false
}

// restartHarnessInPlaceResponse answers RestartHarnessInPlace: it restarts
// only the harness bridge under the updated launch configuration while the
// in-place move hold of the operation holds dispatch.
func restartHarnessInPlaceResponse(ctx context.Context, envelope core.RelayRequestEnvelope, commands *commandChannel, relay *RelayCell, credentials *sharedCredentials, relayRoot string) core.RelayResponseEnvelope {
	if body := core.RelayProtocolRejection(&envelope); body != nil {
		return core.RelayResponseEnvelope{RequestID: envelope.RequestID, ProtocolVersion: envelope.ProtocolVersion, Body: body}
	}
	protocolVersion, requestID := envelope.ProtocolVersion, envelope.RequestID
	request, ok := envelope.Request.(core.RelayRequestRestartHarnessInPlace)
	if !ok {
		panic("unreachable: restartHarnessInPlaceResponse only serves RestartHarnessInPlace")
	}
	answer := func(body core.RelayResponseBody) core.RelayResponseEnvelope {
		return core.RelayResponseEnvelope{RequestID: requestID, ProtocolVersion: protocolVersion, Body: body}
	}
	guard := relay.Lock()
	decision, refusal := guard.admitInPlaceHarnessRestart(request.OperationID, request.ExpectedStoreID)
	guard.Unlock()
	if refusal != nil {
		return answer(core.RelayResponseBodyError{Error: *refusal})
	}
	if decision == InPlaceHarnessRestartAlreadyRestarted {
		return answer(core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadHarnessRestartedInPlace{}})
	}
	restartGuard := &inPlaceHarnessRestartGuard{
		relay:           relay,
		operationID:     request.OperationID,
		expectedStoreID: request.ExpectedStoreID,
	}
	defer restartGuard.Close()
	if commands == nil {
		return answer(compactionError(core.RelayErrorCodeInvalidState,
			"session is closed; no ACP runtime can restart the harness in place"))
	}
	launchConfig := request.LaunchConfig
	persistedLaunchConfig := request.LaunchConfig
	// Apply the same target_environment merge, policy enforcement,
	// staged-home env, and $MJ_KEEP_DIR setup as initial startup
	// (RunDaemonOwned / PrepareManagedHarness) so managed harness discovery
	// sees the target's XDG_CACHE_HOME/PATH and the restarted bridge keeps
	// $MJ_KEEP_DIR.
	mergedEnvironment := maps.Clone(launchConfig.TargetEnvironment)
	if mergedEnvironment == nil {
		mergedEnvironment = map[string]string{}
	}
	strip := func(environment map[string]string) {
		delete(environment, core.SessionNitroMbxEnv)
		delete(environment, core.SessionGitConfigIncludePath)
		delete(environment, core.FastinoKeyEnvironment)
	}
	strip(mergedEnvironment)
	maps.Copy(mergedEnvironment, launchConfig.Environment)
	strip(mergedEnvironment)
	launchConfig.Environment = mergedEnvironment
	launchConfig.TargetEnvironment = maps.Clone(launchConfig.TargetEnvironment)
	if startupDirectory, err := os.Getwd(); err == nil {
		resolveRelativeHarnessHome(&launchConfig, startupDirectory)
	}
	if launchConfig.NoGithubCredentials {
		core.RemoveGithubAuthenticationEnvironment(launchConfig.Environment)
		core.RemoveGithubAuthenticationEnvironment(launchConfig.TargetEnvironment)
	}
	if err := enforceExecutionPolicy(&launchConfig); err != nil {
		return answer(compactionError(core.RelayErrorCodeInvalidState,
			fmt.Sprintf("enforce execution policy for in-place harness restart: %v", err)))
	}
	if launchConfig.Harness == core.HarnessKindOpenCode {
		if endpoint, err := credentialEndpoint(&launchConfig); err == nil {
			launchConfig.Harness.ConfigureStagedHomeEnvironment(endpoint.Home, launchConfig.Environment)
		}
	}
	keepCwd, keepSessionID := launchConfig.Cwd, launchConfig.SessionID
	keepEnvironment := maps.Clone(launchConfig.Environment)
	_, keepErr, keepTimedOut := inPlaceRestartStep(ctx, func() (string, error) {
		return core.InstallSessionKeepDir(keepCwd, keepSessionID, keepEnvironment)
	})
	switch {
	case keepTimedOut:
		slog.Warn("session keep directory preparation timed out during in-place harness restart",
			"session_id", launchConfig.SessionID, "cwd", launchConfig.Cwd, "timeout", localPreparationStepTimeout)
	case keepErr != nil:
		slog.Warn("could not prepare the session keep directory during in-place harness restart",
			"session_id", launchConfig.SessionID, "cwd", launchConfig.Cwd, "error", keepErr)
	default:
		launchConfig.Environment = keepEnvironment
	}
	supervisorPath := filepath.Join(relayRoot, "acp-supervisor.json")
	prepared, err := PrepareHarnessLaunch(ctx, launchConfig.Harness, launchConfig.HarnessRuntime, launchConfig.ExecutionPolicy, primarySupervisorSpec(&launchConfig))
	if err != nil {
		return answer(compactionError(core.RelayErrorCodeInternal, fmt.Sprintf("prepare updated harness launch: %v", err)))
	}
	if err := freezeNodeEntrypoints(ctx, relayRoot, prepared); err != nil {
		prepared.Release()
		return answer(compactionError(core.RelayErrorCodeInternal, fmt.Sprintf("freeze updated harness entrypoints: %v", err)))
	}
	additionalDirectories := acpAdditionalDirectories(&launchConfig)
	extendWritableDirectories(launchConfig.Harness, prepared.environment, &additionalDirectories)
	for _, name := range []string{core.SessionGitConfigIncludePath, core.SessionManagedSubagentEnv,
		core.SessionRawLaunchRecordEnv, core.SessionMessageMCPEnv, core.SessionProjectToolsRoots} {
		delete(prepared.environment, name)
	}
	updatedEndpoint, updatedEndpointErr := credentialEndpoint(&launchConfig)
	updatedCredentials := credentialEndpointResult{endpoint: updatedEndpoint, err: updatedEndpointErr}
	updatedHarness := launchConfig.Harness
	var updatedClaudeLoginPath *string
	if updatedHarness == core.HarnessKindClaude {
		if path, ok := core.ClaudeOauthLoginPath(launchConfig.Environment); ok {
			updatedClaudeLoginPath = &path
		}
	}
	boundaryMailboxHookAvailable := false
	var updatedMailboxDeliveryMode *core.MailboxDeliveryMode
	if updatedEndpointErr == nil {
		hookSessionID, hookHome := launchConfig.SessionID, updatedEndpoint.Home
		verifyHook := func(name string, configure func(string, string, string) (bool, error)) bool {
			available, err, timedOut := inPlaceRestartStep(ctx, func() (bool, error) {
				return configure(relayRoot, hookSessionID, hookHome)
			})
			switch {
			case timedOut:
				slog.Warn(name+" mailbox hook verification timed out during in-place harness restart",
					"session_id", launchConfig.SessionID)
				return false
			case err != nil:
				slog.Warn("could not verify "+name+" mailbox hook during in-place harness restart",
					"session_id", launchConfig.SessionID, "error", err)
				return false
			}
			return available
		}
		switch updatedHarness {
		case core.HarnessKindClaude:
			boundaryMailboxHookAvailable = verifyHook("Claude", configureClaudeMailboxHook)
		case core.HarnessKindAntigravity:
			boundaryMailboxHookAvailable = verifyHook("Antigravity", configureAntigravityMailboxHook)
			mode := core.MailboxDeliveryModeTurnBoundary
			if boundaryMailboxHookAvailable {
				mode = core.MailboxDeliveryModeToolBoundary
			}
			updatedMailboxDeliveryMode = &mode
		case core.HarnessKindCodex:
			provider, err, timedOut := inPlaceRestartStep(ctx, func() (*core.CodexProvider, error) {
				return core.ReadCodexProvider(hookHome, nil)
			})
			if err == nil && !timedOut {
				mode := core.MailboxDeliveryModeForCodexProvider(provider)
				updatedMailboxDeliveryMode = &mode
			}
		}
	}
	resolved := make(chan error, 1)
	reload := CommandRequestReloadLaunchConfig{
		LaunchConfig:          &launchConfig,
		Environment:           prepared.environment,
		AdditionalDirectories: additionalDirectories,
		SupervisorSpec:        &ReloadSupervisorSpec{Path: supervisorPath, Spec: prepared.Spec},
		Leases:                prepared,
		Resolved:              resolved,
	}
	if err := commands.send(ctx, reload); err != nil {
		prepared.Release()
		return answer(compactionError(core.RelayErrorCodeInvalidState, "ACP runtime is unavailable for in-place harness restart"))
	}
	var resolution error
	var resolutionOK bool
	select {
	case resolution, resolutionOK = <-resolved:
	case <-ctx.Done():
	}
	if !resolutionOK {
		return answer(compactionError(core.RelayErrorCodeInvalidState,
			"ACP runtime stopped before completing in-place harness restart"))
	}
	if resolution != nil {
		return answer(compactionError(core.RelayErrorCodeInvalidState, resolution.Error()))
	}
	// The replacement bridge is now live under the updated launch config.
	// Refresh the shared credential endpoint and harness policies
	// immediately so subsequent credential RPCs and relay state match the
	// live harness.
	credentials.set(updatedCredentials)
	guard = relay.Lock()
	guard.SetTurnVerdictHarness(updatedHarness)
	guard.authenticationLoginPath = updatedClaudeLoginPath
	guard.SetSteeringDeliveryMode(updatedHarness, boundaryMailboxHookAvailable)
	guard.updateMailboxDeliveryMode(updatedMailboxDeliveryMode)
	guard.SetHarnessTurnPolicy(harnessTurnPolicyFor(updatedHarness))
	loginIdentity := guard.loginIdentity
	guard.Unlock()
	if updatedEndpointErr == nil {
		if err := observeInitialLogin(ctx, &updatedEndpoint, loginIdentity); err != nil {
			slog.Warn("could not read staged login after in-place harness restart", "error", err)
		}
	} else {
		loginIdentity.observeFile(nil)
	}
	if err := persistedLaunchConfig.Write(filepath.Join(relayRoot, "launch.json")); err != nil {
		return answer(compactionError(core.RelayErrorCodeInternal, fmt.Sprintf("persist updated launch.json: %v", err)))
	}
	if err := restartGuard.complete(); err != nil {
		return answer(compactionError(core.RelayErrorCodeInternal, fmt.Sprintf("persist in-place harness restart state: %v", err)))
	}
	return answer(core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadHarnessRestartedInPlace{}})
}
