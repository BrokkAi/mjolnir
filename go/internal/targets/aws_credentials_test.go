// Port of the tests in mj-targets/src/aws_credentials.rs.

package targets

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// awsCredentialsExecutor runs every command through execute.
type awsCredentialsExecutor struct {
	core.CommandExecutorDefaults
	execute func(command *core.CommandSpec) (core.CommandOutput, error)
}

func (e awsCredentialsExecutor) PauseDeadline() *core.DeadlinePause { return core.DeadlinePauseNone() }

func (e awsCredentialsExecutor) CancellationRequestedWithoutDeadline() bool {
	return e.CancellationRequested()
}

func (e awsCredentialsExecutor) ExecuteWithoutDeadline(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e awsCredentialsExecutor) ExecuteWithStdinWithoutDeadline(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	return e.ExecuteWithStdin(command, input)
}

func (e awsCredentialsExecutor) ExecuteCleanup(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e awsCredentialsExecutor) ExecuteCleanupWithTimeout(command *core.CommandSpec, _ time.Duration) (core.CommandOutput, error) {
	return e.ExecuteCleanup(command)
}

func (e awsCredentialsExecutor) Execute(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.execute(command)
}

// isolatedAws runs the real command with only PATH from this process.
var isolatedAws = awsCredentialsExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
	isolated := *command
	isolated.ClearEnv = true
	isolated.Env = map[string]string{}
	for name, value := range command.Env {
		isolated.Env[name] = value
	}
	isolated.Env["PATH"] = os.Getenv("PATH")
	return core.ProcessExecutor{}.Execute(&isolated)
}}

func awsTestRole() *core.AwsRoleConfig {
	return &core.AwsRoleConfig{RoleARN: "arn:aws:iam::123456789012:role/mj-bedrock"}
}

func awsTestContainer() *core.TargetsTargetLocator {
	return &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: strings.Repeat("c", 64)}
}

