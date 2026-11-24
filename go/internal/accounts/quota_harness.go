// Port of mj-accounts/src/quota_harness.rs.

// Host CLI resolution shared by quota probes and login renewals.

package accounts

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

// quotaProgram is the program a quota check or login renewal runs.
type quotaProgram struct {
	command     string
	environment map[string]string
	// installation keeps the harness and managed Node leases until the child
	// exits; release gives them back. Always nil on Windows, which has no
	// managed installation (Rust: `Infallible`).
	installation *harnessinstall.Located
	// shared counts the holders beyond the first that share this program
	// (Rust: the `Arc` count less one); nil until the first retain, when
	// there is one holder. A pointer, so a program copied by value still
	// passes vet's copylocks check.
	shared *atomic.Int64
}

// retain adds a holder, as cloning the Rust `Arc` does; each holder
// releases once. Retain and release a program from the goroutine that owns
// its holders, as the quota poller does.
func (p *quotaProgram) retain() *quotaProgram {
	if p == nil {
		return nil
	}
	if p.shared == nil {
		p.shared = &atomic.Int64{}
	}
	p.shared.Add(1)
	return p
}

// release drops one holder; the last one gives back the installation's
// leases (Rust drops `_installation` with the last reference). Safe on nil,
// and a release after the last is a no-op.
func (p *quotaProgram) release() {
	if p == nil {
		return
	}
	if p.shared != nil {
		if p.shared.Add(-1) >= 0 {
			return
		}
		p.shared.Store(0)
	}
	if p.installation == nil {
		return
	}
	if p.installation.Managed != nil {
		p.installation.Managed.Release()
	}
	p.installation = nil
}

// commandText: CommandSpec carries UTF-8 text. Reject paths it cannot
// represent instead of changing their bytes or asking the child's PATH to
// choose again.
func commandText(command string) (string, error) {
	if !utf8.ValidString(command) {
		return "", fmt.Errorf("quota harness path is not UTF-8: %q", command)
	}
	return command, nil
}

// quotaEnvironmentValue is name's value in environment, else this process's.
// CommandSpec applies its environment in key order; Windows folds aliases,
// so the last spelling wins there too.
func quotaEnvironmentValue(environment map[string]string, name string) (string, bool) {
	var (
		selected string
		value    string
		found    bool
	)
	for key, candidate := range environment {
		matches := key == name
		if runtime.GOOS == "windows" {
			matches = strings.EqualFold(key, name)
		}
		if matches && (!found || key > selected) {
			selected, value, found = key, candidate, true
		}
	}
	if found {
		return value, true
	}
	return os.LookupEnv(name)
}

// hostProgram is the host's own CLI for harness, when it has one: the
// program a quota check runs without any installation. The one lookup both
// locate and PrepareManaged use, so a prewarm never installs what a check
// would not.
func hostProgram(ctx context.Context, harness core.HarnessKind, environment map[string]string) (string, bool, error) {
	name := harness.CLIBinaryName()
	path, pathOK := quotaEnvironmentValue(environment, "PATH")
	home, homeOK := quotaEnvironmentValue(environment, "GROK_HOME")
	selected, found := harnessinstall.FindExecutableOnPath(name, path, pathOK)
	if !found && harness == core.HarnessKindGrok && homeOK {
		// The official installer also uses the inherited profile home.
		selected, found = harnessinstall.FindExecutableOnPath(name, filepath.Join(home, "bin"), true)
	}
	if !found {
		return "", false, nil
	}
	absolute, err := filepath.Abs(selected)
	if err != nil {
		return "", false, fmt.Errorf("resolve absolute quota harness path: %w", err)
	}
	command, err := commandText(absolute)
	if err != nil {
		return "", false, err
	}
	return command, true, nil
}

