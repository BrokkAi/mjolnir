//go:build unix

// Port of mj-worker-staging/src/worker_binary/tests.rs, part 5: the
// `#[cfg(unix)]` tests of closing a local session, the container harness
// runtime (`mod container_runtime`), digests and local login links.

package workerstaging

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
	"github.com/BrokkAi/mjolnir/go/internal/worker"
)

// A local session's project-memory replica lands in its staged home and
// goes with the session. Closing the session removes the staged home with
// the replica inside, both for a harness staged under the worker root and
// for Muse, whose root lies under the data directory. The profile home it
// was staged from keeps its login and gains no `projects/` directory.
func TestClosingALocalSessionRemovesItsStagedHomeAndMemoryReplica(t *testing.T) {
	const child = "MJ_CLOSE_REMOVES_REPLICA_CHILD"
	if _, ok := os.LookupEnv(child); !ok {
		directory := t.TempDir()
		targets.NewIsolatedTest(targets.TestName(testsModule, "TestClosingALocalSessionRemovesItsStagedHomeAndMemoryReplica")).
			Env(child, "1").
			IsolatedStore(directory).
			Run()
		return
	}

	directory := t.TempDir()
	project := filepath.Join(directory, "project")
	makeFixtureDir(t, project)
	for index, kind := range []core.HarnessKind{core.HarnessKindCodex, core.HarnessKindKimi, core.HarnessKindMuse} {
		sessionID := fmt.Sprintf("%032x", index+1)
		home := filepath.Join(directory, kind.ID()+"-home")
		profile := core.NewHarnessProfile(kind, home)
		marker := profile.AuthenticationMarker()
		writeFixture(t, marker, LoginBytes(kind, 1))
		workerRoot := filepath.Join(directory, "workers", sessionID)
		locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: workerRoot}
		session := store.CheckpointTestSession(sessionID)
		session.HarnessKind = kind
		session.LastProfile = kind.ID()
		session.TargetTemplateID = "localhost"
		session.ProjectDirectory = &project
		session.Target = core.TargetLocatorLocalBare{WorkerRoot: workerRoot}
		settings := core.TargetRuntimeSettingsFromTemplate(core.TargetTemplateLocalBare{})
		config := core.DefaultConfig()
		_, memory, targetHome, err := WorkerLaunchConfig(&session, &profile, nil, &locator,
			LaunchWorkspace{SessionID: sessionID, WorkingDirectories: []string{}}, &settings, &config)
		if err != nil {
			t.Fatal(err)
		}
		if memory == nil {
			t.Fatalf("%v: a session without the opt-out has project memory", kind)
		}
		if !pathStartsWith(memory.Root, targetHome) {
			t.Fatalf("%v: %s is outside %s", kind, memory.Root, targetHome)
		}

		// Stage and install as prepare_worker_files does on this machine.
		canonical := CanonicalMemoryRoot(memory.ProjectKey)
		writeFixture(t, filepath.Join(canonical, "MEMORY.md"), []byte("- a remembered fact\n"))
		stage := t.TempDir()
		if err := StageProfile(&profile, stage); err != nil {
			t.Fatal(err)
		}
		if _, err := StageMemoryReplica(memory, targetHome, stage); err != nil {
			t.Fatal(err)
		}
		makeFixtureDir(t, workerRoot)
		entries, err := os.ReadDir(stage)
		if err != nil {
			t.Fatal(err)
		}
		for _, entry := range entries {
			if err := CopyProfileEntry(filepath.Join(stage, entry.Name()), filepath.Join(targetHome, entry.Name())); err != nil {
				t.Fatal(err)
			}
		}
		if !isFile(filepath.Join(memory.Root, "MEMORY.md")) {
			t.Fatalf("%v: no replica", kind)
		}

		plan, err := targets.ClosePlan(&locator, sessionID)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := plan.Execute(core.ProcessExecutor{}, ControlWorker{}); err != nil {
			t.Fatal(err)
		}

		if fixtureExists(memory.Root) {
			t.Fatalf("%v: the replica goes", kind)
		}
		if fixtureExists(targetHome) {
			t.Fatalf("%v: the staged home goes", kind)
		}
		if fixtureExists(workerRoot) {
			t.Fatalf("%v: the worker root goes", kind)
		}
		if !isFile(marker) {
			t.Fatalf("%v: the profile keeps its login", kind)
		}
		if fixtureExists(filepath.Join(home, "projects")) {
			t.Fatalf("%v: the profile home gained projects/", kind)
		}
		if !isFile(filepath.Join(canonical, "MEMORY.md")) {
			t.Fatal("the canonical project memory outlives the session")
		}
	}
}

