// Port of mj-targets/src/targets/tests.rs: target recovery, provisioning
// plans, image pull policies, attachments, the filesystem probe, cleanup
// confirmation and the SSH boundary.

package targets

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// fakeExecutor records every command and fails the one at failAt (-1: none).
type fakeExecutor struct {
	testExecutor
	seen   []core.CommandSpec
	failAt int
}

func newFakeExecutor(failAt int) *fakeExecutor {
	executor := &fakeExecutor{failAt: failAt}
	executor.execute = func(command *core.CommandSpec) (core.CommandOutput, error) {
		index := len(executor.seen)
		executor.seen = append(executor.seen, *command)
		status := 0
		if executor.failAt == index {
			status = 1
		}
		return core.CommandOutput{Status: status, Stdout: []byte{}, Stderr: []byte("failure")}, nil
	}
	return executor
}

func emptyRecoveryPlan() *core.TargetRecoveryPlan {
	return &core.TargetRecoveryPlan{
		Exists:    core.NewCommandSpec("exists", nil),
		Inspect:   core.NewCommandSpec("inspect", nil),
		Start:     core.NewCommandSpec("start", nil),
		SessionID: testSession,
	}
}

func TestPodmanTargetRecoveryUsesTheExactLocalOrRemoteContainer(t *testing.T) {
	name := mustResourceName(t, testSession)
	local, err := TargetRecoveryPlan(localPodmanLocator(name, core.TargetsPodmanWorkspaceLocator{}), testSession)
	if err != nil || local == nil {
		t.Fatalf("%v %v", local, err)
	}
	if !slices.Equal(local.Exists.Args, []string{"container", "exists", name}) {
		t.Fatalf("%q", local.Exists.Args)
	}
	if !slices.Equal(local.Inspect.Args, []string{"container", "inspect", name}) {
		t.Fatalf("%q", local.Inspect.Args)
	}
	if !slices.Equal(local.Start.Args, []string{"start", name}) {
		t.Fatalf("%q", local.Start.Args)
	}

	remote, err := TargetRecoveryPlan(sshPodmanLocator(name, core.TargetsPodmanWorkspaceLocator{}), testSession)
	if err != nil || remote == nil {
		t.Fatalf("%v %v", remote, err)
	}
	if remote.Exists.Program != "ssh" {
		t.Fatal(remote.Exists.Program)
	}
	if got := UnwrappedRemoteLine(lastArg(&remote.Exists)); got != fmt.Sprintf("'podman' 'container' 'exists' '%s'", name) {
		t.Fatal(got)
	}
	if remote.Inspect.Program != "ssh" {
		t.Fatal(remote.Inspect.Program)
	}
	if got := UnwrappedRemoteLine(lastArg(&remote.Inspect)); got != fmt.Sprintf("'podman' 'container' 'inspect' '%s'", name) {
		t.Fatal(got)
	}
	if got := UnwrappedRemoteLine(lastArg(&remote.Start)); got != fmt.Sprintf("'podman' 'start' '%s'", name) {
		t.Fatal(got)
	}
}

func TestStoppedOwnedPodmanTargetIsStartedAndReinspected(t *testing.T) {
	name := mustResourceName(t, testSession)
	plan, err := TargetRecoveryPlan(localPodmanLocator(name, core.TargetsPodmanWorkspaceLocator{}), testSession)
	if err != nil {
		t.Fatal(err)
	}
	executor := newPodmanPreflightExecutor(
		podmanOutput(""),
		podmanInspection("exited", testSession, "true"),
		podmanOutput("container-id\n"),
		podmanInspection("running", testSession, "true"),
	)

	outcome, err := EnsureRecoveryTargetRunning(executor, plan)
	if err != nil || outcome != core.TargetRecoveryOutcomeStarted {
		t.Fatalf("%v %v", outcome, err)
	}
	if len(executor.seen) != 4 {
		t.Fatalf("%d commands", len(executor.seen))
	}
	for index, purpose := range []string{
		"check for Mjolnir session container",
		"inspect Mjolnir session container",
		"start stopped Mjolnir session container",
		"inspect Mjolnir session container",
	} {
		if executor.seen[index].Purpose != purpose {
			t.Fatalf("%d: %s", index, executor.seen[index].Purpose)
		}
	}
}

func TestRunningPodmanTargetIsNotStarted(t *testing.T) {
	executor := newPodmanPreflightExecutor(
		podmanOutput(""),
		podmanInspection("running", testSession, "true"),
	)

	outcome, err := EnsureRecoveryTargetRunning(executor, emptyRecoveryPlan())
	if err != nil || outcome != core.TargetRecoveryOutcomeAlreadyRunning {
		t.Fatalf("%v %v", outcome, err)
	}
	if len(executor.seen) != 2 {
		t.Fatalf("%d commands", len(executor.seen))
	}
}

func TestMissingPodmanTargetIsReportedWithoutInspectionOrStart(t *testing.T) {
	executor := newPodmanPreflightExecutor(core.CommandOutput{Status: 1, Stdout: []byte{}, Stderr: []byte{}})

	outcome, err := EnsureRecoveryTargetRunning(executor, emptyRecoveryPlan())
	if err != nil || outcome != core.TargetRecoveryOutcomeMissing {
		t.Fatalf("%v %v", outcome, err)
	}
	if len(executor.seen) != 1 {
		t.Fatalf("%d commands", len(executor.seen))
	}
}

func TestUnsafePodmanTargetStatesAndOwnershipNeverStart(t *testing.T) {
	for _, testCase := range []struct{ status, session, managed, expected string }{
		{"paused", testSession, "true", "paused"},
		{"stopping", testSession, "true", "stopping"},
		{"exited", "another-session", "true", "another session"},
		{"exited", testSession, "false", "Mjolnir does not own"},
	} {
		executor := newPodmanPreflightExecutor(
			podmanOutput(""),
			podmanInspection(testCase.status, testCase.session, testCase.managed),
		)

		_, err := EnsureRecoveryTargetRunning(executor, emptyRecoveryPlan())
		if err == nil || !strings.Contains(err.Error(), testCase.expected) {
			t.Fatalf("%v", err)
		}
		if len(executor.seen) != 2 {
			t.Fatalf("%d commands", len(executor.seen))
		}
	}
}

func TestPodmanTargetMustStillBeRunningAfterStart(t *testing.T) {
	executor := newPodmanPreflightExecutor(
		podmanOutput(""),
		podmanInspection("exited", testSession, "true"),
		podmanOutput("container-id\n"),
		podmanInspection("exited", testSession, "true"),
	)

	_, err := EnsureRecoveryTargetRunning(executor, emptyRecoveryPlan())
	if err == nil || !strings.Contains(err.Error(), "after start") {
		t.Fatalf("%v", err)
	}
}

func TestPodmanInspectOrStartFailuresStopRecovery(t *testing.T) {
	plan := &core.TargetRecoveryPlan{
		Exists:    core.NewCommandSpec("exists", nil).WithPurpose("check test target"),
		Inspect:   core.NewCommandSpec("inspect", nil).WithPurpose("inspect test target"),
		Start:     core.NewCommandSpec("start", nil).WithPurpose("start test target"),
		SessionID: testSession,
	}
	inspectFailed := newPodmanPreflightExecutor(
		podmanOutput(""),
		core.CommandOutput{Status: 125, Stdout: []byte{}, Stderr: []byte("storage unavailable")},
	)
	_, err := EnsureRecoveryTargetRunning(inspectFailed, plan)
	if err == nil || !strings.Contains(err.Error(), "inspect container session target") {
		t.Fatalf("%v", err)
	}
	if len(inspectFailed.seen) != 2 {
		t.Fatalf("%d commands", len(inspectFailed.seen))
	}

	startFailed := newPodmanPreflightExecutor(
		podmanOutput(""),
		podmanInspection("exited", testSession, "true"),
		core.CommandOutput{Status: 1, Stdout: []byte{}, Stderr: []byte("start refused")},
	)
	_, err = EnsureRecoveryTargetRunning(startFailed, plan)
	if err == nil || !strings.Contains(err.Error(), "start confirmed stopped") {
		t.Fatalf("%v", err)
	}
	if len(startFailed.seen) != 3 {
		t.Fatalf("%d commands", len(startFailed.seen))
	}
}

func mustProvisionPlan(t *testing.T, template *core.TargetsTargetTemplate, mounts []core.AdditionalMount, workspace *string) core.CommandPlan {
	t.Helper()
	bundle := testBundle()
	plan, err := ProvisionPlan(template, testSession, &bundle, mounts, workspace)
	if err != nil {
		t.Fatal(err)
	}
	return plan
}

func findPurpose(t *testing.T, commands []core.CommandSpec, purpose string) *core.CommandSpec {
	t.Helper()
	for index := range commands {
		if commands[index].Purpose == purpose {
			return &commands[index]
		}
	}
	t.Fatalf("no command %q", purpose)
	return nil
}

func TestPodmanPlanUsesOwnedNameLabelAndArgvClones(t *testing.T) {
	plan := mustProvisionPlan(t, containerTarget(core.TargetsTargetTemplateLocalPodman,
		containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, []string{"--cpus=4"}, core.TargetsPodmanWorkspaceStorage{})), nil, nil)
	name := mustResourceName(t, testSession)
	if plan.Commands[0].Program != "sh" {
		t.Fatal(plan.Commands[0].Program)
	}
	if !containsWindow(plan.Commands[0].Args, "--name", name) {
		t.Fatalf("%q", plan.Commands[0].Args)
	}
	identity := managedResourceIdentityArgs(managedResourceKindContainer, testSession)
	if !containsWindow(plan.Commands[0].Args, identity...) {
		t.Fatalf("%q", plan.Commands[0].Args)
	}
	clone := findPurpose(t, plan.Commands, "clone app")
	if !slices.Equal(clone.Args[:4], []string{"exec", "-i", name, "git"}) {
		t.Fatalf("%q", clone.Args)
	}
	if !slices.Contains(clone.Args, "--") {
		t.Fatalf("%q", clone.Args)
	}
	if !slices.Contains(clone.Args, "/workspace/app") {
		t.Fatalf("%q", clone.Args)
	}
	bootstrap := findPurpose(t, plan.Commands, "install Git")
	for _, expected := range []string{"command -v git", "command -v gh", "gh auth git-credential"} {
		if !strings.Contains(lastArg(bootstrap), expected) {
			t.Fatalf("%s lacks %s", lastArg(bootstrap), expected)
		}
	}
}