// searchEnvironment is the environment the installer searches and caches
// under: this process's, overlaid with the profile's.
func searchEnvironment(environment map[string]string) map[string]string {
	search := map[string]string{}
	for _, entry := range os.Environ() {
		if name, value, ok := strings.Cut(entry, "="); ok {
			search[name] = value
		}
	}
	for name, value := range environment {
		search[name] = value
	}
	return search
}

// managedInstallation is the pinned managed installation a quota check runs
// when the host has no CLI. The first call on a host does the cold install.
func managedInstallation(ctx context.Context, harness core.HarnessKind, environment map[string]string) (harnessinstall.Located, error) {
	name := harness.CLIBinaryName()
	search := searchEnvironment(environment)
	policy := core.ExecutionPolicyConfiguredApprovals
	command, args := core.FallbackLaunch(harness, policy)
	located, err := harnessinstall.Locate(ctx, core.LocalHarnessRuntime(), harness, policy, command, args, search)
	if err != nil {
		return harnessinstall.Located{}, fmt.Errorf("no %s to run for quota: could not prepare the pinned harness: %w", name, err)
	}
	return located, nil
}

// ManagedPreparationFailure is one harness PrepareManaged could not prepare
// (Rust: a `(HarnessKind, anyhow::Error)` pair).
type ManagedPreparationFailure struct {
	Harness core.HarnessKind
	Err     error
}

// PrepareManaged installs, ahead of the first quota check, the pinned
// harness each request would otherwise install inline. The caller passes
// only requests that run a host harness, filtered with
// QuotaRefreshRequest.RunsHostHarness on a blocking goroutine, because it
// may read the profile's login file; they are prepared only on a host that
// does not provide the harness. The installer caches per
// `XDG_CACHE_HOME`/`HOME`, so work is grouped by (harness, cache root):
// groups run concurrently, and within a group the profiles are tried in
// order until one preparation succeeds, so one profile's broken environment
// does not hide the others. The daemon container's image carries its
// harnesses and installs nothing. The leases are released on return: a
// check takes its own, and an installation in progress is shared through
// the installer's own lock. Every failure is returned with its harness;
// nothing is retried here, because the next check that needs the harness
// prepares it again. Cancelling ctx cancels the installers' process groups.
// Unix only in Rust.
func PrepareManaged(ctx context.Context, requests []QuotaRefreshRequest) []ManagedPreparationFailure {
	if _, ok := core.DaemonContainer(); ok {
		return nil
	}
	type groupKey struct {
		harness core.HarnessKind
		root    string
	}
	failures := []ManagedPreparationFailure{}
	groups := map[groupKey][]quotaHarnessCandidate{}
	for _, request := range requests {
		environment := make(map[string]string, len(request.Environment))
		for name, value := range request.Environment {
			environment[name] = value
		}
		// The environment each check resolves its program under, so the
		// cache root is the one the check's own installation uses.
		var (
			candidate quotaHarnessCandidate
			search    map[string]string
		)
		if request.Harness == core.HarnessKindAntigravity {
			candidate = quotaHarnessCandidate{
				antigravity: true,
				environment: antigravityRuntimeEnvironment(request.SourceHome, environment),
			}
			search = candidate.environment
		} else {
			candidate = quotaHarnessCandidate{environment: environment}
			search = searchEnvironment(environment)
		}
		root, err := core.ManagedHarnessCacheRoot(search["XDG_CACHE_HOME"], search["HOME"])
		if err != nil {
			failures = append(failures, ManagedPreparationFailure{Harness: request.Harness, Err: err})
			continue
		}
		key := groupKey{harness: request.Harness, root: root}
		groups[key] = append(groups[key], candidate)
	}
	// Rust's BTreeMap orders the groups by harness, then cache root.
	keys := make([]groupKey, 0, len(groups))
	for key := range groups {
		keys = append(keys, key)
	}
	slices.SortFunc(keys, func(left, right groupKey) int {
		if order := core.HarnessKindOrdinal(left.harness) - core.HarnessKindOrdinal(right.harness); order != 0 {
			return order
		}
		return strings.Compare(left.root, right.root)
	})
	cancellation := core.PreparationCancellation(ctx)
	results := make([][]ManagedPreparationFailure, len(keys))
	var wait sync.WaitGroup
	for index, key := range keys {
		wait.Add(1)
		go func() {
			defer wait.Done()
			results[index] = prepareManagedGroup(ctx, cancellation, key.harness, groups[key])
		}()
	}
	wait.Wait()
	for _, result := range results {
		failures = append(failures, result...)
	}
	return failures
}

