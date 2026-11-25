//go:build unix

// Port of the tests in mj-accounts/src/harness_login/acp_login.rs.

package accounts

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// acpLoginTestLock serializes the sign-in tests that share the process-wide
// SIGINT handling.
var acpLoginTestLock sync.Mutex

func acpWriteScript(directory, name, body string) string {
	core.InstallFakeCommand(directory, name, body)
	return filepath.Join(directory, name)
}

func acpHome(directory string) map[string]string {
	return map[string]string{
		"HOME":            directory,
		"XDG_CONFIG_HOME": filepath.Join(directory, ".config"),
		"XDG_DATA_HOME":   filepath.Join(directory, ".local", "share"),
		"PATH":            os.Getenv("PATH"),
		"DISPLAY":         ":0",
	}
}

// acpWriteTools installs `node`, `npm` and `npx` stand-ins that report the
// versions the managed installation expects.
func acpWriteTools(tools string) {
	for _, name := range []string{"node", "npm", "npx"} {
		body := "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 11.6.2; fi\n"
		if name == "node" {
			body = "#!/bin/sh\nif [ \"$1\" = --version ]; then echo v24.22.1; fi\n"
		}
		acpWriteScript(tools, name, body)
	}
}

func acpWriteManifest(t *testing.T, install string, kind core.HarnessKind) {
	t.Helper()
	manifest, err := json.MarshalIndent(core.ManagedHarnessManifestForHarness(kind), "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := core.AtomicWrite(filepath.Join(install, core.ManifestFile), manifest); err != nil {
		t.Fatal(err)
	}
}

