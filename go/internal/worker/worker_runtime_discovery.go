// Port of mj-worker/src/worker_runtime/discovery.rs.

// A short-lived harness that discovers configuration without a prompt.

//go:build unix

package worker

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"os"
	"path/filepath"
	"time"

	"github.com/BrokkAi/acp-go/schema"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// DiscoverProfileConfig starts the profile's harness without a prompt and
// reports the models and efforts it offers.
func DiscoverProfileConfig(ctx context.Context, spec core.ProfileProbeSpec) (core.ProfileConfig, error) {
	policy := core.ExecutionPolicyConfiguredApprovals
	// Discovery probes a fresh target before any session launch exists.
	// Resolve the account's login environment first so nvm/Homebrew Node
	// installations and the cache root (`HOME` or `XDG_CACHE_HOME`) are
	// visible to the system-or-managed Node decision.
	environment, err := harnessinstall.WithOverrides(ctx, spec.Environment)
	if err != nil {
		return core.ProfileConfig{}, err
	}
	var managedNode *harnessinstall.ManagedNodeRuntime
	if spec.HarnessRuntime.MayInstall() {
		needed, err := needsNodeRuntime(ctx, spec.Harness, environment)
		if err != nil {
			return core.ProfileConfig{}, err
		}
		if needed {
			managedNode, err = harnessinstall.EnsureNodeRuntime(ctx, environment)
			if err != nil {
				return core.ProfileConfig{}, fmt.Errorf("prepare Node.js for %s discovery: %w", spec.Harness.DisplayName(), err)
			}
		}
	}
	// The installation leases last until the probe's harness has stopped.
	if managedNode != nil {
		defer managedNode.Release()
	}
	// The probe runs from a staged copy of the profile, like a session, and
	// starts the harness a local session would.
	spec.Harness.ConfigureHomeEnvironment(spec.ProfileHome, environment)
	builtInCommand, builtInArgs := core.FallbackLaunch(spec.Harness, policy)
	located, err := harnessinstall.Locate(ctx, spec.HarnessRuntime, spec.Harness, policy, builtInCommand, builtInArgs, environment)
	if err != nil {
		return core.ProfileConfig{}, err
	}
	var leasePath *string
	if located.Managed != nil {
		lease := located.Managed.LeasePath
		leasePath = &lease
		defer located.Managed.Release()
	}
	maps.Copy(environment, located.Environment)
	sessionEnvironment := maps.Clone(environment)
	excludeFromHarnessEnvironment(spec.Harness, spec.ExcludedEnvironment, sessionEnvironment, environment)
	for _, name := range spec.ExcludedEnvironment {
		delete(sessionEnvironment, name)
	}
	supervisor := filepath.Join(spec.Cwd, "acp-supervisor.json")
	supervisorSpec := AcpSupervisorSpec{
		Command:             located.Command,
		Args:                located.Args,
		Environment:         environment,
		ExcludedEnvironment: spec.ExcludedEnvironment,
		Cwd:                 spec.Cwd,
		HarnessLease:        leasePath,
	}
	if managedNode != nil {
		lease := managedNode.LeasePath()
		supervisorSpec.NodeLease = &lease
	}
	if err := supervisorSpec.writeSpec(supervisor); err != nil {
		return core.ProfileConfig{}, err
	}
	worker, err := os.Executable()
	if err != nil {
		return core.ProfileConfig{}, err
	}
	launch := discoveryLaunchSpec(spec.Harness, policy, worker, []string{
		"--login-environment-ready",
		"worker",
		"acp-supervisor",
		"--spec",
		supervisor,
	}, sessionEnvironment, spec.Cwd)
	return discoveryProbe(ctx, launch, spec.Model)
}

// discoveryLaunchSpec is the launch of a throwaway session that never sends
// a prompt: a native session with fresh shared state and nothing to resume.
// Discovery opens a session that never accepts a selector, so there is no
// bridge spec to pin into.
func discoveryLaunchSpec(harness core.HarnessKind, policy core.ExecutionPolicy, command string, args []string, environment map[string]string, cwd string) LaunchSpec {
	return LaunchSpec{
		LoginIdentity:         &ClaudeLoginIdentity{},
		OperatorAway:          &OperatorAway{},
		GoalRecovery:          NewShared(core.GoalRecoveryContext{}),
		Command:               command,
		Args:                  args,
		Environment:           environment,
		Cwd:                   cwd,
		AdditionalDirectories: []string{},
		ExtraMCPServers:       []ReviewerMcpServer{},
		SubagentPolicy:        core.SubagentPolicy{Mode: core.SubagentPolicyNative},
		NativeSessionHistory:  NativeSessionHistoryLoadBeforeReplace,
		AcceptedConfig:        NewShared(core.AcceptedSessionConfig{}),
		Harness:               harness,
		ExecutionPolicy:       policy,
		ACPActivity:           core.NewAcpActivityClock(),
		StepClock:             &core.StepClock{},
		ToolsInFlight:         &core.ToolsInFlight{},
		TurnContext:           &transcript.TurnContext{},
	}
}

func discoveryProbe(ctx context.Context, launch LaunchSpec, model *string) (core.ProfileConfig, error) {
	return probeWithCloseTimeout(ctx, launch, model, 15*time.Second)
}