func TestPodmanVolumeWorkspaceIsPerSessionAndMountedForLocalAndSSHTargets(t *testing.T) {
	container := containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, nil,
		core.TargetsPodmanWorkspaceStorage{Kind: core.TargetsPodmanWorkspaceStoragePodmanVolume})
	local := mustProvisionPlan(t, containerTarget(core.TargetsTargetTemplateLocalPodman, container), nil, nil)
	volume := mustResourceName(t, testSession) + "-workspace"
	create := local.Commands[0]
	if create.Program != "sh" {
		t.Fatal(create.Program)
	}
	if !strings.Contains(create.Args[1], "podman volume create") {
		t.Fatal(create.Args[1])
	}
	if !strings.Contains(create.Args[1], "podman volume rm --force") {
		t.Fatal(create.Args[1])
	}
	if !slices.Contains(create.Args, volume) {
		t.Fatalf("%q", create.Args)
	}
	if !slices.Contains(create.Args, fmt.Sprintf("%s:%s:rw,U", volume, core.ContainerWorkspace)) {
		t.Fatalf("%q", create.Args)
	}

	other, err := core.PodmanWorkspaceLocatorFor(&container, "118f9dd2-a3b4-7c8d-9000-123456789abc")
	if err != nil {
		t.Fatal(err)
	}
	if reflect.DeepEqual(other, core.TargetsPodmanWorkspaceLocator{Kind: core.TargetsPodmanWorkspaceLocatorVolume, Name: volume}) {
		t.Fatalf("%v", other)
	}

	remote := mustProvisionPlan(t, sshContainerTarget(core.TargetsTargetTemplateSshPodman, container), nil, nil)
	remoteLine := lastArg(&remote.Commands[0])
	for _, expected := range []string{"podman volume create", volume, "/workspace:rw,U"} {
		if !strings.Contains(remoteLine, expected) {
			t.Fatalf("%s lacks %s", remoteLine, expected)
		}
	}
}

type enginePlan struct {
	engine string
	plan   core.CommandPlan
}

// containerPlansFor is every container plan for one session, keyed by
// engine, so the workspace a plan clones into and mounts can be checked the
// same way for all of them.
func containerPlansFor(t *testing.T, workspace *string) []enginePlan {
	t.Helper()
	container := func(storage core.TargetsPodmanWorkspaceStorage) core.TargetsContainerTemplate {
		return containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, nil, storage)
	}
	hostHelper := core.TargetsPodmanWorkspaceStorage{
		Kind:   core.TargetsPodmanWorkspaceStorageHostHelper,
		Root:   "/srv/mj-workspaces",
		Helper: []string{"/opt/mj-helper"},
	}
	volume := core.TargetsPodmanWorkspaceStorage{Kind: core.TargetsPodmanWorkspaceStoragePodmanVolume}
	templates := []struct {
		engine   string
		template *core.TargetsTargetTemplate
	}{
		{"local podman volume", containerTarget(core.TargetsTargetTemplateLocalPodman, container(volume))},
		{"local podman host path", containerTarget(core.TargetsTargetTemplateLocalPodman, container(hostHelper))},
		{"local docker", containerTarget(core.TargetsTargetTemplateLocalDocker, container(core.TargetsPodmanWorkspaceStorage{}))},
		{"apple container", containerTarget(core.TargetsTargetTemplateAppleContainer, container(core.TargetsPodmanWorkspaceStorage{}))},
		{"ssh podman volume", sshContainerTarget(core.TargetsTargetTemplateSshPodman, container(volume))},
		{"ssh docker", sshContainerTarget(core.TargetsTargetTemplateSshDocker, container(core.TargetsPodmanWorkspaceStorage{}))},
	}
	plans := make([]enginePlan, 0, len(templates))
	for _, entry := range templates {
		plans = append(plans, enginePlan{entry.engine, mustProvisionPlan(t, entry.template, nil, workspace)})
	}
	return plans
}

// commandText is both plan shapes in one string: a local plan carries its
// argv, and an SSH plan carries one quoted command line.
func commandText(command *core.CommandSpec) string {
	return command.Program + " " + strings.Join(command.Args, " ")
}

func TestContainerPlansCloneAndMountUnderTheSessionWorkspace(t *testing.T) {
	workspace, err := core.NewContainerWorkspace(testSession)
	if err != nil {
		t.Fatal(err)
	}
	if workspace != core.Join("/workspace", testSession) {
		t.Fatal(workspace)
	}
	for _, entry := range containerPlansFor(t, &workspace) {
		clone := commandText(findPurpose(t, entry.plan.Commands, "clone app"))
		if !strings.Contains(clone, fmt.Sprintf("/workspace/%s/app", testSession)) {
			t.Fatalf("%s: %s", entry.engine, clone)
		}
		if strings.Contains(clone, " /workspace/app") {
			t.Fatalf("%s: %s", entry.engine, clone)
		}
		create := commandText(&entry.plan.Commands[0])
		if strings.Contains(entry.engine, "podman") && !strings.Contains(create, fmt.Sprintf(":/workspace/%s:rw", testSession)) {
			t.Fatalf("%s: %s", entry.engine, create)
		}
	}
}

func TestASessionWithoutARecordedContainerWorkspaceKeepsTheSharedOne(t *testing.T) {
	for _, entry := range containerPlansFor(t, nil) {
		clone := commandText(findPurpose(t, entry.plan.Commands, "clone app"))
		if !strings.Contains(clone, "/workspace/app") {
			t.Fatalf("%s: %s", entry.engine, clone)
		}
		if strings.Contains(clone, "/workspace/"+testSession) {
			t.Fatalf("%s: %s", entry.engine, clone)
		}
		create := commandText(&entry.plan.Commands[0])
		if strings.Contains(entry.engine, "podman") && !strings.Contains(create, ":/workspace:rw") {
			t.Fatalf("%s: %s", entry.engine, create)
		}
	}
}

func TestPodmanHostHelperCreatesTheExactWorkspaceBeforeLaunch(t *testing.T) {
	root := "/srv/mj-workspaces"
	plan := mustProvisionPlan(t, containerTarget(core.TargetsTargetTemplateLocalPodman,
		containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, nil, core.TargetsPodmanWorkspaceStorage{
			Kind:   core.TargetsPodmanWorkspaceStorageHostHelper,
			Root:   root,
			Helper: []string{"sudo", "-n", "/opt/mj-helper"},
		})), nil, nil)
	resource := mustResourceName(t, testSession) + "-workspace"
	create := plan.Commands[0]
	if !strings.Contains(create.Args[1], `'/opt/mj-helper' create "$resource"`) {
		t.Fatal(create.Args[1])
	}
	if !strings.Contains(create.Args[1], `'/opt/mj-helper' destroy "$resource"`) {
		t.Fatal(create.Args[1])
	}
	if !slices.Contains(create.Args, fmt.Sprintf("%s/%s:/workspace:rw", root, resource)) {
		t.Fatalf("%q", create.Args)
	}
}

// assertSecretOnlyInSensitiveStdin: the secret never reaches the command's
// arguments, its rendered form or its serialized plan. Rust also checks that
// the Debug form says `<redacted>`; Go's fmt renders the unexported input as
// an address, so the check here is that the secret travels as sensitive
// stdin instead.
func assertSecretOnlyInSensitiveStdin(t *testing.T, command *core.CommandSpec, secret string) {
	t.Helper()
	if slices.ContainsFunc(command.Args, func(argument string) bool { return strings.Contains(argument, secret) }) {
		t.Fatalf("%q", command.Args)
	}
	for _, format := range []string{"%v", "%+v", "%#v"} {
		if strings.Contains(fmt.Sprintf(format, *command), secret) {
			t.Fatalf("%s rendering leaks the secret", format)
		}
	}
	if input, ok := command.SensitiveStdin(); !ok || !strings.Contains(string(input), secret) {
		t.Fatalf("the secret is not carried as sensitive stdin")
	}
	encoded, err := json.Marshal(*command)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(encoded), secret) {
		t.Fatal(string(encoded))
	}
}

// Hard-won: 15867df0: GitHub tokens were exposed in container argv.
func TestContainerSecretIsStreamedWithoutEnteringLocalCommandArguments(t *testing.T) {
	skipOnWindows(t, "#1297: runs sh, du or touch on the controller")
	secret := "github-token-that-must-not-reach-argv"
	target := containerTarget(core.TargetsTargetTemplateLocalPodman,
		containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, []string{"--env", "GH_TOKEN"}, core.TargetsPodmanWorkspaceStorage{}))
	plan := core.CommandPlan{
		Description: "exercise secret launcher",
		Commands: []core.CommandSpec{
			core.NewCommandSpec("sh", []string{"-c", `printf %s "$GH_TOKEN"`}).
				WithPurpose("read inherited secret").
				WithCreatesTarget(),
		},
	}

	if err := plan.ProvideTargetEnvironmentSecret(target, "GH_TOKEN", secret); err != nil {
		t.Fatal(err)
	}

	command := &plan.Commands[0]
	if command.Program != "sh" {
		t.Fatal(command.Program)
	}
	assertSecretOnlyInSensitiveStdin(t, command, secret)
	output, err := plan.Execute(core.ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	if string(output[0].Stdout) != secret {
		t.Fatalf("%q", output[0].Stdout)
	}
}

