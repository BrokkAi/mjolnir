// Port of the tests in mj-build-cache/src/mbx/install.rs, with its test-only
// install_mbx_with.

package buildcache

import (
	"errors"
	"fmt"
	"io"
	"maps"
	"net"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// installMbxWith installs with a home and download chosen by the test.
func installMbxWith(machine core.Machine, executor core.CommandExecutor, homeOverride *string, getBinary func(triple string) (string, error)) (MbxInstallResult, error) {
	prepared, err := prepareMbxWith(machine, executor, homeOverride, getBinary)
	if err != nil {
		return MbxInstallResult{}, err
	}
	defer prepared.drop()
	return prepared.publish(executor)
}

// fakeInstallExecutor answers the installer's commands by purpose.
type fakeInstallExecutor struct {
	mu           sync.Mutex
	commands     []core.CommandSpec
	probeAnswers []core.CommandOutput
	setupError   *string
}

func fakeWithProbes(probes ...core.CommandOutput) *fakeInstallExecutor {
	return &fakeInstallExecutor{probeAnswers: probes}
}

func fakeFailingSetup(probes ...core.CommandOutput) *fakeInstallExecutor {
	fake := fakeWithProbes(probes...)
	fake.setupError = stringPointer("mbx setup failed")
	return fake
}

func (f *fakeInstallExecutor) recorded() []core.CommandSpec {
	f.mu.Lock()
	defer f.mu.Unlock()
	return slices.Clone(f.commands)
}

func (f *fakeInstallExecutor) executor() *funcExecutor {
	return &funcExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		f.mu.Lock()
		defer f.mu.Unlock()
		f.commands = append(f.commands, *command)
		switch command.Purpose {
		case "detect build cache host platform":
			return output(0, "Linux x86_64\n", ""), nil
		case "detect mbx host architecture":
			return output(0, "x86_64\n", ""), nil
		case "read the container host mbx version":
			if len(f.probeAnswers) == 0 {
				panic("a probe answer was configured")
			}
			answer := f.probeAnswers[0]
			f.probeAnswers = f.probeAnswers[1:]
			return answer, nil
		case "run mbx setup on the container host":
			if f.setupError != nil {
				return output(7, "", *f.setupError), nil
			}
			return output(0, "", ""), nil
		case "resolve the host login profile for the mbx PATH block":
			return output(0, "preview\n~/.bash_profile\nposix\n/home/build/.local/share/mbx/bin", ""), nil
		case "add the mbx PATH block to the container host login profile":
			return output(0, "changed\n~/.bash_profile\nposix\n/home/build/.local/share/mbx/bin", ""), nil
		}
		return output(0, "", ""), nil
	}}
}

func localMachine() core.Machine { return core.MachineLocal{} }

func sshMachine() core.Machine {
	return core.MachineSsh{
		SSH:              core.SshConnection{Host: "builder.example.test", User: stringPointer("build")},
		WorkspacePrefix:  ".local/share/mjolnir/workspaces",
		TargetBuildCache: &core.TargetBuildCache{},
	}
}

func absentThen(path, version string) []core.CommandOutput {
	return []core.CommandOutput{
		output(1, "", ""),
		output(0, path+"\nmbx "+version+"\n", ""),
	}
}

func cachePreview(nativeMbx *string, offReason core.BuildCacheOff) *core.BuildCachePreview {
	return &core.BuildCachePreview{
		NativeMbx:      nativeMbx,
		MbxProfileFile: stringPointer("~/.profile"),
		Application:    core.BuildCacheApplicationPending{},
		OffReason:      offReason,
	}
}

func installKindText(preview *core.BuildCachePreview) string {
	kind, ok := InstallKind(preview)
	switch {
	case !ok:
		return "none"
	case kind == MbxInstallKindInstall:
		return "install"
	default:
		return "upgrade"
	}
}

func TestInstallOfferTracksAbsentOldCompatibleAndUnsupportedHosts(t *testing.T) {
	for _, check := range []struct {
		preview *core.BuildCachePreview
		want    string
	}{
		{cachePreview(nil, nil), "install"},
		{cachePreview(stringPointer("1.21.0"), nil), "upgrade"},
		{cachePreview(stringPointer("1.22.0"), nil), "upgrade"},
		{cachePreview(stringPointer(MbxVersion), nil), "upgrade"},
	} {
		if got := installKindText(check.preview); got != check.want {
			t.Fatalf("%s: got %s, want %s", optionalText(check.preview.NativeMbx), got, check.want)
		}
	}
	external := cachePreview(stringPointer("1.22.0"), nil)
	external.MbxProfileFile = nil
	if got := installKindText(external); got != "none" {
		t.Fatal(got)
	}
	// A Mac reports its own older mbx, which the Linux release cannot
	// replace, as well as having none.
	for _, native := range []*string{nil, stringPointer("1.16.0")} {
		if got := installKindText(cachePreview(native, core.BuildCacheOffUnsupportedPlatform{Value: "macOS"})); got != "none" {
			t.Fatal(got)
		}
	}
}

