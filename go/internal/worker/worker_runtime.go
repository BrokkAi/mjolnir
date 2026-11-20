// Port of mj-worker/src/worker_runtime.rs.

// Target-side daemon and stdio proxy for the durable ACP relay protocol.
//
// The `cfg(not(unix))` stubs live in worker_runtime_other.go. The Unix
// runtime itself (run_daemon, proxy, the supervisor) is worker_runtime_unix.go
// and its submodules.

package worker

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// RecordStartupStep records which startup step this worker is on, in
// `worker-startup.json` in the worker root.
//
// A detached worker's only channel before it binds its control socket is its
// root directory, and its log stays empty until something logs at `warn`.
// Without this file a controller waiting for the socket cannot tell a worker
// that is still making progress from one that died without a word: both look
// like a directory with nothing in it. Every step is appended, so the record
// also says how long each step took, and the login-environment re-exec shows
// up as a second `start`.
//
// Best effort by design: a breadcrumb must never be the reason a worker fails
// to start.
func RecordStartupStep(root string, step string) {
	panic("unported: mj-worker/src/worker_runtime.rs record_startup_step")
}

// RecordStartupFailure records the preparation error alongside the last
// startup breadcrumb. The worker stays alive after this point, so its exit
// record cannot carry the failure that the daemon needs to explain a failed
// launch.
func RecordStartupFailure(root string, step string, errorText string) {
	panic("unported: mj-worker/src/worker_runtime.rs record_startup_failure")
}

const githubCLIBinEnv = "MJ_GITHUB_CLI_BIN"

func githubCLILoginShellCommand(command string) string {
	return fmt.Sprintf(
		`if [ -n "${%[1]s:-}" ]; then PATH="$%[1]s:$PATH"; export PATH; fi; unset %[1]s GH_TOKEN GITHUB_TOKEN; %[2]s`,
		githubCLIBinEnv, command,
	)
}

// AcpSupervisorSpec is the launch description the ACP supervisor re-execs
// against (serde deny_unknown_fields).
type AcpSupervisorSpec struct {
	Command     string            `json:"command"`
	Args        []string          `json:"args"`
	Environment map[string]string `json:"environment"`
	// ExcludedEnvironment names variables removed from the bridge's
	// environment after the login environment is merged in, as the launch
	// description names them.
	ExcludedEnvironment []string `json:"excluded_environment,omitempty"`
	Cwd                 string   `json:"cwd"`
	// HarnessLease is the shared advisory lock the supervisor holds for the
	// complete lifetime of a managed harness process tree.
	HarnessLease *string `json:"harness_lease,omitempty"`
	// NodeLease is the shared advisory lock the supervisor holds for the
	// managed Node.js runtime used by this ACP process tree.
	NodeLease *string `json:"node_lease,omitempty"`
}

// MarshalJSON writes the spec as serde does: an empty args list and
// environment are present as `[]` and `{}`.
func (s AcpSupervisorSpec) MarshalJSON() ([]byte, error) {
	type plain AcpSupervisorSpec
	value := plain(s)
	if value.Args == nil {
		value.Args = []string{}
	}
	if value.Environment == nil {
		value.Environment = map[string]string{}
	}
	return workerRuntimeMarshalJSON(value)
}

// UnmarshalJSON decodes the spec with serde's deny_unknown_fields and its
// required fields.
func (s *AcpSupervisorSpec) UnmarshalJSON(data []byte) error {
	type plain AcpSupervisorSpec
	var value plain
	if err := workerRuntimeDecodeStrict(data, &value, "command", "args", "environment", "cwd"); err != nil {
		return err
	}
	*s = AcpSupervisorSpec(value)
	return nil
}

// AcpSupervisorSpecFrom is Rust's `From<&WorkerLaunchConfig>`.
func AcpSupervisorSpecFrom(config *core.WorkerLaunchConfig) AcpSupervisorSpec {
	excluded := slices.Clone(config.ExcludedEnvironment)
	excluded = append(excluded, core.FastinoKeyEnvironment)
	if config.NoGithubCredentials {
		for _, name := range core.GithubAuthenticationEnvironment {
			if !slices.Contains(excluded, name) {
				excluded = append(excluded, name)
			}
		}
	}
	environment := make(map[string]string, len(config.Environment))
	maps.Copy(environment, config.Environment)
	return AcpSupervisorSpec{
		Command:             config.BridgeCommand,
		Args:                slices.Clone(config.BridgeArgs),
		Environment:         environment,
		ExcludedEnvironment: excluded,
		Cwd:                 config.Cwd,
	}
}