// Hard-won: 15867df0: GitHub tokens were exposed in container argv.
func TestContainerSecretIsStreamedWithoutEnteringRemoteSSHArguments(t *testing.T) {
	secret := "remote-github-token-that-must-not-reach-argv"
	target := sshContainerTarget(core.TargetsTargetTemplateSshPodman,
		containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, []string{"--env", "GH_TOKEN"}, core.TargetsPodmanWorkspaceStorage{}))
	plan := mustProvisionPlan(t, target, nil, nil)

	if err := plan.ProvideTargetEnvironmentSecret(target, "GH_TOKEN", secret); err != nil {
		t.Fatal(err)
	}

	index := slices.IndexFunc(plan.Commands, func(command core.CommandSpec) bool { return command.CreatesTarget })
	if index < 0 {
		t.Fatal("no command creates the target")
	}
	command := &plan.Commands[index]
	if command.Program != "ssh" {
		t.Fatal(command.Program)
	}
	if !strings.Contains(lastArg(command), "read -r GH_TOKEN") {
		t.Fatal(lastArg(command))
	}
	if !strings.Contains(lastArg(command), "'--env' 'GH_TOKEN'") {
		t.Fatal(lastArg(command))
	}
	assertSecretOnlyInSensitiveStdin(t, command, secret)
}

func TestPodmanPlanOnlyMarksPerRepositoryCloneCommandsForParallelExecution(t *testing.T) {
	plan := mustProvisionPlan(t, containerTarget(core.TargetsTargetTemplateLocalPodman,
		containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, nil, core.TargetsPodmanWorkspaceStorage{})), nil, nil)

	if bootstrap := findPurpose(t, plan.Commands, "install Git"); bootstrap.ParallelGroup != nil {
		t.Fatal(*bootstrap.ParallelGroup)
	}
	if mkdir := findPurpose(t, plan.Commands, "create bundle workspace"); mkdir.ParallelGroup != nil {
		t.Fatal(*mkdir.ParallelGroup)
	}
	cloneApp := findPurpose(t, plan.Commands, "clone app")
	cloneLib := findPurpose(t, plan.Commands, "clone libs/lib")
	if cloneApp.ParallelGroup == nil {
		t.Fatal("clone app joins no parallel group")
	}
	if !reflect.DeepEqual(cloneApp.ParallelGroup, cloneLib.ParallelGroup) {
		t.Fatalf("%v %v", cloneApp.ParallelGroup, cloneLib.ParallelGroup)
	}
}

// Hard-won: 9805bf67: Podman workers accumulated zombie children after turns.
func TestPodmanContainersReapZombiesAndAppleContainersKeepTheirDefaults(t *testing.T) {
	podman := mustProvisionPlan(t, containerTarget(core.TargetsTargetTemplateLocalPodman,
		containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, nil, core.TargetsPodmanWorkspaceStorage{})), nil, nil)
	if !containsWindow(podman.Commands[0].Args, "run", "--init") {
		t.Fatalf("%q", podman.Commands[0].Args)
	}

	remote := mustProvisionPlan(t, sshContainerTarget(core.TargetsTargetTemplateSshPodman,
		containerTemplate("dev:1", core.ImagePullPolicyAuto, nil, core.TargetsPodmanWorkspaceStorage{})), nil, nil)
	if !strings.Contains(lastArg(&remote.Commands[0]), "'podman' 'run' '--init'") {
		t.Fatal(lastArg(&remote.Commands[0]))
	}

	apple := mustProvisionPlan(t, containerTarget(core.TargetsTargetTemplateAppleContainer,
		containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, nil, core.TargetsPodmanWorkspaceStorage{})), nil, nil)
	if apple.Commands[1].Args[0] != "run" {
		t.Fatalf("%q", apple.Commands[1].Args)
	}
	if slices.Contains(apple.Commands[1].Args, "--init") {
		t.Fatalf("%q", apple.Commands[1].Args)
	}
}

// `never` means never, including the daemon's own background download.
func TestANeverPolicyIsNotPrePulled(t *testing.T) {
	if _, ok := core.ImageRefreshFor(core.ImageHost{Kind: core.ImageHostLocalPodman}, "ghcr.io/example/dev:latest", nil, core.ImagePullPolicyNever); ok {
		t.Fatal("a never policy asked for no background download")
	}
}

// Apple's `container` CLI spells both commands differently from Podman and
// Docker: `image pull` rather than `pull`, no `--format` on `image inspect`,
// no `--platform` (it runs native images only), and no `-f` on `image prune`.
// Hard-won: 8c5355ea: Auto images left first Create waiting for a background download.
func TestAnAppleContainerImageRefreshUsesTheContainerCLI(t *testing.T) {
	refresh, ok := core.ImageRefreshFor(core.ImageHost{Kind: core.ImageHostAppleContainer}, "ghcr.io/example/dev:latest", stringPointer("linux/arm64"), core.ImagePullPolicyAuto)
	if !ok {
		t.Fatal("an apple container image is downloaded in the background")
	}

	if refresh.Host.Label() != "apple container" {
		t.Fatal(refresh.Host.Label())
	}
	for _, check := range []struct {
		command core.CommandSpec
		args    []string
	}{
		{refresh.Pull, []string{"image", "pull", "ghcr.io/example/dev:latest"}},
		{refresh.ImageID, []string{"image", "inspect", "ghcr.io/example/dev:latest"}},
		{refresh.Prune, []string{"image", "prune"}},
	} {
		if check.command.Program != "container" {
			t.Fatal(check.command.Program)
		}
		if !slices.Equal(check.command.Args, check.args) {
			t.Fatalf("%q", check.command.Args)
		}
	}
}

func pullArgument(args []string) (string, bool) {
	index := slices.IndexFunc(args, func(argument string) bool { return strings.HasPrefix(argument, "--pull=") })
	if index < 0 {
		return "", false
	}
	return args[index], true
}

func mustContainerRunArgs(t *testing.T, engine string, template core.TargetsContainerTemplate) []string {
	t.Helper()
	args, err := containerRunArgs(engine, &template, "container-name", testSession, nil, nil, "/workspace", true)
	if err != nil {
		t.Fatal(err)
	}
	return args
}

// A launch never waits on a registry under the default policy. The daemon
// refreshes remote `:latest` images in the background instead, so a session
// starts from whatever the host already has.
// Hard-won: 49a5b3e9: default image refreshes delayed session startup by minutes.
func TestAnAutomaticPullPolicyNeverPullsDuringALaunch(t *testing.T) {
	for _, engine := range []string{"podman", "docker"} {
		for _, image := range []string{
			"ghcr.io/example/dev:latest",
			"ghcr.io/example/dev",
			"ghcr.io/example/dev:1.2.3",
			"localhost/example/dev:latest",
			"local/example:latest",
			"ghcr.io/example/dev@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
		} {
			args := mustContainerRunArgs(t, engine, containerTemplate(image, core.ImagePullPolicyAuto, nil, core.TargetsPodmanWorkspaceStorage{}))
			pull, found := pullArgument(args)
			// Podman spells "use the cached image" as no flag at all; Docker
			// needs the flag.
			if engine == "podman" {
				if found {
					t.Fatalf("unexpected %s pull policy for %s: %s", engine, image, pull)
				}
			} else if !found || pull != "--pull=missing" {
				t.Fatalf("unexpected %s pull policy for %s: %s", engine, image, pull)
			}
		}
	}
}

// A configured policy is a decision about launches, and it survives.
func TestAnExplicitNewerPullPolicyStillPullsDuringALaunch(t *testing.T) {
	for _, image := range []string{"ghcr.io/example/dev:latest", "ghcr.io/example/dev:1.2.3"} {
		args := mustContainerRunArgs(t, "podman", containerTemplate(image, core.ImagePullPolicyNewer, nil, core.TargetsPodmanWorkspaceStorage{}))
		if !slices.Contains(args, "--pull=newer") {
			t.Fatalf("explicit newer policy lost for %s: %q", image, args)
		}
	}
}

func TestExplicitPodmanPullPolicyOverridesImageTagDefaults(t *testing.T) {
	for _, testCase := range []struct {
		policy   core.ImagePullPolicy
		expected string
	}{
		{core.ImagePullPolicyAlways, "--pull=always"},
		{core.ImagePullPolicyNewer, "--pull=newer"},
		{core.ImagePullPolicyMissing, ""},
		{core.ImagePullPolicyNever, "--pull=never"},
	} {
		args := mustContainerRunArgs(t, "podman", containerTemplate("ghcr.io/example/dev:1.2.3", testCase.policy, nil, core.TargetsPodmanWorkspaceStorage{}))
		if pull, _ := pullArgument(args); pull != testCase.expected {
			t.Fatalf("%s: %q", testCase.policy, pull)
		}
	}
}

func TestDockerPullPolicyUsesSupportedDigestAwareRunModes(t *testing.T) {
	for _, testCase := range []struct {
		policy   core.ImagePullPolicy
		expected string
	}{
		{core.ImagePullPolicyAlways, "--pull=always"},
		{core.ImagePullPolicyNewer, "--pull=always"},
		{core.ImagePullPolicyMissing, "--pull=missing"},
		{core.ImagePullPolicyNever, "--pull=never"},
	} {
		args := mustContainerRunArgs(t, "docker", containerTemplate("ghcr.io/example/dev:1.2.3", testCase.policy, nil, core.TargetsPodmanWorkspaceStorage{}))
		if !slices.Contains(args, testCase.expected) {
			t.Fatalf("%q", args)
		}
		if slices.Contains(args, "--pull=newer") {
			t.Fatalf("%q", args)
		}
	}
}

