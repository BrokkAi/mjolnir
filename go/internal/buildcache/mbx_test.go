// Port of the tests in mj-build-cache/src/mbx.rs. The Unix-only ones are in
// mbx_unix_test.go.

package buildcache

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// isolatedMbx serializes the tests that use the process-wide resolution
// cache, and starts each from an empty cache.
var isolatedMbx sync.Mutex

func isolated(t *testing.T) {
	t.Helper()
	isolatedMbx.Lock()
	t.Cleanup(isolatedMbx.Unlock)
	resolutions.Lock()
	clear(resolutions.byKey)
	resolutions.Unlock()
	applications.Lock()
	clear(applications.byKey)
	applications.Unlock()
}

func clearResolutions() {
	resolutions.Lock()
	clear(resolutions.byKey)
	resolutions.Unlock()
}

type probeAnswer struct {
	needle string
	status int
	stdout string
}

// probeExecutor answers canned commands by a substring of their joined
// argument list.
type probeExecutor struct {
	answers []probeAnswer
	mu      sync.Mutex
	seen    []string
}

func newProbeExecutor(answers []probeAnswer) *probeExecutor {
	return &probeExecutor{answers: append(slices.Clone(answers), probeAnswer{"uname -sm", 0, "Linux x86_64\n"})}
}

func (p *probeExecutor) ran() []string {
	p.mu.Lock()
	defer p.mu.Unlock()
	return slices.Clone(p.seen)
}

func (p *probeExecutor) answer(command *core.CommandSpec) (core.CommandOutput, error) {
	line := command.Program + " " + strings.Join(command.Args, " ")
	p.mu.Lock()
	p.seen = append(p.seen, line)
	p.mu.Unlock()
	// Undo the quoting added by RemoteCommandLine for fixture matching.
	searchable := line
	if command.Program == "ssh" {
		searchable = strings.ReplaceAll(strings.ReplaceAll(line, `'\''`, "'"), "' '", " ")
	}
	if strings.Contains(searchable, "hel-mbx-profile") {
		return output(0, "preview\n~/.profile\nposix\n/home/jonathan/.local/share/mbx/bin", ""), nil
	}
	for _, answer := range p.answers {
		if strings.Contains(searchable, answer.needle) {
			return output(answer.status, answer.stdout, ""), nil
		}
	}
	return output(127, "", "no canned answer for "+line), nil
}

func (p *probeExecutor) executor() *funcExecutor {
	return &funcExecutor{execute: p.answer}
}

func mbxContainer(buildCache *core.TargetBuildCache) *core.TargetsContainerTemplate {
	return &core.TargetsContainerTemplate{
		Image:      "example/image:latest",
		PullPolicy: core.ImagePullPolicyMissing,
		BuildCache: buildCache,
	}
}

func podman(buildCache *core.TargetBuildCache) *core.TargetsTargetTemplate {
	return &core.TargetsTargetTemplate{Kind: core.TargetsTargetTemplateLocalPodman, Container: mbxContainer(buildCache)}
}

func docker(buildCache *core.TargetBuildCache) *core.TargetsTargetTemplate {
	return &core.TargetsTargetTemplate{Kind: core.TargetsTargetTemplateLocalDocker, Container: mbxContainer(buildCache)}
}

// configuredLocalMachine is the settings draft's view of this machine with
// blank build cache fields.
func configuredLocalMachine(t *testing.T) core.Machine {
	t.Helper()
	machine, err := core.UnmarshalMachineJSON([]byte(`{"kind": "local"}`))
	if err != nil {
		t.Fatal(err)
	}
	return machine
}

// currentNativeMbx is a native mbx's `--version` answer at the release
// containers run.
func currentNativeMbx() string {
	return "/usr/local/bin/mbx\nmbx " + MbxVersion
}

func nativeHost() []probeAnswer {
	return []probeAnswer{
		{`$resolved" --version`, 0, "/usr/local/bin/mbx\nmbx 1.22.0"},
		{"hel-mbx-home", 0, "/home/dev"},
		{"mbx cache dir --json", 0, `{"version":1,"store":"/mnt/fast/mbx-cache/actions"}`},
		{"XDG_CONFIG_HOME", 0, "/home/dev/.config/mbx"},
		{`[ -f "$1" ]`, 3, ""},
		{"while [ ! -d", 0, "/mnt/fast/mbx-cache"},
		{"mj-reflink", 0, ""},
		{"mkdir -p", 0, ""},
		{"stat -f -c %T", 0, "xfs"},
		{"source=$1 destination=$2 expected=$3", 0, "synced\n"},
	}
}

func absentHost() []probeAnswer {
	return []probeAnswer{{`$resolved" --version`, 1, ""}}
}

// withAnswer replaces the answer for needle, keeping the others in order.
func withAnswer(answers []probeAnswer, needle string, status int, stdout string) []probeAnswer {
	answers = slices.DeleteFunc(slices.Clone(answers), func(answer probeAnswer) bool { return answer.needle == needle })
	return append(answers, probeAnswer{needle, status, stdout})
}

func optionalOffReason(reason core.BuildCacheOff) string {
	if reason == nil {
		return "<none>"
	}
	return fmt.Sprintf("%#v", reason)
}

