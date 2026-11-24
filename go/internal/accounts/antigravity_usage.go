// Port of mj-accounts/src/antigravity_usage.rs.

// Antigravity's own structured `/usage` command, with an isolated login
// copy.

package accounts

import (
	"context"
	"errors"
	"fmt"
	"math"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

// antigravityRuntimeEnvironment is the environment the quota CLI is
// resolved and run under: the daemon's, then the profile's, with the
// Antigravity home settings applied. The prewarm resolves the managed
// runtime from this same environment.
func antigravityRuntimeEnvironment(home string, environment map[string]string) map[string]string {
	// Resolve the runtime from the daemon's environment before HOME is
	// replaced for the disposable login. Profile overrides retain their
	// usual precedence.
	inherited := map[string]string{}
	for _, entry := range os.Environ() {
		if name, value, ok := strings.Cut(entry, "="); ok {
			inherited[name] = value
		}
	}
	for name, value := range environment {
		inherited[name] = value
	}
	core.HarnessKindAntigravity.ConfigureHomeEnvironment(home, inherited)
	return inherited
}

// antigravityManagedRuntime is the pinned `agy` a quota check runs. Unlike
// the other quota harnesses it has no host-CLI step: the host is always
// served the managed installation. Unix only in Rust.
func antigravityManagedRuntime(ctx context.Context, environment map[string]string) (harnessinstall.Located, error) {
	policy := core.ExecutionPolicyConfiguredApprovals
	command, args := core.FallbackLaunch(core.HarnessKindAntigravity, policy)
	located, err := harnessinstall.Locate(ctx, core.LocalHarnessRuntime(), core.HarnessKindAntigravity, policy, command, args, environment)
	if err != nil {
		return harnessinstall.Located{}, fmt.Errorf("prepare Antigravity quota CLI: %w", err)
	}
	return located, nil
}

// AntigravityUsageQuery runs `agy -p /usage` against a disposable copy of
// the profile's login and returns its quota windows.
func AntigravityUsageQuery(ctx context.Context, home string, environment map[string]string) ([]client.QuotaWindow, error) {
	// Only the managed runtime, which is Unix's, adds to the environment.
	runEnvironment := antigravityRuntimeEnvironment(home, environment)
	var program string
	if runtime.GOOS != "windows" {
		located, err := antigravityManagedRuntime(ctx, runEnvironment)
		if err != nil {
			return nil, err
		}
		// Retain Located until capture completes: its lease protects the
		// running CLI.
		if located.Managed != nil {
			defer located.Managed.Release()
		}
		for name, value := range located.Environment {
			runEnvironment[name] = value
		}
		if located.CLIEntrypoint == nil {
			return nil, errors.New("Antigravity quota CLI was not resolved")
		}
		program = *located.CLIEntrypoint
	} else {
		found, ok, err := harnessinstall.FindAntigravityProgram(ctx, runEnvironment)
		if err != nil {
			return nil, err
		}
		if !ok {
			return nil, errors.New("Antigravity requires agy installed on this target")
		}
		program = found
	}
	source := core.HarnessAuthenticationMarker(core.HarnessKindAntigravity, home)
	staged, err := os.MkdirTemp("", ".tmp")
	if err != nil {
		return nil, fmt.Errorf("stage Antigravity quota login: %w", err)
	}
	defer os.RemoveAll(staged)
	_, login, err := core.ReadCredentialFile(core.HarnessKindAntigravity, source)
	if err != nil {
		return nil, err
	}
	if len(login) == 0 {
		return nil, errors.New("Antigravity is not signed in")
	}
	if err := core.WriteCredentialFile(
		core.HarnessKindAntigravity,
		core.HarnessAuthenticationMarker(core.HarnessKindAntigravity, filepath.Join(staged, ".gemini")),
		login,
	); err != nil {
		return nil, err
	}
	// --gemini-dir belongs to `agy acp`, not the top-level print command. A
	// temporary HOME selects the copied account on every supported host.
	command := exec.Command(program, "-p", "/usage", "--output-format", "json")
	command.Env = os.Environ()
	for name, value := range runEnvironment {
		command.Env = append(command.Env, name+"="+value)
	}
	command.Env = append(command.Env,
		"HOME="+staged,
		"USERPROFILE="+staged,
		"XDG_CONFIG_HOME="+filepath.Join(staged, "config"),
		"XDG_CACHE_HOME="+filepath.Join(staged, "cache"),
		"XDG_DATA_HOME="+filepath.Join(staged, "data"),
	)
	command.Dir = staged
	output, err := core.Capture(ctx, command)
	if err != nil {
		return nil, fmt.Errorf("run agy /usage: %w", err)
	}
	if output.Status == nil || !output.Status.Success() {
		return nil, fmt.Errorf("agy /usage failed: %s", strings.TrimSpace(strings.ToValidUTF8(string(output.Stderr), "�")))
	}
	return antigravityUsageParse(output.Stdout)
}

// antigravityBucket is one allowance agy reported.
type antigravityBucket struct {
	id              string
	pool            core.AntigravityQuotaPool
	label           string
	durationSeconds int64
	percent         uint8
	reset           *int64
}

// antigravityPoolDebug is the pool as Rust's derived Debug prints it.
func antigravityPoolDebug(pool core.AntigravityQuotaPool) string {
	switch pool {
	case core.AntigravityQuotaPoolGemini:
		return "Gemini"
	case core.AntigravityQuotaPoolClaude:
		return "Claude"
	}
	return "Argon"
}

func antigravityUsageParse(bytes []byte) ([]client.QuotaWindow, error) {
	response, err := jsonvalue.Parse(bytes)
	if err != nil {
		return nil, fmt.Errorf("decode agy /usage JSON: %w", err)
	}
	if status, ok := jvStr(jvGet(response, "status")); !ok || status != "SUCCESS" {
		reason, ok := jvStr(jvGet(response, "error"))
		if !ok {
			reason = "no successful result"
		}
		return nil, fmt.Errorf("agy /usage refused the request: %s", reason)
	}
	name, nameOK := jvStr(jvPointer(response, "command", "name"))
	turns, turnsOK := jvU64(jvGet(response, "num_turns"))
	if !nameOK || name != "usage" || !turnsOK || turns != 0 {
		return nil, errors.New("agy did not return the built-in quota command")
	}
	groupsValue, _ := jvPointer(response, "command", "data", "groups")
	groups, ok := groupsValue.([]any)
	if !ok {
		return nil, errors.New("agy /usage reported no quota groups")
	}
	buckets := []antigravityBucket{}
	for _, group := range groups {
		bucketsValue, _ := jvGet(group, "buckets")
		groupBuckets, ok := bucketsValue.([]any)
		if !ok {
			return nil, errors.New("agy quota group has no buckets")
		}
		for _, bucket := range groupBuckets {
			id, ok := jvStr(jvGet(bucket, "id"))
			if !ok {
				return nil, errors.New("agy quota bucket has no id")
			}
			var (
				pool  core.AntigravityQuotaPool
				found bool
			)
			for _, candidate := range core.AntigravityQuotaPoolAll {
				if !strings.HasPrefix(id, candidate.BucketPrefix()) {
					continue
				}
				// max_by_key keeps the last of equal keys.
				if !found || len(candidate.BucketPrefix()) >= len(pool.BucketPrefix()) {
					pool, found = candidate, true
				}
			}
			if !found {
				return nil, fmt.Errorf("agy reported an unknown quota bucket: %s", id)
			}
			// Google's /usage documentation defines a weekly and a
			// five-hour allowance for each group:
			// https://antigravity.google/docs/cli/commands/usage/
			// Argon's group reports only the weekly one.
			var (
				label           string
				durationSeconds int64
			)
			window, windowOK := jvStr(jvGet(bucket, "window"))
			switch {
			case windowOK && window == "weekly":
				label, durationSeconds = "Week", 7*24*60*60
			case windowOK && window == "5h":
				label, durationSeconds = "5H", 5*60*60
			default:
				shown := "None"
				if windowOK {
					shown = fmt.Sprintf("Some(%q)", window)
				}
				return nil, fmt.Errorf("agy reported an unknown quota window %s for %s", shown, id)
			}
			remaining, ok := jvF64(jvGet(bucket, "remaining_fraction"))
			if !ok {
				return nil, errors.New("agy quota bucket has no remaining fraction")
			}
			if math.IsNaN(remaining) || math.IsInf(remaining, 0) || remaining < 0 || remaining > 1 {
				return nil, fmt.Errorf("invalid agy quota fraction for %s", id)
			}
			// A pool nobody has used yet reports the Unix epoch: no window is
			// open, so there is no reset to wait for.
			var reset *int64
			if text, ok := jvStr(jvGet(bucket, "reset_time")); ok {
				at, ok := core.ParseChronoRFC3339(text)
				if !ok {
					// chrono's ParseError for a malformed timestamp.
					return nil, errors.New("decode agy quota reset time: input contains invalid characters")
				}
				if seconds := at.Unix(); seconds > 0 {
					reset = &seconds
				}
			}
			// Only a provider-reported zero means the pool is exhausted.
			var percent uint8
			if remaining != 0 {
				percent = saturatingU8(math.Max(math.Round(remaining*100), 1))
			}
			for _, earlier := range buckets {
				if earlier.pool == pool && earlier.label == label {
					return nil, fmt.Errorf("agy reported two %s allowances for its %s pool: %s and %s",
						label, antigravityPoolDebug(pool), earlier.id, id)
				}
			}
			buckets = append(buckets, antigravityBucket{
				id:              id,
				pool:            pool,
				label:           label,
				durationSeconds: durationSeconds,
				percent:         percent,
				reset:           reset,
			})
		}
	}
	if len(buckets) == 0 {
		return nil, errors.New("agy /usage reported no quota windows")
	}
	separateArgon := false
	for _, bucket := range buckets {
		if bucket.pool == core.AntigravityQuotaPoolArgon {
			separateArgon = true
		}
	}
	windows := make([]client.QuotaWindow, 0, len(buckets))
	for _, bucket := range buckets {
		percent := bucket.percent
		duration := bucket.durationSeconds
		scope := bucket.pool.ReportedScope(separateArgon)
		var resets *string
		if bucket.reset != nil {
			resets = FormatResetLocalSeconds(*bucket.reset)
		}
		windows = append(windows, client.QuotaWindow{
			Label:                bucket.label,
			RemainingPercent:     &percent,
			DurationSeconds:      &duration,
			ModelScope:           &scope,
			Resets:               resets,
			ResetsAtEpochSeconds: bucket.reset,
		})
	}
	return windows, nil
}