func TestPinnedCLIComesFromTheSameManagedInstallAsTheBridge(t *testing.T) {
	acpLoginTestLock.Lock()
	defer acpLoginTestLock.Unlock()
	root := t.TempDir()
	tools := filepath.Join(root, "tools")
	if err := os.MkdirAll(tools, 0o777); err != nil {
		t.Fatal(err)
	}
	acpWriteTools(tools)
	xdgCache := filepath.Join(root, "cache")
	cacheRoot, err := core.ManagedHarnessCacheRoot(xdgCache, root)
	if err != nil {
		t.Fatal(err)
	}
	install := core.ManagedHarnessInstallDir(cacheRoot, core.HarnessKindClaude)
	bin := filepath.Join(install, "node_modules", ".bin")
	if err := os.MkdirAll(bin, 0o777); err != nil {
		t.Fatal(err)
	}
	bridge := acpWriteScript(bin, "claude-agent-acp", "#!/bin/sh\nexit 0\n")
	cli := acpWriteScript(bin, "claude", "#!/bin/sh\necho pinned-claude\n")
	acpWriteManifest(t, install, core.HarnessKindClaude)
	environment := acpHome(root)
	environment["PATH"] = tools + ":/usr/bin:/bin"
	environment["XDG_CACHE_HOME"] = xdgCache
	profile := core.NewHarnessProfile(core.HarnessKindClaude, filepath.Join(root, "claude-profile"))
	profile.Environment = core.EnvironmentFromMap(environment)
	if err := os.MkdirAll(profile.Home, 0o777); err != nil {
		t.Fatal(err)
	}

	pinned, err := locatePinnedCLI(t.Context(), &profile)
	if err != nil {
		t.Fatal(err)
	}
	defer pinned.release()

	if pinned.Command != cli {
		t.Fatalf("command = %q", pinned.Command)
	}
	if !slices.Contains(strings.Split(pinned.Environment["PATH"], ":"), tools) {
		t.Fatalf("PATH = %q", pinned.Environment["PATH"])
	}
	if _, err := os.Lstat(bridge); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: #1452: interactive sign-in started the account login-shell
// probe, so a slow shell spent its startup budget before every login.
func TestSignInUsesTheTerminalEnvironmentWithoutStartingTheAccountShell(t *testing.T) {
	acpLoginTestLock.Lock()
	defer acpLoginTestLock.Unlock()
	directory := t.TempDir()
	slowShell := acpWriteScript(directory, "slow-login-shell", "#!/bin/sh\nsleep 30\n")
	ambientHome := filepath.Join(directory, "ambient-home")
	if err := os.Mkdir(ambientHome, 0o777); err != nil {
		t.Fatal(err)
	}
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	command := exec.Command(executable, "-test.run=^TestSignInEnvironmentChild$", "-test.v")
	command.Env = append(os.Environ(),
		targets.IncludeIgnoredEnv+"=1",
		"MJ_TEST_SIGNIN_ROOT="+directory,
		"MJ_TEST_ACCOUNT_SHELL="+slowShell,
		"MJ_TEST_AMBIENT_VALUE=kept",
		"HOME="+ambientHome,
	)
	ctx, cancel := context.WithTimeout(t.Context(), 5*time.Second)
	defer cancel()
	output, err := core.RunCapturingStdoutCancellable(ctx, command)
	if ctx.Err() != nil {
		t.Fatal("interactive sign-in must not wait for the account login shell")
	}
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() {
		t.Fatalf("%s", output.Stdout)
	}
}

// TestSignInEnvironmentChild is the subprocess fixture for interactive
// sign-in environment handling (Rust: an #[ignore] test run with
// --ignored).
func TestSignInEnvironmentChild(t *testing.T) {
	if os.Getenv(targets.IncludeIgnoredEnv) != "1" {
		t.Skip("subprocess fixture for interactive sign-in environment handling")
	}
	core.EnableTestHooks()
	root := os.Getenv("MJ_TEST_SIGNIN_ROOT")
	profile := core.NewHarnessProfile(core.HarnessKindDsh, filepath.Join(root, "profile"))
	located, environment, err := locatedLogin(t.Context(), &profile)
	if err != nil {
		t.Fatal(err)
	}
	if located.Managed != nil {
		defer located.Managed.Release()
	}
	if got, ok := environment["MJ_TEST_AMBIENT_VALUE"]; !ok || got != "kept" {
		t.Fatalf("MJ_TEST_AMBIENT_VALUE = %q, %t", got, ok)
	}
}

// Hard-won: d42fd3cbe: headless daemon login refused Codex ChatGPT sign-in with no recovery
func TestHeadlessDaemonLoginStreamsCodexDeviceAuth(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("Linux only")
	}
	acpLoginTestLock.Lock()
	defer acpLoginTestLock.Unlock()
	root := t.TempDir()
	tools := filepath.Join(root, "tools")
	if err := os.MkdirAll(tools, 0o777); err != nil {
		t.Fatal(err)
	}
	acpWriteTools(tools)
	cache := filepath.Join(root, "cache")
	cacheRoot, err := core.ManagedHarnessCacheRoot(cache, root)
	if err != nil {
		t.Fatal(err)
	}
	install := core.ManagedHarnessInstallDir(cacheRoot, core.HarnessKindCodex)
	selected := core.Pin(core.HarnessKindCodex)
	bridgePath := filepath.Join(install, filepath.FromSlash(selected.Entrypoint))
	cliPath := filepath.Join(install, filepath.FromSlash(selected.CLIEntrypoint))
	for _, path := range []string{bridgePath, cliPath} {
		if err := os.MkdirAll(filepath.Dir(path), 0o777); err != nil {
			t.Fatal(err)
		}
	}
	acpWriteScript(filepath.Dir(bridgePath), filepath.Base(bridgePath), acpBridgeScript(t, root, `[{"id":"chat-gpt","name":"ChatGPT"}]`))
	acpWriteScript(filepath.Dir(cliPath), filepath.Base(cliPath),
		"#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HOME/argv\"\necho 'https://auth.openai.com/codex/device CODE-1234'\n")
	acpWriteManifest(t, install, core.HarnessKindCodex)
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	command := exec.Command(executable, "-test.run=^TestDaemonLoginChild$", "-test.v")
	command.Env = append(os.Environ(),
		targets.IncludeIgnoredEnv+"=1",
		"MJ_DEVICE_LOGIN_TEST_ROOT="+root,
		"MJ_DATA_DIR="+filepath.Join(root, "data"),
	)
	output, err := core.RunCapturingStdoutCancellable(t.Context(), command)
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() {
		t.Fatalf("%s", output.Stdout)
	}
	argv, err := os.ReadFile(filepath.Join(root, "argv"))
	if err != nil || string(argv) != "login\n--device-auth\n" {
		t.Fatalf("argv = %q, %v", argv, err)
	}
	text := string(output.Stdout)
	if !strings.Contains(text, "https://auth.openai.com/codex/device CODE-1234") {
		t.Fatalf("%s", text)
	}
	if !strings.Contains(text, "no desktop browser") {
		t.Fatalf("%s", text)
	}
}

