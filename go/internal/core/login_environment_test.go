// Port of the tests in mj-harness-install/src/login_environment.rs (Unix only).

package core

import (
	"maps"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"testing"
	"time"
)

func skipUnlessUnix(t *testing.T) {
	t.Helper()
	if runtime.GOOS == "windows" {
		t.Skip("Unix only")
	}
}

// Hard-won: af22566c: rebuilding the environment from login exports dropped required container image variables
func TestImageOverlayCarriesAbsentImageVariablesOnly(t *testing.T) {
	skipUnlessUnix(t)
	discovered := LoginEnvironmentEnvironment{
		"PATH": "/login/bin:/usr/bin",
		"HOME": "/home/hel",
	}
	ambient := [][2]string{
		// Image ENV a profile script never re-exported: carried.
		{"RUSTUP_HOME", "/usr/local/rustup"},
		{"PLAYWRIGHT_BROWSERS_PATH", "/ms-playwright"},
		// The login shell already built PATH and HOME: left untouched.
		{"PATH", "/image/only"},
		{"HOME", "/root"},
		// Launcher, shell, and credential noise: never carried.
		{"MJ_DISCOVER_LOGIN_PATH", "1"},
		{"SHLVL", "3"},
		{"GH_TOKEN", "secret"},
	}

	OverlayImageEnvironment(discovered, func(yield func(string, string) bool) {
		for _, pair := range ambient {
			if !yield(pair[0], pair[1]) {
				return
			}
		}
	})

	want := map[string]string{
		"RUSTUP_HOME":              "/usr/local/rustup",
		"PLAYWRIGHT_BROWSERS_PATH": "/ms-playwright",
		"PATH":                     "/login/bin:/usr/bin",
		"HOME":                     "/home/hel",
	}
	for key, value := range want {
		if discovered[key] != value {
			t.Errorf("%s = %q, want %q", key, discovered[key], value)
		}
	}
	for _, key := range []string{"MJ_DISCOVER_LOGIN_PATH", "SHLVL", "GH_TOKEN"} {
		if _, ok := discovered[key]; ok {
			t.Errorf("%s carried", key)
		}
	}
}

func loginEnvironmentAccount(home string) Account {
	return Account{user: "fixture-user", home: home, shell: filepath.Join(home, "login shell")}
}

func loginEnvironmentFixture(t *testing.T) string {
	t.Helper()
	home := t.TempDir()
	// Link to the checked-in fixture shell rather than writing one here.
	// Other tests spawn processes concurrently; a fork while our write
	// descriptor is still open leaves a child holding it, and the exec then
	// fails with ETXTBSY. The name keeps its space so the tests still cover
	// quoting of the shell path.
	fixtureShell, err := filepath.Abs(filepath.Join("testdata", "login_environment", "login-shell.sh"))
	if err != nil {
		t.Fatal(err)
	}
	if info, err := os.Stat(fixtureShell); err != nil || !info.Mode().IsRegular() {
		t.Fatalf("fixture login shell is missing at %s", fixtureShell)
	}
	if err := os.Symlink(fixtureShell, filepath.Join(home, "login shell")); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, ".profile"), []byte("export PROFILE_SETTING='profile value'\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	return home
}

func TestLoginCaptureIgnoresParentExportsAndPreservesProfileExports(t *testing.T) {
	skipUnlessUnix(t)
	const homeKey = "MJ_TEST_LOGIN_HOME"
	pollution := []string{
		"CARGO_TARGET_DIR",
		"RUSTFLAGS",
		"LD_LIBRARY_PATH",
		"PYTHONPATH",
		"VIRTUAL_ENV",
		"NODE_OPTIONS",
		"GIT_DIR",
		"CODEX_HOME",
		"MJ_DEV_RESTART_STALE_DAEMON",
		"ARBITRARY_PARENT_EXPORT",
	}
	if home, ok := os.LookupEnv(homeKey); ok {
		environment, err := discoverAccount(loginEnvironmentAccount(home), 5*time.Second)
		if err != nil {
			t.Fatal(err)
		}
		for _, key := range pollution {
			if _, ok := environment[key]; ok {
				t.Errorf("parent export survived: %s", key)
			}
		}
		if environment["PROFILE_SETTING"] != "profile value" {
			t.Errorf("PROFILE_SETTING %q", environment["PROFILE_SETTING"])
		}
		if environment["MULTILINE"] != "first\nsecond=third" {
			t.Errorf("MULTILINE %q", environment["MULTILINE"])
		}
		if len(environment["LARGE"]) != 80_000 {
			t.Errorf("LARGE has %d bytes", len(environment["LARGE"]))
		}
		if environment["USER"] != "fixture-user" {
			t.Errorf("USER %q", environment["USER"])
		}
		return
	}
	home := loginEnvironmentFixture(t)
	profile := "printf 'startup chatter\\n'\nexport PROFILE_SETTING='profile value'\nexport MULTILINE='first\nsecond=third'\nexport LARGE='" + strings.Repeat("x", 80_000) + "'\n"
	if err := os.WriteFile(filepath.Join(home, ".profile"), []byte(profile), 0o644); err != nil {
		t.Fatal(err)
	}
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	command := NewCommandSpec(executable, []string{
		"-test.run=^TestLoginCaptureIgnoresParentExportsAndPreservesProfileExports$",
		"-test.v",
	})
	command.Env[homeKey] = home
	for _, key := range pollution {
		command.Env[key] = "parent-only-value"
	}
	output, err := NewBoundedProcessExecutor(15 * time.Second).Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("%s%s", output.Stdout, output.Stderr)
	}
}