func TestDestinationInstallsAbsentMbxAndReplacesOnlyManagedPaths(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	home := "/home/build"
	selected, err := destination(home, nil)
	if err != nil {
		t.Fatal(err)
	}
	if selected != (installDestination{supported: true, program: "/home/build/.local/bin/mbx", kind: MbxInstallKindInstall}) {
		t.Fatalf("%#v", selected)
	}
	for _, path := range []string{"/home/build/.local/bin/mbx", "/home/build/.cargo/bin/mbx"} {
		selected, err := destination(home, &NativeMbx{Program: path, Version: "1.21.0"})
		if err != nil {
			t.Fatal(err)
		}
		if selected != (installDestination{supported: true, program: path, kind: MbxInstallKindUpgrade}) {
			t.Fatalf("%#v", selected)
		}
	}
	mise := "/home/build/.local/share/mise/installs/mbx/1.21/bin/mbx"
	unsupported, err := destination(home, &NativeMbx{Program: mise, Version: "1.21.0"})
	if err != nil {
		t.Fatal(err)
	}
	if unsupported != (installDestination{program: mise}) {
		t.Fatalf("%#v", unsupported)
	}
}

func TestLocalInstallCopiesAtomicallyRunsSetupAndVerifiesTheVersion(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	temp := t.TempDir()
	home := filepath.Join(temp, "home with ' quotes")
	mkdirAll(t, home)
	binary := filepath.Join(temp, "downloaded-mbx")
	if err := os.WriteFile(binary, []byte("recorded mbx executable"), 0o666); err != nil {
		t.Fatal(err)
	}
	program := filepath.Join(home, ".local/bin/mbx")
	fake := fakeWithProbes(absentThen(program, MbxVersion)...)

	result, err := installMbxWith(localMachine(), fake.executor(), &home, func(triple string) (string, error) {
		if triple != "x86_64" {
			t.Errorf("triple = %s", triple)
		}
		return binary, nil
	})
	if err != nil {
		t.Fatal(err)
	}

	if result.Program != program || result.Version != MbxVersion || result.Kind != MbxInstallKindInstall {
		t.Fatalf("%#v", result)
	}
	if !result.ProfileChanged || result.ProfileFile != "~/.bash_profile" {
		t.Fatalf("%#v", result)
	}
	if readText(t, program) != "recorded mbx executable" {
		t.Fatal("the installed program differs from the download")
	}
	info, err := os.Stat(program)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm() != 0o755 {
		t.Fatalf("mode = %o", info.Mode().Perm())
	}
	commands := fake.recorded()
	index := slices.IndexFunc(commands, func(command core.CommandSpec) bool {
		return command.Purpose == "run mbx setup on the container host"
	})
	if index < 0 {
		t.Fatal("setup never ran")
	}
	setup := commands[index]
	if setup.Program != program || !slices.Equal(setup.Args, []string{"setup", "--yes"}) {
		t.Fatalf("%#v", setup)
	}
	if !slices.ContainsFunc(commands, func(command core.CommandSpec) bool {
		return command.Purpose == "add the mbx PATH block to the container host login profile"
	}) {
		t.Fatal("the profile was not updated")
	}
}