// New rootless Podman containers store uid 0 as their configured user and
// pin HOME to the image's reference home. Podman exec then uses the recorded
// identity, including the original image user in legacy containers.
func TestPodmanContainersUseRootInTheDefaultNamespace(t *testing.T) {
	template := containerTemplate("ghcr.io/example/dev:1.2.3", core.ImagePullPolicyMissing, nil, core.TargetsPodmanWorkspaceStorage{})
	podman := mustContainerRunArgs(t, "podman", template)
	if !containsWindow(podman, "--user", "0:0") {
		t.Fatalf("%q", podman)
	}
	if !slices.Contains(podman, "--cap-add=PERFMON") {
		t.Fatalf("%q", podman)
	}
	if !containsWindow(podman, "--security-opt", "label=disable") {
		t.Fatalf("SELinux would deny the unlabeled host cache and workspace mounts: %q", podman)
	}
	if !containsWindow(podman, "--env", "HOME=/home/hel") {
		t.Fatalf("%q", podman)
	}
	if slices.ContainsFunc(podman, func(argument string) bool { return strings.HasPrefix(argument, "--userns") }) {
		t.Fatalf("%q", podman)
	}

	for _, engine := range []string{"docker", "container"} {
		args := mustContainerRunArgs(t, engine, template)
		if slices.ContainsFunc(args, func(argument string) bool { return strings.HasPrefix(argument, "--userns") }) {
			t.Fatalf("%s must keep its own user namespace", engine)
		}
		if slices.Contains(args, "0:0") {
			t.Fatalf("%s: %q", engine, args)
		}
		if slices.Contains(args, "label=disable") {
			t.Fatalf("%s does not need Podman's SELinux adjustment: %q", engine, args)
		}
		if slices.Contains(args, "--cap-add=PERFMON") {
			t.Fatalf("%s: %q", engine, args)
		}
	}
}

func TestPodmanExecUsesTheContainerRecordedUser(t *testing.T) {
	podman := core.ContainerExec("podman", "container-name", []string{"id"})
	if !slices.Equal(podman.Args, []string{"exec", "-i", "container-name", "id"}) {
		t.Fatalf("%q", podman.Args)
	}
	docker := core.ContainerExec("docker", "container-name", []string{"id"})
	if !slices.Equal(docker.Args, []string{"exec", "-i", "container-name", "id"}) {
		t.Fatalf("%q", docker.Args)
	}
}

func threeAttachments() []core.AdditionalMount {
	return []core.AdditionalMount{
		{Source: "/host/cache", Destination: "/mnt/cache", Access: core.MountAccessCow},
		{Source: "/host/models", Destination: "/mnt/models", Access: core.MountAccessRo},
		{Source: "/host/build-cache", Destination: "/mnt/build-cache", Access: core.MountAccessRw},
	}
}

func TestPodmanAdditionalMountsUseCopyOnWriteOverlayVolumes(t *testing.T) {
	plan := mustProvisionPlan(t, containerTarget(core.TargetsTargetTemplateLocalPodman,
		containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, nil, core.TargetsPodmanWorkspaceStorage{})), threeAttachments(), nil)

	for _, volume := range []string{"/host/cache:/mnt/cache:O", "/host/models:/mnt/models:ro", "/host/build-cache:/mnt/build-cache:rw"} {
		if !containsWindow(plan.Commands[0].Args, "--volume", volume) {
			t.Fatalf("%q lacks %s", plan.Commands[0].Args, volume)
		}
	}
}

func TestAdditionalMountsKeepTheLegacyShapeUnlessReadWrite(t *testing.T) {
	parse := func(text string) core.MountAccess {
		var mount core.AdditionalMount
		if err := json.Unmarshal([]byte(text), &mount); err != nil {
			t.Fatal(err)
		}
		return mount.Access
	}
	if got := parse(`{"source":"/a","destination":"/b","read_only":true}`); got != core.MountAccessRo {
		t.Fatal(got)
	}
	if got := parse(`{"source":"/a","destination":"/b","read_only":false}`); got != core.MountAccessCow {
		t.Fatal(got)
	}
	if got := parse(`{"source":"/a","destination":"/b"}`); got != core.MountAccessCow {
		t.Fatal(got)
	}

	mount := func(access core.MountAccess) core.AdditionalMount {
		return core.AdditionalMount{Source: "/a", Destination: "/b", Access: access}
	}
	asValue := func(value any) any {
		encoded, err := json.Marshal(value)
		if err != nil {
			t.Fatal(err)
		}
		var decoded any
		if err := json.Unmarshal(encoded, &decoded); err != nil {
			t.Fatal(err)
		}
		return decoded
	}
	// Older builds reject unknown fields, so only read-write may add one.
	if got := asValue(mount(core.MountAccessRo)); !reflect.DeepEqual(got, map[string]any{"source": "/a", "destination": "/b", "read_only": true}) {
		t.Fatalf("%v", got)
	}
	if got := asValue(mount(core.MountAccessCow)); !reflect.DeepEqual(got, map[string]any{"source": "/a", "destination": "/b", "read_only": false}) {
		t.Fatalf("%v", got)
	}
	readWrite := mount(core.MountAccessRw)
	encoded, err := json.Marshal(readWrite)
	if err != nil {
		t.Fatal(err)
	}
	var decoded any
	if err := json.Unmarshal(encoded, &decoded); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(decoded, map[string]any{"source": "/a", "destination": "/b", "read_only": false, "access": "rw"}) {
		t.Fatalf("%v", decoded)
	}
	var roundTrip core.AdditionalMount
	if err := json.Unmarshal(encoded, &roundTrip); err != nil {
		t.Fatal(err)
	}
	if roundTrip != readWrite {
		t.Fatalf("%v", roundTrip)
	}
}

func TestDockerAdditionalMountsUseManagedOverlayAndReadOnlyBindVolumes(t *testing.T) {
	plan := mustProvisionPlan(t, containerTarget(core.TargetsTargetTemplateLocalDocker,
		containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, nil, core.TargetsPodmanWorkspaceStorage{})), threeAttachments(), nil)

	create := plan.Commands[0]
	name := mustResourceName(t, testSession)
	volume := name + "-mount-0"
	if create.Program != "sh" {
		t.Fatal(create.Program)
	}
	if !create.CreatesTarget {
		t.Fatal("the overlay launch does not create the target")
	}
	for _, expected := range []string{"docker volume create", "--opt type=overlay", "lowerdir=$source,upperdir=$upper,workdir=$work", "refusing foreign Docker volume"} {
		if !strings.Contains(create.Args[1], expected) {
			t.Fatalf("script lacks %s", expected)
		}
	}
	if !slices.Contains(create.Args, "/host/cache") || !slices.Contains(create.Args, volume) {
		t.Fatalf("%q", create.Args)
	}
	if !containsWindow(create.Args, "--volume", volume+":/mnt/cache") {
		t.Fatalf("%q", create.Args)
	}
	if !containsWindow(create.Args, "--volume", "/host/models:/mnt/models:ro") {
		t.Fatalf("%q", create.Args)
	}
	// A read-write mount is a plain bind: no overlay volume is created for it.
	if !containsWindow(create.Args, "--volume", "/host/build-cache:/mnt/build-cache") {
		t.Fatalf("%q", create.Args)
	}
	// The overlay script reads its own leading arguments and then one
	// `ordinal source volume` triple per copy-on-write attachment, up to the
	// `--` separator. Only the copy-on-write source is handed to it.
	separator := slices.Index(create.Args, "--")
	if separator < 0 {
		t.Fatal("the overlay script separates its arguments from the run command")
	}
	if !slices.Equal(create.Args[2:7], []string{"hel-docker-run", testSession, name, "ubuntu:24.04", "missing"}) {
		t.Fatalf("%q", create.Args[2:7])
	}
	if !slices.Equal(create.Args[8:separator], []string{"0", "/host/cache", volume}) {
		t.Fatalf("%q", create.Args[8:separator])
	}
	if !slices.Contains(create.Args, "--pull=missing") || !slices.Contains(create.Args, "--init") {
		t.Fatalf("%q", create.Args)
	}

	clone := findPurpose(t, plan.Commands, "clone app")
	if clone.Program != "docker" {
		t.Fatal(clone.Program)
	}
	if !slices.Equal(clone.Args[:4], []string{"exec", "-i", name, "git"}) {
		t.Fatalf("%q", clone.Args)
	}
}

func TestFilesystemProbeAnswersPositionallyAndReachesThePodmanHost(t *testing.T) {
	executor := newPodmanPreflightExecutor(podmanOutput("ext4\nnfs\n"))
	paths := []string{"/host/cache", "/host/models"}

	types, err := ProbeFilesystemTypes(nil, paths, executor)
	if err != nil || !slices.Equal(types, []string{"ext4", "nfs"}) {
		t.Fatalf("%q %v", types, err)
	}
	seen := executor.seen
	if seen[0].Program != "sh" || seen[0].Args[0] != "-c" {
		t.Fatalf("%v", seen[0])
	}
	if !slices.Equal(seen[0].Args[3:], []string{"/host/cache", "/host/models"}) {
		t.Fatalf("%q", seen[0].Args)
	}

	remote := newPodmanPreflightExecutor(podmanOutput("ext4\nnfs\n"))
	ssh := testSSH()
	if _, err := ProbeFilesystemTypes(&ssh, paths, remote); err != nil {
		t.Fatal(err)
	}
	if remote.seen[0].Program != "ssh" {
		t.Fatal(remote.seen[0].Program)
	}
	line := lastArg(&remote.seen[0])
	if !strings.HasPrefix(UnwrappedRemoteLine(line), "'sh' '-c' ") ||
		!strings.HasSuffix(line, "'hel-filesystem' '/host/cache' '/host/models'") {
		t.Fatalf("%q", remote.seen[0].Args)
	}
}