func probeWithCloseTimeout(ctx context.Context, launch LaunchSpec, model *string, closeTimeout time.Duration) (core.ProfileConfig, error) {
	harness := launch.Harness
	commands := make(chan CommandRequest, 8)
	updates := make(chan core.RuntimeEvent, 128)
	shutdownContext, shutdown := context.WithCancel(ctx)
	defer shutdown()
	task := core.Spawn(func() error {
		return RunWithShutdown(shutdownContext, launch, commands, updates)
	})
	discovery, discoveryErr := func() (core.ProfileConfig, error) {
		timeout := time.NewTimer(240 * time.Second)
		defer timeout.Stop()
		warning := ""
		var initialModels *[]core.SessionConfigChoice
		for {
			var event core.RuntimeEvent
			select {
			case event = <-updates:
			case <-task.Done():
				// The runtime has stopped: its last events are still queued.
				select {
				case event = <-updates:
				default:
					return core.ProfileConfig{}, errors.New("discovery harness stopped before reporting choices")
				}
			case <-timeout.C:
				return core.ProfileConfig{}, errors.New("profile discovery timed out")
			case <-ctx.Done():
				return core.ProfileConfig{}, ctx.Err()
			}
			switch event := event.(type) {
			case core.RuntimeEventSessionConfigured:
				if initialModels != nil {
					continue
				}
				models := core.SessionConfigChoices(event.ConfigOptions, "model")
				offered := false
				if model != nil {
					for _, choice := range models {
						if choice.Value == *model {
							offered = true
							break
						}
					}
				}
				if !offered {
					return discovered(harness, event.ConfigOptions, models), nil
				}
				initialModels = &models
				select {
				case commands <- CommandRequestSetConfig{RequestID: "discover-model", Key: "model", Value: *model}:
				case <-task.Done():
					return core.ProfileConfig{}, errors.New("channel closed")
				case <-timeout.C:
					return core.ProfileConfig{}, errors.New("profile discovery timed out")
				}
			case core.RuntimeEventConfigApplied:
				if event.RequestID != "discover-model" {
					continue
				}
				var models []core.SessionConfigChoice
				if initialModels != nil {
					models = *initialModels
					initialModels = nil
				}
				return discovered(harness, event.ConfigOptions, models), nil
			case core.RuntimeEventCommandRejected:
				return core.ProfileConfig{}, fmt.Errorf("discover model configuration: %s", event.Message)
			case core.RuntimeEventCommandInterrupted:
				return core.ProfileConfig{}, fmt.Errorf("discover model configuration: %s", event.Message)
			case core.RuntimeEventWarning:
				warning = event.Message
			case core.RuntimeEventSessionFault:
				warning = event.Message
			case core.RuntimeEventStopped:
				return core.ProfileConfig{}, fmt.Errorf("discovery harness stopped: %s", warning)
			}
		}
	}()
	// Keep draining while the runtime shuts down; it can emit more than one
	// channel's worth of final events. The supervisor owns process cleanup.
	cleanup := func(deadline <-chan time.Time, closing bool) (bool, error) {
		sent := !closing
		for {
			var send chan<- CommandRequest
			if !sent {
				send = commands
			}
			select {
			case send <- CommandRequestClose{RequestID: "discovery-close"}:
				sent = true
			case <-task.Done():
				result, err := task.Join(context.Background())
				if err != nil {
					return true, fmt.Errorf("discovery task panicked: %w", err)
				}
				return true, result
			case <-updates:
			case <-deadline:
				return false, nil
			}
		}
	}
	closeTimer := time.NewTimer(closeTimeout)
	finished, cleanupErr := cleanup(closeTimer.C, true)
	closeTimer.Stop()
	if !finished {
		shutdown()
		// Closing the transport lets the supervisor terminate its process
		// group.
		stopTimer := time.NewTimer(10 * time.Second)
		finished, stopped := cleanup(stopTimer.C, false)
		stopTimer.Stop()
		switch {
		case finished && stopped == nil:
			cleanupErr = errors.New("profile discovery close timed out; harness terminated")
		case finished:
			cleanupErr = fmt.Errorf("terminate profile discovery after close timeout: %w", stopped)
		default:
			// Go cannot abort the task; it was told to stop and is left to
			// finish on its own.
			cleanupErr = errors.New("profile discovery forced shutdown timed out")
		}
	}
	return preserveDiscovery(discovery, discoveryErr, cleanupErr)
}

// preserveDiscovery is Rust's preserve_discovery over two Results: a
// cleanup failure is logged and never replaces the discovery's own outcome.
func preserveDiscovery(discovery core.ProfileConfig, discoveryErr error, cleanupErr error) (core.ProfileConfig, error) {
	if cleanupErr != nil {
		slog.Warn("profile discovery cleanup failed", "discovery_succeeded", discoveryErr == nil, "error", cleanupErr.Error())
	}
	return discovery, discoveryErr
}

func discovered(harness core.HarnessKind, options []schema.SessionConfigOption, models []core.SessionConfigChoice) core.ProfileConfig {
	facts := core.AcpSessionFactsFromOperational(harness, map[string]string{}, options, nil)
	var model *string
	if current, ok := facts.CurrentModel(); ok {
		model = &current
	}
	if models == nil {
		models = []core.SessionConfigChoice{}
	}
	return core.ProfileConfig{
		Model:      model,
		Models:     models,
		Efforts:    core.SessionConfigChoices(options, "effort"),
		ObservedAt: time.Now().Unix(),
	}
}