// Hard-won: #1330: a Mac's working native mbx was reported as missing.
func TestDarwinHostsReportNativeMbxWithoutSharingItWithContainers(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	for _, remote := range []bool{false, true} {
		for _, enabled := range []*bool{nil, boolPointer(true)} {
			for _, native := range []*string{stringPointer("1.16.0"), nil} {
				// Each case asks its own host afresh.
				clearResolutions()
				settings := core.TargetBuildCache{Enabled: enabled}
				var machine core.Machine
				var target *core.TargetsTargetTemplate
				if remote {
					encoded, err := core.TargetBuildCache.MarshalJSON(settings)
					if err != nil {
						t.Fatal(err)
					}
					machine, err = core.UnmarshalMachineJSON([]byte(
						`{"kind": "ssh", "host": "mac.test", "user": "builder", "build_cache": ` + string(encoded) + `}`))
					if err != nil {
						t.Fatal(err)
					}
					copied := settings
					target = &core.TargetsTargetTemplate{
						Kind:      core.TargetsTargetTemplateSshPodman,
						SSH:       &core.SshTarget{Destination: "builder@mac.test", SSHArgs: []string{}},
						Container: mbxContainer(&copied),
					}
				} else {
					copied := settings
					machine = core.MachineLocal{TargetBuildCache: &copied}
					targetSettings := settings
					target = podman(&targetSettings)
				}
				probe := ""
				status := 1
				if native != nil {
					probe = "/Users/builder/.local/bin/mbx\nmbx " + *native
					status = 0
				}
				probes := newProbeExecutor([]probeAnswer{
					{"uname -sm", 0, "Darwin arm64\n"},
					{`$resolved" --version`, status, probe},
				})
				executor := probes.executor()
				preview, err := PreviewBuildCache(machine, executor)
				if err != nil || preview == nil {
					t.Fatalf("preview = %v, %v", preview, err)
				}
				// The Mac's own mbx is reported, even one too old to share.
				if optionalText(preview.NativeMbx) != optionalText(native) {
					t.Fatalf("native = %s", optionalText(preview.NativeMbx))
				}
				off, ok := preview.OffReason.(core.BuildCacheOffUnsupportedPlatform)
				if !ok {
					t.Fatalf("%s", optionalOffReason(preview.OffReason))
				}
				reason := off.Value
				if native != nil {
					if !strings.Contains(reason, "mbx "+*native+" runs natively on this Mac at /Users/builder/.local/bin/mbx") ||
						!strings.Contains(reason, "when it is on their login PATH") {
						t.Fatal(reason)
					}
				} else if !strings.Contains(reason, "not installed on this Mac") {
					t.Fatal(reason)
				}
				if !strings.Contains(reason, "Linux in a virtual machine") {
					t.Fatal(reason)
				}
				// The pinned Linux release cannot install or upgrade it.
				if _, ok := InstallKind(preview); ok {
					t.Fatal("a Mac must not offer the Linux mbx release")
				}
				if preview.Directory != nil {
					t.Fatalf("directory = %s", *preview.Directory)
				}
				// An installed native mbx must not enter a container.
				if Resolve(target, executor) != nil {
					t.Fatal("a Mac's native mbx entered a container")
				}
				if err := ApplyMachineBuildCache(machine, []string{"/existing/cache"}, executor); err != nil {
					t.Fatal(err)
				}
				// Only the platform and the native mbx are asked about: no
				// GNU-only probes and no writes.
				commands := probes.ran()
				if !slices.ContainsFunc(commands, func(command string) bool { return strings.Contains(command, "-sm") }) ||
					!all(commands, func(command string) bool {
						return strings.Contains(command, "uname") && strings.Contains(command, "-sm") ||
							strings.Contains(command, "--version")
					}) {
					t.Fatalf("%q", commands)
				}
				if !all(commands, func(command string) bool {
					return !remote || strings.HasPrefix(command, "ssh ") && strings.Contains(command, "mac.test")
				}) {
					t.Fatalf("%q", commands)
				}
			}
		}
	}
}

func all[T any](values []T, predicate func(T) bool) bool {
	for _, value := range values {
		if !predicate(value) {
			return false
		}
	}
	return true
}

func TestLinuxSshCacheRemainsAvailableOnAnyControllerPlatform(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	probes := newProbeExecutor(nativeHost())
	target := &core.TargetsTargetTemplate{
		Kind:      core.TargetsTargetTemplateSshDocker,
		SSH:       &core.SshTarget{Destination: "builder@linux.test", SSHArgs: []string{}},
		Container: mbxContainer(nil),
	}
	cache := Resolve(target, probes.executor())
	if cache == nil {
		t.Fatal("a Linux SSH host shares its cache")
	}
	if cache.Directory != "/mnt/fast/mbx-cache" {
		t.Fatalf("directory = %s", cache.Directory)
	}
	if optionalText(cache.PreviousConfig) != optionalText(cache.ConfigFile) {
		t.Fatalf("previous %s, config %s", optionalText(cache.PreviousConfig), optionalText(cache.ConfigFile))
	}
	if !all(probes.ran(), func(command string) bool {
		return strings.HasPrefix(command, "ssh ") && strings.Contains(command, "builder@linux.test")
	}) {
		t.Fatalf("%q", probes.ran())
	}
}