func TestSshInstallUsesScpToAHomeRelativeSiblingThenRenamesAtomically(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	temp := t.TempDir()
	binary := filepath.Join(temp, "downloaded-mbx")
	if err := os.WriteFile(binary, []byte("recorded mbx executable"), 0o666); err != nil {
		t.Fatal(err)
	}
	home := "/remote home/O'Neil"
	program := pathJoin(home, ".local/bin/mbx")
	fake := fakeWithProbes(absentThen(program, MbxVersion)...)

	result, err := installMbxWith(sshMachine(), fake.executor(), &home, func(string) (string, error) { return binary, nil })
	if err != nil {
		t.Fatal(err)
	}

	if result.Program != program {
		t.Fatal(result.Program)
	}
	commands := fake.recorded()
	find := func(purpose string) core.CommandSpec {
		t.Helper()
		index := slices.IndexFunc(commands, func(command core.CommandSpec) bool { return command.Purpose == purpose })
		if index < 0 {
			t.Fatalf("no command for %s", purpose)
		}
		return commands[index]
	}
	upload := find("upload the pinned mbx binary to the SSH host")
	if upload.Program != "scp" {
		t.Fatal(upload.Program)
	}
	if upload.SSHSession == nil {
		t.Fatal("the upload does not share the SSH connection")
	}
	if !slices.ContainsFunc(upload.Args, func(argument string) bool {
		return strings.HasPrefix(argument, "build@builder.example.test:.local/bin/.mbx.mjolnir-") && strings.HasSuffix(argument, ".tmp")
	}) {
		t.Fatalf("%q", upload.Args)
	}
	quoted := `'/remote home/O'\''Neil/.local/bin/mbx'`
	publish := find("publish the mbx binary atomically on the SSH host")
	last := publish.Args[len(publish.Args)-1]
	if !strings.Contains(last, "chmod 755") || !strings.Contains(last, "mv -f") || !strings.Contains(last, quoted) {
		t.Fatal(last)
	}
	if !slices.ContainsFunc(commands, func(command core.CommandSpec) bool {
		return command.Purpose == "run mbx setup on the container host" &&
			strings.Contains(command.Args[len(command.Args)-1], quoted)
	}) {
		t.Fatal("setup did not run the installed program")
	}
}

func TestSetupFailureSurfacesStderrAndStopsBeforeProfileChanges(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	temp := t.TempDir()
	home := filepath.Join(temp, "home")
	mkdirAll(t, home)
	binary := filepath.Join(temp, "downloaded-mbx")
	if err := os.WriteFile(binary, []byte("recorded mbx executable"), 0o666); err != nil {
		t.Fatal(err)
	}
	program := filepath.Join(home, ".local/bin/mbx")
	fake := fakeFailingSetup(output(1, "", ""), output(0, program+"\nmbx 1.21.0\n", ""))

	_, err := installMbxWith(localMachine(), fake.executor(), &home, func(string) (string, error) { return binary, nil })
	if err == nil || !strings.Contains(alternate(err), "mbx setup failed") {
		t.Fatalf("error = %v", err)
	}
	if slices.ContainsFunc(fake.recorded(), func(command core.CommandSpec) bool {
		return command.Purpose == "add the mbx PATH block to the container host login profile"
	}) {
		t.Fatal("the profile changed after setup failed")
	}
}

func TestMiseInstallRefusalHappensBeforeDownloadOrHostMutation(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	home := "/home/build"
	fake := fakeWithProbes(output(0, "/home/build/.local/share/mise/installs/mbx/1.21/bin/mbx\nmbx 1.21.0\n", ""))
	downloaded := false

	_, err := installMbxWith(localMachine(), fake.executor(), &home, func(string) (string, error) {
		downloaded = true
		return "", errors.New("download should not run")
	})
	if err == nil || !strings.Contains(alternate(err), "mise upgrade") {
		t.Fatalf("error = %v", err)
	}
	if downloaded {
		t.Fatal("the download ran")
	}
	if !all(fake.recorded(), func(command core.CommandSpec) bool {
		return !strings.HasPrefix(command.Purpose, "run mbx setup") && !strings.HasPrefix(command.Purpose, "add the mbx PATH")
	}) {
		t.Fatal("the host was changed")
	}
}

