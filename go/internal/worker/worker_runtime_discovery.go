// Port of mj-worker/src/worker_runtime/discovery.rs.

// A short-lived harness that discovers configuration without a prompt.

//go:build unix

package worker

import (
	"context"
	"log/slog"
	"time"

	"github.com/BrokkAi/acp-go/schema"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// DiscoverProfileConfig starts the profile's harness without a prompt and
// reports the models and efforts it offers.
func DiscoverProfileConfig(ctx context.Context, spec core.ProfileProbeSpec) (core.ProfileConfig, error) {
	panic("unported: mj-worker/src/worker_runtime/discovery.rs discover_profile_config")
}

func discoveryProbe(ctx context.Context, launch LaunchSpec, model *string) (core.ProfileConfig, error) {
	return probeWithCloseTimeout(ctx, launch, model, 15*time.Second)
}

func probeWithCloseTimeout(ctx context.Context, launch LaunchSpec, model *string, closeTimeout time.Duration) (core.ProfileConfig, error) {
	panic("unported: mj-worker/src/worker_runtime/discovery.rs probe_with_close_timeout")
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
	return core.ProfileConfig{
		Model:      model,
		Models:     models,
		Efforts:    core.SessionConfigChoices(options, "effort"),
		ObservedAt: time.Now().Unix(),
	}
}