// Port of `mod container_runtime`.

func containerExecutable(t *testing.T, path, body string) {
	t.Helper()
	writeFixture(t, path, []byte(body))
	if err := os.Chmod(path, 0o755); err != nil {
		t.Fatal(err)
	}
}

func containerPackage(t *testing.T, root, name, version string) string {
	t.Helper()
	pkg := filepath.Join(root, "node_modules", name)
	command := filepath.Join(pkg, "bin/cli")
	containerExecutable(t, command, fmt.Sprintf("#!/bin/sh\nprintf '%%s\\n' '%s %s'\n", name, version))
	manifest, err := json.Marshal(map[string]string{"name": name, "version": version})
	if err != nil {
		t.Fatal(err)
	}
	writeFixture(t, filepath.Join(pkg, "package.json"), manifest)
	return command
}

func installedBridge(t *testing.T, root string, harness core.HarnessKind) string {
	t.Helper()
	bridge, ok := core.NpmBridgeFor(harness)
	if !ok {
		t.Fatalf("%v has no npm bridge", harness)
	}
	command := containerPackage(t, root, bridge.Package, bridge.Version)
	if harness == core.HarnessKindCodex {
		provider := containerPackage(t, root, "@openai/codex", core.CodexCLIVersion)
		if err := os.Rename(provider, filepath.Join(filepath.Dir(provider), "codex.js")); err != nil {
			t.Fatal(err)
		}
	} else {
		containerPackage(t, root, "@anthropic-ai/claude-agent-sdk", "test-sdk")
	}
	bin := filepath.Join(root, "node_modules/.bin")
	makeFixtureDir(t, bin)
	makeFixtureLink(t, "../"+bridge.Package+"/bin/cli", filepath.Join(bin, bridge.Command))
	return command
}

func containerLaunch(t *testing.T, root string, harness core.HarnessKind) core.WorkerLaunchConfig {
	t.Helper()
	profileHome := filepath.Join(root, "profile")
	makeFixtureDir(t, profileHome)
	profile := codexLoginProfile(t, profileHome, "chatgpt")
	profile.Kind = harness
	var launch core.WorkerLaunchConfig
	for _, entry := range launchesOnEveryTarget(t, &profile) {
		if entry.target == "container" {
			launch = entry.launch
		}
	}
	if launch.HarnessRuntime != core.HarnessRuntimePolicyAmbient {
		t.Fatalf("%v", launch.HarnessRuntime)
	}
	launch.Cwd = root
	bin := filepath.Join(root, "node_modules/.bin")
	containerExecutable(t, filepath.Join(bin, "node"), "#!/bin/sh\nif [ \"$1\" = --version ]; then echo v24.22.1; fi\n")
	for _, command := range []string{"npm", "npx"} {
		containerExecutable(t, filepath.Join(bin, command), "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 11.6.2; fi\n")
	}
	launch.Environment["PATH"] = bin
	launch.Environment["XDG_CACHE_HOME"] = filepath.Join(root, "cache")
	launch.Environment["CODEX_PATH"] = filepath.Join(root, "node_modules/@openai/codex/bin/codex.js")
	return launch
}

func prepareContainerHarness(t *testing.T, launch *core.WorkerLaunchConfig) *worker.PreparedHarnessLaunch {
	t.Helper()
	prepared, err := worker.PrepareHarnessLaunch(context.Background(), launch.Harness, launch.HarnessRuntime,
		launch.ExecutionPolicy, worker.AcpSupervisorSpecFrom(launch))
	if err != nil {
		t.Fatal(err)
	}
	return prepared
}