// TestDaemonLoginChild is the subprocess fixture for the daemon terminal
// login driver (Rust: an #[ignore] test run with --ignored).
func TestDaemonLoginChild(t *testing.T) {
	if runtime.GOOS != "linux" || os.Getenv(targets.IncludeIgnoredEnv) != "1" {
		t.Skip("subprocess fixture for the daemon terminal login driver")
	}
	root := os.Getenv("MJ_DEVICE_LOGIN_TEST_ROOT")
	environment := acpHome(root)
	for _, name := range []string{"DISPLAY", "WAYLAND_DISPLAY", "CODEX_API_KEY", "OPENAI_API_KEY"} {
		environment[name] = ""
	}
	environment["XDG_CACHE_HOME"] = filepath.Join(root, "cache")
	environment["PATH"] = filepath.Join(root, "tools") + ":/usr/bin:/bin"
	profile := core.NewHarnessProfile(core.HarnessKindCodex, root)
	profile.Environment = core.EnvironmentFromMap(environment)
	code, err := Login(t.Context(), "device-test", &profile, LoginOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if code != 0 {
		t.Fatalf("exit code %d", code)
	}
}

// acpBridge is a bridge that answers initialize with methods and records
// every later request in `requests`.
func acpBridge(t *testing.T, directory, methods string) string {
	return acpWriteScript(directory, "bridge.sh", acpBridgeScript(t, directory, methods))
}

func acpBridgeScript(t *testing.T, directory, methods string) string {
	t.Helper()
	authMethods, err := jsonvalue.Parse([]byte(methods))
	if err != nil {
		t.Fatal(err)
	}
	response, err := jsonvalue.Marshal(jsonvalue.Object{
		{Key: "jsonrpc", Value: "2.0"},
		{Key: "id", Value: int64(1)},
		{Key: "result", Value: jsonvalue.Object{
			{Key: "protocolVersion", Value: int64(1)},
			{Key: "authMethods", Value: authMethods},
		}},
	})
	if err != nil {
		t.Fatal(err)
	}
	return fmt.Sprintf("#!/bin/sh\n"+
		"read -r line\n"+
		"printf '%%s\\n' '%s'\n"+
		"while read -r line; do\n"+
		"  printf '%%s\\n' \"$line\" >> %s\n"+
		"  case \"$line\" in\n"+
		"    *'\"id\":2'*) printf '%%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}' ;;\n"+
		"  esac\n"+
		"done\n", response, filepath.Join(directory, "requests"))
}

func TestAnAPIKeyProfileReportsNoInteractiveLogin(t *testing.T) {
	acpLoginTestLock.Lock()
	defer acpLoginTestLock.Unlock()
	directory := t.TempDir()
	profile := core.NewHarnessProfile(core.HarnessKindDsh, directory)
	profile.Environment.Insert("DEEPSEEK_API_KEY", "secret")
	outcome, err := acpLogin(t.Context(), &profile, nil)
	if err != nil {
		t.Fatal(err)
	}
	if noLogin, ok := outcome.(acpLoginOutcomeNoLogin); !ok || !strings.Contains(noLogin.Value, "DEEPSEEK_API_KEY") {
		t.Fatalf("%#v", outcome)
	}
}

func TestATerminalMethodRunsTheAdvertisedCommandWithTheProfileEnvironment(t *testing.T) {
	acpLoginTestLock.Lock()
	defer acpLoginTestLock.Unlock()
	directory := t.TempDir()
	marker := filepath.Join(directory, "terminal-ran")
	login := acpWriteScript(directory, "login.sh",
		fmt.Sprintf("#!/bin/sh\nprintf '%%s' \"$HOME:$MJ_LOGIN_ENV\" > %s\n", marker))
	methods, err := jsonvalue.Marshal([]any{jsonvalue.Object{
		{Key: "id", Value: "muse-login"},
		{Key: "name", Value: "Log in with Muse"},
		{Key: "type", Value: "terminal"},
		{Key: "args", Value: []any{"login"}},
		{Key: "env", Value: jsonvalue.Object{{Key: "MJ_LOGIN_ENV", Value: "from-bridge"}}},
		{Key: "_meta", Value: jsonvalue.Object{{Key: "terminal-auth", Value: jsonvalue.Object{
			{Key: "command", Value: login},
			{Key: "args", Value: []any{}},
		}}}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	bridge := acpBridge(t, directory, string(methods))
	outcome, err := signIn(t.Context(), core.HarnessKindMuse, directory, bridge, nil, bridge, acpHome(directory), nil)
	if err != nil {
		t.Fatal(err)
	}
	if signedIn, ok := outcome.(acpLoginOutcomeSignedIn); !ok || signedIn.Method != "muse-login" {
		t.Fatalf("%#v", outcome)
	}
	ran, err := os.ReadFile(marker)
	if err != nil || string(ran) != directory+":from-bridge" {
		t.Fatalf("marker = %q, %v", ran, err)
	}
}

func TestAnAgentMethodSendsAuthenticate(t *testing.T) {
	acpLoginTestLock.Lock()
	defer acpLoginTestLock.Unlock()
	directory := t.TempDir()
	bridge := acpBridge(t, directory, `[{"id":"chat-gpt","name":"ChatGPT"}]`)
	outcome, err := signIn(t.Context(), core.HarnessKindCodex, directory, bridge, nil, bridge, acpHome(directory), nil)
	if err != nil {
		t.Fatal(err)
	}
	if signedIn, ok := outcome.(acpLoginOutcomeSignedIn); !ok || signedIn.Method != "chat-gpt" {
		t.Fatalf("%#v", outcome)
	}
	requests, err := os.ReadFile(filepath.Join(directory, "requests"))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(requests), `"method":"authenticate"`) || !strings.Contains(string(requests), "chat-gpt") {
		t.Fatalf("%s", requests)
	}
}

func TestAnUnknownRequestedMethodListsTheAvailableIDs(t *testing.T) {
	acpLoginTestLock.Lock()
	defer acpLoginTestLock.Unlock()
	directory := t.TempDir()
	bridge := acpBridge(t, directory, `[{"id":"first","name":"First"},{"id":"second","name":"Second"}]`)
	missing := "missing"
	_, err := signIn(t.Context(), core.HarnessKindCodex, directory, bridge, nil, bridge, acpHome(directory), &missing)
	if err == nil {
		t.Fatal("an unknown method is refused")
	}
	if message := err.Error(); !strings.Contains(message, "missing") || !strings.Contains(message, "first, second") {
		t.Fatalf("%q", message)
	}
}

func TestABridgeWithoutMethodsIsUnavailable(t *testing.T) {
	acpLoginTestLock.Lock()
	defer acpLoginTestLock.Unlock()
	directory := t.TempDir()
	bridge := acpBridge(t, directory, "[]")
	outcome, err := signIn(t.Context(), core.HarnessKindCodex, directory, bridge, nil, bridge, acpHome(directory), nil)
	if err != nil {
		t.Fatal(err)
	}
	if unavailable, ok := outcome.(acpLoginOutcomeUnavailable); !ok || !strings.Contains(unavailable.Value, "no login methods") {
		t.Fatalf("%#v", outcome)
	}
}

func TestAServerRequestSharingTheResponseIDIsNotMistakenForALogin(t *testing.T) {
	acpLoginTestLock.Lock()
	defer acpLoginTestLock.Unlock()
	directory := t.TempDir()
	response, err := jsonvalue.Marshal(jsonvalue.Object{
		{Key: "jsonrpc", Value: "2.0"},
		{Key: "id", Value: int64(1)},
		{Key: "result", Value: jsonvalue.Object{
			{Key: "protocolVersion", Value: int64(1)},
			{Key: "authMethods", Value: []any{jsonvalue.Object{{Key: "id", Value: "chat-gpt"}, {Key: "name", Value: "ChatGPT"}}}},
		}},
	})
	if err != nil {
		t.Fatal(err)
	}
	bridge := acpWriteScript(directory, "bridge.sh", fmt.Sprintf("#!/bin/sh\n"+
		"read -r line\n"+
		"printf '%%s\\n' '%s'\n"+
		"read -r line\n"+
		"printf '%%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"elicitation/create\",\"params\":{}}'\n"+
		"sleep 5\n", response))
	_, err = signIn(t.Context(), core.HarnessKindCodex, directory, bridge, nil, bridge, acpHome(directory), nil)
	if err == nil || !strings.Contains(err.Error(), "elicitation/create") {
		t.Fatalf("%v", err)
	}
}

// An empty list is what makes login fall back to the harness CLI; this pins
// the wording callers rely on.
func TestEmptyMethodSelectionNamesTheReason(t *testing.T) {
	_, err := selectAuthMethod(nil, nil)
	if err == nil || !strings.Contains(err.Error(), "no login methods") {
		t.Fatalf("%v", err)
	}
}

func TestOpencodeTerminalFallbackUsesThePinnedEntrypoint(t *testing.T) {
	description := "Run `opencode auth login` in the terminal"
	method := authMethod{
		id:          "opencode-login",
		name:        "Login with opencode",
		description: &description,
	}
	command, err := method.terminalCommand(core.HarnessKindOpenCode, "/home/opencode", "/pinned/opencode", map[string]string{})
	if err != nil {
		t.Fatal(err)
	}
	if command == nil || command.command != "/pinned/opencode" || !slices.Equal(command.args, []string{"auth", "login", "--standalone"}) {
		t.Fatalf("%#v", command)
	}
}