// An unreadable provider configuration is unknown, not "not Bedrock": a
// profile with a role stays managed by the sync, whose mint reads the file
// when it runs and fails closed on an error, while launch (which resolves it
// at once) fails. Nothing is captured in the sync's source.
func TestAnUnreadableCodexConfigurationFailsLaunchAndTheMintButLeavesTheSyncManaged(t *testing.T) {
	home := t.TempDir()
	config := filepath.Join(home, "config.toml")
	profile := core.NewHarnessProfile(core.HarnessKindCodex, home)
	profile.AWS = awsTestRole()
	container := awsTestContainer()
	none := map[string]string{}

	if err := os.WriteFile(config, []byte("model_provider = [unterminated"), 0o666); err != nil {
		t.Fatal(err)
	}
	if _, err := Source(&profile, container, none); err == nil {
		t.Fatal("launch resolved an unreadable configuration")
	}
	synced, err := SyncSource(&profile, container, none)
	if err != nil {
		t.Fatal(err)
	}
	if synced == nil {
		t.Fatal("still managed")
	}
	if synced.Role == nil || !synced.Mints {
		t.Fatalf("synced source = %v, mints %v", synced, synced.Mints)
	}
	_, err = Resolve(synced)
	if err == nil || !strings.Contains(err.Error(), "TOML parse error") {
		t.Fatalf("resolve error = %v", err)
	}

	// Repaired, and it selects an AWS profile: the same source, with nothing
	// rebuilt, resolves to it at the next mint.
	if err := os.WriteFile(config, []byte("model_provider = \"amazon-bedrock-runtime\"\n[model_providers.amazon-bedrock-runtime.aws]\nprofile = \"team\"\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	again, err := SyncSource(&profile, container, none)
	if err != nil {
		t.Fatal(err)
	}
	if again == nil || !again.Equal(synced) {
		t.Fatalf("the repaired configuration changed the sync source: %v", again)
	}
	resolved, err := Resolve(synced)
	if err != nil {
		t.Fatal(err)
	}
	if got := resolved.Environment["AWS_PROFILE"]; got != "team" {
		t.Fatalf("resolved AWS_PROFILE = %q", got)
	}
	selected, err := SelectedSource(&profile)
	if err != nil {
		t.Fatal(err)
	}
	if got := selected.Environment["AWS_PROFILE"]; got != "team" {
		t.Fatalf("selected AWS_PROFILE = %q", got)
	}

	// A raw local session has no credentials of Mjolnir's to manage.
	raw := &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: "/home/me/.local/share/hel/workers/s"}
	if source, err := SyncSource(&profile, raw, none); err != nil || source != nil {
		t.Fatalf("raw local sync source = %v, %v", source, err)
	}
	// Without a role it is the unscoped path, as before role scoping.
	profile.AWS = nil
	if err := os.WriteFile(config, []byte("model_provider = [unterminated"), 0o666); err != nil {
		t.Fatal(err)
	}
	if source, err := SyncSource(&profile, container, none); err != nil || source != nil {
		t.Fatalf("unscoped sync source = %v, %v", source, err)
	}
}

// A role configured for a profile that no longer authenticates with the AWS
// credential chain still manages its sessions, so the sync withdraws the
// credentials they hold; it only does not mint.
func TestARoleOnAProfileThatIsNotBedrockManagesItsSessionsWithoutMinting(t *testing.T) {
	home := t.TempDir()
	profile := core.NewHarnessProfile(core.HarnessKindClaude, home)
	profile.AWS = awsTestRole()
	container := awsTestContainer()
	none := map[string]string{}

	source, err := SyncSource(&profile, container, none)
	if err != nil {
		t.Fatal(err)
	}
	if source == nil {
		t.Fatal("a configured role manages the profile's sessions")
	}
	if source.Mints {
		t.Fatal("nothing is minted for a profile not on Bedrock")
	}
	// Bedrock again: minted.
	profile.Environment.Insert("CLAUDE_CODE_USE_BEDROCK", "1")
	source, err = SyncSource(&profile, container, none)
	if err != nil {
		t.Fatal(err)
	}
	if source == nil || !source.Mints {
		t.Fatal("a Bedrock profile's role mints")
	}
}

// Hard-won: #1385: copying profile selection left isolated Bedrock workers without credentials.
// Hard-won: #1390: background installation omitted the native process-provider configuration.
func TestAwsCliResolvesStagedCredentialsAndTheirRenewal(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix only, as in Rust")
	}
	if os.Getenv(IncludeIgnoredEnv) != "1" {
		t.Skip("requires AWS CLI v2; uses isolated fixture credentials without network access")
	}
	root := t.TempDir()
	home := filepath.Join(root, "profile ' λ")
	if err := os.Mkdir(home, 0o777); err != nil {
		t.Fatal(err)
	}
	empty := filepath.Join(root, "empty")
	if err := os.WriteFile(empty, nil, 0o666); err != nil {
		t.Fatal(err)
	}
	sourceFile := filepath.Join(root, "selected.json")
	config := filepath.Join(root, "config")
	if err := os.WriteFile(config, []byte(fmt.Sprintf("[profile selected]\ncredential_process = cat %s\n", core.PosixQuote(sourceFile))), 0o666); err != nil {
		t.Fatal(err)
	}
	source := core.AwsCredentialSource{
		Environment: map[string]string{
			"AWS_PROFILE":                 "selected",
			"AWS_CONFIG_FILE":             config,
			"AWS_SHARED_CREDENTIALS_FILE": empty,
			"AWS_EC2_METADATA_DISABLED":   "true",
		},
		Mints: true,
	}
	for _, generation := range []string{"original", "renewed"} {
		fixture, err := json.Marshal(map[string]any{
			"Version": 1, "AccessKeyId": "key-" + generation,
			"SecretAccessKey": "secret-" + generation,
			"SessionToken":    "token-" + generation,
			"Expiration":      "2099-01-01T00:00:00Z",
		})
		if err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(sourceFile, fixture, 0o666); err != nil {
			t.Fatal(err)
		}
		exported, err := Export(&source, isolatedAws)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := exported.Install(home, home); err != nil {
			t.Fatal(err)
		}
		workerSource := cloneAwsCredentialSource(&source)
		workerSource.Environment["AWS_CONFIG_FILE"] = filepath.Join(home, "aws-config")
		awsAssertSameExport(t, &workerSource, exported)
		// SSH starts relative profile homes beneath HOME, while the
		// credential process may run from an unrelated working directory.
		configuration, err := exported.Configuration("profile ' λ")
		if err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(home, "aws-config"), configuration, 0o666); err != nil {
			t.Fatal(err)
		}
		workerSource.Environment["HOME"] = root
		awsAssertSameExport(t, &workerSource, exported)
	}
	// Missing credentials fail with an authentication explanation before
	// starting a harness, without falling through to instance metadata.
	source.Environment["AWS_PROFILE"] = "missing"
	_, err := Export(&source, isolatedAws)
	if err == nil {
		t.Fatal("a missing profile exported credentials")
	}
	if !strings.Contains(err.Error(), "Could not resolve AWS credentials") {
		t.Fatal(err)
	}
	if strings.Contains(err.Error(), "secret-renewed") {
		t.Fatal(err)
	}
}

func awsAssertSameExport(t *testing.T, source *core.AwsCredentialSource, expected core.AwsCredentials) {
	t.Helper()
	worker, err := Export(source, isolatedAws)
	if err != nil {
		t.Fatal(err)
	}
	got, err := worker.Encode()
	if err != nil {
		t.Fatal(err)
	}
	want, err := expected.Encode()
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(got, want) {
		t.Fatal("the worker resolved different credentials from the controller")
	}
}