func TestReconciliationRefreshesActiveMountsWhenCachePolicyIsDisabled(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	machine := core.MachineLocal{TargetBuildCache: &core.TargetBuildCache{Enabled: boolPointer(false)}}
	probes := newProbeExecutor(nativeHost())
	mounted := "/existing/cache"

	if err := ApplyMachineBuildCache(machine, []string{mounted}, probes.executor()); err != nil {
		t.Fatal(err)
	}

	if !slices.ContainsFunc(probes.ran(), func(command string) bool {
		return strings.Contains(command, "source=$1 destination=$2 expected=$3") &&
			strings.Contains(command, "/existing/cache/.mjolnir/bin/mbx")
	}) {
		t.Fatalf("%q", probes.ran())
	}
}

func TestRecordedDarwinCacheFailsExplicitlyWithoutWriting(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	probes := newProbeExecutor([]probeAnswer{
		{"uname -sm", 0, "Darwin x86_64"},
		{`$resolved" --version`, 1, ""},
	})
	recorded := &core.SessionBuildCache{Host: "local", Directory: "/existing/cache"}
	backend := &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: "saved-container"}
	config := core.DefaultConfig()
	_, err := PrepareSessionConfiguration(&config, backend, recorded, probes.executor())
	if err == nil || !strings.Contains(alternate(err), "Linux in a virtual machine") {
		t.Fatalf("error = %v", err)
	}
	ran := probes.ran()
	if ran[0] != "uname -sm" {
		t.Fatalf("%q", ran)
	}
	if !all(ran[1:], func(command string) bool { return strings.Contains(command, "--version") }) {
		t.Fatalf("%q", ran)
	}
}

func TestFailedPlatformProbeDoesNotAttemptCacheOperations(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	probes := newProbeExecutor([]probeAnswer{{"uname -sm", 1, ""}})
	if _, err := inspectHost(CacheHost{}, &core.TargetBuildCache{}, probes.executor()); err == nil {
		t.Fatal("a failed platform probe is an error")
	}
	if !slices.Equal(probes.ran(), []string{"uname -sm"}) {
		t.Fatalf("%q", probes.ran())
	}
}

func TestInstalledWorkerReadsCacheConfigurationFromItsRecordedHost(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	probes := newProbeExecutor([]probeAnswer{
		{"XDG_CONFIG_HOME", 0, "/home/builder/.config/mbx"},
		{"$HOME", 0, "/home/builder"},
		{`[ -f "$1" ]`, 0, "[gc]\nmax_total_size = '50GB'\n"},
	})
	target := &core.TargetsTargetLocator{
		Kind:        core.TargetsTargetLocatorSshPodman,
		SSH:         &core.SshTarget{Destination: "builder@recorded-cache.test", SSHArgs: []string{}},
		ContainerID: "saved-container",
	}
	host, ok := HostForLocator(target)
	if !ok {
		t.Fatal("an SSH Podman container has a host")
	}
	config, err := hostConfigFile(host, probes.executor())
	if err != nil || config == nil {
		t.Fatalf("config = %s, %v", optionalText(config), err)
	}
	if !strings.Contains(*config, "50GB") {
		t.Fatal(*config)
	}
	if !all(probes.ran(), func(command string) bool { return strings.Contains(command, "builder@recorded-cache.test") }) {
		t.Fatalf("%q", probes.ran())
	}
}

func TestANativeMbxSuppliesTheCacheDirectoryAndItsOwnLimits(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	probes := newProbeExecutor([]probeAnswer{
		{`$resolved" --version`, 0, currentNativeMbx()},
		{"mbx cache dir --json", 0, `{"version":1,"store":"/mnt/fast/mbx-cache/actions"}`},
		{`printf '%s' "$HOME"`, 0, "/home/dev"},
		{`[ -f "$1" ]`, 0, "cache_dir = \"/mnt/fast/mbx-cache\"\n[gc]\nmax_size = \"500GiB\"\n"},
		{"while [ ! -d", 0, "/mnt/fast/mbx-cache"},
		{"mj-reflink", 0, ""},
		{"mkdir -p", 0, ""},
		{"stat -f -c %T", 0, "xfs"},
	})
	resolved := Resolve(podman(nil), probes.executor())
	if resolved == nil {
		t.Fatal("the native cache resolves")
	}
	if resolved.Directory != "/mnt/fast/mbx-cache" {
		t.Fatalf("directory = %s", resolved.Directory)
	}
	// The host's own configuration file carries the budget.
	limit, err := configurationConfiguredLimit(resolved.ConfigFile, "gc", "max_total_size")
	if err != nil || limit != nil {
		t.Fatalf("limit = %s, %v", optionalText(limit), err)
	}
	if resolved.TargetRoot != nil {
		t.Fatalf("target root = %s", *resolved.TargetRoot)
	}
	if !strings.Contains(*resolved.ConfigFile, "500GiB") {
		t.Fatal(*resolved.ConfigFile)
	}
	if slices.ContainsFunc(probes.ran(), func(line string) bool { return strings.Contains(line, "apply machine") }) {
		t.Fatal("a host configuration is never rewritten")
	}
}