// Hard-won: #1514: disposable staging discarded verified binaries needed for offline repair.
func TestVerifiedDownloadCacheSurvivesStagingCleanupAndSupportsOfflineRetry(t *testing.T) {
	const child = "MJ_MBX_OFFLINE_CACHE_TEST"
	if _, ok := os.LookupEnv(child); !ok {
		root := t.TempDir()
		targets.NewIsolatedTest(targets.TestName(
			"mj_build_cache::mbx::install::tests",
			"TestVerifiedDownloadCacheSurvivesStagingCleanupAndSupportsOfflineRetry",
		)).
			Env(child, "1").
			Env("MJ_INSTANCE", "mbx-offline-cache").
			IsolatedStore(root).
			Run()
		return
	}
	destination := pathJoin(pathJoin(pathJoin(pathJoin(core.DataDir(), "mbx"), MbxVersion), "x86_64"), "mbx")
	contents := []byte("recorded verified mbx executable")
	if _, err := retainVerifiedDownload(destination, contents); err != nil {
		t.Fatal(err)
	}
	// The real download entry point must reuse its durable cache with no
	// runtime or network.
	if !SetMbxTestReleaseURL("http://127.0.0.1:0/offline") {
		t.Fatal("the test release URL was already set")
	}
	cached, err := PinnedLinuxBinary("x86_64", core.ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	if cached != destination {
		t.Fatal(cached)
	}
	if readText(t, cached) != string(contents) {
		t.Fatal("the cached binary changed")
	}
}

// Hard-won: #1514: a short profile append left an open function that another append could not repair.
func TestShortProfileAppendRepairsOnlyItsOwnTailOnRetry(t *testing.T) {
	requireUnix(t)
	root := t.TempDir()
	home := filepath.Join(root, "home")
	tools := filepath.Join(root, "tools")
	mkdirAll(t, home, tools)
	for _, link := range [][2]string{
		{"sh", "/bin/sh"},
		{"cat", "/bin/cat"},
		{"wc", "/usr/bin/wc"},
		{"awk", "/usr/bin/awk"},
		{"dd", "/bin/dd"},
		{"mkdir", "/bin/mkdir"},
	} {
		symlink(t, link[1], filepath.Join(tools, link[0]))
	}
	target := filepath.Join(root, "profile-target")
	profile := filepath.Join(home, ".profile")
	original := []byte("# keep user bytes unchanged\n")
	if err := os.WriteFile(target, original, 0o666); err != nil {
		t.Fatal(err)
	}
	symlink(t, target, profile)
	binaryDir := filepath.Join(home, ".local/bin")
	shimDir := filepath.Join(home, "data/mbx/bin")
	block := posixProfileBlock(binaryDir, shimDir)
	fish := fishProfileBlock(binaryDir, shimDir)
	executor := core.ProcessExecutor{}
	lock, err := lockLocalMbx(home, executor)
	if err != nil {
		t.Fatal(err)
	}
	defer lock.release()
	appendCommand := core.NewCommandSpec("/bin/sh", []string{"-c", profileScript, "mj-profile", "update", binaryDir, block, fish, ""})
	appendCommand.Env = map[string]string{
		"HOME":          home,
		"SHELL":         "/bin/sh",
		"PATH":          tools,
		"XDG_DATA_HOME": filepath.Join(home, "data"),
	}
	limited := appendCommand
	limited.Args = append([]string{
		"-c",
		"import os,resource,signal,sys; resource.setrlimit(resource.RLIMIT_FSIZE,(int(sys.argv[1]),int(sys.argv[1]))); signal.signal(signal.SIGXFSZ,signal.SIG_IGN); os.execv(sys.argv[2],sys.argv[2:])",
		strconv.Itoa(len(original) + 70),
		limited.Program,
	}, appendCommand.Args...)
	limited.Program = "/usr/bin/python3"
	failed := mustRun(t, limited)
	if failed.Status == 0 {
		t.Fatal("the limited append succeeded")
	}
	failure := string(failed.Stderr)
	if !strings.Contains(failure, profile) || !strings.Contains(failure, "I/O error") {
		t.Fatal(failure)
	}
	partial, err := os.ReadFile(target)
	if err != nil {
		t.Fatal(err)
	}
	if len(partial) != len(original)+70 {
		t.Fatalf("partial length = %d", len(partial))
	}
	repaired := mustRun(t, appendCommand)
	if repaired.Status != 0 {
		t.Fatal(lossy(repaired.Stderr))
	}
	if info, err := os.Lstat(profile); err != nil || info.Mode()&os.ModeSymlink == 0 {
		t.Fatal("the profile is no longer a symlink")
	}
	body := readText(t, target)
	if !strings.HasPrefix(body, string(original)) {
		t.Fatal(body)
	}
	if strings.Count(body, "# >>> mbx (added by Mjolnir) >>>") != 1 || strings.Count(body, "# <<< mbx <<<") != 1 {
		t.Fatal(body)
	}
	if !strings.Contains(body, block) {
		t.Fatal(body)
	}
	syntax := mustRun(t, core.NewCommandSpec("/bin/sh", []string{"-n", profile}))
	if syntax.Status != 0 {
		t.Fatal(lossy(syntax.Stderr))
	}
	if again := mustRun(t, appendCommand); again.Status != 0 {
		t.Fatal(lossy(again.Stderr))
	}
	if readText(t, target) != body {
		t.Fatal("a repeated update changed the profile")
	}
	external := append(slices.Clone(partial), []byte("\nexport OTHER_CONTENT=preserve\n")...)
	if err := os.WriteFile(target, external, 0o666); err != nil {
		t.Fatal(err)
	}
	refused := mustRun(t, appendCommand)
	if refused.Status == 0 {
		t.Fatal("an incomplete block followed by other content was rewritten")
	}
	refusal := string(refused.Stderr)
	if !strings.Contains(refusal, profile) || !strings.Contains(refusal, "line 3") || !strings.Contains(refusal, "Delete only bytes") {
		t.Fatal(refusal)
	}
	if readText(t, target) != string(external) {
		t.Fatal("the refused update changed the profile")
	}
}

// Hard-won: #1514: interruption after publishing mbx must be repaired by rerunning Install.
// This fixture installs on the local host; pinned mbx installs require Linux.
func TestInterruptedPublicationRerunConvergesWithoutAdmission(t *testing.T) {
	requireLinux(t)
	fixture := func(executor core.CancellableProcessExecutor, home string) *funcExecutor {
		return &funcExecutor{
			execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
				copied := *command
				copied.Env = maps.Clone(command.Env)
				if copied.Env == nil {
					copied.Env = map[string]string{}
				}
				copied.Env["HOME"] = home
				copied.Env["PATH"] = filepath.Join(home, ".local/bin") + ":/usr/bin:/bin"
				copied.Env["XDG_DATA_HOME"] = filepath.Join(home, "data")
				copied.Env["XDG_CONFIG_HOME"] = filepath.Join(home, "config")
				copied.Args = slices.Clone(command.Args)
				// Read the recorded fixture through Python: concurrent test
				// forks can inherit a staging writer and make script exec
				// return ETXTBSY.
				switch copied.Purpose {
				case "run mbx setup on the container host":
					copied.Args = append([]string{copied.Program}, copied.Args...)
					copied.Program = "/usr/bin/python3"
				case "read the container host mbx version":
					for index, argument := range copied.Args {
						copied.Args[index] = strings.ReplaceAll(argument, `"$resolved" --version`, `/usr/bin/python3 "$resolved" --version`)
					}
				}
				return executor.Execute(&copied)
			},
			cancelled: executor.IsCancelled,
		}
	}
	root := t.TempDir()
	home := filepath.Join(root, "home")
	mkdirAll(t, home)
	marker := filepath.Join(home, "setup-started")
	ready, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer ready.Close()
	binary := filepath.Join(root, "recorded-mbx")
	script := fmt.Sprintf(`#!/usr/bin/python3
import os, socket, sys
if '--version' in sys.argv:
    print('mbx %s')
else:
    marker = os.path.join(os.environ['HOME'], 'setup-started')
    if not os.path.exists(marker):
        open(marker, 'w').close()
        sys.stdout.buffer.write(b'x' * 131072)
        sys.stdout.buffer.flush()
        with socket.create_connection(('127.0.0.1', %d)) as ready:
            ready.sendall(b'R')
            ready.recv(1)
`, MbxVersion, ready.Addr().(*net.TCPAddr).Port)
	writeExecutable(t, binary, script)
	var cancelled atomic.Bool
	executor := fixture(core.NewCancellableProcessExecutor(&cancelled), home)
	installDone := make(chan error, 1)
	go func() {
		_, err := installMbxWith(localMachine(), executor, &home, func(string) (string, error) { return binary, nil })
		installDone <- err
	}()
	if err := ready.(*net.TCPListener).SetDeadline(time.Now().Add(30 * time.Second)); err != nil {
		t.Fatal(err)
	}
	signal, err := ready.Accept()
	if err != nil {
		t.Fatal(err)
	}
	defer signal.Close()
	readByte := func() (byte, error) {
		var buffer [1]byte
		if err := signal.SetReadDeadline(time.Now().Add(30 * time.Second)); err != nil {
			return 0, err
		}
		_, err := io.ReadFull(signal, buffer[:])
		return buffer[0], err
	}
	if value, err := readByte(); err != nil || value != 'R' {
		t.Fatalf("signal = %q, %v", value, err)
	}
	program := filepath.Join(home, ".local/bin/mbx")
	if readText(t, program) != readText(t, binary) {
		t.Fatal("the published program differs from the download")
	}
	if info, err := os.Stat(marker); err != nil || !info.Mode().IsRegular() {
		t.Fatal("setup did not start")
	}
	if slices.ContainsFunc(core.UpgradeActiveLabels(), func(label string) bool { return strings.Contains(label, "mbx installation") }) {
		t.Fatal("mbx installation holds daemon admission")
	}
	cancelled.Store(true)
	if err := <-installDone; err == nil {
		t.Fatal("the cancelled install succeeded")
	}
	if _, err := readByte(); !errors.Is(err, io.EOF) {
		t.Fatalf("setup survived its cancelled install: %v", err)
	}

	results := make([]MbxInstallResult, 2)
	failures := make([]error, 2)
	var reruns sync.WaitGroup
	for index := range 2 {
		reruns.Add(1)
		go func() {
			defer reruns.Done()
			executor := fixture(core.NewCancellableProcessExecutor(new(atomic.Bool)), home)
			results[index], failures[index] = installMbxWith(localMachine(), executor, &home, func(string) (string, error) { return binary, nil })
		}()
	}
	reruns.Wait()
	for index := range 2 {
		if failures[index] != nil {
			t.Fatal(failures[index])
		}
		if results[index].Version != MbxVersion {
			t.Fatal(results[index].Version)
		}
	}
	result := results[0]
	profile := pathJoin(home, strings.TrimPrefix(result.ProfileFile, "~/"))
	body := readText(t, profile)
	if strings.Count(body, "# >>> mbx (added by Mjolnir) >>>") != 1 || strings.Count(body, "# <<< mbx <<<") != 1 {
		t.Fatal(body)
	}
	if !strings.Contains(body, posixProfileBlock(filepath.Dir(program), filepath.Join(home, "data/mbx/bin"))) {
		t.Fatal(body)
	}
	entries, err := os.ReadDir(filepath.Dir(program))
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range entries {
		if strings.HasSuffix(entry.Name(), ".tmp") {
			t.Fatalf("a staged upload remains: %s", entry.Name())
		}
	}
}