// Hard-won: #1385: AWS helpers can return secrets in either output stream on failure.
func TestFailedAwsExportDiagnosticsNeverIncludeHelperOutput(t *testing.T) {
	failedExport := awsCredentialsExecutor{execute: func(*core.CommandSpec) (core.CommandOutput, error) {
		return core.CommandOutput{Status: 1, Stdout: []byte("private-key"), Stderr: []byte("private-secret")}, nil
	}}
	source := core.AwsCredentialSource{Environment: map[string]string{}, Mints: true}
	_, err := Export(&source, failedExport)
	if err == nil {
		t.Fatal("a failed export succeeded")
	}
	if strings.Contains(err.Error(), "private-") {
		t.Fatal(err)
	}
	if runtime.GOOS == "windows" {
		return
	}
	// The shared executor normally logs stderr on a disk-full failure. AWS
	// helper output must bypass that observer too.
	failedProcess := awsCredentialsExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		failing := *command
		failing.Program = "sh"
		failing.Args = []string{"-c", "echo 'No space left on device: private-secret' >&2; exit 1"}
		return core.ProcessExecutor{}.Execute(&failing)
	}}
	log := core.NewCapturedLog()
	previous := slog.Default()
	slog.SetDefault(slog.New(log))
	defer slog.SetDefault(previous)
	_, err = Export(&source, failedProcess)
	if err == nil {
		t.Fatal("a failed export succeeded")
	}
	if strings.Contains(err.Error(), "private-secret") {
		t.Fatal(err)
	}
	for _, event := range log.Events() {
		if strings.Contains(event.Text, "private-secret") {
			t.Fatalf("logged helper output: %s", event.Text)
		}
	}
}

func awsNoticeRole() *core.AwsRoleConfig { return awsTestRole() }

func awsNoticeBedrock(home string) core.HarnessProfile {
	profile := core.NewHarnessProfile(core.HarnessKindClaude, home)
	profile.Environment.Insert("CLAUDE_CODE_USE_BEDROCK", "1")
	profile.AWS = awsNoticeRole()
	return profile
}

func TestARoleThatCannotApplyIsANoticeThatNamesWhyNotARefusal(t *testing.T) {
	home := t.TempDir()
	none := map[string]string{}
	plain := core.NewHarnessProfile(core.HarnessKindClaude, home)
	if notice, ok := UnusedRoleNotice("work", &plain, SessionHostIsolated, none); ok {
		t.Fatalf("a profile without a role got a notice: %s", notice)
	}
	plain.AWS = awsNoticeRole()
	notice, ok := UnusedRoleNotice("work", &plain, SessionHostIsolated, none)
	if !ok {
		t.Fatal("a non-Bedrock profile's role got no notice")
	}
	if !strings.Contains(notice, "[profiles.work.aws] has no effect") {
		t.Fatal(notice)
	}
	if !strings.Contains(notice, "Amazon Bedrock") {
		t.Fatal(notice)
	}

	// A Bedrock profile: the reason depends on where the session runs.
	profile := awsNoticeBedrock(home)
	if notice, ok := UnusedRoleNotice("work", &profile, SessionHostIsolated, none); ok {
		t.Fatalf("an isolated Bedrock session got a notice: %s", notice)
	}
	reason := func(host SessionHost, environment map[string]string) string {
		t.Helper()
		notice, ok := UnusedRoleNotice("work", &profile, host, environment)
		if !ok {
			t.Fatal("no notice")
		}
		return notice
	}
	if got := reason(SessionHostRawLocal, none); !strings.Contains(got, "raw local sessions read the host's own AWS files") {
		t.Fatal(got)
	}
	if got := reason(SessionHostEc2, none); !strings.Contains(got, "instance's own role") {
		t.Fatal(got)
	}
	ownProfile := map[string]string{"AWS_PROFILE": "team"}
	if got := reason(SessionHostIsolated, ownProfile); !strings.Contains(got, "target's environment sets its own AWS") {
		t.Fatal(got)
	}
	ownKeys := map[string]string{"AWS_ACCESS_KEY_ID": "AKIA"}
	if got := reason(SessionHostIsolated, ownKeys); !strings.Contains(got, "target's environment sets its own AWS") {
		t.Fatal(got)
	}
	// Keys in the profile's own environment take precedence too.
	withKeys := awsNoticeBedrock(home)
	withKeys.Environment.Insert("AWS_ACCESS_KEY_ID", "AKIA")
	notice, ok = UnusedRoleNotice("work", &withKeys, SessionHostIsolated, none)
	if !ok || !strings.Contains(notice, "profile's environment sets its own AWS credentials") {
		t.Fatalf("notice = %q", notice)
	}
}