// Hard-won: 24f3d72c: settings refresh left sessions using a stale failed host inspection
func TestLookingAtTheSettingsPageLetsTheNextSessionSeeARepairedHost(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	brokenAnswers := nativeHost()
	for index := range brokenAnswers {
		if brokenAnswers[index].needle == "mj-reflink" {
			brokenAnswers[index].status = 1
		}
	}
	broken := newProbeExecutor(brokenAnswers)
	if Resolve(podman(nil), broken.executor()) != nil {
		t.Fatal("a volume that cannot clone runs without the cache")
	}

	// The host is repaired, and the user opens the machine's build cache
	// page to check.
	repaired := newProbeExecutor(nativeHost())
	preview, err := PreviewBuildCache(configuredLocalMachine(t), repaired.executor())
	if err != nil {
		t.Fatalf("the host answers: %v", err)
	}
	if preview == nil {
		t.Fatal("a local machine can hold a cache")
	}
	if preview.OffReason != nil {
		t.Fatal(optionalOffReason(preview.OffReason))
	}

	if Resolve(podman(nil), repaired.executor()) == nil {
		t.Fatal("the next session asks the repaired host again instead of reusing the old verdict")
	}
}

// Hard-won: #1614: a cached Unavailable inspection stayed sticky under MemoizedSuccess for 10 minutes
func TestRepairedReflinksRecoverOnTheNextSessionConfigurationWithoutWaiting(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	base := newProbeExecutor(append(nativeHost(), probeAnswer{".mj-apply.lock", 0, ""}))
	var reflinkAttempts atomic.Int64
	executor := &funcExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		line := command.Program + " " + strings.Join(command.Args, " ")
		if strings.Contains(line, "mj-reflink") && reflinkAttempts.Add(1) == 1 {
			return output(1, "", ""), nil
		}
		return base.answer(command)
	}}
	recorded := &core.SessionBuildCache{Host: "local", Directory: "/mnt/fast/mbx-cache"}
	backend := &core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorLocalPodman, ContainerID: "saved-container"}
	config := core.DefaultConfig()

	_, first := PrepareSessionConfiguration(&config, backend, recorded, executor)
	if first == nil || !strings.Contains(alternate(first), "does not support reflinks") {
		t.Fatalf("first = %v", first)
	}
	if reflinkAttempts.Load() != 1 {
		t.Fatalf("reflink attempts = %d", reflinkAttempts.Load())
	}

	second, err := PrepareSessionConfiguration(&config, backend, recorded, executor)
	if err != nil {
		t.Fatalf("a repaired host should recover on the next launch inside RESOLUTION_LIFETIME: %v", err)
	}
	if second != configurationSharedDirectory(recorded.Directory) {
		t.Fatalf("second = %s", second)
	}
	if reflinkAttempts.Load() != 2 {
		t.Fatalf("reflink attempts = %d", reflinkAttempts.Load())
	}

	// A deliberate TurnedOff setting stays memoized under MemoizedSuccess.
	disabled := core.TargetBuildCache{Enabled: boolPointer(false)}
	disabledProbes := newProbeExecutor(nativeHost())
	firstOff, err := inspect(CacheHost{}, &disabled, freshnessMemoizedSuccess, disabledProbes.executor())
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := firstOff.preview.OffReason.(core.BuildCacheOffTurnedOff); !ok {
		t.Fatal(optionalOffReason(firstOff.preview.OffReason))
	}
	probesAfterFirst := len(disabledProbes.ran())
	secondOff, err := inspect(CacheHost{}, &disabled, freshnessMemoizedSuccess, disabledProbes.executor())
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := secondOff.preview.OffReason.(core.BuildCacheOffTurnedOff); !ok {
		t.Fatal(optionalOffReason(secondOff.preview.OffReason))
	}
	if len(disabledProbes.ran()) != probesAfterFirst {
		t.Fatal("TurnedOff should remain memoized under MemoizedSuccess")
	}
}

func TestATargetRootThatCannotBeClonedIntoStillGetsTheCache(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	probes := newProbeExecutor([]probeAnswer{
		{`$resolved" --version`, 0, currentNativeMbx()},
		{"mbx cache dir --json", 0, `{"version":1,"store":"/mnt/fast/mbx-cache/actions"}`},
		{`printf '%s' "$HOME"`, 0, "/home/dev"},
		{`[ -f "$1" ]`, 0, "[target]\nroot = \"/mnt/slow/mbx-targets\"\n"},
		{"while [ ! -d", 0, "/mnt/fast/mbx-cache"},
		// Cloning from the store into the relocated root fails; cloning
		// within the store still works.
		{"src=$1", 1, ""},
		{"mj-reflink", 0, ""},
		{"mkdir -p", 0, ""},
		{"stat -f -c %T", 0, "xfs"},
	})
	resolved := Resolve(podman(nil), probes.executor())
	if resolved == nil {
		t.Fatal("a target root that copies instead of cloning is slower, not unusable")
	}
	if optionalText(resolved.TargetRoot) != "/mnt/slow/mbx-targets" {
		t.Fatalf("target root = %s", optionalText(resolved.TargetRoot))
	}
	if !slices.ContainsFunc(probes.ran(), func(line string) bool { return strings.Contains(line, "src=$1") }) {
		t.Fatalf("the store and the target root are probed as a pair: %q", probes.ran())
	}
}

func TestATargetRootInsideTheCacheDirectoryNeedsNoSecondMount(t *testing.T) {
	if root := relocatedTargetRoot("[target]\nroot = \"targets\"\n", "/cache"); root != nil {
		t.Fatal(*root)
	}
	if root := relocatedTargetRoot("[target]\nroot = \"/cache/targets\"\n", "/cache"); root != nil {
		t.Fatal(*root)
	}
}