// Hard-won: 7f871fd03: inherited fixture writers made the shell probe silently select the wrong profile.
func TestProfileBlockSelectsAndUpdatesTheLoginShellFileIdempotently(t *testing.T) {
	requireUnix(t)
	type profileCase struct {
		name       string
		loginShell string
		existing   []string
		selected   *string
		shellKind  string
	}
	cases := []profileCase{
		{"zsh", "/bin/zsh", []string{".bash_profile"}, stringPointer("~/.zprofile"), "posix"},
		{"bash-profile", "/bin/bash", []string{".bash_profile", ".bash_login"}, stringPointer("~/.bash_profile"), "posix"},
		{"bash-login", "/usr/bin/bash", []string{".bash_login"}, stringPointer("~/.bash_login"), "posix"},
		{"bash-default", "/bin/bash", nil, stringPointer("~/.profile"), "posix"},
		{"unknown", "/usr/bin/nu", []string{".zprofile", ".bash_profile", ".bash_login"}, stringPointer("~/.profile"), "unknown"},
		{"fish", "/usr/bin/fish", nil, nil, "fish"},
	}

	for _, check := range cases {
		temp := t.TempDir()
		home := filepath.Join(temp, "home "+check.name+" with ' quotes")
		tools := filepath.Join(temp, "tools")
		xdgConfig := filepath.Join(temp, "xdg config")
		xdgData := filepath.Join(temp, "xdg data with ' quote")
		binaryDir := filepath.Join(home, ".local/bin")
		mkdirAll(t, home, tools)
		core.InstallFakeCommand(tools, "id", "#!/bin/sh\nprintf 'profile-test-user\\n'\n")
		core.InstallFakeCommand(tools, "getent",
			"#!/bin/sh\nprintf 'profile-test-user:x:1000:1000::/tmp:%s\\n' \"$PROFILE_TEST_LOGIN_SHELL\"\n")
		// A concurrent fork can keep fixture writers open past setup. Only
		// the dispatcher is executed; these files are read by sh.
		var writers []*os.File
		for _, name := range []string{"id", "getent"} {
			writer, err := os.OpenFile(filepath.Join(tools, name+".script"), os.O_WRONLY, 0)
			if err != nil {
				t.Fatal(err)
			}
			writers = append(writers, writer)
		}
		for _, name := range check.existing {
			if err := os.WriteFile(filepath.Join(home, name), []byte("export EXISTING_PROFILE=yes"), 0o666); err != nil {
				t.Fatal(err)
			}
		}
		executor := core.CancellableProcessExecutorWithTimeout(5 * time.Second)
		command := core.NewCommandSpec("/bin/sh", []string{"-c", profileScript, "mj-profile", "preview", binaryDir})
		command.Env = map[string]string{
			"HOME":                     home,
			"PATH":                     tools + ":/usr/bin:/bin",
			"PROFILE_TEST_LOGIN_SHELL": check.loginShell,
			"SHELL":                    "/bin/sh",
			"XDG_CONFIG_HOME":          xdgConfig,
			"XDG_DATA_HOME":            xdgData,
		}
		preview, err := executor.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		selection, err := parseProfileSelection(preview.Stdout)
		if err != nil {
			t.Fatalf("%s: %v (%s)", check.name, err, preview.Stderr)
		}
		selected := filepath.Join(xdgConfig, "fish/conf.d/mbx.fish")
		if check.selected != nil {
			selected = *check.selected
		}
		if selection.file != selected || selection.shellKind != check.shellKind || selection.shimDir != filepath.Join(xdgData, "mbx/bin") {
			t.Fatalf("%s: %#v", check.name, selection)
		}

		posixBlock := posixProfileBlock(binaryDir, selection.shimDir)
		fishBlock := fishProfileBlock(binaryDir, selection.shimDir)
		update := core.NewCommandSpec("/bin/sh", []string{
			"-c", profileScript, "mj-profile", "update", binaryDir, posixBlock, fishBlock,
			"", // The local test caller owns the host lock below.
		})
		update.Env = maps.Clone(command.Env)
		profileLock, err := lockLocalMbx(home, executor)
		if err != nil {
			t.Fatal(err)
		}
		firstUpdate, err := executor.Execute(&update)
		if err != nil {
			t.Fatal(err)
		}
		if got, want := string(firstUpdate.Stdout), "changed\n"+selected+"\n"+check.shellKind+"\n"+selection.shimDir; got != want {
			t.Fatalf("%s: got %q, want %q (%s)", check.name, got, want, firstUpdate.Stderr)
		}
		secondUpdate, err := executor.Execute(&update)
		if err != nil {
			t.Fatal(err)
		}
		if got, want := string(secondUpdate.Stdout), "unchanged\n"+selected+"\n"+check.shellKind+"\n"+selection.shimDir; got != want {
			t.Fatalf("%s: got %q, want %q", check.name, got, want)
		}
		profilePath := selected
		if relative, ok := strings.CutPrefix(selected, "~/"); ok {
			profilePath = filepath.Join(home, relative)
		}
		profile := readText(t, profilePath)
		if slices.ContainsFunc(check.existing, func(name string) bool { return selected == "~/"+name }) &&
			!strings.HasPrefix(profile, "export EXISTING_PROFILE=yes\n# >>> mbx") {
			t.Fatalf("%s: %s", check.name, profile)
		}
		if strings.Count(profile, "# >>> mbx (added by Mjolnir) >>>") != 1 || strings.Count(profile, "# <<< mbx <<<") != 1 {
			t.Fatalf("%s: %s", check.name, profile)
		}
		for _, name := range check.existing {
			if selected != "~/"+name && strings.Contains(readText(t, filepath.Join(home, name)), "# >>> mbx") {
				t.Fatalf("%s: %s was changed", check.name, name)
			}
		}
		if check.shellKind == "unknown" {
			details := profileDetails(&selection, binaryDir)
			if details.Warning == nil || !strings.Contains(*details.Warning, "may not read ~/.profile") {
				t.Fatalf("%s: warning = %s", check.name, optionalText(details.Warning))
			}
			if details.ManualPathLine == nil || !strings.HasPrefix(*details.ManualPathLine, "export PATH='") {
				t.Fatalf("%s: manual line = %s", check.name, optionalText(details.ManualPathLine))
			}
		}
		if check.shellKind == "posix" {
			if !strings.Contains(profile, shellQuote(selection.shimDir)) || !strings.Contains(profile, shellQuote(binaryDir)) ||
				strings.Contains(profile, "$HOME/.local/share/mbx/bin") {
				t.Fatalf("%s: %s", check.name, profile)
			}
		}
		profileLock.release()
		for _, writer := range writers {
			writer.Close()
		}
	}
}