// macOS output, recorded from `df -P` and `mount`, where device names and
// mount points carry spaces and `%`, names share prefixes, and one point is
// mounted twice. Every awkward case lands in one run of the real script.
// Hard-won: #1330: a `%` in a macOS mount point cut the mount point short and lost its filesystem.
// Hard-won: #1330: macOS `/bin/sh`, bash 3.2, could not parse the probe's `case` patterns inside `$(...)`.
func TestMacosFilesystemProbeReadsAwkwardMountPointsFromDfAndMount(t *testing.T) {
	skipOnWindows(t, "Unix only")
	bin := t.TempDir()
	core.InstallFakeCommand(bin, "uname", "echo Darwin\n")
	core.InstallFakeCommand(bin, "mount", `cat <<'EOF'
/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)
devfs on /dev (devfs, local, nobrowse)
/dev/disk3s5 on /System/Volumes/Data (apfs, local, journaled, nobrowse, protect, root data)
map auto_home on /System/Volumes/Data/home (autofs, automounted, nobrowse)
//builder@nas._smb._tcp.local/My Share on /Volumes/My Share (smbfs, nodev, nosuid, mounted by builder)
/dev/disk5s1 on /Volumes/My Share 2 (exfat, local, nodev, nosuid, noowners)
/dev/disk6s1 on /Volumes/50% done (hfs, local, journaled)
map -fstab on /Volumes/stack (autofs, automounted)
nas:/export on /Volumes/stack (nfs, nodev, nosuid, mounted by builder)
EOF
`)
	core.InstallFakeCommand(bin, "df", `echo 'Filesystem                              512-blocks      Used Available Capacity  Mounted on'
case $3 in
    /Users/*) echo '/dev/disk3s5                             965595304 773624968 154366136    84%    /System/Volumes/Data' ;;
    '/Volumes/My Share 2'/*) echo '/dev/disk5s1  1000 100 900  10%    /Volumes/My Share 2' ;;
    '/Volumes/My Share'/*) echo '//builder@nas._smb._tcp.local/My Share  1000 100 900  10%    /Volumes/My Share' ;;
    '/Volumes/50% done'/*) echo '/dev/disk6s1  1000 500 500  50%    /Volumes/50% done' ;;
    /Volumes/stack/*) echo 'nas:/export  1000 100 900  10%    /Volumes/stack' ;;
    /nowhere/*) echo 'ghost  1000 100 900  10%    /nowhere' ;;
    *) echo '/dev/disk3s1s1  1000 100 900  10%    /' ;;
esac
`)
	probe := func(paths ...string) core.CommandOutput {
		args := append([]string{"-c", filesystemTypesScript, "hel-filesystem"}, paths...)
		command := core.NewCommandSpec("/bin/sh", args).WithPurpose("test macOS filesystem probe")
		command.Env["PATH"] = bin + ":" + os.Getenv("PATH")
		output, err := core.ProcessExecutor{}.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		return output
	}

	output := probe(
		"/Users/builder/.cache/mjolnir/build",
		"/Volumes/My Share/models",
		"/Volumes/My Share 2/scratch",
		"/Volumes/50% done/cache",
		"/Volumes/stack/data",
		"/opt/tools",
	)
	if got := preflightLossy(output.Stdout); got != "apfs\nsmbfs\nexfat\nhfs\nnfs\napfs\n" {
		t.Fatalf("%q: %s", got, output.Stderr)
	}

	// A mount point `mount` does not list fails visibly instead of guessing.
	output = probe("/nowhere/cache")
	if output.Status == 0 {
		t.Fatal("the probe guessed a filesystem for an unlisted mount point")
	}
	if !strings.Contains(preflightLossy(output.Stderr), "mount lists no filesystem holding /nowhere/cache") {
		t.Fatal(preflightLossy(output.Stderr))
	}
}

// The probe asks the host's own tools, whose output differs by platform:
// macOS `stat` cannot name a filesystem, so there it reads `df` and `mount`.
func TestFilesystemProbeNamesThisHostsFilesystemsInOrder(t *testing.T) {
	skipOnWindows(t, "Unix only")
	root := t.TempDir()
	spaced := filepath.Join(root, "a directory with spaces")
	if err := os.Mkdir(spaced, 0o755); err != nil {
		t.Fatal(err)
	}
	paths := []string{root, spaced}

	filesystems, err := ProbeFilesystemTypes(nil, paths, core.ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}

	if len(filesystems) != 2 {
		t.Fatalf("%q", filesystems)
	}
	for _, filesystem := range filesystems {
		if filesystem == "" || strings.ContainsAny(filesystem, " \t\n\r\v\f") {
			t.Fatalf("%q", filesystems)
		}
	}
	if filesystems[0] != filesystems[1] {
		t.Fatalf("%q", filesystems)
	}
}

func TestFilesystemProbeRejectsAPartialOrFailedAnswer(t *testing.T) {
	short := newPodmanPreflightExecutor(podmanOutput("ext4\n"))
	_, err := ProbeFilesystemTypes(nil, []string{"/host/cache", "/host/models"}, short)
	if err == nil || !strings.Contains(err.Error(), "named 1 filesystems for 2 directories") {
		t.Fatalf("%v", err)
	}

	failed := newPodmanPreflightExecutor(core.CommandOutput{
		Status: 1,
		Stdout: []byte{},
		Stderr: []byte("stat: cannot read file system information"),
	})
	_, err = ProbeFilesystemTypes(nil, []string{"/host/cache"}, failed)
	if err == nil || !strings.Contains(err.Error(), "cannot read file system information") {
		t.Fatalf("%v", err)
	}
}

func TestAppleAdditionalMountsUseReadOnlyBindFallback(t *testing.T) {
	mounts := []core.AdditionalMount{{Source: "/Users/me/assets", Destination: "/mnt/assets", Access: core.MountAccessCow}}
	plan := mustProvisionPlan(t, containerTarget(core.TargetsTargetTemplateAppleContainer,
		containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, nil, core.TargetsPodmanWorkspaceStorage{})), mounts, nil)

	if !containsWindow(plan.Commands[1].Args, "--mount", "type=bind,source=/Users/me/assets,target=/mnt/assets,readonly") {
		t.Fatalf("%q", plan.Commands[1].Args)
	}
}

func TestApplePullPolicyPreparesMutableAndPinnedImages(t *testing.T) {
	for _, testCase := range []struct {
		policy   core.ImagePullPolicy
		expected []string
	}{
		{core.ImagePullPolicyAlways, []string{"image", "pull", "ghcr.io/example/dev:1"}},
		{core.ImagePullPolicyNewer, []string{"image", "pull", "ghcr.io/example/dev:1"}},
		{core.ImagePullPolicyNever, []string{"image", "inspect", "ghcr.io/example/dev:1"}},
		{core.ImagePullPolicyMissing, nil},
	} {
		template := containerTemplate("ghcr.io/example/dev:1", testCase.policy, nil, core.TargetsPodmanWorkspaceStorage{})
		commands := appleImagePrepareCommands(&template)
		if testCase.expected == nil {
			if len(commands) != 0 {
				t.Fatalf("%s: %v", testCase.policy, commands)
			}
			continue
		}
		if len(commands) == 0 || !slices.Equal(commands[0].Args, testCase.expected) {
			t.Fatalf("%s: %v", testCase.policy, commands)
		}
	}
}

func TestAppleCleanupConfirmsAbsenceByTheExactProvisionedContainerID(t *testing.T) {
	containerID := mustResourceName(t, testSession)
	locator := &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorAppleContainer, ContainerID: containerID}
	stillLive := newPodmanPreflightExecutor(podmanOutput(fmt.Sprintf("unrelated-id\n%s\n", containerID)))

	absent, err := CleanupTargetIsConfirmedAbsent(locator, testSession, stillLive)
	if err != nil || absent {
		t.Fatalf("%v %v", absent, err)
	}
	if !slices.Equal(stillLive.seen[0].Args, []string{"list", "--all", "--quiet"}) {
		t.Fatalf("%q", stillLive.seen[0].Args)
	}

	gone := newPodmanPreflightExecutor(podmanOutput("unrelated-id\n"))
	if absent, err := CleanupTargetIsConfirmedAbsent(locator, testSession, gone); err != nil || !absent {
		t.Fatalf("%v %v", absent, err)
	}

	failed := newPodmanPreflightExecutor(core.CommandOutput{Status: 1, Stdout: []byte{}, Stderr: []byte("service unavailable")})
	if _, err := CleanupTargetIsConfirmedAbsent(locator, testSession, failed); err == nil {
		t.Fatal("an unavailable service confirmed absence")
	}
}

func TestDockerCleanupConfirmationDistinguishesLiveAbsentAndUnreachable(t *testing.T) {
	locator := &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalDocker, ContainerID: mustResourceName(t, testSession)}
	live := newPodmanPreflightExecutor(core.CommandOutput{Status: 1, Stdout: []byte{}, Stderr: []byte{}})
	if absent, err := CleanupTargetIsConfirmedAbsent(locator, testSession, live); err != nil || absent {
		t.Fatalf("%v %v", absent, err)
	}
	if !strings.Contains(live.seen[0].Args[1], "then exit 1") {
		t.Fatal(live.seen[0].Args[1])
	}

	gone := newPodmanPreflightExecutor(podmanOutput(""))
	if absent, err := CleanupTargetIsConfirmedAbsent(locator, testSession, gone); err != nil || !absent {
		t.Fatalf("%v %v", absent, err)
	}

	unreachable := newPodmanPreflightExecutor(core.CommandOutput{Status: 2, Stdout: []byte{}, Stderr: []byte("daemon unavailable")})
	if _, err := CleanupTargetIsConfirmedAbsent(locator, testSession, unreachable); err == nil {
		t.Fatal("an unreachable daemon confirmed absence")
	}
}