func TestLocalStoragePathsFollowMbxCacheAndTargetRootResolution(t *testing.T) {
	defaultCache := "/home/dev/.cache/mbx"
	check := func(got []string, want ...string) {
		t.Helper()
		if !slices.Equal(got, want) {
			t.Fatalf("got %q, want %q", got, want)
		}
	}
	resolve := func(a, b, c, d *string) []string {
		paths, err := resolveLocalStoragePaths(a, b, c, d)
		if err != nil {
			t.Fatal(err)
		}
		return paths
	}

	check(resolve(nil, nil,
		stringPointer("cache_dir = \"/mnt/optane/mbx-cache\"\n[target]\nroot = \"/mnt/optane/mbx-targets\"\n"),
		&defaultCache),
		"/mnt/optane/mbx-cache", "/mnt/optane/mbx-targets")
	check(resolve(nil, nil,
		stringPointer("cache_dir = \"/mnt/optane/mbx-cache\"\n[target]\nroot = \"views\"\n"),
		&defaultCache),
		"/mnt/optane/mbx-cache", "/mnt/optane/mbx-cache/views")
	check(resolve(nil, nil, nil, &defaultCache),
		defaultCache, pathJoin(defaultCache, "targets"))
	check(resolve(stringPointer("/env/cache"), stringPointer("views"),
		stringPointer("cache_dir = \"/file/cache\"\n[target]\nroot = \"file-views\"\n"),
		&defaultCache),
		"/env/cache", "/env/cache/views")
}

// Hard-won: ecab42fb: non-login SSH PATH hid cargo-installed mbx and selected the wrong cache store
func TestACargoInstalledMbxOffThePathIsQueriedWhereItWasFound(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	found := "/home/dev/.cargo/bin/mbx\nmbx " + MbxVersion
	answers := slices.DeleteFunc(nativeHost(), func(answer probeAnswer) bool { return answer.needle == `$resolved" --version` })
	answers = append(answers,
		probeAnswer{`$resolved" --version`, 0, found},
		probeAnswer{"/home/dev/.cargo/bin/mbx cache dir --json", 0, `{"version":1,"store":"/mnt/fast/mbx-cache/actions"}`})
	probes := newProbeExecutor(answers)
	resolved := Resolve(podman(nil), probes.executor())
	if resolved == nil {
		t.Fatal("the cache resolves")
	}
	if resolved.Directory != "/mnt/fast/mbx-cache" {
		t.Fatalf("directory = %s", resolved.Directory)
	}
	if resolved.NativeMbx.Program != "/home/dev/.cargo/bin/mbx" {
		t.Fatalf("program = %s", resolved.NativeMbx.Program)
	}
}

func TestAnUnrecognizedNativeMbxMustNotShareTheStore(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	probes := newProbeExecutor([]probeAnswer{
		{`$resolved" --version`, 0, "/home/jonathan/.local/bin/mbx\nmbx unknown"},
		{"hel-mbx-home", 0, "/home/jonathan"},
	})
	if Resolve(podman(nil), probes.executor()) != nil {
		t.Fatal("an unrecognized mbx must not share the store")
	}
	preview, err := PreviewBuildCache(configuredLocalMachine(t), probes.executor())
	if err != nil || preview == nil {
		t.Fatalf("preview = %v, %v", preview, err)
	}
	off, ok := preview.OffReason.(core.BuildCacheOffUnavailable)
	if !ok || !strings.Contains(off.Value, "unrecognized mbx version") || !strings.Contains(off.Value, "Settings › Setup › Machines") {
		t.Fatal(optionalOffReason(preview.OffReason))
	}
}

func TestAHostWithoutMbxHasNoSharedCacheAndPreviewShowsSetupGuidance(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	probes := newProbeExecutor(append(absentHost(), probeAnswer{"hel-mbx-home", 0, "/home/jonathan"}))
	if Resolve(podman(nil), probes.executor()) != nil {
		t.Fatal("a host without mbx has no shared cache")
	}
	preview, err := PreviewBuildCache(configuredLocalMachine(t), probes.executor())
	if err != nil || preview == nil {
		t.Fatalf("preview = %v, %v", preview, err)
	}
	if preview.NativeMbx != nil {
		t.Fatal(*preview.NativeMbx)
	}
	if preview.Directory != nil {
		t.Fatal(*preview.Directory)
	}
	off, ok := preview.OffReason.(core.BuildCacheOffUnavailable)
	if !ok || !strings.Contains(off.Value, "Settings › Setup › Machines") {
		t.Fatal(optionalOffReason(preview.OffReason))
	}
	// The shared profile script contains a mkdir branch, but preview must
	// not invoke a separate host mkdir command for the cache directory.
	if slices.ContainsFunc(probes.ran(), func(line string) bool { return strings.HasPrefix(line, "mkdir ") }) {
		t.Fatalf("%q", probes.ran())
	}
}