func TestProfileSelectionFallsBackToShellThenSh(t *testing.T) {
	requireUnix(t)
	temp := t.TempDir()
	home := filepath.Join(temp, "home")
	tools := filepath.Join(temp, "tools")
	xdgConfig := filepath.Join(temp, "xdg config")
	xdgData := filepath.Join(temp, "xdg data")
	mkdirAll(t, home, tools)
	writeExecutable(t, filepath.Join(tools, "id"), "#!/bin/sh\nprintf 'test-user\\n'\n")
	writeExecutable(t, filepath.Join(tools, "getent"), "#!/bin/sh\nexit 2\n")
	executor := core.CancellableProcessExecutorWithTimeout(5 * time.Second)
	binaryDir := filepath.Join(home, ".local/bin")
	command := core.NewCommandSpec("/bin/sh", []string{"-c", profileScript, "mj-profile", "preview", binaryDir})
	command.Env = map[string]string{
		"HOME":            home,
		"PATH":            tools + ":/usr/bin:/bin",
		"SHELL":           "/usr/bin/zsh",
		"XDG_CONFIG_HOME": xdgConfig,
		"XDG_DATA_HOME":   xdgData,
	}
	selectedFile := func() string {
		t.Helper()
		result, err := executor.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		selection, err := parseProfileSelection(result.Stdout)
		if err != nil {
			t.Fatal(err)
		}
		return selection.file
	}
	if file := selectedFile(); file != "~/.zprofile" {
		t.Fatal(file)
	}
	command.Env["SHELL"] = ""
	if file := selectedFile(); file != "~/.profile" {
		t.Fatal(file)
	}
}