func TestPodmanCleanupConfirmationChecksContainerAndWorkspaceStorage(t *testing.T) {
	name := mustResourceName(t, testSession)
	volume := name + "-workspace"
	locator := localPodmanLocator(name, core.TargetsPodmanWorkspaceLocator{Kind: core.TargetsPodmanWorkspaceLocatorVolume, Name: volume})
	live := newPodmanPreflightExecutor(core.CommandOutput{Status: 1, Stdout: []byte{}, Stderr: []byte{}})
	if absent, err := CleanupTargetIsConfirmedAbsent(locator, testSession, live); err != nil || absent {
		t.Fatalf("%v %v", absent, err)
	}
	command := live.seen[0]
	if command.Program != "sh" {
		t.Fatal(command.Program)
	}
	if !strings.Contains(command.Args[1], "podman container exists") || !strings.Contains(command.Args[1], "podman volume exists") {
		t.Fatal(command.Args[1])
	}
	if !slices.Contains(command.Args, volume) {
		t.Fatalf("%q", command.Args)
	}

	gone := newPodmanPreflightExecutor(podmanOutput(""))
	if absent, err := CleanupTargetIsConfirmedAbsent(locator, testSession, gone); err != nil || !absent {
		t.Fatalf("%v %v", absent, err)
	}

	unreachable := newPodmanPreflightExecutor(core.CommandOutput{Status: 2, Stdout: []byte{}, Stderr: []byte("daemon unavailable")})
	if _, err := CleanupTargetIsConfirmedAbsent(locator, testSession, unreachable); err == nil {
		t.Fatal("an unreachable daemon confirmed absence")
	}
}

func TestRemotePodmanCleanupConfirmationUsesTheRecordedHelperResource(t *testing.T) {
	name := mustResourceName(t, testSession)
	resource := name + "-workspace"
	locator := sshPodmanLocator(name, core.TargetsPodmanWorkspaceLocator{
		Kind:     core.TargetsPodmanWorkspaceLocatorHostPath,
		Path:     "/srv/mj-workspaces/" + resource,
		Helper:   []string{"sudo", "-n", "/opt/mj-helper"},
		Resource: resource,
	})
	gone := newPodmanPreflightExecutor(podmanOutput(""))

	if absent, err := CleanupTargetIsConfirmedAbsent(locator, testSession, gone); err != nil || !absent {
		t.Fatalf("%v %v", absent, err)
	}
	command := gone.seen[0]
	if command.Program != "ssh" {
		t.Fatal(command.Program)
	}
	remote := lastArg(&command)
	for _, expected := range []string{"'/opt/mj-helper'", "status", resource} {
		if !strings.Contains(remote, expected) {
			t.Fatalf("%s lacks %s", remote, expected)
		}
	}
}

func TestSetupSmokeTestRemovesAContainerAfterAFailedExec(t *testing.T) {
	executor := newFakeExecutor(1)

	err := RunSetupSmokeTest(containerTarget(core.TargetsTargetTemplateAppleContainer,
		containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, nil, core.TargetsPodmanWorkspaceStorage{})), "setup-123", executor)
	if err == nil {
		t.Fatal("the smoke test passed after a failed exec")
	}
	if len(executor.seen) != 3 {
		t.Fatalf("%d commands", len(executor.seen))
	}
	if executor.seen[2].Args[0] != "rm" {
		t.Fatalf("%q", executor.seen[2].Args)
	}
}

func TestHostCapacityCountsZfsArcAboveItsMinimumAsAvailableMemory(t *testing.T) {
	skipUnlessLinux(t)
	directory := t.TempDir()
	writeFile(t, filepath.Join(directory, "meminfo"), "MemTotal: 1000 kB\nMemAvailable: 400 kB\nSwapTotal: 10 kB\nSwapFree: 8 kB\n")
	sample := func() core.CommandOutput {
		command := core.NewCommandSpec("sh", []string{"-c", hostResourceUsageScript, "sh", directory}).
			WithPurpose("test host available-memory accounting")
		output, err := core.ProcessExecutor{}.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		return output
	}

	withoutArc := sample()
	if withoutArc.Status != 0 {
		t.Fatalf("%d: %s", withoutArc.Status, withoutArc.Stderr)
	}
	values := parseKeyValues(withoutArc.Stdout)
	if values["memory.current"] != "614400" || values["memory.max"] != "1024000" {
		t.Fatalf("%v", values)
	}

	arcstats := filepath.Join(directory, "spl/kstat/zfs")
	mkdirAll(t, arcstats)
	writeFile(t, filepath.Join(arcstats, "arcstats"), "c_min 4 102400\nsize 4 409600\n")

	withArc := sample()
	if withArc.Status != 0 {
		t.Fatalf("%d: %s", withArc.Status, withArc.Stderr)
	}
	values = parseKeyValues(withArc.Stdout)
	if values["memory.current"] != "307200" || values["memory.max"] != "1024000" {
		t.Fatalf("%v", values)
	}
}

// The host probe measures storage and still samples CPU and memory: the
// storage loop must not leave its paths where the resource script reads its
// optional /proc override.
// Hard-won: 540c9202: full-disk hosts stopped workers while recovery retried writes endlessly.
func TestHostCapacityProbeMeasuresStorageBesideCPUAndMemory(t *testing.T) {
	skipUnlessLinux(t)
	directory := t.TempDir()
	ssh := core.SshTarget{Destination: "host", SSHArgs: []string{}}
	remote := SSHHostCapacityCommand(&ssh, []string{filepath.Join(directory, "missing"), ".cache/mjolnir"})
	// Run the remote words locally rather than over SSH.
	words := lastArg(&remote)
	command := core.NewCommandSpec("sh", []string{"-c", words}).WithPurpose("test host probe")
	output, err := core.ProcessExecutor{}.Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("%d: %s", output.Status, output.Stderr)
	}
	usage, err := ParseHostCapacity(output.Stdout, "host")
	if err != nil {
		t.Fatal(err)
	}
	if usage.MemoryTotalBytes == 0 {
		t.Fatalf("%+v", usage)
	}
	if len(usage.Storage) == 0 || usage.Storage[0].Filesystems[0].TotalBytes == 0 {
		t.Fatalf("%+v", usage.Storage)
	}
	if usage.MemoryPool {
		t.Fatal("a host's RAM read as a pool")
	}

	// A Nitro host appends the engine's report, and the pool replaces host
	// RAM as the total. The engine and `sudo` are stand-ins that record
	// nothing: the point is the line the probe adds and how it is read.
	home := t.TempDir()
	engine := filepath.Join(home, core.MicrovmEngineWorker)
	mkdirAll(t, filepath.Dir(engine))
	writeExecutable(t, engine, "#!/bin/sh\nprintf '{\\n \"engine_version\":\"2.70.0\",\"state_format\":1,\"firecracker_version\":\"1.18.0\",\"kernel_version\":\"6.18.56\",\\n \"memory_pool_total_mib\":49152,\"memory_pool_free_mib\":45054,\"slots_in_use\":1,\"slots_total\":80,\"cpus\":16}\\n'\n")
	fakes := t.TempDir()
	core.InstallFakeCommand(fakes, "sudo", "#!/bin/sh\nshift\nexec \"$@\"\n")
	nitro := SSHNitroHostCapacityCommand(&ssh, []string{".cache/mjolnir"})
	probe := core.NewCommandSpec("sh", []string{"-c", lastArg(&nitro)}).WithPurpose("test Nitro host probe")
	probe.Env["HOME"] = home
	probe.Env["PATH"] = fakes + ":/usr/bin:/bin"
	output, err = core.ProcessExecutor{}.Execute(&probe)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("%d: %s", output.Status, output.Stderr)
	}
	usage, err = ParseHostCapacity(output.Stdout, "host")
	if err != nil {
		t.Fatal(err)
	}
	if !usage.MemoryPool {
		t.Fatal("a Nitro host's pool was not read as a pool")
	}
	if usage.MemoryTotalBytes != 49152<<20 {
		t.Fatal(usage.MemoryTotalBytes)
	}
	if usage.MemoryUsedBytes != 4098<<20 {
		t.Fatal(usage.MemoryUsedBytes)
	}
}

func TestSSHDirectoryCheckQuotesTheSourceAndDistinguishesMissing(t *testing.T) {
	ssh := testSSH()
	present := newFakeExecutor(-1)
	if found, err := core.SSHDirectoryExists(&ssh, "/srv/user's data", present); err != nil || !found {
		t.Fatalf("%v %v", found, err)
	}
	command := present.seen[0]
	if command.Program != "ssh" {
		t.Fatal(command.Program)
	}
	remoteCommand := lastArg(&command)
	if !strings.HasPrefix(UnwrappedRemoteLine(remoteCommand), "'test' '-d' ") {
		t.Fatal(remoteCommand)
	}
	if !strings.Contains(remoteCommand, `'/srv/user'\''s data'`) {
		t.Fatal(remoteCommand)
	}

	missing := newFakeExecutor(0)
	if found, err := core.SSHDirectoryExists(&ssh, "/missing", missing); err != nil || found {
		t.Fatalf("%v %v", found, err)
	}
}