func TestContainerRuntimeFreezesRelativePathAndProviderSelection(t *testing.T) {
	temp := t.TempDir()
	bridge := installedBridge(t, temp, core.HarnessKindCodex)
	launch := containerLaunch(t, temp, core.HarnessKindCodex)
	launch.Environment["PATH"] = "node_modules/.bin"
	launch.Environment["CODEX_PATH"] = "node_modules/@openai/codex/bin/codex.js"
	prepared := prepareContainerHarness(t, &launch)
	defer prepared.Release()
	canonical, err := filepath.EvalSymlinks(bridge)
	if err != nil {
		t.Fatal(err)
	}
	if prepared.Spec.Command != canonical {
		t.Fatalf("%s, want %s", prepared.Spec.Command, canonical)
	}
	if !filepath.IsAbs(prepared.Spec.Environment["CODEX_PATH"]) {
		t.Fatal(prepared.Spec.Environment["CODEX_PATH"])
	}
	link := filepath.Join(temp, "node_modules/.bin/codex-acp")
	if err := os.Remove(link); err != nil {
		t.Fatal(err)
	}
	makeFixtureLink(t, "/bin/false", link)
	// Execute the prepared spec: changing PATH's link cannot redirect it.
	command := exec.Command(prepared.Spec.Command, prepared.Spec.Args...)
	command.Env = []string{}
	for name, value := range prepared.Spec.Environment {
		command.Env = append(command.Env, name+"="+value)
	}
	command.Dir = prepared.Spec.Cwd
	output, err := core.RunBounded(context.Background(), command, 1024, 5*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() {
		t.Fatalf("%v %s", output.Status, output.Stderr)
	}
	if got := strings.TrimSpace(string(output.Stdout)); got != core.CodexACPPackage+" "+core.CodexACPVersion {
		t.Fatal(got)
	}
}

func TestContainerRuntimePreparesBeforeStartupAndPreservesTheLiveWorkerOnUpgradeFailure(t *testing.T) {
	preparation := func(recorder *commandRecorder, failPrepare bool) *workerTestExecutor {
		return &workerTestExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
			recorder.record(command)
			if failPrepare && command.Purpose == "prepare exact container harness before worker upgrade" {
				return core.CommandOutput{}, errors.New("test preparation connection failed")
			}
			return core.CommandOutput{}, nil
		}}
	}
	temp := t.TempDir()
	session, err := core.NewSessionID()
	if err != nil {
		t.Fatal(err)
	}
	containerID, err := core.ResourceName(session)
	if err != nil {
		t.Fatal(err)
	}
	ssh := &core.SshTarget{Destination: "user@host.test", SSHArgs: []string{}}
	locators := []core.TargetsTargetLocator{
		{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: containerID},
		{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: containerID},
		{Kind: core.TargetsTargetLocatorAppleContainer, ContainerID: containerID},
		{Kind: core.TargetsTargetLocatorSshDocker, ContainerID: containerID, SSH: ssh},
		{Kind: core.TargetsTargetLocatorSshPodman, ContainerID: containerID, SSH: ssh},
	}
	launch := containerLaunch(t, temp, core.HarnessKindCodex)
	workerPath := filepath.Join(temp, "worker")
	writeFixture(t, workerPath, StampedWorker([]byte("new worker")))
	for _, locator := range locators {
		root, err := core.WorkerRoot(&locator, session)
		if err != nil {
			t.Fatal(err)
		}
		var recorder commandRecorder
		if err := PrepareInstalledManagedHarness(preparation(&recorder, false), &locator, root, &launch); err != nil {
			t.Fatal(err)
		}
		commands := recorder.all()
		if len(commands) != 1 || !strings.Contains(strings.Join(commands[0].Args, " "), "prepare-harness") {
			t.Fatalf("%#v", commands)
		}
		for _, failPrepare := range []bool{false, true} {
			var recorder commandRecorder
			err := PrepareManagedHarnessForUpgrade(preparation(&recorder, failPrepare), &locator, session, WorkerInstallHost{Path: workerPath}, &launch)
			if (err != nil) != failPrepare {
				t.Fatalf("%v: %v", locator.Kind, err)
			}
			commands := recorder.all()
			find := func(purpose string) core.CommandSpec {
				t.Helper()
				index := slices.IndexFunc(commands, func(command core.CommandSpec) bool { return command.Purpose == purpose })
				if index < 0 {
					t.Fatalf("%v: no %q in %#v", locator.Kind, purpose, commands)
				}
				return commands[index]
			}
			if prepare := find("prepare exact container harness before worker upgrade"); !strings.Contains(strings.Join(prepare.Args, " "), "harness-prepare-") {
				t.Fatalf("%#v", prepare)
			}
			if stage := find("stage container harness launch configuration"); !strings.Contains(strings.Join(stage.Args, " "), "harness-prepare-") {
				t.Fatalf("%#v", stage)
			}
			for _, command := range commands {
				rendered := strings.Join(command.Args, " ")
				if strings.Contains(rendered, "'"+root+"/hel'") || strings.Contains(rendered, "'"+root+"/launch.json'") ||
					slices.Contains(command.Args, root+"/hel") || slices.Contains(command.Args, root+"/launch.json") ||
					strings.Contains(rendered, "control.sock") || strings.Contains(rendered, "kill") {
					t.Fatalf("%v touched the live worker: %s", locator.Kind, rendered)
				}
			}
			removed := slices.ContainsFunc(commands, func(command core.CommandSpec) bool {
				return command.Purpose == "remove container harness preparation staging"
			})
			if removed != !failPrepare {
				t.Fatalf("%v: staging removal %v after failure %v", locator.Kind, removed, failPrepare)
			}
		}
	}
}