func TestCaptureRejectsMissingOrTruncatedFramesWithoutEchoingValues(t *testing.T) {
	skipUnlessUnix(t)
	marker := []byte("\x00marker\x00")
	for _, output := range []string{
		"secret",
		"\x00marker\x00PATH=secret",
		"\x00marker\x00secret\x00",
		"\x00marker\x00=secret\x00",
		"\x00marker\x00OTHER=secret\x00",
	} {
		_, err := loginEnvironmentParse([]byte(output), marker)
		if err == nil || strings.Contains(err.Error(), "secret") {
			t.Fatalf("%q: %v", output, err)
		}
	}
	environment, err := loginEnvironmentParse([]byte("greeting\n\x00marker\x00PATH=/bin\x00EMPTY=\x00VALUE=a=b\nline\x00"), marker)
	if err != nil {
		t.Fatal(err)
	}
	if value, ok := environment["EMPTY"]; !ok || value != "" {
		t.Fatalf("EMPTY %q %v", value, ok)
	}
	if environment["VALUE"] != "a=b\nline" {
		t.Fatalf("VALUE %q", environment["VALUE"])
	}
}

func TestShellEnvironmentIncludesInteractiveStartupExports(t *testing.T) {
	skipUnlessUnix(t)
	home := loginEnvironmentFixture(t)
	if err := os.WriteFile(filepath.Join(home, ".shellrc"), []byte("export INTERACTIVE_SETTING='from shellrc'\nexport PATH=\"/interactive/bin:$PATH\"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	environment, err := discoverAccount(loginEnvironmentAccount(home), 5*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if environment["INTERACTIVE_SETTING"] != "from shellrc" {
		t.Fatalf("INTERACTIVE_SETTING %q", environment["INTERACTIVE_SETTING"])
	}
	if !strings.HasPrefix(environment["PATH"], "/interactive/bin:") {
		t.Fatal(environment["PATH"])
	}
}

func TestShellProbeUsesEachShellFamilysLoginForm(t *testing.T) {
	skipUnlessUnix(t)
	csh, err := loginShellCommand("/bin/tcsh", "capture")
	if err != nil {
		t.Fatal(err)
	}
	if csh.command.arg0 == nil || *csh.command.arg0 != "-tcsh" {
		t.Fatalf("arg0 %v", csh.command.arg0)
	}
	if !slices.Equal(csh.command.Args, []string{"-i", "-s"}) || !csh.scriptOnStdin {
		t.Fatalf("csh %q %v", csh.command.Args, csh.scriptOnStdin)
	}

	posix, err := loginShellCommand("/bin/zsh", "capture")
	if err != nil {
		t.Fatal(err)
	}
	if posix.command.arg0 != nil {
		t.Fatalf("arg0 %v", *posix.command.arg0)
	}
	if !slices.Equal(posix.command.Args, []string{"-l", "-i", "-c", "capture"}) || posix.scriptOnStdin {
		t.Fatalf("posix %q %v", posix.command.Args, posix.scriptOnStdin)
	}
}

// Hard-won: 27b534a5: tcsh rejected the probe's POSIX -l option before it ran any startup file
// Hard-won: 90fe3a484: tcsh left prompt unset during .cshrc when the probe used -i -c
func TestCshLoginProbeUsesLoginArgv0AndReadsCshStartupFiles(t *testing.T) {
	skipUnlessUnix(t)
	shell, ok := os.LookupEnv("MJ_TEST_CSH")
	if !ok {
		for _, candidate := range []string{"/bin/csh", "/bin/tcsh", "/usr/bin/csh", "/usr/bin/tcsh"} {
			if info, err := os.Stat(candidate); err == nil && info.Mode().IsRegular() {
				shell, ok = candidate, true
				break
			}
		}
	}
	if !ok {
		return
	}
	home := t.TempDir()
	if err := os.WriteFile(filepath.Join(home, ".cshrc"), []byte("if ( $?prompt ) then\n"+
		"setenv CSH_RC_SETTING from-cshrc\n"+
		"else\n"+
		"setenv CSH_RC_SETTING prompt-not-set\n"+
		"endif\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, ".login"), []byte("setenv CSH_LOGIN_SETTING from-login\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	environment, err := discoverAccount(Account{user: "fixture-user", home: home, shell: shell}, 5*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if environment["CSH_RC_SETTING"] != "from-cshrc" || environment["CSH_LOGIN_SETTING"] != "from-login" {
		t.Fatalf("%v", maps.Collect(maps.All(environment)))
	}
}

func TestFailedLoginNeverReturnsAnAmbientEnvironment(t *testing.T) {
	skipUnlessUnix(t)
	home := loginEnvironmentFixture(t)
	if err := os.WriteFile(filepath.Join(home, ".profile"), []byte("echo secret >&2\nexit 42\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	// The deadline was raised to 600 seconds for an intermittent failure
	// that was really ETXTBSY on the written fixture shell, not a timeout.
	_, err := discoverAccount(loginEnvironmentAccount(home), 5*time.Second)
	if err == nil {
		t.Fatal("login succeeded")
	}
	if !strings.Contains(err.Error(), "42") || strings.Contains(err.Error(), "secret") {
		t.Fatal(err)
	}
}

func TestCaptureDeadlineIncludesDescendantsHoldingOutputPipes(t *testing.T) {
	skipUnlessUnix(t)
	home := loginEnvironmentFixture(t)
	if err := os.WriteFile(filepath.Join(home, ".profile"), []byte("sleep 60 &\nprintf '%s' \"$!\" > \"$HOME/child.pid\"\nexit 0\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	started := time.Now()
	_, err := discoverAccount(loginEnvironmentAccount(home), 250*time.Millisecond)
	if err == nil || !strings.Contains(err.Error(), "did not answer") {
		t.Fatalf("%v", err)
	}
	if time.Since(started) >= 3*time.Second {
		t.Fatalf("took %v", time.Since(started))
	}
}