func TestProfilePreviewAndInstallUseOneSharedRuleScript(t *testing.T) {
	for _, expected := range []string{"getent passwd", "${SHELL:-sh}", `case "$existing" in`, "fish/conf.d/mbx.fish", "XDG_DATA_HOME"} {
		if !strings.Contains(profileScript, expected) {
			t.Fatalf("missing %q", expected)
		}
	}
	directory := "/home/example/.local/bin"
	if !strings.Contains(posixProfileBlock(directory, "/home/example/.local/share/mbx/bin"), "# >>> mbx (added by Mjolnir) >>>") {
		t.Fatal("the POSIX block has no start marker")
	}
	if !strings.Contains(fishProfileBlock(directory, "/home/example/.local/share/mbx/bin"), "# <<< mbx <<<") {
		t.Fatal("the fish block has no end marker")
	}
}

func TestPosixProfileMovesShimAndBinaryDirectoriesAheadOfExistingPathEntries(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	temp := t.TempDir()
	home := filepath.Join(temp, "home")
	shimDir := filepath.Join(temp, "xdg data's/mbx/bin")
	binaryDir := filepath.Join(temp, "local bin's")
	mkdirAll(t, home)
	block := posixProfileBlock(binaryDir, shimDir)
	if err := os.WriteFile(filepath.Join(home, ".profile"), []byte(block), 0o666); err != nil {
		t.Fatal(err)
	}
	initial := "/usr/bin:" + binaryDir + ":/bin:" + shimDir + ":" + binaryDir
	command := core.NewCommandSpec("/bin/sh", []string{"-c", `. "$HOME/.profile"; . "$HOME/.profile"; printf '%s' "$PATH"`})
	command.Env = map[string]string{"HOME": home, "PATH": initial}
	result := mustRun(t, command)
	if got, want := string(result.Stdout), shimDir+":"+binaryDir+":/usr/bin:/bin"; got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestFishProfileIsIdempotentWhenFishIsInstalled(t *testing.T) {
	probeCommand := core.NewCommandSpec("fish", []string{"--version"})
	probe, err := core.ProcessExecutor{}.Execute(&probeCommand)
	if err != nil || probe.Status != 0 {
		return
	}
	temp := t.TempDir()
	home := filepath.Join(temp, "home")
	config := filepath.Join(temp, "xdg config")
	data := filepath.Join(temp, "xdg data")
	binaryDir := filepath.Join(temp, "binary")
	shimDir := filepath.Join(temp, "shim")
	mkdirAll(t, filepath.Join(config, "fish/conf.d"), binaryDir, shimDir)
	file := filepath.Join(config, "fish/conf.d/mbx.fish")
	if err := os.WriteFile(file, []byte(fishProfileBlock(binaryDir, shimDir)), 0o666); err != nil {
		t.Fatal(err)
	}
	path := "/usr/bin:" + binaryDir + ":/bin:" + shimDir + ":" + binaryDir
	fishScript := fmt.Sprintf("source '%s'; source '%s'; string join ':' $PATH", file, file)
	command := core.NewCommandSpec("fish", []string{"-c", fishScript})
	command.Env = map[string]string{"HOME": home, "XDG_CONFIG_HOME": config, "XDG_DATA_HOME": data, "PATH": path}
	result := mustRun(t, command)
	if result.Status != 0 {
		t.Fatal(lossy(result.Stderr))
	}
	if got, want := strings.TrimSpace(string(result.Stdout)), shimDir+":"+binaryDir+":/usr/bin:/bin"; got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestInstallerScriptsCoverAbsoluteCandidatesAndMarkedProfilePaths(t *testing.T) {
	for _, expected := range []string{
		"command -v mbx",
		"$HOME/.local/bin/mbx",
		"$HOME/.cargo/bin/mbx",
		"cd -P",
		`case "$resolved" in /*)`,
	} {
		if !strings.Contains(NativeVersionScript, expected) {
			t.Fatalf("missing %q", expected)
		}
	}
}