func TestANativeInstallationOwnsTheBudgetDespiteSavedMjOverrides(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	answers := []probeAnswer{
		{`$resolved" --version`, 0, currentNativeMbx()},
		{"mbx cache dir --json", 0, `{"store":"/native/cache/actions"}`},
		{`[ -f "$1" ]`, 0, "[gc]\nmax_total_size = '400GiB'\n[target]\nmax_size = 'none'\n"},
	}
	probes := newProbeExecutor(append(answers, nativeHost()...))
	settings := core.TargetBuildCache{Directory: stringPointer("/ignored"), MaxTotalSize: stringPointer("1GB")}
	inspected, err := inspectHost(CacheHost{}, &settings, probes.executor())
	if err != nil {
		t.Fatal(err)
	}
	if !inspected.preview.UserManaged {
		t.Fatal("a native installation is user managed")
	}
	if optionalText(inspected.preview.Directory) != "/native/cache" {
		t.Fatalf("directory = %s", optionalText(inspected.preview.Directory))
	}
	limit, ok := inspected.preview.MaxTotalSize.(core.BuildCacheLimitHostConfiguration)
	if !ok || optionalText(limit.Value) != "400GiB" {
		t.Fatalf("limit = %#v", inspected.preview.MaxTotalSize)
	}
	if slices.ContainsFunc(probes.ran(), func(command string) bool { return strings.Contains(command, ".mj-apply.lock") }) {
		t.Fatalf("%q", probes.ran())
	}
}

// Turning the cache on cannot override the host: without reflinks a restore
// would copy every byte, so sessions still run without it.
func TestAnEnabledSettingDoesNotSurviveAVolumeWithoutReflinks(t *testing.T) {
	isolated(t)
	probes := newProbeExecutor(withAnswer(nativeHost(), "mj-reflink", 1, ""))
	if Resolve(podman(&core.TargetBuildCache{Enabled: boolPointer(true)}), probes.executor()) != nil {
		t.Fatal("an enabled setting survived a volume without reflinks")
	}
	// An unset target preference is overridden by the same host capability.
	if Resolve(podman(nil), probes.executor()) != nil {
		t.Fatal("an unset preference survived a volume without reflinks")
	}
}

func TestANetworkFilesystemRunsWithoutTheCache(t *testing.T) {
	isolated(t)
	probes := newProbeExecutor(withAnswer(nativeHost(), "stat -f -c %T", 0, "nfs4"))
	if Resolve(podman(nil), probes.executor()) != nil {
		t.Fatal("a network filesystem must not hold the cache")
	}
}

func TestLocalPodmanAndLocalDockerInspectOneMachineOnce(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	probes := newProbeExecutor(nativeHost())
	first := Resolve(podman(nil), probes.executor())
	if first == nil {
		t.Fatal("podman resolves")
	}
	ran := len(probes.ran())
	if ran == 0 {
		t.Fatal("the first resolve inspects the host")
	}
	second := Resolve(docker(nil), probes.executor())
	if second == nil || !resolvedEqual(*first, *second) {
		t.Fatal("both engines on this machine share one cache")
	}
	// The inspection is memoized; creating the directory is not, because a
	// remembered inspection says what the host looked like, not that the
	// directory still exists.
	added := probes.ran()[ran:]
	if len(added) != 1 {
		t.Fatalf("the second runtime is answered from the machine's recorded inspection: %q", added)
	}
	if !strings.Contains(added[0], "mkdir -p") {
		t.Fatalf("the one repeated command creates the directory: %q", added)
	}
}

// resolvedEqual is ResolvedBuildCache's derived PartialEq.
func resolvedEqual(left, right ResolvedBuildCache) bool {
	return left.Directory == right.Directory && left.NativeMbx == right.NativeMbx &&
		optionalStringEqual(left.TargetRoot, right.TargetRoot) &&
		optionalStringEqual(left.ConfigFile, right.ConfigFile) &&
		left.ConfigDirectory == right.ConfigDirectory &&
		optionalStringEqual(left.PreviousConfig, right.PreviousConfig)
}

func TestThePreviewReportsWhatTheCacheHasAlreadyDone(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	// Ahead of the configuration read, which tests the same `[ -f ]`.
	answers := []probeAnswer{{
		"tally.json", 0,
		`{"version":1,"since_secs":1789824719,"builds":155,"cached_compilations":12050,"avoided_compiler_ns":6004997818721,"reflinked_bytes":47612059386}`,
	}}
	probes := newProbeExecutor(append(answers, nativeHost()...))
	preview, err := PreviewBuildCache(configuredLocalMachine(t), probes.executor())
	if err != nil {
		t.Fatalf("the host answers: %v", err)
	}
	if preview == nil {
		t.Fatal("a local machine can hold a cache")
	}
	want := core.BuildCacheStats{
		Builds:             155,
		CachedCompilations: 12050,
		AvoidedCompilerNs:  6_004_997_818_721,
		ReflinkedBytes:     47_612_059_386,
	}
	if preview.Stats == nil || *preview.Stats != want {
		t.Fatalf("stats = %#v", preview.Stats)
	}
}

func TestACacheNothingHasUsedYetReportsNoTotals(t *testing.T) {
	isolated(t)
	// nativeHost answers every `[ -f ]` with 3: no configuration file and no
	// tally beside the store.
	probes := newProbeExecutor(nativeHost())
	preview, err := PreviewBuildCache(configuredLocalMachine(t), probes.executor())
	if err != nil {
		t.Fatalf("the host answers: %v", err)
	}
	if preview == nil {
		t.Fatal("a local machine can hold a cache")
	}
	if preview.Stats != nil {
		t.Fatalf("stats = %#v", *preview.Stats)
	}
}

func mbxSession(containerWorkspace *string) core.SessionRecord {
	record := store.CheckpointTestSession("session-1")
	record.ContainerWorkspace = containerWorkspace
	return record
}