// AcpSupervisorSpecRead reads a spec written by writeSpec.
func AcpSupervisorSpecRead(path string) (AcpSupervisorSpec, error) {
	body, err := os.ReadFile(path)
	if err != nil {
		return AcpSupervisorSpec{}, fmt.Errorf("read ACP supervisor spec %s: %w", path, err)
	}
	var spec AcpSupervisorSpec
	if err := json.Unmarshal(body, &spec); err != nil {
		return AcpSupervisorSpec{}, fmt.Errorf("parse ACP supervisor spec %s: %w", path, err)
	}
	return spec, nil
}

func (s *AcpSupervisorSpec) writeSpec(path string) error {
	body, err := workerRuntimeMarshalPretty(s)
	if err != nil {
		return err
	}
	return core.AtomicWrite(path, body)
}

// excludeFromHarnessEnvironment removes the variables a launch excludes from
// the harness environment it is about to write into a bridge spec, and says
// once, at info, which of them the target had set. `environment` is the
// merged session environment, login environment included; `overrides` is
// what the spec carries.
func excludeFromHarnessEnvironment(harness core.HarnessKind, excluded []string, environment map[string]string, overrides map[string]string) {
	var present []string
	for _, name := range excluded {
		_, inEnvironment := environment[name]
		_, inOverrides := overrides[name]
		if inEnvironment || inOverrides {
			present = append(present, name)
		}
	}
	if len(present) > 0 {
		slog.Info("removed API key settings from the harness environment: this profile signs in with ChatGPT and must not fall back to an API key",
			"harness", harness.DisplayName(), "removed", strings.Join(present, ", "))
	}
	for _, name := range excluded {
		delete(overrides, name)
	}
}

// enforceExecutionPolicy applies a target's execution policy to the harness
// environment.
//
// This is worker behaviour, so it lives here rather than with the foundation
// type it edits.
func enforceExecutionPolicy(config *core.WorkerLaunchConfig) error {
	if config.Environment == nil {
		config.Environment = map[string]string{}
	}
	return config.Harness.ConfigureExecutionEnvironment(config.ExecutionPolicy, config.Environment)
}

// codexConfigEnv is the environment variable codex-acp reads as its startup
// configuration.
const codexConfigEnv = "CODEX_CONFIG"

// pinAcceptedBridgeSelectors pins the selectors a harness can only take
// before it opens a session into the bridge's environment.
//
// This is the one place that knows what a harness needs pre-start. Claude
// takes its model in the ACP `session/new` and `session/load` request, which
// the acp module builds from the same accepted configuration at every bridge
// launch; Codex has no such field and takes it here instead. Kimi, Grok and
// Muse need nothing before the handshake: the worker restores their accepted
// selectors on the live session once it is open.
//
// Codex derives a resumed thread's turn context from the launch request, not
// from what the thread recorded, and codex-acp sends the profile's provider
// but never a model. A resume therefore opens on the `model` in the profile's
// `config.toml` — and Codex reports that as a mismatch ("This session was
// recorded with X but is resuming with Y") before the worker can restore the
// accepted selector after the handshake. `CODEX_CONFIG` is the only pre-start
// channel codex-acp exposes for this; the model cannot travel in the ACP
// request, the way Claude's `claudeCode.options.model` does.
//
// Keys already in `CODEX_CONFIG` are kept, and only `model` is written: mode
// and permission keys set here would be discarded, because codex-acp merges
// this configuration ahead of the policy keys it derives from the target.
//
// The accepted reasoning effort is deliberately not pinned. Codex does not
// warn about it, and the worker restores it on the live session once the
// handshake completes, so pinning it here would only risk changing the
// effort the resumed thread starts on.
//
// `model` is LaunchSpec.startupModel: the accepted model, or for a new
// session the one it was created for, so a new thread also starts on its own
// model rather than the profile default (issue 1217).
func pinAcceptedBridgeSelectors(harness core.HarnessKind, environment map[string]string, model *string) error {
	if harness != core.HarnessKindCodex || model == nil {
		return nil
	}
	config := jsonvalue.Object{}
	if existing, ok := environment[codexConfigEnv]; ok {
		value, err := jsonvalue.Parse([]byte(existing))
		object, isObject := value.(jsonvalue.Object)
		if err != nil || !isObject {
			return fmt.Errorf("%s must be a JSON object to restore this session's model", codexConfigEnv)
		}
		config = object
	}
	config.Set("model", *model)
	encoded, err := jsonvalue.Marshal(config)
	if err != nil {
		return err
	}
	environment[codexConfigEnv] = string(encoded)
	return nil
}