func TestInstalledDigestMatchesBytesOnLinuxAndDarwinWithQuotedPaths(t *testing.T) {
	directory := t.TempDir()
	// macOS ships shasum but not GNU sha256sum. Exercise the Linux command
	// shape there through the native digest tool as well.
	if runtime.GOOS == "darwin" {
		core.InstallFakeCommand(directory, "sha256sum", "#!/bin/sh\nexec /usr/bin/shasum -a 256 \"$@\"\n")
	}
	file := filepath.Join(directory, "worker's bytes")
	writeFixture(t, file, []byte(strings.Repeat("\x5a", 128*1024)))
	expected, err := core.WorkerExecutableDigest(file)
	if err != nil {
		t.Fatal(err)
	}
	locator := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: directory}
	for _, system := range []string{"Linux", "Darwin"} {
		core.InstallFakeCommand(directory, "uname", "#!/bin/sh\necho "+system+"\n")
		command := InstalledFileDigestCommand(&locator, file, "digest test")
		if command.Env == nil {
			command.Env = map[string]string{}
		}
		command.Env["PATH"] = directory + ":/usr/bin:/bin"
		output, err := core.ProcessExecutor{}.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		if output.Status != 0 {
			t.Fatalf("%s", output.Stderr)
		}
		if fields := strings.Fields(string(output.Stdout)); len(fields) == 0 || fields[0] != expected {
			t.Fatalf("%s: %q", system, output.Stdout)
		}
	}
}

// Installing links the login file to the profile's own, and installing
// again over that link, with or without sharing, never writes the stage's
// copy through it into the profile's login.
func TestInstallingALocalStageLinksTheLoginAndNeverWritesThroughAnEarlierLink(t *testing.T) {
	profile := t.TempDir()
	profileLogin := filepath.Join(profile, "auth.json")
	writeFixture(t, profileLogin, []byte("profile login"))
	stage := t.TempDir()
	writeFixture(t, filepath.Join(stage, "auth.json"), []byte("staged copy"))
	writeFixture(t, filepath.Join(stage, "config.toml"), []byte("staged config"))
	home := filepath.Join(t.TempDir(), "profile")
	makeFixtureDir(t, home)
	login := SharedLogin{Name: "auth.json", File: profileLogin}

	for range 2 {
		if err := InstallLocalProfileStage(stage, home, &login); err != nil {
			t.Fatal(err)
		}
		if target, err := os.Readlink(filepath.Join(home, "auth.json")); err != nil || target != profileLogin {
			t.Fatalf("%s %v", target, err)
		}
		if readFixture(t, profileLogin) != "profile login" {
			t.Fatal("the stage wrote through the link")
		}
		if readFixture(t, filepath.Join(home, "config.toml")) != "staged config" {
			t.Fatal("the configuration was not installed")
		}
	}

	if err := InstallLocalProfileStage(stage, home, nil); err != nil {
		t.Fatal(err)
	}
	installed := filepath.Join(home, "auth.json")
	if info, err := os.Lstat(installed); err != nil || info.Mode()&os.ModeSymlink != 0 {
		t.Fatal("the unshared install kept the link")
	}
	if readFixture(t, installed) != "staged copy" || readFixture(t, profileLogin) != "profile login" {
		t.Fatal("the unshared install wrote through the link")
	}
}