func TestAContainerSessionMountsTheNativeCacheAndRecordsItsSynchronizedBinary(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	probes := newProbeExecutor(nativeHost())
	var mounts []core.AdditionalMount
	session := mbxSession(stringPointer("/workspace/session-1"))
	buildCache := MbxPrepare(podman(nil), &session, &mounts, probes.executor())
	if buildCache == nil {
		t.Fatal("a container session uses the build cache")
	}
	if buildCache.Directory != "/mnt/fast/mbx-cache" {
		t.Fatalf("directory = %s", buildCache.Directory)
	}
	if CacheBinaryPath(buildCache.Directory) != "/mnt/fast/mbx-cache/.mjolnir/bin/mbx" {
		t.Fatal(CacheBinaryPath(buildCache.Directory))
	}
	want := []core.AdditionalMount{{Source: "/mnt/fast/mbx-cache", Destination: "/mnt/fast/mbx-cache", Access: core.MountAccessRw}}
	if !slices.Equal(mounts, want) {
		t.Fatalf("mounts = %v", mounts)
	}
}

func TestASessionAtTheLegacySharedWorkspaceRunsWithoutTheCache(t *testing.T) {
	isolated(t)
	probes := newProbeExecutor(nativeHost())
	var mounts []core.AdditionalMount
	session := mbxSession(nil)
	if buildCache := MbxPrepare(podman(nil), &session, &mounts, probes.executor()); buildCache != nil {
		t.Fatalf("build cache = %#v", *buildCache)
	}
	if len(probes.ran()) != 0 {
		t.Fatalf("%q", probes.ran())
	}
}

func TestAResumedSessionUsesCurrentMachinePolicyInsteadOfItsSavedBudget(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	probes := newProbeExecutor(nativeHost())
	record := mbxSession(stringPointer("/workspace/session-1"))
	record.BuildCache = &core.SessionBuildCache{
		Host:      "local",
		Directory: "/mnt/fast/mbx-cache",
		MaxSize:   stringPointer("1GB"),
	}
	var mounts []core.AdditionalMount
	buildCache := MbxPrepare(podman(nil), &record, &mounts, probes.executor())
	if buildCache == nil {
		t.Fatal("the resumed session uses the cache")
	}
	if buildCache.MaxSize != nil {
		t.Fatal(*buildCache.MaxSize)
	}
	if buildCache.Directory != "/mnt/fast/mbx-cache" {
		t.Fatalf("directory = %s", buildCache.Directory)
	}
	if len(mounts) != 1 {
		t.Fatalf("mounts = %v", mounts)
	}
	if !slices.ContainsFunc(probes.ran(), func(line string) bool { return strings.Contains(line, ".mj-apply.lock") }) {
		t.Fatalf("%q", probes.ran())
	}
}

func TestASessionMovedToAnotherHostResolvesItsBuildCacheAgain(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	probes := newProbeExecutor(nativeHost())
	record := mbxSession(stringPointer("/workspace/session-1"))
	record.BuildCache = &core.SessionBuildCache{
		// The host the session was provisioned on, which the target below is
		// not.
		Host:       "ssh:dev@example.test",
		Directory:  "/mnt/fast/mbx-cache",
		TargetRoot: stringPointer("/mnt/fast/mbx-targets"),
	}
	var mounts []core.AdditionalMount

	buildCache := MbxPrepare(podman(nil), &record, &mounts, probes.executor())
	if buildCache == nil {
		t.Fatal("the destination host qualifies on its own")
	}

	if buildCache.Host != "local" {
		t.Fatalf("host = %s", buildCache.Host)
	}
	if buildCache.Directory != "/mnt/fast/mbx-cache" {
		t.Fatalf("directory = %s", buildCache.Directory)
	}
	if buildCache.TargetRoot != nil {
		t.Fatal(*buildCache.TargetRoot)
	}
	var destinations []string
	for _, mount := range mounts {
		destinations = append(destinations, mount.Destination)
	}
	if !slices.Equal(destinations, []string{"/mnt/fast/mbx-cache"}) {
		t.Fatalf("%q", destinations)
	}
	if !slices.ContainsFunc(probes.ran(), func(line string) bool { return strings.Contains(line, "mj-reflink") }) {
		t.Fatalf("%q", probes.ran())
	}
}

func TestAnAttachedDirectoryOverTheCacheWins(t *testing.T) {
	buildCache := &core.SessionBuildCache{Host: "local-podman", Directory: "/mnt/fast/mbx-cache"}
	mounts := []core.AdditionalMount{{Source: "/elsewhere", Destination: "/mnt/fast/mbx-cache/actions", Access: core.MountAccessRo}}
	if attachMounts(buildCache, &mounts) {
		t.Fatal("an attached directory over the cache wins")
	}
	if len(mounts) != 1 {
		t.Fatalf("mounts = %v", mounts)
	}
}

func TestVersionsCompareByReleaseOrder(t *testing.T) {
	native := func(version string) *NativeMbx {
		return &NativeMbx{Program: "/usr/local/bin/mbx", Version: version}
	}
	for _, version := range []string{"1.22.0", MbxVersion, "1.15.0"} {
		if _, ok := classifyNativeMbx(native(version)).(nativeMbxCompatible); !ok {
			t.Fatalf("%s is compatible", version)
		}
	}
	if _, ok := classifyNativeMbx(native("not-a-version")).(nativeMbxUnknown); !ok {
		t.Fatal("not-a-version is unknown")
	}
	if _, ok := classifyNativeMbx(nil).(nativeMbxAbsent); !ok {
		t.Fatal("no mbx is absent")
	}
}