// repinBridgeSelectors brings the bridge's supervisor spec up to date with
// what this session has accepted, so the next bridge process starts on it.
//
// The spec is written once, before the worker's first bridge starts, and
// every later bridge re-execs the supervisor against that same file. A
// session accepts its model after that point far more often than before it:
// a session created with an explicit model applies the selector once the
// session exists, and a model set later never reaches the file at all. So a
// pin taken only at worker start is missing exactly when it is needed, and
// every in-worker resume reopens the harness on the profile default. Rewrite
// it from the live accepted configuration at each launch instead.
func repinBridgeSelectors(path string, harness core.HarnessKind, model *string) error {
	spec, err := AcpSupervisorSpecRead(path)
	if err != nil {
		return err
	}
	environment := maps.Clone(spec.Environment)
	if spec.Environment == nil {
		spec.Environment = map[string]string{}
	}
	if err := pinAcceptedBridgeSelectors(harness, spec.Environment, model); err != nil {
		return err
	}
	if maps.Equal(spec.Environment, environment) {
		return nil
	}
	return spec.writeSpec(path)
}

// CredentialEndpoint is where this relay's harness keeps its home, resolved
// solely from the launch config. Credential and skills requests carry no
// path, so a caller cannot steer a read or write outside the session's
// harness home.
type CredentialEndpoint struct {
	NoGithubCredentials bool
	Harness             core.HarnessKind
	// Home is the session's harness home; skills trees sync under it.
	Home   string
	Marker string
}

// credentialEndpoint is Rust's `Result<CredentialEndpoint, String>`: the
// error carries only the message.
func credentialEndpoint(config *core.WorkerLaunchConfig) (CredentialEndpoint, error) {
	home := config.HarnessHome
	if home == "" {
		// A config persisted before the home was stated outright. Those
		// releases always set the harness home variable.
		key := config.Harness.HomeEnv()
		value, ok := config.Environment[key]
		if !ok {
			return CredentialEndpoint{}, fmt.Errorf("worker launch config has no harness home and no %s entry, so it cannot locate harness credentials", key)
		}
		home = config.Harness.HomeFromEnvironment(value)
	}
	marker := core.HarnessAuthenticationMarker(config.Harness, home)
	if config.AuthenticationMarker != nil {
		marker = filepath.Join(home, *config.AuthenticationMarker)
	}
	return CredentialEndpoint{
		NoGithubCredentials: config.NoGithubCredentials,
		Harness:             config.Harness,
		Home:                home,
		Marker:              marker,
	}, nil
}

func resolveRelativeHarnessHome(config *core.WorkerLaunchConfig, base string) {
	panic("unported: mj-worker/src/worker_runtime.rs resolve_relative_harness_home")
}

func reviewerWorkspaceDirectories(config *core.WorkerLaunchConfig) []string {
	var directories []string
	for _, directory := range config.AdditionalDirectories {
		if config.ProjectMemory == nil || directory != config.ProjectMemory.Root {
			directories = append(directories, directory)
		}
	}
	return directories
}

func acpAdditionalDirectories(config *core.WorkerLaunchConfig) []string {
	directories := slices.Clone(config.AdditionalDirectories)
	if config.Harness != core.HarnessKindClaude && config.Harness != core.HarnessKindMuse &&
		config.ProjectMemory != nil && !slices.Contains(directories, config.ProjectMemory.Root) {
		directories = append(directories, config.ProjectMemory.Root)
	}
	return directories
}

func resolveRelativeWorkerRoot(root string, base string) string {
	if !filepath.IsAbs(root) {
		return filepath.Join(base, root)
	}
	return root
}

// workerRuntimeMarshalJSON encodes v as serde_json::to_vec does: without
// Go's HTML escaping and without a trailing newline.
func workerRuntimeMarshalJSON(v any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// workerRuntimeMarshalPretty encodes v as serde_json::to_vec_pretty does:
// two-space indent, no HTML escaping, no trailing newline.
func workerRuntimeMarshalPretty(v any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// workerRuntimeDecodeStrict decodes data into v as serde does with
// deny_unknown_fields, and fails as serde does when a required field is
// absent.
func workerRuntimeDecodeStrict(data []byte, v any, required ...string) error {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(v); err != nil {
		return err
	}
	if len(required) == 0 {
		return nil
	}
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	for _, name := range required {
		if _, ok := fields[name]; !ok {
			return errors.New("missing field `" + name + "`")
		}
	}
	return nil
}