// Launch finding R3-6: a host-key refusal during `mj new --target <ssh>`
// reached the CLI as "500 ... the daemon log records the reason under
// reference ...". The caller gets ssh's own words and what to do, as a
// refusal (a 4xx), and the sentence names no host.
// Hard-won: 1afbc63f: SSH host-key refusal surfaced to callers only as an opaque server error.
func TestAHostKeyRefusalAtTheRemoteDirectoryCheckTellsTheCallerWhatToDo(t *testing.T) {
	refused := func(stderr string) error {
		executor := newPodmanPreflightExecutor(core.CommandOutput{Status: 255, Stdout: []byte{}, Stderr: []byte(stderr)})
		ssh := testSSH()
		err := core.ValidateBareProjectDirectory(&ssh, "/srv/project", executor)
		if err == nil {
			t.Fatal("a refused host passed the directory check")
		}
		return err
	}
	err := refused("No ED25519 host key is known for 203.0.113.9 and you have requested strict checking.\r\nHost key verification failed.\r\n")
	refusal, ok := core.RefusalOf(err)
	if !ok {
		t.Fatal("a host-key refusal is a reason the caller may see, not a log reference")
	}
	if refusal.Kind() != core.RefusalKindPrecondition {
		t.Fatal(refusal.Kind())
	}
	message := refusal.Message()
	for _, expected := range []string{"Host key verification failed", "known_hosts", "-o StrictHostKeyChecking=accept-new"} {
		if !strings.Contains(message, expected) {
			t.Fatalf("%s lacks %s", message, expected)
		}
	}
	if strings.Contains(message, "203.0.113.9") {
		t.Fatal(message)
	}
	// The daemon log keeps ssh's full text.
	if !strings.Contains(alternateErrorText(err), "203.0.113.9") {
		t.Fatal(alternateErrorText(err))
	}

	changed := refused("@@@@@@@@\r\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\r\nHost key verification failed.\r\n")
	refusal, ok = core.RefusalOf(changed)
	if !ok || !strings.Contains(refusal.Message(), "ssh-keygen -R") {
		t.Fatalf("%v", changed)
	}

	// Any other ssh failure stays internal.
	other := refused("ssh: connect to host example.test port 22: Connection refused\r\n")
	if _, ok := core.RefusalOf(other); ok {
		t.Fatal(other)
	}
}

// Launch finding R6-5: the host-key refusal said the key "is not in
// ~/.ssh/known_hosts" when the machine's extra_args pointed ssh at another
// file with `UserKnownHostsFile` (cli/006). It now names the file the
// options name, and says "the known_hosts file ssh uses" when a config file
// of the machine's own may name one.
// Hard-won: 8e3ea78d: host-key refusal advice named the wrong known_hosts file.
func TestAHostKeyRefusalNamesTheKnownHostsFileSSHWasToldToUse(t *testing.T) {
	const unknown = "No ED25519 host key is known for 203.0.113.9 and you have requested strict checking.\r\nHost key verification failed.\r\n"
	const changed = "@@@@@@@@\r\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\r\nHost key verification failed.\r\n"
	refusal := func(extraArgs []string, stderr string) string {
		target := testSSH()
		target.SSHArgs = append(append([]string{}, extraArgs...), target.SSHArgs...)
		executor := newPodmanPreflightExecutor(core.CommandOutput{Status: 255, Stdout: []byte{}, Stderr: []byte(stderr)})
		err := core.ValidateBareProjectDirectory(&target, "/srv/project", executor)
		refusal, ok := core.RefusalOf(err)
		if !ok {
			t.Fatalf("a host-key refusal reaches the caller: %v", err)
		}
		return refusal.Message()
	}

	// Nothing in the options names a file, so ssh uses its default.
	if message := refusal(nil, unknown); !strings.Contains(message, "not in ~/.ssh/known_hosts") {
		t.Fatal(message)
	}

	for _, args := range [][]string{
		{"-o", "UserKnownHostsFile=/lab/r6-known_hosts", "-o", "StrictHostKeyChecking=yes"},
		{"-oUserKnownHostsFile=/lab/r6-known_hosts"},
		{"-o", "userknownhostsfile /lab/r6-known_hosts"},
	} {
		message := refusal(args, unknown)
		if !strings.Contains(message, "not in /lab/r6-known_hosts") {
			t.Fatalf("%q: %s", args, message)
		}
		if strings.Contains(message, "~/.ssh/known_hosts") {
			t.Fatalf("%q: %s", args, message)
		}
		message = refusal(args, changed)
		if !strings.Contains(message, "saved in /lab/r6-known_hosts") {
			t.Fatalf("%q: %s", args, message)
		}
		if !strings.Contains(message, "`ssh-keygen -f /lab/r6-known_hosts -R`") {
			t.Fatalf("%q: %s", args, message)
		}
	}

	// Several files: ssh reads them all and adds keys to the first.
	several := []string{"-o", "UserKnownHostsFile=/lab/a /lab/b"}
	if message := refusal(several, unknown); !strings.Contains(message, "not in /lab/a or /lab/b") {
		t.Fatal(message)
	}
	if message := refusal(several, changed); !strings.Contains(message, "`ssh-keygen -f /lab/a -R`") {
		t.Fatal(message)
	}

	// A config file of the machine's own may name any file.
	message := refusal([]string{"-F", "/lab/ssh_config"}, unknown)
	if !strings.Contains(message, "not in the known_hosts file ssh uses") {
		t.Fatal(message)
	}
	if strings.Contains(message, "~/.ssh/known_hosts") {
		t.Fatal(message)
	}
}

func TestSSHAnyKindListsFilesAndMarksDirectories(t *testing.T) {
	executor := newPodmanPreflightExecutor(core.CommandOutput{
		Status: 0,
		Stdout: []byte("/srv/projects/\n/srv/prompts.txt\n/srv/pr\uFFFDx/\n/srv/projects/nested/\n"),
		Stderr: []byte{},
	})

	ssh := testSSH()
	matches, err := core.SSHCompletions(&ssh, "/srv/pr", core.CompletionKindAny, executor)
	if err != nil {
		t.Fatal(err)
	}

	if !slices.Equal(matches, []string{"/srv/projects/", "/srv/prompts.txt"}) {
		t.Fatalf("%q", matches)
	}
	if !strings.Contains(lastArg(&executor.seen[0]), "ls -dp -- '/srv/pr'* 2>/dev/null") {
		t.Fatal(lastArg(&executor.seen[0]))
	}
}

// Provisioning has to know exactly when its target came into existence,
// because every step after that owes the target's teardown on failure.
func TestEveryProvisioningPlanNamesTheCommandThatCreatesItsTarget(t *testing.T) {
	container := containerTemplate("ubuntu:24.04", core.ImagePullPolicyAuto, nil, core.TargetsPodmanWorkspaceStorage{})
	ssh := testSSH()
	creating := []struct {
		template *core.TargetsTargetTemplate
		purpose  string
	}{
		{containerTarget(core.TargetsTargetTemplateLocalPodman, container), "start session container"},
		{containerTarget(core.TargetsTargetTemplateLocalDocker, container), "start session container"},
		{containerTarget(core.TargetsTargetTemplateAppleContainer, container), "start session container"},
		{sshContainerTarget(core.TargetsTargetTemplateSshPodman, container), "start remote Podman container"},
		{&core.TargetsTargetTemplate{Kind: core.TargetsTargetTemplateSshBare, SSH: &ssh, WorkspacePrefix: "/srv/hel"}, "create SSH session workspace"},
	}
	for _, entry := range creating {
		plan := mustProvisionPlan(t, entry.template, nil, nil)
		if plan.Description != "provision Mjolnir session "+testSession {
			t.Fatal(plan.Description)
		}
		creation, remainder, ok := plan.SplitAtTargetCreation()
		if !ok {
			t.Fatalf("%s: no target creation", entry.purpose)
		}

		if creation.Commands[len(creation.Commands)-1].Purpose != entry.purpose {
			t.Fatal(creation.Commands[len(creation.Commands)-1].Purpose)
		}
		creators := 0
		for _, command := range creation.Commands {
			if command.CreatesTarget {
				creators++
			}
		}
		if creators != 1 {
			t.Fatalf("%d commands create the target", creators)
		}
		if slices.ContainsFunc(remainder.Commands, func(command core.CommandSpec) bool { return command.CreatesTarget }) {
			t.Fatal("a command after the creation creates the target")
		}
		if len(creation.Commands)+len(remainder.Commands) != len(plan.Commands) {
			t.Fatal("the split lost commands")
		}
		// Cloning a bundle happens against a target that already exists.
		if !slices.ContainsFunc(remainder.Commands, func(command core.CommandSpec) bool { return strings.HasPrefix(command.Purpose, "clone ") }) {
			t.Fatal("cloning happens before the target exists")
		}
	}

	// An existing project directory is never created, so nothing about it
	// can leak.
	bare, err := ProvisionBareProjectPlan(&core.TargetsTargetTemplate{Kind: core.TargetsTargetTemplateLocalBare}, testSession, "/srv/project")
	if err != nil {
		t.Fatal(err)
	}
	if _, _, ok := bare.SplitAtTargetCreation(); ok {
		t.Fatal("a bare project plan creates its target")
	}
}

func awsLocator() *core.TargetsTargetLocator {
	ssh := testSSH()
	return &core.TargetsTargetLocator{
		Kind:       core.TargetsTargetLocatorAwsEc2,
		Profile:    "work",
		Region:     "us-east-2",
		InstanceID: "i-0123456789abcdef0",
		SSH:        &ssh,
		Workspace:  ".local/share/hel/workspaces/" + testSession,
	}
}

func TestLegacyEc2CloseUsesRecordedID(t *testing.T) {
	close, err := ClosePlan(awsLocator(), testSession)
	if err != nil {
		t.Fatal(err)
	}
	if got := lastArg(&close.Scripts()[0]); got != "i-0123456789abcdef0" {
		t.Fatal(got)
	}
}