// prepareManagedGroup tries the candidates of one (harness, cache root)
// group in order until one preparation succeeds.
func prepareManagedGroup(ctx context.Context, cancellation context.Context, harness core.HarnessKind, candidates []quotaHarnessCandidate) []ManagedPreparationFailure {
	failures := []ManagedPreparationFailure{}
	for _, candidate := range candidates {
		var prepared error
		if candidate.antigravity {
			located, err := antigravityManagedRuntime(ctx, candidate.environment)
			if err == nil && located.Managed != nil {
				located.Managed.Release()
			}
			prepared = err
		} else {
			// The PATH lookup is blocking filesystem reads that a stalled
			// mount can hang, and shutdown must not wait for them. It only
			// reads, so abandoning it on cancellation is safe. Racing here
			// leaves the quota check's own locate unchanged.
			type lookup struct {
				found bool
				err   error
			}
			done := make(chan lookup, 1)
			go func() {
				_, found, err := hostProgram(ctx, harness, candidate.environment)
				done <- lookup{found: found, err: err}
			}()
			var host lookup
			select {
			case <-cancellation.Done():
				return append(failures, ManagedPreparationFailure{Harness: harness, Err: core.PreparationCancelled{}})
			case <-ctx.Done():
				return append(failures, ManagedPreparationFailure{Harness: harness, Err: core.PreparationCancelled{}})
			case host = <-done:
			}
			switch {
			case host.err != nil:
				prepared = host.err
			case host.found:
				continue
			default:
				located, err := managedInstallation(ctx, harness, candidate.environment)
				if err == nil && located.Managed != nil {
					located.Managed.Release()
				}
				prepared = err
			}
		}
		if prepared == nil {
			break
		}
		failures = append(failures, ManagedPreparationFailure{Harness: harness, Err: prepared})
	}
	return failures
}

// quotaHarnessCandidate is one profile's way to the managed harness: through
// the shared quota resolution (host CLI first), or Antigravity's, which has
// no host step.
type quotaHarnessCandidate struct {
	antigravity bool
	environment map[string]string
}

// locate is the program a quota check for harness runs: the host's CLI, or
// the pinned managed installation.
func locate(ctx context.Context, harness core.HarnessKind, environment map[string]string) (*quotaProgram, error) {
	name := harness.CLIBinaryName()
	command, found, err := hostProgram(ctx, harness, environment)
	if err != nil {
		return nil, err
	}
	if found {
		return &quotaProgram{command: command, environment: map[string]string{}}, nil
	}
	if runtime.GOOS == "windows" {
		return nil, fmt.Errorf("no %s to run for quota: Mjolnir has no managed installation on this platform and the CLI is not on PATH", name)
	}
	located, err := managedInstallation(ctx, harness, environment)
	if err != nil {
		return nil, err
	}
	program := &quotaProgram{installation: &located}
	if located.CLIEntrypoint == nil {
		program.release()
		return nil, fmt.Errorf("no %s to run for quota: installation names no CLI", name)
	}
	command, err = commandText(*located.CLIEntrypoint)
	if err != nil {
		program.release()
		return nil, err
	}
	program.command = command
	program.environment = make(map[string]string, len(located.Environment))
	for key, value := range located.Environment {
		program.environment[key] = value
	}
	return program, nil
}
