// Port of the tests in mj-targets/src/targets/container_upgrade.rs.

package targets

import (
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Runs the Linux container's tar/flock protocol on a real filesystem.
func TestControlStateArchiveStoresHardLinkBytesAndRestoreNeverWritesThroughLinks(t *testing.T) {
	skipUnlessLinux(t)
	directory := t.TempDir()
	root := filepath.Join(directory, "control")
	stage := filepath.Join(directory, "stage")
	if err := os.Mkdir(root, 0o755); err != nil {
		t.Fatal(err)
	}
	oldStore := filepath.Join(directory, "old-store")
	newStore := filepath.Join(directory, "new-store")
	old := strings.Repeat("o", 256*1024)
	replacement := strings.Repeat("n", 256*1024)
	writeFile(t, oldStore, old)
	writeFile(t, newStore, replacement)
	if err := os.Link(oldStore, filepath.Join(root, "hel")); err != nil {
		t.Fatal(err)
	}
	if err := os.Link(oldStore, filepath.Join(root, "second-hel")); err != nil {
		t.Fatal(err)
	}
	plan := ContainerUpgradeTestPlan("55555555555555555555555555555555")
	plan.Locator = core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalBare, WorkerRoot: root}
	plan.controlState = &controlState{Root: root, StagingDirectory: stage, Excludes: []string{}}
	if err := os.Chmod(root, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := plan.CaptureControlState(core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(root, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(filepath.Join(root, "hel")); err != nil {
		t.Fatal(err)
	}
	if err := os.Link(newStore, filepath.Join(root, "hel")); err != nil {
		t.Fatal(err)
	}
	if err := plan.RestoreControlState(core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}
	if readFile(t, filepath.Join(root, "hel")) != old {
		t.Fatal("the restored worker does not hold the captured bytes")
	}
	if readFile(t, newStore) != replacement {
		t.Fatal("the restore wrote through a hard link")
	}
	if readFile(t, oldStore) != old {
		t.Fatal("the capture changed its source")
	}
	info, err := os.Stat(root)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm() != 0o700 {
		t.Fatalf("%o", info.Mode().Perm())
	}
	first, err := os.Stat(filepath.Join(root, "hel"))
	if err != nil {
		t.Fatal(err)
	}
	second, err := os.Stat(filepath.Join(root, "second-hel"))
	if err != nil {
		t.Fatal(err)
	}
	if !os.SameFile(first, second) {
		t.Fatal("the restore split the hard links")
	}
}

// containerUpgradeResources removes the isolated test's containers and
// volumes when it ends.
type containerUpgradeResources struct {
	containers []string
	volumes    []string
}

func (r containerUpgradeResources) drop() {
	for _, group := range []struct {
		resource string
		names    []string
	}{{"container", r.containers}, {"volume", r.volumes}} {
		for _, name := range group.names {
			command := core.NewCommandSpec("podman", []string{group.resource, "rm", "--force", name})
			if _, err := (core.ProcessExecutor{}).ExecuteCleanup(&command); err != nil {
				slog.Warn("isolated container upgrade test cleanup failed", "error", err, "name", name)
			}
		}
	}
}

func failReplacementRun() *testExecutor {
	return &testExecutor{
		execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
			if command.Purpose == "start replacement on retained storage" {
				return core.CommandOutput{}, errors.New("injected engine failure after retaining the stopped source")
			}
			return core.ProcessExecutor{}.Execute(command)
		},
	}
}

func retainedInspectionExecutor(inspection string) *testExecutor {
	return &testExecutor{
		execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
			switch command.Purpose {
			case "inspect retained session container":
				return core.CommandOutput{Status: 0, Stdout: []byte(inspection), Stderr: []byte{}}, nil
			case "inspect selected upgrade image":
				return core.CommandOutput{Status: 0, Stdout: []byte("sha256:prepared\n"), Stderr: []byte{}}, nil
			default:
				return core.CommandOutput{}, fmt.Errorf("unexpected fixture command %s", command.Purpose)
			}
		},
	}
}

func retainedInspection(securityOpt []string) string {
	encoded, err := json.Marshal([]any{map[string]any{
		"Id":    "source-id",
		"Name":  "/container-name",
		"Image": "sha256:source-image",
		"Config": map[string]any{
			"Image": "example/image:old",
			"Labels": map[string]string{
				"dev.mj.managed": "true",
				"dev.mj.session": "session-fixture",
			},
			"User":       "",
			"Env":        []string{},
			"WorkingDir": "",
			"Cmd":        []string{"sh", "-c", keepaliveScript},
			"Entrypoint": nil,
		},
		"HostConfig": map[string]any{
			"Memory":         0,
			"NanoCpus":       0,
			"PidsLimit":      nil,
			"NetworkMode":    "default",
			"Privileged":     false,
			"ReadonlyRootfs": false,
			"Devices":        nil,
			"PortBindings":   nil,
			"UsernsMode":     "",
			"SecurityOpt":    securityOpt,
			"CapAdd":         []string{"PERFMON"},
			"CapDrop":        []string{},
		},
		"Mounts": []any{map[string]any{
			"Type":        "bind",
			"Source":      "/tmp",
			"Destination": "/",
			"RW":          true,
			"Propagation": "rprivate",
			"Options":     []string{},
		}},
		"State": map[string]any{"Running": true},
	}})
	if err != nil {
		panic(err)
	}
	return string(encoded)
}

func inspectFixture(locator *core.TargetsTargetLocator, securityOpt []string) (RetainedContainer, error) {
	return InspectRetainedContainer(
		locator,
		"session-fixture",
		"example/image:new",
		"operation-fixture",
		"/workspace/session-fixture",
		"/var/lib/hel/workers/session-fixture",
		retainedInspectionExecutor(retainedInspection(securityOpt)),
	)
}

func TestRetainedPodmanContainerMayCarryTheManagedSecurityOption(t *testing.T) {
	skipOnWindows(t, "Unix only")
	locator := localPodmanLocator("container-name", core.TargetsPodmanWorkspaceLocator{})
	retained, err := inspectFixture(locator, []string{"label=disable"})
	if err != nil {
		t.Fatal(err)
	}
	if retained.Name != "container-name" {
		t.Fatal(retained.Name)
	}
}

func TestRetainedContainerRejectsASecurityOptionTheReplacementWouldDrop(t *testing.T) {
	skipOnWindows(t, "Unix only")
	podman := localPodmanLocator("container-name", core.TargetsPodmanWorkspaceLocator{})
	_, err := inspectFixture(podman, []string{"seccomp=unconfined"})
	if err == nil || !strings.Contains(err.Error(), "storage-preserving upgrades do not support") {
		t.Fatalf("%v", err)
	}

	docker := &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: "container-name"}
	_, err = inspectFixture(docker, []string{"label=disable"})
	if err == nil || !strings.Contains(err.Error(), "storage-preserving upgrades do not support") {
		t.Fatalf("%v", err)
	}
}