func TestAWSCloneReceivesGithubTokenOnStdinOnly(t *testing.T) {
	locator := awsLocator()
	token := "ghs_secret-token"
	repositories := testBundle()
	repositories.Repositories[0].URL = stringPointer("https://github.com/example/app.git")
	plan, err := ProvisionOnLocatorPlan(locator, testSession, &repositories, &token)
	if err != nil {
		t.Fatal(err)
	}
	var clones []core.CommandSpec
	for _, command := range plan.Commands {
		if strings.HasPrefix(command.Purpose, "clone ") {
			clones = append(clones, command)
		}
	}
	if len(clones) == 0 {
		t.Fatal("no clone commands")
	}
	for _, command := range clones {
		remote := lastArg(&command)
		if !strings.HasPrefix(UnwrappedRemoteLine(remote), "'sh' '-c' 'IFS= read -r GH_TOKEN") {
			t.Fatal(remote)
		}
		if !strings.Contains(remote, "'credential.https://github.com.helper=' '-c' 'credential.https://github.com.helper=!f()") {
			t.Fatal(remote)
		}
		if !strings.Contains(remote, "'git' '-c'") || !strings.Contains(remote, "'clone'") {
			t.Fatal(remote)
		}
		if strings.Contains(fmt.Sprintf("%+v", command), token) {
			t.Fatal("the rendered command leaks the token")
		}
	}

	bundle := testBundle()
	plan, err = ProvisionOnLocatorPlan(locator, testSession, &bundle, nil)
	if err != nil {
		t.Fatal(err)
	}
	for _, command := range plan.Commands {
		if strings.HasPrefix(command.Purpose, "clone ") && !strings.HasPrefix(UnwrappedRemoteLine(lastArg(&command)), "'git' 'clone'") {
			t.Fatal(lastArg(&command))
		}
	}
}

// Hard-won: 6dbb3ac44: withholding worker credentials also removed private-checkout authentication.
func TestCheckoutAuthenticationUsesRealGitWithoutPersistingTheToken(t *testing.T) {
	directory := t.TempDir()
	config := filepath.Join(directory, ".gitconfig")
	original := "[credential \"https://github.com\"]\nhelper = \"!f() { echo username=old; echo password=old; }; f\"\n"
	writeFile(t, config, original)
	token := "checkout-only-secret"
	args, input := cloneWithGithubToken([]string{"git", "credential", "fill"}, token)
	input = append(input, []byte("protocol=https\nhost=github.com\n\n")...)
	command := core.NewCommandSpec(args[0], args[1:]).WithSensitiveStdin(input)
	command.ClearEnv = true
	command.Env["PATH"] = os.Getenv("PATH")
	command.Env["HOME"] = directory
	command.Env["GIT_CONFIG_GLOBAL"] = config
	command.Env["GIT_CONFIG_NOSYSTEM"] = "1"
	command.Env["GIT_TERMINAL_PROMPT"] = "0"
	output, err := core.ProcessExecutor{}.Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("%d: %s", output.Status, output.Stderr)
	}
	if !strings.Contains(string(output.Stdout), "password="+token) {
		t.Fatalf("%s", output.Stdout)
	}
	if readFile(t, config) != original {
		t.Fatal("the token was persisted in the Git configuration")
	}
	if strings.Contains(fmt.Sprintf("%+v", command), token) {
		t.Fatal("the rendered command leaks the token")
	}

	bundle := testBundle()
	bundle.Repositories[0].URL = stringPointer("https://github.com/example/app.git")
	bundle.Repositories[1].URL = stringPointer("https://example.invalid/non-github.git")
	commands := CloneCommands(&bundle, "/workspace", &token, func(args []string) core.CommandSpec {
		return core.NewCommandSpec(args[0], args[1:])
	})
	if input, ok := findPurpose(t, commands, "clone app").SensitiveStdin(); !ok || string(input) != "checkout-only-secret\n" {
		t.Fatalf("%q %v", input, ok)
	}
	if _, ok := findPurpose(t, commands, "clone libs/lib").SensitiveStdin(); ok {
		t.Fatal("a non-GitHub clone received the token")
	}
}

func cutoffBundle(reference *string) core.ProjectBundleSpec {
	bundle := testBundle()
	bundle.Repositories = bundle.Repositories[:1]
	bundle.Repositories[0].Reference = reference
	base := strings.Repeat("b", 40)
	bundle.Repositories[0].HistoryCutoff = &core.HistoryCutoff{Head: strings.Repeat("a", 40), Base: &base}
	return bundle
}

func TestAHistoryCutoffInitializesAndFetchesByObjectIDInsteadOfCloning(t *testing.T) {
	wrap := func(args []string) core.CommandSpec { return core.NewCommandSpec(args[0], args[1:]) }
	bundle := cutoffBundle(nil)
	commands := CloneCommands(&bundle, "/workspace", nil, wrap)
	argv := make([]string, 0, len(commands))
	for _, command := range commands {
		argv = append(argv, strings.Join(append([]string{command.Program}, command.Args...), " "))
	}
	head := strings.Repeat("a", 40)
	base := strings.Repeat("b", 40)
	want := []string{
		"mkdir -p /workspace",
		"git init --quiet -- /workspace/app",
		fmt.Sprintf("git -C /workspace/app fetch --quiet --no-tags -- git@github.com:example/app.git %s %s:refs/mj/base", head, base),
		fmt.Sprintf("git -C /workspace/app checkout --quiet --detach %s", head),
	}
	if !slices.Equal(argv, want) {
		t.Fatalf("%q", argv)
	}
	// The steps depend on each other, so none of them joins a parallel batch.
	for _, command := range commands {
		if command.ParallelGroup != nil {
			t.Fatalf("%s joins a parallel batch", command.Purpose)
		}
	}
	for _, command := range commands[1:] {
		if !sameStage(command.Stage, stageOf(core.ProvisionStageCloning)) {
			t.Fatalf("%s: %v", command.Purpose, command.Stage)
		}
	}

	// A prepared cutoff bundle replaces the network, and is never a
	// `--reference` whose alternates would lend the checkout every object.
	bundle = cutoffBundle(stringPointer("/run/hel/git-cache/key.bundle"))
	commands = CloneCommands(&bundle, "/workspace", nil, wrap)
	for _, command := range commands {
		if slices.ContainsFunc(command.Args, func(arg string) bool { return arg == "clone" || strings.HasPrefix(arg, "--reference") }) {
			t.Fatalf("%q", command.Args)
		}
	}
	fetch := commands[2].Args
	if !slices.Equal(fetch[len(fetch)-4:], []string{"--", "/run/hel/git-cache/key.bundle", "refs/mj/head", "refs/mj/base:refs/mj/base"}) {
		t.Fatalf("%q", fetch)
	}
}

func TestAWSCutoffFetchReceivesGithubTokenOnStdinOnly(t *testing.T) {
	token := "ghs_secret-token"
	repositories := cutoffBundle(nil)
	repositories.Repositories[0].URL = stringPointer("https://github.com/example/app.git")
	plan, err := ProvisionOnLocatorPlan(awsLocator(), testSession, &repositories, &token)
	if err != nil {
		t.Fatal(err)
	}
	var withToken []core.CommandSpec
	for _, command := range plan.Commands {
		if strings.Contains(lastArg(&command), "GH_TOKEN") {
			withToken = append(withToken, command)
		}
	}
	if len(withToken) != 1 {
		t.Fatalf("%+v", plan.Commands)
	}
	if !strings.HasPrefix(withToken[0].Purpose, "fetch app history") {
		t.Fatal(withToken[0].Purpose)
	}
	if strings.Contains(fmt.Sprintf("%+v", plan.Commands), token) {
		t.Fatal("the rendered plan leaks the token")
	}
}

func TestTildeWorkspacePrefixBecomesHomeRelative(t *testing.T) {
	// Remote commands are single-quoted, so a literal "~" would name a real
	// directory instead of the login home.
	ssh := testSSH()
	template := &core.TargetsTargetTemplate{Kind: core.TargetsTargetTemplateSshBare, SSH: &ssh, WorkspacePrefix: "~/hel"}
	if workspace, err := core.WorkspaceFor(template, testSession); err != nil || workspace != "hel/"+testSession {
		t.Fatalf("%q %v", workspace, err)
	}

	for _, degenerate := range []string{"~", "~/"} {
		template := &core.TargetsTargetTemplate{Kind: core.TargetsTargetTemplateSshBare, SSH: &ssh, WorkspacePrefix: degenerate}
		if _, err := core.WorkspaceFor(template, testSession); err == nil {
			t.Fatalf("prefix %q must be rejected", degenerate)
		}
	}
}

func TestShellArgumentsAreSingleQuotedAtSSHBoundary(t *testing.T) {
	hostile := "repo'; touch /tmp/pwned; echo '"
	ssh := testSSH()
	command := core.SSHCommand(core.SshTrafficClassControl, &ssh, []string{"git", "clone", "--", hostile})
	if got := UnwrappedRemoteLine(lastArg(&command)); got != `'git' 'clone' '--' 'repo'\''; touch /tmp/pwned; echo '\'''` {
		t.Fatal(got)
	}
}

func TestBareProjectPlanLeavesProjectValidationToDialogAndLaunch(t *testing.T) {
	local, err := ProvisionBareProjectPlan(&core.TargetsTargetTemplate{Kind: core.TargetsTargetTemplateLocalBare}, testSession, "/home/me/project")
	if err != nil {
		t.Fatal(err)
	}
	if local.Description != "provision Mjolnir session "+testSession {
		t.Fatal(local.Description)
	}
	if len(local.Commands) != 0 {
		t.Fatalf("%v", local.Commands)
	}

	ssh := testSSH()
	template := &core.TargetsTargetTemplate{Kind: core.TargetsTargetTemplateSshBare, SSH: &ssh, WorkspacePrefix: ".local/share/hel/workspaces"}
	provision, err := ProvisionBareProjectPlan(template, testSession, "/srv/project")
	if err != nil {
		t.Fatal(err)
	}
	if len(provision.Commands) != 0 {
		t.Fatalf("%v", provision.Commands)
	}

	close, err := ClosePlan(sshBareLocator(".local/share/hel/workspaces/"+testSession), testSession)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(lastArg(&close.Scripts()[0]), "/srv/project") {
		t.Fatal(lastArg(&close.Scripts()[0]))
	}
}