func TestAvailableBytesReadsTheDfAvailableColumn(t *testing.T) {
	available, ok := AvailableBytes("Filesystem 1K-blocks Used Available Capacity Mounted on\n" +
		"/dev/sda1 1000 400 600 40% /\n")
	if !ok || available != 600 {
		t.Fatalf("available = %d, %v", available, ok)
	}
	if _, ok := AvailableBytes("Filesystem 1K-blocks\n"); ok {
		t.Fatal("a report without a data row has no available column")
	}
}

func TestCacheSyncReprobesAndRetriesAfterAnotherProcessChangesTheHostBinary(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	var syncs atomic.Int64
	var mu sync.Mutex
	var purposes []string
	executor := &funcExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		mu.Lock()
		purposes = append(purposes, command.Purpose)
		mu.Unlock()
		switch command.Purpose {
		case "synchronize native mbx into the shared cache":
			if syncs.Add(1) == 1 {
				return output(75, "", "native mbx changed"), nil
			}
			return output(0, "synced\n", ""), nil
		case "read the container host mbx version":
			return output(0, "/opt/mbx/current\nmbx 1.23.0", ""), nil
		}
		return core.CommandOutput{}, errors.New("unexpected command purpose " + command.Purpose)
	}}

	binary, err := SyncMbxBinaryFromNative(CacheHost{}, &NativeMbx{Program: "/opt/mbx/old", Version: "1.22.0"},
		"/tmp/mjolnir-mbx-retry", executor)
	if err != nil {
		t.Fatal(err)
	}

	if binary.Version != "1.23.0" {
		t.Fatalf("version = %s", binary.Version)
	}
	want := []string{
		"synchronize native mbx into the shared cache",
		"read the container host mbx version",
		"synchronize native mbx into the shared cache",
	}
	if !slices.Equal(purposes, want) {
		t.Fatalf("%q", purposes)
	}
}

func TestAbsentOrUnrecognizedNativeMbxLeavesTheExistingCacheCopy(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	root := t.TempDir()
	cache := pathJoin(root, "cache")
	copyPath := CacheBinaryPath(cache)
	parent, _ := pathParent(copyPath)
	if err := os.MkdirAll(parent, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(copyPath, []byte("previous compatible copy"), 0o666); err != nil {
		t.Fatal(err)
	}

	for _, answer := range []core.CommandOutput{
		output(1, "", ""),
		output(0, "/opt/old/mbx\nmbx unknown", ""),
	} {
		executor := &funcExecutor{execute: func(*core.CommandSpec) (core.CommandOutput, error) { return answer, nil }}
		result, err := SyncCurrentMbxBinary(CacheHost{}, cache, executor)
		if err != nil {
			t.Fatal(err)
		}
		if _, ok := result.(CachedMbxSyncUnavailable); !ok {
			t.Fatalf("result = %#v", result)
		}
		if readText(t, copyPath) != "previous compatible copy" {
			t.Fatal("the existing copy changed")
		}
	}
}

func TestThePreviewReportsNativeCacheValuesWithoutCreatingDirectories(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	isolated(t)
	probes := newProbeExecutor(nativeHost())
	preview, err := PreviewBuildCache(configuredLocalMachine(t), probes.executor())
	if err != nil || preview == nil {
		t.Fatalf("preview = %v, %v", preview, err)
	}
	if optionalText(preview.NativeMbx) != "1.22.0" {
		t.Fatalf("native = %s", optionalText(preview.NativeMbx))
	}
	if preview.MbxProfileFile != nil {
		t.Fatal(*preview.MbxProfileFile)
	}
	if _, ok := InstallKind(preview); ok {
		t.Fatal("no install is offered")
	}
	if optionalText(preview.Directory) != "/mnt/fast/mbx-cache" {
		t.Fatalf("directory = %s", optionalText(preview.Directory))
	}
	if limit, ok := preview.MaxTotalSize.(core.BuildCacheLimitMbxDefault); !ok || limit.Value != nil {
		t.Fatalf("limit = %#v", preview.MaxTotalSize)
	}
	if preview.OffReason != nil {
		t.Fatal(optionalOffReason(preview.OffReason))
	}
	// Profile previews carry the shared script, including its update-only
	// mkdir branch, but do not execute a host mkdir command.
	if slices.ContainsFunc(probes.ran(), func(line string) bool { return strings.HasPrefix(line, "mkdir ") }) {
		t.Fatalf("%q", probes.ran())
	}
}

// Hard-won: #1597: invalid config silently measured default filesystems.
func TestStorageConfigRejectsBrokenFiles(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	if text, err := readLocalStorageConfig(path); err != nil || text != nil {
		t.Fatalf("missing: %v %v", text, err)
	}
	for _, text := range []string{"[", "cache_dir = 3", "[target]\nroot = 3", "target = 3"} {
		if _, err := resolveLocalStoragePaths(stringPointer("/override"), nil, &text, nil); err == nil {
			t.Fatalf("accepted %q", text)
		}
	}
	if runtime.GOOS != "windows" {
		if err := os.Symlink(path+"-missing", path); err != nil {
			t.Fatal(err)
		}
		if _, err := readLocalStorageConfig(path); err == nil || !strings.Contains(err.Error(), path) {
			t.Fatalf("dangling: %v", err)
		}
	}
}