func mustInspect(t *testing.T, host *core.ImageHost, name string) *inspection {
	t.Helper()
	record, err := inspect(host, name, core.ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	if record == nil {
		t.Fatalf("%s is missing", name)
	}
	return record
}

func mustChecked(t *testing.T, host *core.ImageHost, args ...string) core.CommandOutput {
	t.Helper()
	output, err := checked(host, args, core.ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	return output
}

// Hard-won: #1326: automatic recreation deleted uncheckpointed workspace and cache storage.
func TestReplacementRecoveryReattachesTheSameStorageWithoutACheckpoint(t *testing.T) {
	skipOnWindows(t, "Unix only")
	skipUnlessIgnoredTestsRequested(t, "requires rootless Podman, cached 2.51.0/2.52.0 images, and MJ_INSTANCE=container-upgrade-regression")
	if instance, ok := core.InstanceName(); !ok || instance != "container-upgrade-regression" {
		t.Fatal(instance)
	}
	session, err := core.NewSessionID()
	if err != nil {
		t.Fatal(err)
	}
	name := mustResourceName(t, session)
	cache := name + "-test-cache"
	workspace := "/workspace/" + session
	largeEnvironment := make([]string, 0, 20)
	for index := range 20 {
		largeEnvironment = append(largeEnvironment, fmt.Sprintf("MJ_UPGRADE_ENV_%d=%s", index, strings.Repeat("e", 4096)))
	}
	template := containerTemplate("ghcr.io/brokkai/mjolnir/agent-dev:2.51.0", core.ImagePullPolicyNever,
		[]string{"--mount", fmt.Sprintf("type=volume,source=%s,target=/cache", cache)},
		core.TargetsPodmanWorkspaceStorage{Kind: core.TargetsPodmanWorkspaceStoragePodmanVolume})
	storage, err := core.PodmanWorkspaceLocatorNamed(&template, name)
	if err != nil {
		t.Fatal(err)
	}
	if storage.EffectiveKind() != core.TargetsPodmanWorkspaceLocatorVolume {
		t.Fatal("unreachable")
	}
	volume := storage.Name
	// Drop the containers before this retained host directory.
	directory := t.TempDir()
	resources := containerUpgradeResources{
		containers: []string{name, name + "-upgrade-test-operation"},
		volumes:    []string{cache, volume, ContainerTemporaryVolumeName(name)},
	}
	defer resources.drop()
	host := core.ImageHost{Kind: core.ImageHostLocalPodman}
	mustChecked(t, &host, "volume", "create", cache)
	for _, assignment := range largeEnvironment {
		template.ExtraRunArgs = append(template.ExtraRunArgs, "--env", assignment)
	}
	locator := localPodmanLocator(name, storage)
	root, err := core.WorkerRoot(locator, session)
	if err != nil {
		t.Fatal(err)
	}
	// A source can carry a value too long for env-file transport. Refuse it
	// before stopping anything, rather than discovering the limit on the far
	// side of replacement.
	unsupported := template
	unsupported.ExtraRunArgs = append(slices.Clone(template.ExtraRunArgs[:2]),
		"--env", "MJ_UPGRADE_TOO_LONG="+strings.Repeat("e", 70*1024))
	run, err := podmanContainerRun(&unsupported, name, session, nil, nil, workspace)
	if err != nil {
		t.Fatal(err)
	}
	started, err := core.ProcessExecutor{}.Execute(&run)
	if err != nil {
		t.Fatal(err)
	}
	if started.Status != 0 {
		t.Fatalf("%s", started.Stderr)
	}
	source := mustInspect(t, &host, name)
	if !slices.ContainsFunc(source.HostConfig.CapAdd, func(capability string) bool {
		return trimCapPrefixes(capability) == "PERFMON"
	}) {
		t.Fatalf("%q", source.HostConfig.CapAdd)
	}
	_, err = InspectRetainedContainer(locator, session, "ghcr.io/brokkai/mjolnir/agent-dev:2.52.0", "unsupported", workspace, root, core.ProcessExecutor{})
	if err == nil || !strings.Contains(err.Error(), "oversized environment") {
		t.Fatalf("%v", err)
	}
	unchanged := mustInspect(t, &host, name)
	if unchanged.ID != source.ID || !unchanged.State.Running {
		t.Fatal("the refused source changed")
	}
	mustChecked(t, &host, "rm", "--force", name)
	start, err := podmanContainerRun(&template, name, session, nil, nil, workspace)
	if err != nil {
		t.Fatal(err)
	}
	started, err = core.ProcessExecutor{}.Execute(&start)
	if err != nil {
		t.Fatal(err)
	}
	if started.Status != 0 {
		t.Fatalf("%s", started.Stderr)
	}
	// Exercise the SSH-side env-file transport through the real engine, with
	// more than a pipe buffer so input/output ordering matters.
	transport := host.Command([]string{
		"podman", "run", "--rm", "--pull=never", "--env-file", "/dev/stdin", "--entrypoint", "sh", template.Image,
		"-c", "test \"${#MJ_UPGRADE_ENV_0}\" -eq 4096 && test \"${#MJ_UPGRADE_ENV_19}\" -eq 4096",
	}, "verify engine stdin environment transport").
		WithSensitiveStdin([]byte(strings.Join(largeEnvironment, "\n") + "\n"))
	output, err := core.ProcessExecutor{}.Execute(&transport)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("%s", output.Stderr)
	}
	script := fmt.Sprintf("set -eu; mkdir -p '%s/uncheckpointed-cache' '%s'; printf workspace >'%s/uncheckpointed-cache/marker'; printf cache >/cache/marker; printf journal >'%s/journal-marker'; printf manual >/manual-layer-marker",
		workspace, root, workspace, root)
	mustChecked(t, &host, "exec", name, "sh", "-c", script)
	plan, err := InspectRetainedContainer(locator, session, "ghcr.io/brokkai/mjolnir/agent-dev:2.52.0", "test-operation", workspace, root, core.ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	// Use the cached immutable target, keeping this isolated test offline.
	image := mustChecked(t, &host, "image", "inspect", "--format", "{{.Id}}", plan.TargetImage)
	prepared := strings.TrimSpace(string(image.Stdout))
	plan.PreparedImage = &prepared
	retained := slices.Clone(plan.mounts)
	if err := plan.CaptureControlState(core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}
	if err := plan.Replace(session, failReplacementRun()); err == nil {
		t.Fatal("the injected engine failure was lost")
	}
	backup := mustInspect(t, &host, plan.BackupName)
	if backup.ID != plan.SourceID || backup.State.Running {
		t.Fatal("the source was not retained stopped")
	}
	// Simulate a daemon replacement after the engine failed. Recovery gets
	// only the serialized plan; no checkpoint exists anywhere in this test.
	encoded, err := json.Marshal(plan)
	if err != nil {
		t.Fatal(err)
	}
	var recovered RetainedContainer
	if err := json.Unmarshal(encoded, &recovered); err != nil {
		t.Fatal(err)
	}
	if err := recovered.Replace(session, core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}
	if err := recovered.RestoreControlState(core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}
	if err := recovered.Replace(session, core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}
	upgraded := mustInspect(t, &host, name)
	if !reflect.DeepEqual(upgraded.HostConfig.CapAdd, source.HostConfig.CapAdd) {
		t.Fatalf("%q %q", upgraded.HostConfig.CapAdd, source.HostConfig.CapAdd)
	}
	replacement, err := InspectRetainedContainer(locator, session, plan.TargetImage, "observation", workspace, root, core.ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	if replacement.SourceID == plan.SourceID {
		t.Fatal("the replacement kept the source's identity")
	}
	if !reflect.DeepEqual(replacement.mounts, retained) {
		t.Fatalf("%v %v", replacement.mounts, retained)
	}
	script = fmt.Sprintf("set -eu; test \"$(cat '%s/uncheckpointed-cache/marker')\" = workspace; test \"$(cat /cache/marker)\" = cache; test \"$(cat '%s/journal-marker')\" = journal; test ! -e /manual-layer-marker; test \"${#MJ_UPGRADE_ENV_0}\" -eq 4096; test \"${#MJ_UPGRADE_ENV_19}\" -eq 4096",
		workspace, root)
	mustChecked(t, &host, "exec", name, "sh", "-c", script)
	if err := recovered.Finish(session, core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}
	if err := recovered.Finish(session, core.ProcessExecutor{}); err != nil {
		t.Fatal(err)
	}
	if record, err := inspect(&host, plan.BackupName, core.ProcessExecutor{}); err != nil || record != nil {
		t.Fatalf("%v %v", record, err)
	}
	// A layout we cannot preserve must fail before any container mutation.
	if _, err := InspectRetainedContainer(locator, session, plan.TargetImage, "unsupported", "/layer-only-workspace", root, core.ProcessExecutor{}); err == nil {
		t.Fatal("a layer-only workspace was accepted")
	}
	if record := mustInspect(t, &host, plan.Name); record.ID != replacement.SourceID {
		t.Fatal("the refused inspection changed the container")
	}

	// Docker-style workspaces can be ordinary recorded bind attachments.
	// Historical recovery must recognize their data instead of replaying
	// repository checkpoint contents over them, even without a Podman
	// workspace-volume locator.
	mustChecked(t, &host, "rm", "--force", plan.Name)
	writeFile(t, filepath.Join(directory, "uncheckpointed"), "retained attachment")
	legacy := localPodmanLocator(plan.Name, core.TargetsPodmanWorkspaceLocator{Kind: core.TargetsPodmanWorkspaceLocatorContainerLayer})
	mounts := []core.AdditionalMount{{Source: directory, Destination: workspace, Access: core.MountAccessRw}}
	restore, err := RecoverRemovedContainer(legacy, &template, session, mounts, workspace, false, core.ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	if restore {
		t.Fatal("recovery asked to restore over a retained attachment")
	}
	mustChecked(t, &host, "exec", plan.Name, "test", "-f", workspace+"/uncheckpointed")
}

// trimCapPrefixes is Rust's `trim_start_matches("CAP_")`.
func trimCapPrefixes(capability string) string {
	for strings.HasPrefix(capability, "CAP_") {
		capability = capability[len("CAP_"):]
	}
	return capability
}

// fakeVM is the one VM the fake engine runs.
type fakeVM struct {
	id              string
	running         bool
	labels          map[string]string
	staged          *string
	applied         bool
	hasPreviousRoot bool
	pinnedImage     *string
}

// fakeNitroEngine is the engine's rules for one VM, as the controller sees
// them: one VM owns the volumes, so the replacement is the same VM with
// another root disk and another id, and `start` is admitted only when no
// other VM of this session runs.
type fakeNitroEngine struct {
	testExecutor
	vm  fakeVM
	log []string
	// fail fails the next command whose verb starts with this text.
	fail string
}

func newFakeNitroEngine(t *testing.T, session string) *fakeNitroEngine {
	engine := &fakeNitroEngine{vm: fakeVM{
		id:      "source-id",
		running: true,
		labels: map[string]string{
			"dev.mj.managed": "true",
			"dev.mj.session": session,
		},
	}}
	engine.execute = func(command *core.CommandSpec) (core.CommandOutput, error) {
		return engine.run(t, command)
	}
	return engine
}

func (e *fakeNitroEngine) inspection() string {
	status := "exited"
	if e.vm.running {
		status = "running"
	}
	encoded, err := json.Marshal([]any{map[string]any{
		"Id":        e.vm.id,
		"Name":      "nitro-name",
		"Image":     "sha256:source-image",
		"ImageName": "example/image:old",
		"Config": map[string]any{
			"Image":      "example/image:old",
			"Labels":     e.vm.labels,
			"User":       "0:0",
			"Env":        []string{"HOME=/home/hel"},
			"WorkingDir": "",
			"Cmd":        []string{"sh", "-c", keepaliveScript},
			"Entrypoint": []string{},
		},
		"HostConfig": map[string]any{"Memory": uint64(4294967296), "NanoCpus": uint64(2000000000), "NetworkMode": "nitro"},
		"Mounts": []any{
			map[string]any{"Type": "volume", "Name": "nitro-name-tmp", "Source": "/v/tmp", "Destination": "/tmp", "RW": true},
			map[string]any{"Type": "volume", "Name": "nitro-name-workspace", "Source": "/v/ws", "Destination": "/workspace/session-fixture", "RW": true},
		},
		"State": map[string]any{"Status": status, "Running": e.vm.running},
	}})
	if err != nil {
		panic(err)
	}
	return string(encoded)
}

func (e *fakeNitroEngine) run(t *testing.T, command *core.CommandSpec) (core.CommandOutput, error) {
	ok := func(stdout string) (core.CommandOutput, error) {
		return core.CommandOutput{Status: 0, Stdout: []byte(stdout), Stderr: []byte{}}, nil
	}
	// ssh sends the remote command as one quoted line.
	remote := ""
	if len(command.Args) != 0 {
		remote = command.Args[len(command.Args)-1]
	}
	if command.Purpose == "initialize session container home" {
		// HOME initialization is a shell transaction of engine execs, after
		// the replacement VM has started.
		if !strings.Contains(remote, core.EngineShellWords(core.MicrovmEngine)) {
			t.Errorf("HOME initialization does not go through the engine: %s", remote)
		}
		if !e.vm.running {
			t.Error("HOME initialization ran before the replacement VM started")
		}
		e.log = append(e.log, "exec initialize-home")
		return ok("")
	}
	position := strings.Index(remote, "mj-microvm")
	if position < 0 {
		return core.CommandOutput{}, fmt.Errorf("unexpected command that does not go through the engine: %s", remote)
	}
	var words []string
	for _, word := range strings.Fields(remote[position+len("mj-microvm"):]) {
		if word = strings.Trim(word, "'"); word != "" {
			words = append(words, word)
		}
	}
	verb := strings.Join(words, " ")
	if e.fail != "" && strings.HasPrefix(verb, e.fail) {
		e.fail = ""
		return core.CommandOutput{}, fmt.Errorf("injected failure of `%s`", verb)
	}
	e.log = append(e.log, verb)
	vm := &e.vm
	switch {
	case words[0] == "container" && words[1] == "inspect":
		return ok(e.inspection())
	case words[0] == "image":
		return ok("sha256:target-image\n")
	// Guest commands: the tar probe, capture and restore.
	case words[0] == "exec":
		return ok("--one-file-system --unlink-first --strip-components --no-recursion --occurrence")
	case words[0] == "container" && words[1] == "replace-root":
		operation := words[slices.Index(words, "--operation")+1]
		switch words[2] {
		case "stage":
			vm.staged = &operation
			pinned := words[len(words)-1]
			vm.pinnedImage = &pinned
		case "apply":
			if vm.running {
				t.Error("the engine refuses to swap a running VM")
			}
			if vm.staged == nil || *vm.staged != operation {
				t.Errorf("applied %s with %v staged", operation, vm.staged)
			}
			if !vm.applied {
				vm.applied = true
				vm.hasPreviousRoot = true
				vm.id = "replacement-id"
				vm.labels["dev.mj.container-upgrade"] = operation
			}
		case "release":
			vm.staged = nil
			vm.pinnedImage = nil
			vm.hasPreviousRoot = false
		default:
			return core.CommandOutput{}, fmt.Errorf("unexpected replace-root %s", words[2])
		}
		return ok("")
	case words[0] == "stop":
		vm.running = false
		return ok("")
	case words[0] == "start":
		vm.running = true
		return ok("")
	default:
		return core.CommandOutput{}, fmt.Errorf("unexpected engine verb %s", words[0])
	}
}

// No VM may run beside its replacement (two kernels on one ext4 volume), and
// a failure between any two engine calls must resume.
func TestNitroReplacementSwapsTheRootOfTheStoppedVMAndResumesAfterEachFailure(t *testing.T) {
	skipOnWindows(t, "Unix only")
	session := "session-fixture"
	locator := &core.TargetsTargetLocator{
		Kind:        core.TargetsTargetLocatorSshNitro,
		SSH:         &core.SshTarget{Destination: "host", SSHArgs: []string{}},
		ContainerID: "nitro-name",
	}
	for _, failing := range []string{"", "stop", "container replace-root apply", "start"} {
		engine := newFakeNitroEngine(t, session)
		plan, err := InspectRetainedContainer(locator, session, "example/image:new", "op1",
			"/workspace/session-fixture", "/var/lib/hel/workers/session-fixture", engine)
		if err != nil {
			t.Fatal(err)
		}
		if plan.controlState == nil {
			t.Fatal("the worker root is on the replaced disk")
		}
		plan.ImagePinReserved = true
		if err := plan.Prepare(engine); err != nil {
			t.Fatal(err)
		}
		if engine.vm.pinnedImage == nil || *engine.vm.pinnedImage != "target-image" {
			t.Fatalf("%v", engine.vm.pinnedImage)
		}
		if current, err := plan.SourceIsCurrent(engine); err != nil || !current {
			t.Fatalf("%v %v", current, err)
		}
		engine.fail = failing
		if failing != "" {
			if err := plan.Replace(session, engine); err == nil {
				t.Fatalf("%q: the injected failure was lost", failing)
			}
		}
		if err := plan.Replace(session, engine); err != nil {
			t.Fatal(err)
		}
		if err := plan.Replace(session, engine); err != nil {
			t.Fatal(err)
		}
		verbs := slices.Clone(engine.log)
		applied := 0
		for _, verb := range verbs {
			if strings.HasPrefix(verb, "container replace-root apply") {
				applied++
			}
		}
		if applied < 1 || applied > 2 {
			t.Fatalf("%q: %q", failing, verbs)
		}
		// The source stops before the swap and the VM starts only after it.
		position := func(prefix string) int {
			for index := len(verbs) - 1; index >= 0; index-- {
				if strings.HasPrefix(verbs[index], prefix) {
					return index
				}
			}
			t.Fatalf("no %s in %q", prefix, verbs)
			return -1
		}
		if position("stop") >= position("container replace-root apply") {
			t.Fatalf("%q", verbs)
		}
		if position("container replace-root apply") >= position("start") {
			t.Fatalf("%q", verbs)
		}
		if !engine.vm.running {
			t.Fatal("the replacement VM is not running")
		}
		if current, err := plan.SourceIsCurrent(engine); err != nil || current {
			t.Fatalf("%v %v", current, err)
		}

		if err := plan.Finish(session, engine); err != nil {
			t.Fatal(err)
		}
		vm := engine.vm
		if vm.hasPreviousRoot || vm.pinnedImage != nil || vm.staged != nil {
			t.Fatalf("%+v", vm)
		}
		if vm.labels["dev.mj.container-upgrade"] != "op1" {
			t.Fatalf("%v", vm.labels)
		}
	}
}
