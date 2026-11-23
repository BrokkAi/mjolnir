// Port of the tests in mj-harness-install/src/harness_install.rs.

package harnessinstall

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"

	"github.com/klauspost/compress/zstd"
)

// harnessTestExecutable installs body as an executable stand-in at path.
func harnessTestExecutable(t *testing.T, path, body string) {
	t.Helper()
	parent := filepath.Dir(path)
	if err := os.MkdirAll(parent, 0o777); err != nil {
		t.Fatal(err)
	}
	core.InstallFakeCommand(parent, filepath.Base(path), body)
}

func harnessTestCompleteFake(t *testing.T, root string, harness core.HarnessKind, installID, entrypoint string) {
	t.Helper()
	install := filepath.Join(root, installID)
	if err := os.MkdirAll(install, 0o777); err != nil {
		t.Fatal(err)
	}
	harnessTestExecutable(t, filepath.Join(install, entrypoint), "#!/bin/sh\nexit 0\n")
	selected := core.Pin(harness)
	if entrypoint == selected.Entrypoint && selected.CLIEntrypoint != entrypoint {
		harnessTestExecutable(t, filepath.Join(install, selected.CLIEntrypoint), "#!/bin/sh\nexit 0\n")
	}
	lease, err := openLock(filepath.Join(install, core.LeaseFile))
	if err != nil {
		t.Fatal(err)
	}
	lease.Close()
	body, err := json.MarshalIndent(core.ManagedHarnessManifestForInstall(harness, installID), "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := core.AtomicWrite(filepath.Join(install, core.ManifestFile), body); err != nil {
		t.Fatal(err)
	}
}

func harnessTestFakeNodeTools(t *testing.T, root, installs string) map[string]string {
	t.Helper()
	bin := filepath.Join(root, "bin")
	harnessTestExecutable(t, filepath.Join(bin, "node"), "#!/bin/sh\nif [ \"$1\" = --version ]; then echo v24.22.1; fi\n")
	harnessTestExecutable(t, filepath.Join(bin, "npx"), "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 11.6.2; fi\n")
	codex, claude := core.Pin(core.HarnessKindCodex), core.Pin(core.HarnessKindClaude)
	harnessTestExecutable(t, filepath.Join(bin, "npm"), fmt.Sprintf(`#!/bin/sh
set -eu
if [ "$1" = --version ]; then echo 12.0.0; exit 0; fi
if [ "$1" = install-scripts ]; then printf '{"allowScripts":[]}'; exit 0; fi
printf x >> '%[1]s'
mkdir -p node_modules/.bin
if grep -q 'mjolnir-managed-codex' package.json; then
    bridge=%[3]s
    cli=%[4]s
else
    bridge=%[5]s
    cli=%[6]s
fi
printf '#!/bin/sh\nexit 0\n' > "$bridge.script"
cat > "$cli.script" <<'CLI'
#!/bin/sh
if [ "$1" != --version ]; then exit 2; fi
if [ -n "$MJ_TEST_CLI_PROBES" ]; then printf x >> "$MJ_TEST_CLI_PROBES"; fi
if [ -n "$MJ_TEST_CLI_MISSING_NATIVE" ]; then
    echo 'missing native platform package' >&2
    exit 1
fi
echo 'installed CLI version'
CLI
ln -s '%[2]s' "$bridge"
ln -s '%[2]s' "$cli"
`,
		installs, core.FakeCommandDispatcher(),
		codex.Entrypoint, codex.CLIEntrypoint, claude.Entrypoint, claude.CLIEntrypoint))
	return map[string]string{"PATH": bin + ":/usr/bin:/bin"}
}

func harnessTestCurrentNodePlatform(t *testing.T) string {
	t.Helper()
	switch harnessPlatformKey() {
	case "linux-x86_64", "linux-aarch64", "macos-x86_64", "macos-aarch64":
		return harnessPlatformKey()
	}
	t.Fatalf("managed Node.js test has no artifact for %s", harnessPlatformKey())
	return ""
}

func harnessTestEnvironmentTarget(t *testing.T) string {
	t.Helper()
	switch harnessTestCurrentNodePlatform(t) {
	case "linux-x86_64":
		return "linux-x64"
	case "linux-aarch64":
		return "linux-arm64"
	case "macos-x86_64":
		return "darwin-x64"
	}
	return "darwin-arm64"
}

// harnessTestMetadata decodes one embedded runtime.json as a mutable tree.
func harnessTestMetadata(t *testing.T, data []byte) map[string]any {
	t.Helper()
	var metadata map[string]any
	if err := json.Unmarshal(data, &metadata); err != nil {
		t.Fatal(err)
	}
	return metadata
}

func harnessTestPlatform(t *testing.T, metadata map[string]any) map[string]any {
	t.Helper()
	platforms := metadata["platforms"].(map[string]any)
	key := harnessTestCurrentNodePlatform(t)
	platform, ok := platforms[key].(map[string]any)
	if !ok {
		platform = map[string]any{}
		platforms[key] = platform
	}
	return platform
}

func harnessTestSHA(t *testing.T, path string) string {
	t.Helper()
	digest, err := harnessFileSHA256(path)
	if err != nil {
		t.Fatal(err)
	}
	return digest
}

type harnessTarEntry struct {
	name, body, link string
	mode             int64
}

func harnessTestTarGz(t *testing.T, path string, entries []harnessTarEntry) {
	t.Helper()
	var compressed bytes.Buffer
	encoder := gzip.NewWriter(&compressed)
	builder := tar.NewWriter(encoder)
	for _, entry := range entries {
		header := &tar.Header{Name: entry.name, Mode: entry.mode, Size: int64(len(entry.body)), Typeflag: tar.TypeReg, Format: tar.FormatGNU}
		if entry.link != "" {
			header.Typeflag, header.Linkname, header.Size = tar.TypeSymlink, entry.link, 0
		}
		if err := builder.WriteHeader(header); err != nil {
			t.Fatal(err)
		}
		if entry.link == "" {
			if _, err := builder.Write([]byte(entry.body)); err != nil {
				t.Fatal(err)
			}
		}
	}
	if err := builder.Close(); err != nil {
		t.Fatal(err)
	}
	if err := encoder.Close(); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, compressed.Bytes(), 0o644); err != nil {
		t.Fatal(err)
	}
}

// harnessTestFakeNodeArchive is the fake Node.js archive plus filler
// one-byte files, so extraction takes long enough for a test to cancel it
// midway.
func harnessTestFakeNodeArchive(t *testing.T, root string, filler int) (string, map[string]any) {
	t.Helper()
	target := harnessTestEnvironmentTarget(t)
	archive := filepath.Join(root, fmt.Sprintf("node-v%s-%s.tar.gz", core.NodeVersion, target))
	folder := fmt.Sprintf("node-v%s-%s", core.NodeVersion, target)
	entries := []harnessTarEntry{
		{name: folder + "/bin/node", body: fmt.Sprintf("#!/bin/sh\necho v%s\n", core.NodeVersion), mode: 0o755},
		{name: folder + "/lib/node_modules/npm/bin/npm-cli.js", body: "#!/bin/sh\necho 11.6.2\n", mode: 0o755},
		{name: folder + "/bin/npm", link: "../lib/node_modules/npm/bin/npm-cli.js", mode: 0o777},
		{name: folder + "/bin/npx", link: "../lib/node_modules/npm/bin/npm-cli.js", mode: 0o777},
	}
	for index := range filler {
		entries = append(entries, harnessTarEntry{name: fmt.Sprintf("%s/lib/filler/%d", folder, index), body: "x", mode: 0o644})
	}
	harnessTestTarGz(t, archive, entries)
	metadata := harnessTestMetadata(t, nodeRuntimeJSON)
	harnessTestPlatform(t, metadata)["sha256"] = harnessTestSHA(t, archive)
	return archive, metadata
}

// harnessTestArtifactServer serves body to every request over loopback
// HTTP, which artifact downloads accept only in tests.
func harnessTestArtifactServer(t *testing.T, body []byte) string {
	t.Helper()
	server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, _ *http.Request) {
		response.Write(body)
	}))
	t.Cleanup(server.Close)
	return server.URL
}

func harnessTestFakeNodeServer(t *testing.T, archive, root string) (string, map[string]string) {
	t.Helper()
	tools := filepath.Join(root, "tools")
	if err := os.MkdirAll(tools, 0o777); err != nil {
		t.Fatal(err)
	}
	body, err := os.ReadFile(archive)
	if err != nil {
		t.Fatal(err)
	}
	mirror := harnessTestArtifactServer(t, body)
	return tools, map[string]string{"PATH": tools, "NODEJS_ORG_MIRROR": mirror}
}

func harnessTestRunShell(t *testing.T, path string) core.ProcessOutput {
	t.Helper()
	output, err := core.RunWithInput(exec.Command("/bin/sh", path), nil)
	if err != nil {
		t.Fatal(err)
	}
	return output
}

func harnessTestNoStaging(t *testing.T, root string) bool {
	t.Helper()
	entries, err := os.ReadDir(root)
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range entries {
		if strings.HasPrefix(entry.Name(), ".install-") {
			return false
		}
	}
	return true
}

func skipHarnessInstallOnWindows(t *testing.T) {
	t.Helper()
	if runtime.GOOS == "windows" {
		t.Skip("managed harness installation is Unix only")
	}
}

func TestDirectKimiArtifactInstallsThePinnedExecutable(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	body := []byte("#!/bin/sh\necho kimi-fixture\n")
	server := harnessTestArtifactServer(t, body)
	fixture := filepath.Join(temp, "kimi-artifact")
	if err := os.WriteFile(fixture, body, 0o644); err != nil {
		t.Fatal(err)
	}
	metadata := harnessTestMetadata(t, kimiRuntimeJSON)
	harnessTestPlatform(t, metadata)["sha256"] = harnessTestSHA(t, fixture)
	environment := map[string]string{"MJ_TEST_KIMI_ARTIFACT_BASE": server}
	staging := filepath.Join(temp, "staging")
	if err := os.MkdirAll(staging, 0o777); err != nil {
		t.Fatal(err)
	}

	if err := installKimiFromMetadata(context.Background(), staging, environment, metadata); err != nil {
		t.Fatal(err)
	}

	// The installed file was just written by this process, so it runs
	// through `/bin/sh` rather than being exec'd: see InstallFakeCommand on
	// ETXTBSY.
	launcher := filepath.Join(staging, "bin/kimi")
	if !EntrypointIsExecutable(launcher) {
		t.Fatal("launcher not executable")
	}
	output := harnessTestRunShell(t, launcher)
	if !output.Status.Success() || string(output.Stdout) != "kimi-fixture\n" {
		t.Fatalf("%v %q", output.Status, output.Stdout)
	}
}

func TestDirectGrokArtifactInstallsRelocatableLaunchers(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	body := []byte("#!/bin/sh\necho grok-fixture\n")
	encoder, err := zstd.NewWriter(nil, zstd.WithEncoderLevel(zstd.SpeedFastest))
	if err != nil {
		t.Fatal(err)
	}
	compressed := encoder.EncodeAll(body, nil)
	server := harnessTestArtifactServer(t, compressed)
	fixture := filepath.Join(temp, "grok-artifact.zst")
	if err := os.WriteFile(fixture, compressed, 0o644); err != nil {
		t.Fatal(err)
	}
	metadata := harnessTestMetadata(t, grokRuntimeJSON)
	harnessTestPlatform(t, metadata)["sha256"] = harnessTestSHA(t, fixture)
	environment := map[string]string{"MJ_TEST_GROK_ARTIFACT_BASE": server}
	staging := filepath.Join(temp, "staging")
	relocated := filepath.Join(temp, "relocated")
	if err := os.MkdirAll(staging, 0o777); err != nil {
		t.Fatal(err)
	}

	if err := installGrokFromMetadata(context.Background(), staging, environment, metadata); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(staging, relocated); err != nil {
		t.Fatal(err)
	}

	// The launchers were just written by this process, so they run through
	// `/bin/sh` rather than being exec'd: see InstallFakeCommand on ETXTBSY.
	// `sh` still opens `agent` through its relative link.
	for _, launcher := range []string{"grok", "agent"} {
		path := filepath.Join(relocated, "bin", launcher)
		if launcher == "agent" {
			if target, err := os.Readlink(path); err != nil || target != "grok" {
				t.Fatalf("agent link %q %v", target, err)
			}
		} else if info, err := os.Stat(path); err != nil || !info.Mode().IsRegular() {
			t.Fatalf("grok %v", err)
		}
		if !EntrypointIsExecutable(path) {
			t.Fatalf("%s not executable", launcher)
		}
		output := harnessTestRunShell(t, path)
		if !output.Status.Success() || string(output.Stdout) != "grok-fixture\n" {
			t.Fatalf("%v %q", output.Status, output.Stdout)
		}
	}
}

func TestAntigravityArchiveIsVerifiedBeforeExtractingTheNativeEntrypoint(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	archive := filepath.Join(temp, "fixture.tar.gz")
	harnessTestTarGz(t, archive, []harnessTarEntry{{name: "antigravity", body: "#!/bin/sh\nprintf 'agy-fixture\\n'\n", mode: 0o755}})
	data, err := os.ReadFile(archive)
	if err != nil {
		t.Fatal(err)
	}
	server := harnessTestArtifactServer(t, data)
	metadata := harnessTestMetadata(t, antigravityRuntimeJSON)
	platform := harnessTestPlatform(t, metadata)
	platform["url"] = server
	platform["sha256"] = harnessTestSHA(t, archive)
	staging := filepath.Join(temp, "install")
	if err := os.Mkdir(staging, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := installAntigravityFromMetadata(context.Background(), staging, map[string]string{}, metadata); err != nil {
		t.Fatal(err)
	}
	// Just extracted by this process, so it runs through `/bin/sh` rather
	// than being exec'd: see InstallFakeCommand on ETXTBSY.
	entrypoint := filepath.Join(staging, "bin/agy")
	if !EntrypointIsExecutable(entrypoint) {
		t.Fatal("entrypoint not executable")
	}
	output := harnessTestRunShell(t, entrypoint)
	if !output.Status.Success() || string(output.Stdout) != "agy-fixture\n" {
		t.Fatalf("%v %q", output.Status, output.Stdout)
	}
	failed := filepath.Join(temp, "failed")
	if err := os.Mkdir(failed, 0o777); err != nil {
		t.Fatal(err)
	}
	platform["sha256"] = "wrong"
	err = installAntigravityFromMetadata(context.Background(), failed, map[string]string{}, metadata)
	if err == nil || !strings.Contains(err.Error(), "checksum mismatch") {
		t.Fatal(err)
	}
	for _, leftover := range []string{"agy.tar.gz", "bin/agy"} {
		if _, err := os.Lstat(filepath.Join(failed, leftover)); err == nil {
			t.Fatalf("%s left behind", leftover)
		}
	}
}

// Hard-won: #1520: Antigravity bypassed managed preparation and failed on empty targets.
func TestAntigravityRuntimePoliciesResolveOnePinnedCLIAndPreserveTheAccount(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	home := filepath.Join(temp, "account with spaces")
	cache := filepath.Join(temp, "cache")
	root := filepath.Join(cache, "mjolnir/harnesses/antigravity")
	selected := core.Pin(core.HarnessKindAntigravity)
	if err := os.MkdirAll(root, 0o777); err != nil {
		t.Fatal(err)
	}
	harnessTestCompleteFake(t, root, core.HarnessKindAntigravity, selected.InstallID, selected.Entrypoint)
	managedPath := filepath.Join(root, selected.InstallID, selected.Entrypoint)
	tools := filepath.Join(temp, "tools")
	if err := os.Mkdir(tools, 0o777); err != nil {
		t.Fatal(err)
	}
	environment := map[string]string{
		"HOME":                temp,
		"XDG_CACHE_HOME":      cache,
		"PATH":                tools,
		"MJ_ANTIGRAVITY_HOME": home,
	}
	policy := core.ExecutionPolicyUnconstrained
	command, args := core.FallbackLaunch(core.HarnessKindAntigravity, policy)
	resolve := func(runtimePolicy core.HarnessRuntimePolicy) (Located, error) {
		return Locate(context.Background(), runtimePolicy, core.HarnessKindAntigravity, policy, command, args, environment)
	}
	missing, err := resolve(core.HarnessRuntimePolicyAmbient)
	if err != nil {
		t.Fatal(err)
	}
	if missing.Command != managedPath || missing.Managed == nil {
		t.Fatalf("missing: %q %v", missing.Command, missing.Managed)
	}
	if missing.Environment["AGY_CLI_DISABLE_AUTO_UPDATE"] != "true" {
		t.Fatal("auto update not disabled")
	}
	gemini := false
	for index := 0; index+1 < len(missing.Args); index++ {
		if missing.Args[index] == "--gemini-dir" && missing.Args[index+1] == home {
			gemini = true
		}
	}
	if !gemini || !slices.Contains(missing.Args, "--dangerously-skip-permissions") {
		t.Fatalf("args %q", missing.Args)
	}
	missing.Managed.Release()
	ambient := filepath.Join(tools, "agy")
	// Lookup canonicalizes executable symlinks; use a real script rather
	// than the shared fake-command dispatcher's symlink.
	if err := os.WriteFile(ambient, []byte(fmt.Sprintf("#!/bin/sh\n[ \"$AGY_CLI_DISABLE_AUTO_UPDATE\" = true ] || exit 23\necho %s\n", core.AntigravityVersion)), 0o755); err != nil {
		t.Fatal(err)
	}
	matching, err := resolve(core.HarnessRuntimePolicyAmbient)
	if err != nil {
		t.Fatal(err)
	}
	// Canonical, like the lookup: macOS keeps temporary directories under
	// the /var -> /private/var link.
	canonical, err := canonicalizePath(ambient)
	if err != nil {
		t.Fatal(err)
	}
	if matching.Command != canonical || matching.Managed != nil {
		t.Fatalf("matching %q %v", matching.Command, matching.Managed)
	}
	managed, err := resolve(core.HarnessRuntimePolicyManaged)
	if err != nil || managed.Command != managedPath {
		t.Fatalf("managed %q %v", managed.Command, err)
	}
	managed.Managed.Release()
	if err := os.WriteFile(ambient, []byte("#!/bin/sh\necho 0.0.0\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	stale, err := resolve(core.HarnessRuntimePolicyAmbient)
	if err != nil || stale.Command != managedPath {
		t.Fatalf("stale %q %v", stale.Command, err)
	}
	stale.Managed.Release()
	if _, err := resolve(core.HarnessRuntimePolicyImage); err == nil || !strings.Contains(err.Error(), "image must provide agy") {
		t.Fatal(err)
	}
	custom, err := Locate(context.Background(), core.HarnessRuntimePolicyManaged, core.HarnessKindAntigravity, policy, "/custom/agy", nil, environment)
	if err != nil || custom.Command != "/custom/agy" || custom.Managed != nil {
		t.Fatalf("custom %q %v", custom.Command, err)
	}
}

func TestArchiveExtractionSelectsOnlyTheExpectedRegularFile(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	archive := filepath.Join(temp, "opencode.tgz")
	harnessTestTarGz(t, archive, []harnessTarEntry{
		{name: "package/bin/opencode", body: "#!/bin/sh\necho ok\n", mode: 0o755},
		{name: "package/unrelated", body: "must not be extracted\n", mode: 0o755},
	})
	destination := filepath.Join(temp, "opencode")

	if err := extractArchiveFile(context.Background(), archive, "package/bin/opencode", destination); err != nil {
		t.Fatal(err)
	}

	if data, err := os.ReadFile(destination); err != nil || string(data) != "#!/bin/sh\necho ok\n" {
		t.Fatalf("%q %v", data, err)
	}
	if _, err := os.Stat(filepath.Join(temp, "package/unrelated")); err == nil {
		t.Fatal("unrelated entry extracted")
	}
}

func TestLinkRelocationPreservesRelativeAndExternalLinksWithoutFollowingThem(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	root := filepath.Join(temp, "staging")
	harnessTestExecutable(t, filepath.Join(root, "bin/tool"), "#!/bin/sh\nexit 0\n")
	external := filepath.Join(temp, "external")
	if err := os.Mkdir(external, 0o777); err != nil {
		t.Fatal(err)
	}
	for _, link := range [][2]string{
		{"tool", filepath.Join(root, "bin/relative")},
		{external, filepath.Join(root, "external")},
		{filepath.Join(root, "bin/tool"), filepath.Join(external, "untouched")},
	} {
		if err := os.Symlink(link[0], link[1]); err != nil {
			t.Fatal(err)
		}
	}
	if err := relativizeInternalLinks(root, root); err != nil {
		t.Fatal(err)
	}
	for _, want := range [][2]string{
		{filepath.Join(root, "bin/relative"), "tool"},
		{filepath.Join(root, "external"), external},
		{filepath.Join(external, "untouched"), filepath.Join(root, "bin/tool")},
	} {
		if target, err := os.Readlink(want[0]); err != nil || target != want[1] {
			t.Fatalf("%s -> %q (%v), want %q", want[0], target, err, want[1])
		}
	}
}

func TestInvalidInstallIsNotRemovedWhileLeased(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	cache := filepath.Join(temp, "cache")
	selected := core.Pin(core.HarnessKindGrok)
	install := filepath.Join(cache, "grok", selected.InstallID)
	harnessTestCompleteFake(t, filepath.Join(cache, "grok"), core.HarnessKindGrok, selected.InstallID, "bin/grok")
	harnessTestExecutable(t, filepath.Join(install, "bin/agent"), "#!/bin/sh\nexit 0\n")
	lease, err := AcquireSupervisorLease(filepath.Join(install, core.LeaseFile))
	if err != nil {
		t.Fatal(err)
	}
	defer lease.Drop()
	if err := os.Remove(filepath.Join(install, "bin/grok")); err != nil {
		t.Fatal(err)
	}
	_, err = resolveAt(cache, core.HarnessKindGrok, core.ExecutionPolicyConfiguredApprovals, map[string]string{})
	if err == nil || !strings.Contains(err.Error(), "repair deferred") {
		t.Fatal(err)
	}
	if _, err := os.Stat(install); err != nil {
		t.Fatal("leased install removed")
	}
}

func TestPendingInstallScriptsNamesPackagesNpmBlocked(t *testing.T) {
	// Recorded from npm 12.0.2, which blocks dependency install scripts
	// unless the root package lists them in allowScripts.
	recorded := `{"allowScripts":[{"name":"esbuild","changes":[{"key":"esbuild@0.25.0","change":"pending"}]}]}`
	if got, err := pendingInstallScripts(recorded); err != nil || !slices.Equal(got, []string{"esbuild@0.25.0"}) {
		t.Fatalf("%q %v", got, err)
	}
	if got, err := pendingInstallScripts(`{"allowScripts":[]}`); err != nil || len(got) != 0 {
		t.Fatalf("%q %v", got, err)
	}
	if _, err := pendingInstallScripts("not json"); err == nil {
		t.Fatal("not json accepted")
	}
}

func TestNodePinMatchesRuntimeMetadata(t *testing.T) {
	metadata := harnessTestMetadata(t, nodeRuntimeJSON)
	if metadata["version"] != core.NodeVersion {
		t.Fatalf("version %v", metadata["version"])
	}
}

func TestKimiPinMatchesRuntimeMetadata(t *testing.T) {
	metadata := harnessTestMetadata(t, kimiRuntimeJSON)
	if metadata["version"] != core.KimiVersion {
		t.Fatalf("version %v", metadata["version"])
	}
	if _, ok := harnessTestPlatform(t, metadata)["sha256"].(string); !ok {
		t.Fatal("no sha256")
	}
}

func TestGrokPinMatchesRuntimeMetadata(t *testing.T) {
	metadata := harnessTestMetadata(t, grokRuntimeJSON)
	if metadata["version"] != core.GrokVersion {
		t.Fatalf("version %v", metadata["version"])
	}
	if _, ok := harnessTestPlatform(t, metadata)["sha256"].(string); !ok {
		t.Fatal("no sha256")
	}
}

func TestNodeDownloadUsesAValidatedHTTPSMirror(t *testing.T) {
	file := fmt.Sprintf("node-v%s-linux-x64.tar.gz", core.NodeVersion)
	check := func(environment map[string]string, want string) {
		t.Helper()
		got, err := nodeDistributionURL(environment, file)
		if err != nil || got != want {
			t.Fatalf("%q %v, want %q", got, err, want)
		}
	}
	check(map[string]string{}, fmt.Sprintf("https://nodejs.org/dist/v%s/%s", core.NodeVersion, file))
	check(map[string]string{"NVM_NODEJS_ORG_MIRROR": "https://mirror.example/node/"},
		fmt.Sprintf("https://mirror.example/node/v%s/%s", core.NodeVersion, file))
	// An explicitly empty NODEJS_ORG_MIRROR clears the inherited NVM one.
	check(map[string]string{"NODEJS_ORG_MIRROR": "", "NVM_NODEJS_ORG_MIRROR": "https://nvm.example/node"},
		fmt.Sprintf("https://nodejs.org/dist/v%s/%s", core.NodeVersion, file))
	check(map[string]string{"NVM_NODEJS_ORG_MIRROR": "https://nvm.example/node", "NODEJS_ORG_MIRROR": "https://mjolnir.example/node/"},
		fmt.Sprintf("https://mjolnir.example/node/v%s/%s", core.NodeVersion, file))

	for _, mirror := range []string{
		"http://mirror.example/node",
		"https://user:secret@mirror.example/node",
		"https://mirror.example/node?token=secret",
		"http://user:secret@mirror.example/node",
		"not a url secret",
		"user:secret@mirror.example/node",
	} {
		_, err := nodeDistributionURL(map[string]string{"NODEJS_ORG_MIRROR": mirror}, file)
		if err == nil {
			t.Fatalf("%q was accepted", mirror)
		}
		// The daemon logs this error, so it must not echo the secret.
		if message := err.Error(); !strings.Contains(message, "NODEJS_ORG_MIRROR") || strings.Contains(message, "secret") {
			t.Fatal(message)
		}
	}
}

func TestLinuxNodeArtifactRequiresItsProgramInterpreter(t *testing.T) {
	metadata := harnessTestMetadata(t, nodeRuntimeJSON)
	platform := map[string]any{}
	for key, value := range harnessTestPlatform(t, metadata) {
		platform[key] = value
	}
	platform["interpreter"] = "/definitely/missing/mjolnir-node-program-interpreter"
	err := ensureNodeArtifactPlatform(platform)
	if err == nil || !strings.Contains(err.Error(), "required program interpreter") || !strings.Contains(err.Error(), "Install Node.js 22 or newer") {
		t.Fatal(err)
	}
}

func TestMusePinMatchesRuntimeMetadata(t *testing.T) {
	metadata := harnessTestMetadata(t, museRuntimeJSON)
	if metadata["muse_version"] != core.MuseVersion {
		t.Fatalf("muse_version %v", metadata["muse_version"])
	}
	bridge, ok := core.NpmBridgeFor(core.HarnessKindMuse)
	if !ok {
		t.Fatal("Muse installs its adapter from npm")
	}
	if bridge.Package != core.MuseACPPackage || bridge.Version != core.MuseACPVersion {
		t.Fatalf("bridge %+v", bridge)
	}
	selected := core.Pin(core.HarnessKindMuse)
	if selected.InstallID != fmt.Sprintf("muse-acp-%s_muse-%s", core.MuseACPVersion, core.MuseVersion) {
		t.Fatalf("install id %q", selected.InstallID)
	}
	if selected.DisplayVersion != fmt.Sprintf("muse-acp %s + Muse Code %s", core.MuseACPVersion, core.MuseVersion) {
		t.Fatalf("display %q", selected.DisplayVersion)
	}
}

func TestChecksumFailureRemovesTheDownloadedArtifact(t *testing.T) {
	temp := t.TempDir()
	server := harnessTestArtifactServer(t, []byte("corrupted"))
	output := filepath.Join(temp, "download")
	err := downloadVerified(server+"/muse", output, strings.Repeat("0", 64), map[string]string{})
	if err == nil || !strings.Contains(err.Error(), "checksum mismatch") {
		t.Fatal(err)
	}
	if _, err := os.Stat(output); err == nil {
		t.Fatal("artifact left behind")
	}
}

func TestAmbientNativeLaunchesUseTargetToolsAndKeepCustomCommands(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	tools := t.TempDir()
	for _, command := range []string{"kimi", "grok", "opencode"} {
		core.InstallFakeCommand(tools, command, "#!/bin/sh\nexit 0\n")
	}
	environment := map[string]string{"PATH": tools}
	for _, harness := range []core.HarnessKind{core.HarnessKindKimi, core.HarnessKindGrok, core.HarnessKindOpenCode} {
		if ok, err := targetNativeHarnessIsAvailable(harness, environment); err != nil || !ok {
			t.Fatalf("%s should reuse its target-provided executable: %v", harness.DisplayName(), err)
		}
		if ok, err := targetNativeHarnessIsAvailable(harness, map[string]string{}); err != nil || ok {
			t.Fatalf("%s should fall back when its executable is absent: %v", harness.DisplayName(), err)
		}
		command, args := core.FallbackLaunch(harness, core.ExecutionPolicyUnconstrained)
		located, err := Locate(context.Background(), core.HarnessRuntimePolicyAmbient, harness, core.ExecutionPolicyUnconstrained, command, args, environment)
		if err != nil {
			t.Fatal(err)
		}
		if located.Managed != nil || located.Command != command {
			t.Fatalf("%s: %q", harness.DisplayName(), located.Command)
		}
	}
	dshCommand, dshArgs := core.FallbackLaunch(core.HarnessKindDsh, core.ExecutionPolicyUnconstrained)
	for _, c := range []struct {
		harness core.HarnessKind
		command string
		args    []string
	}{
		{core.HarnessKindKimi, "/opt/custom-kimi", []string{}},
		{core.HarnessKindDsh, dshCommand, dshArgs},
	} {
		located, err := Locate(context.Background(), core.HarnessRuntimePolicyAmbient, c.harness, core.ExecutionPolicyUnconstrained, c.command, c.args, map[string]string{})
		if err != nil {
			t.Fatal(err)
		}
		if located.Managed != nil || located.Command != c.command || !slices.Equal(located.Args, c.args) {
			t.Fatalf("%s's launch must be kept as given", c.harness.DisplayName())
		}
	}
}

// Hard-won: #1477: upgrading a retained container's worker left its older Claude bridge in use.
func TestAnOlderContainerClaudeBridgeSelectsTheManagedPinWithoutReplacingIt(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	cacheHome := filepath.Join(temp, "cache")
	cache, err := core.ManagedHarnessCacheRoot(cacheHome, "")
	if err != nil {
		t.Fatal(err)
	}
	selected := core.Pin(core.HarnessKindClaude)
	root := filepath.Join(cache, core.HarnessKindClaude.ID())
	harnessTestCompleteFake(t, root, core.HarnessKindClaude, selected.InstallID, selected.Entrypoint)
	install := filepath.Join(root, selected.InstallID)
	environment := harnessTestFakeNodeTools(t, temp, filepath.Join(temp, "unused"))
	environment["XDG_CACHE_HOME"] = cacheHome
	environment["HOME"] = temp
	imageBridge := filepath.Join(temp, "bin/claude-agent-acp")
	probe, err := filepath.Abs(filepath.Join("testdata", "harness_install", "bridge-version.sh"))
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(probe, imageBridge); err != nil {
		t.Fatal(err)
	}
	environment["MJ_TEST_BRIDGE_VERSION"] = "0.99.0"
	command, args := core.FallbackLaunch(core.HarnessKindClaude, core.ExecutionPolicyConfiguredApprovals)
	located, err := Locate(context.Background(), core.HarnessRuntimePolicyAmbient, core.HarnessKindClaude, core.ExecutionPolicyConfiguredApprovals, command, args, environment)
	if err != nil {
		t.Fatal(err)
	}
	if located.Managed == nil || located.Command != filepath.Join(install, selected.Entrypoint) {
		t.Fatalf("located %q", located.Command)
	}
	if located.Environment["CLAUDE_CODE_EXECUTABLE"] != filepath.Join(install, selected.CLIEntrypoint) {
		t.Fatalf("executable %q", located.Environment["CLAUDE_CODE_EXECUTABLE"])
	}
	if target, err := os.Readlink(imageBridge); err != nil || target != probe {
		t.Fatalf("image bridge replaced: %q %v", target, err)
	}
	located.Managed.Release()

	bridge, _ := core.NpmBridgeFor(core.HarnessKindClaude)
	environment["MJ_TEST_BRIDGE_VERSION"] = bridge.Version
	located, err = Locate(context.Background(), core.HarnessRuntimePolicyAmbient, core.HarnessKindClaude, core.ExecutionPolicyConfiguredApprovals, command, args, environment)
	if err != nil {
		t.Fatal(err)
	}
	canonical, err := canonicalizePath(imageBridge)
	if err != nil {
		t.Fatal(err)
	}
	if located.Managed != nil || located.Command != canonical {
		t.Fatalf("located %q", located.Command)
	}
}

// Hard-won: #1392: an ambient bridge selected from PATH could fail to spawn with ENOENT.
func TestAnUnspawnableAmbientBridgeSelectsTheManagedPin(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	cacheHome := filepath.Join(temp, "cache")
	cache, err := core.ManagedHarnessCacheRoot(cacheHome, "")
	if err != nil {
		t.Fatal(err)
	}
	installs := filepath.Join(temp, "installs")
	environment := harnessTestFakeNodeTools(t, temp, installs)
	environment["XDG_CACHE_HOME"] = cacheHome
	environment["HOME"] = filepath.Join(temp, "absent-home")
	harness := core.HarnessKindCodex
	bridge, _ := core.NpmBridgeFor(harness)
	environment["MJ_TEST_BRIDGE_VERSION"] = bridge.Package + " " + bridge.Version
	approvals := core.ExecutionPolicyConfiguredApprovals
	command, args := core.FallbackLaunch(harness, approvals)
	launcher := filepath.Join(temp, "bin/codex-acp")
	fixtures, err := filepath.Abs(filepath.Join("testdata", "harness_install"))
	if err != nil {
		t.Fatal(err)
	}
	locate := func(policy core.HarnessRuntimePolicy) (Located, error) {
		return Locate(context.Background(), policy, harness, approvals, command, args, environment)
	}

	if err := os.Symlink(filepath.Join(fixtures, "bridge-version.sh"), launcher); err != nil {
		t.Fatal(err)
	}
	harnessTestExecutable(t, filepath.Join(temp, "bin/codex"), "#!/bin/sh\nexit 0\n")
	located, err := locate(core.HarnessRuntimePolicyImage)
	if err != nil {
		t.Fatal(err)
	}
	canonical, err := canonicalizePath(launcher)
	if err != nil {
		t.Fatal(err)
	}
	if located.Command != canonical {
		t.Fatalf("located %q, want %q", located.Command, canonical)
	}

	if err := os.Remove(launcher); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(filepath.Join(fixtures, "bridge-missing-interpreter.sh"), launcher); err != nil {
		t.Fatal(err)
	}
	_, err = locate(core.HarnessRuntimePolicyImage)
	if err == nil {
		t.Fatal("an image bridge that cannot start must be an error")
	}
	var cause syscall.Errno
	if !errors.As(err, &cause) || !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("not a NotFound cause: %v", err)
	}
	message := err.Error()
	canonical, err = canonicalizePath(launcher)
	if err != nil {
		t.Fatal(err)
	}
	for _, detail := range []string{canonical, "working directory /:", cause.Error()} {
		if !strings.Contains(message, detail) {
			t.Fatalf("%q missing from %s", detail, message)
		}
	}
	if _, err := os.Stat(cache); err == nil {
		t.Fatal("the image source must not create a managed cache")
	}

	located, err = locate(core.HarnessRuntimePolicyAmbient)
	if err != nil {
		t.Fatal(err)
	}
	if located.Managed == nil {
		t.Fatal("an unspawnable ambient bridge must select the managed pin")
	}
	defer located.Managed.Release()
	install := core.ManagedHarnessInstallDir(cache, harness)
	if want := filepath.Join(install, core.Pin(harness).Entrypoint); located.Command != want {
		t.Fatalf("located %q, want %q", located.Command, want)
	}
	if body, err := os.ReadFile(installs); err != nil || string(body) != "x" {
		t.Fatalf("installs %q %v", body, err)
	}
}

// The daemon container's only harness source is its image: a missing program
// is an error naming the image, a complete image is used as is, and neither
// case touches a managed cache under `HOME`.
func TestImageHarnessesNeverFallBackToAManagedInstall(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	home := t.TempDir()
	tools := t.TempDir()
	environment := func(path string) map[string]string {
		return map[string]string{"HOME": home, "PATH": path, "MJ_ANTIGRAVITY_HOME": home}
	}
	for _, harness := range core.HarnessKindAll {
		command, args := core.FallbackLaunch(harness, core.ExecutionPolicyUnconstrained)
		_, err := Locate(context.Background(), core.HarnessRuntimePolicyImage, harness, core.ExecutionPolicyUnconstrained, command, args, environment(tools))
		if err == nil || !strings.Contains(err.Error(), "the daemon's image does not provide") {
			t.Fatalf("%s: %v", harness.DisplayName(), err)
		}
	}
	codex, _ := core.NpmBridgeFor(core.HarnessKindCodex)
	versions := map[string]string{
		"agy":              core.AntigravityVersion,
		"codex-acp":        codex.Package + " " + codex.Version,
		"claude-agent-acp": core.ClaudeACPVersion,
		"dsh-acp":          "dsh-acp " + core.DshACPVersion,
	}
	for _, harness := range core.HarnessKindAll {
		bridge, entry, cli := imagePrograms(harness)
		for _, name := range []string{bridge, entry, cli} {
			// Real files: lookup canonicalizes, which would turn a
			// dispatcher symlink into the dispatcher itself.
			program := filepath.Join(tools, name)
			if err := os.WriteFile(program, []byte(fmt.Sprintf("#!/bin/sh\necho '%s'\n", versions[name])), 0o755); err != nil {
				t.Fatal(err)
			}
		}
	}
	canonicalTools, err := canonicalizePath(tools)
	if err != nil {
		t.Fatal(err)
	}
	for _, harness := range core.HarnessKindAll {
		command, args := core.FallbackLaunch(harness, core.ExecutionPolicyUnconstrained)
		located, err := Locate(context.Background(), core.HarnessRuntimePolicyImage, harness, core.ExecutionPolicyUnconstrained, command, args, environment(tools))
		if err != nil {
			t.Fatalf("%s: %v", harness.DisplayName(), err)
		}
		if located.Managed != nil {
			t.Fatalf("%s: managed", harness.DisplayName())
		}
		if harness == core.HarnessKindAntigravity {
			if located.Environment["AGY_CLI_DISABLE_AUTO_UPDATE"] != "true" {
				t.Fatal("auto update not disabled")
			}
			gemini := false
			for index := 0; index+1 < len(located.Args); index++ {
				if located.Args[index] == "--gemini-dir" && located.Args[index+1] == home {
					gemini = true
				}
			}
			if !gemini {
				t.Fatalf("args %q", located.Args)
			}
		}
		if located.CLIEntrypoint == nil {
			t.Fatalf("%s must use the image's CLI", harness.DisplayName())
		}
		if _, ok := harnessStripPrefix(*located.CLIEntrypoint, canonicalTools); !ok {
			t.Fatalf("%s must use the image's CLI: %q", harness.DisplayName(), *located.CLIEntrypoint)
		}
	}
	if _, err := os.Stat(filepath.Join(home, ".cache")); err == nil {
		t.Fatal("the image source must not create a managed cache")
	}
}

func TestCompatibleSystemNodeIsReusedWithoutDownloading(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	environment := harnessTestFakeNodeTools(t, temp, filepath.Join(temp, "unused"))
	environment["XDG_CACHE_HOME"] = filepath.Join(temp, "cache")
	originalPath := environment["PATH"]

	runtime, err := EnsureNodeRuntime(context.Background(), environment)
	if err != nil {
		t.Fatal(err)
	}

	if runtime != nil {
		t.Fatal("system Node.js replaced")
	}
	if environment["PATH"] != originalPath {
		t.Fatalf("PATH %q", environment["PATH"])
	}
	if _, err := os.Stat(filepath.Join(temp, "cache/mjolnir/node")); err == nil {
		t.Fatal("managed cache created")
	}
}

func TestMissingNodeIsDownloadedVerifiedAndPrependedOnce(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	archive, metadata := harnessTestFakeNodeArchive(t, temp, 0)
	_, environment := harnessTestFakeNodeServer(t, archive, temp)
	environment["XDG_CACHE_HOME"] = filepath.Join(temp, "cache")
	root := filepath.Join(temp, "node")

	runtime, err := ensureNodeRuntimeFromMetadata(context.Background(), root, environment, metadata)
	if err != nil {
		t.Fatal(err)
	}
	if runtime == nil {
		t.Fatal("missing Node.js selects the managed installation")
	}
	install := runtime.Path()
	if filepath.Base(install) != fmt.Sprintf("node-v%s-%s", core.NodeVersion, harnessTestEnvironmentTarget(t)) {
		t.Fatalf("install %q", install)
	}
	for _, name := range []string{"node", "npm", "npx"} {
		if info, err := os.Stat(filepath.Join(install, "bin", name)); err != nil || !info.Mode().IsRegular() {
			t.Fatalf("%s missing: %v", name, err)
		}
	}
	if first := filepath.SplitList(environment["PATH"])[0]; first != filepath.Join(install, "bin") {
		t.Fatalf("PATH starts with %q", first)
	}
	runtime.Release()

	firstPath := environment["PATH"]
	runtime, err = installManagedNode(context.Background(), root, environment, metadata)
	if err != nil {
		t.Fatal(err)
	}
	defer runtime.Release()
	if runtime.Path() != install || environment["PATH"] != firstPath {
		t.Fatalf("%q %q", runtime.Path(), environment["PATH"])
	}
	if !harnessTestNoStaging(t, root) {
		t.Fatal("staging left behind")
	}
}

// The extraction runs on its own goroutine, so the installer must await it:
// staging and the install lock are released only after its writer stopped.
func TestCancellingNodeExtractionStopsTheWriterBeforeStagingIsReleased(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	archive, metadata := harnessTestFakeNodeArchive(t, temp, 30_000)
	_, environment := harnessTestFakeNodeServer(t, archive, temp)
	environment["XDG_CACHE_HOME"] = filepath.Join(temp, "cache")
	root := filepath.Join(temp, "node")
	folder := fmt.Sprintf("node-v%s-%s", core.NodeVersion, harnessTestEnvironmentTarget(t))

	// The extractor stops at entry 100, with 29,900 entries still to unpack,
	// until the test has cancelled: cancellation then lands mid-extraction
	// however the goroutines are scheduled.
	reached := make(chan struct{}, 1)
	resume := make(chan struct{})
	// The hook pauses the first extraction under root only: the later
	// install that proves the cache is repaired must run unhindered. The
	// wait is bounded so a broken test fails instead of hanging.
	var paused atomic.Bool
	registerNodeExtractionHook(root, func(index int) {
		if index == 100 && !paused.Swap(true) {
			reached <- struct{}{}
			select {
			case <-resume:
			case <-time.After(60 * time.Second):
				panic("the test never resumed the paused extractor")
			}
		}
	})
	cancelledEnvironment := maps2Clone(environment)
	token, cancel := context.WithCancel(context.Background())
	ctx := core.WithPreparationCancellation(context.Background(), token)
	result := make(chan error, 1)
	go func() {
		runtime, err := installManagedNode(ctx, root, cancelledEnvironment, metadata)
		if runtime != nil {
			runtime.Release()
		}
		result <- err
	}()
	select {
	case err := <-result:
		t.Fatalf("install ended before extraction paused: %v", err)
	case <-reached:
	}
	cancel()
	close(resume)
	err := <-result
	if err == nil {
		t.Fatal("a cancelled install must fail")
	}
	if !strings.Contains(err.Error(), "while extracting") {
		t.Fatalf("cancellation did not land during extraction: %v", err)
	}
	if !harnessTestNoStaging(t, root) {
		t.Fatal("staging outlived the install")
	}
	if _, err := os.Stat(filepath.Join(root, folder)); err == nil {
		t.Fatal("cancelled install published")
	}

	runtime, err := installManagedNode(context.Background(), root, environment, metadata)
	if err != nil {
		t.Fatal(err)
	}
	defer runtime.Release()
	if info, err := os.Stat(filepath.Join(runtime.Path(), "bin/node")); err != nil || !info.Mode().IsRegular() {
		t.Fatal("node missing after repair")
	}
}

// A probe cut short by cancellation is not evidence that the cached Node.js
// is broken: the installer must report the cancellation and keep the cache.
func TestCancellingTheCachedNodeProbeKeepsTheCache(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	_, metadata := harnessTestFakeNodeArchive(t, temp, 0)
	root := filepath.Join(temp, "node")
	install := filepath.Join(root, fmt.Sprintf("node-v%s-%s", core.NodeVersion, harnessTestEnvironmentTarget(t)))
	started := filepath.Join(temp, "probe-started")
	harnessTestExecutable(t, filepath.Join(install, "bin/node"), fmt.Sprintf("#!/bin/sh\n: > '%s'\nexec sleep 300\n", started))
	for _, name := range []string{"npm", "npx"} {
		harnessTestExecutable(t, filepath.Join(install, "bin", name), "#!/bin/sh\nexit 0\n")
	}
	lease, err := openLock(filepath.Join(install, core.LeaseFile))
	if err != nil {
		t.Fatal(err)
	}
	lease.Close()
	environment := map[string]string{"PATH": "/usr/bin:/bin"}

	token, cancel := context.WithCancel(context.Background())
	ctx := core.WithPreparationCancellation(context.Background(), token)
	result := make(chan error, 1)
	go func() {
		runtime, err := installManagedNode(ctx, root, environment, metadata)
		if runtime != nil {
			runtime.Release()
		}
		result <- err
	}()
	for waited := 0; ; waited++ {
		if _, err := os.Stat(started); err == nil {
			break
		}
		if waited >= 500 {
			t.Fatal("the cached Node.js probe never started")
		}
		select {
		case err := <-result:
			t.Fatalf("call ended before the probe ran: %v", err)
		case <-time.After(20 * time.Millisecond):
		}
	}
	cancel()
	err = <-result
	if err == nil {
		t.Fatal("a cancelled call must fail")
	}
	if !core.IsPreparationCancelled(err) {
		t.Fatalf("not a cancellation: %v", err)
	}
	if !EntrypointIsExecutable(filepath.Join(install, "bin/node")) {
		t.Fatal("cancellation removed the cached Node.js")
	}
	if !harnessTestNoStaging(t, root) {
		t.Fatal("cancellation left set-aside or staging directories")
	}
}

func TestUnsupportedSystemNodeIsReplacedWithTheManagedRuntime(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	archive, metadata := harnessTestFakeNodeArchive(t, temp, 0)
	tools, environment := harnessTestFakeNodeServer(t, archive, temp)
	harnessTestExecutable(t, filepath.Join(tools, "node"), "#!/bin/sh\nif [ \"$1\" = --version ]; then echo v20.19.0; fi\n")
	for _, command := range []string{"npm", "npx"} {
		harnessTestExecutable(t, filepath.Join(tools, command), "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 11.6.2; fi\n")
	}

	runtime, err := ensureNodeRuntimeFromMetadata(context.Background(), filepath.Join(temp, "node"), environment, metadata)
	if err != nil {
		t.Fatal(err)
	}
	if runtime == nil {
		t.Fatal("Node.js 20 selects the managed installation")
	}
	defer runtime.Release()
	if first := filepath.SplitList(environment["PATH"])[0]; first != filepath.Join(runtime.Path(), "bin") {
		t.Fatalf("PATH starts with %q", first)
	}
}

func TestCorruptCachedNodeIsRepairedFromTheVerifiedArchive(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	archive, metadata := harnessTestFakeNodeArchive(t, temp, 0)
	_, environment := harnessTestFakeNodeServer(t, archive, temp)
	root := filepath.Join(temp, "node")
	install := filepath.Join(root, fmt.Sprintf("node-v%s-%s", core.NodeVersion, harnessTestEnvironmentTarget(t)))
	for _, name := range []string{"node", "npm", "npx"} {
		harnessTestExecutable(t, filepath.Join(install, "bin", name), "#!/definitely/missing/mjolnir-node-program-interpreter\nexit 1\n")
	}
	lease, err := openLock(filepath.Join(install, core.LeaseFile))
	if err != nil {
		t.Fatal(err)
	}
	lease.Close()

	runtime, err := installManagedNode(context.Background(), root, environment, metadata)
	if err != nil {
		t.Fatal(err)
	}
	defer runtime.Release()

	if runtime.Path() != install {
		t.Fatalf("install %q", runtime.Path())
	}
	output, err := core.RunWithInput(exec.Command(filepath.Join(install, "bin/node")), nil)
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() || string(output.Stdout) != fmt.Sprintf("v%s\n", core.NodeVersion) {
		t.Fatalf("%v %q", output.Status, output.Stdout)
	}
}

func TestManagedNodeChecksumFailureNeverPublishesAnInstall(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	archive, metadata := harnessTestFakeNodeArchive(t, temp, 0)
	_, environment := harnessTestFakeNodeServer(t, archive, temp)
	harnessTestPlatform(t, metadata)["sha256"] = strings.Repeat("0", 64)
	root := filepath.Join(temp, "node")

	_, err := installManagedNode(context.Background(), root, environment, metadata)

	if err == nil || !strings.Contains(err.Error(), "checksum mismatch") {
		t.Fatal(err)
	}
	entries, err := os.ReadDir(root)
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range entries {
		if strings.HasPrefix(entry.Name(), "node-v") {
			t.Fatal("a corrupt download must not leave a final Node.js install")
		}
	}
}

func TestConcurrentFirstUsePublishesOneManagedNodeInstall(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	archive, metadata := harnessTestFakeNodeArchive(t, temp, 0)
	_, environment := harnessTestFakeNodeServer(t, archive, temp)
	root := filepath.Join(temp, "node")
	var start, done sync.WaitGroup
	start.Add(1)
	runtimes := make([]*ManagedNodeRuntime, 2)
	errs := make([]error, 2)
	for index := range 2 {
		done.Add(1)
		go func() {
			defer done.Done()
			environment := maps2Clone(environment)
			start.Wait()
			runtimes[index], errs[index] = installManagedNode(context.Background(), root, environment, metadata)
		}()
	}
	start.Done()
	done.Wait()
	for index, err := range errs {
		if err != nil {
			t.Fatal(err)
		}
		defer runtimes[index].Release()
	}

	if runtimes[0].Path() != runtimes[1].Path() {
		t.Fatalf("%q %q", runtimes[0].Path(), runtimes[1].Path())
	}
	if !harnessTestNoStaging(t, root) {
		t.Fatal("concurrent installers must not leave abandoned staging")
	}
}

// Hard-won: #1605: npm success could publish a receipt for a CLI missing its native package.
func TestNpmInstallRetriesAfterThePinnedCLICannotRun(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	for _, harness := range []core.HarnessKind{core.HarnessKindCodex, core.HarnessKindClaude} {
		temp := t.TempDir()
		cache := filepath.Join(temp, "cache")
		installs := filepath.Join(temp, "installs")
		probes := filepath.Join(temp, "probes")
		environment := harnessTestFakeNodeTools(t, temp, installs)
		environment["MJ_TEST_CLI_MISSING_NATIVE"] = "1"
		environment["MJ_TEST_CLI_PROBES"] = probes
		install := core.ManagedHarnessInstallDir(cache, harness)
		resolve := func() (*ManagedHarness, error) {
			return resolveAt(cache, harness, core.ExecutionPolicyConfiguredApprovals, environment)
		}
		counts := func(want string) {
			t.Helper()
			for _, path := range []string{installs, probes} {
				if body, err := os.ReadFile(path); err != nil || string(body) != want {
					t.Fatalf("%s: %s: %q %v, want %q", harness.DisplayName(), path, body, err, want)
				}
			}
		}

		_, err := resolve()
		if err == nil {
			t.Fatalf("%s: a CLI that cannot run must fail the installation", harness.DisplayName())
		}
		message := err.Error()
		for _, detail := range []string{
			harness.DisplayName(),
			core.Pin(harness).CLIEntrypoint,
			"npm package installation",
			"missing native platform package",
		} {
			if !strings.Contains(message, detail) {
				t.Fatalf("%q missing from %s", detail, message)
			}
		}
		if _, err := os.Lstat(install); err == nil {
			t.Fatalf("%s: a failed installation was published", harness.DisplayName())
		}
		counts("x")

		delete(environment, "MJ_TEST_CLI_MISSING_NATIVE")
		managed, err := resolve()
		if err != nil {
			t.Fatal(err)
		}
		if info, err := os.Stat(filepath.Join(install, core.ManifestFile)); err != nil || !info.Mode().IsRegular() {
			t.Fatalf("%s: no receipt: %v", harness.DisplayName(), err)
		}
		if want := filepath.Join(install, core.Pin(harness).CLIEntrypoint); managed.CLIEntrypoint != want {
			t.Fatalf("%s: CLI %q, want %q", harness.DisplayName(), managed.CLIEntrypoint, want)
		}
		counts("xx")

		environment["MJ_TEST_CLI_MISSING_NATIVE"] = "1"
		reused, err := resolve()
		if err != nil {
			t.Fatal(err)
		}
		if reused.Command != managed.Command {
			t.Fatalf("%s: reused %q, want %q", harness.DisplayName(), reused.Command, managed.Command)
		}
		counts("xx")
		reused.Release()
		managed.Release()
	}
}

func TestConcurrentFirstUsePublishesOneCompleteInstall(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	cache := filepath.Join(temp, "cache")
	installCounter := filepath.Join(temp, "installs")
	environment := harnessTestFakeNodeTools(t, temp, installCounter)
	var start, done sync.WaitGroup
	start.Add(1)
	resolved := make([]*ManagedHarness, 2)
	errs := make([]error, 2)
	for index := range 2 {
		done.Add(1)
		go func() {
			defer done.Done()
			start.Wait()
			resolved[index], errs[index] = resolveAt(cache, core.HarnessKindCodex, core.ExecutionPolicyConfiguredApprovals, environment)
		}()
	}
	start.Done()
	done.Wait()
	for index, err := range errs {
		if err != nil {
			t.Fatal(err)
		}
		defer resolved[index].Release()
	}

	if data, err := os.ReadFile(installCounter); err != nil || string(data) != "x" {
		t.Fatalf("installs %q %v", data, err)
	}
	if resolved[0].Command != resolved[1].Command || !filepath.IsAbs(resolved[0].Command) {
		t.Fatalf("%q %q", resolved[0].Command, resolved[1].Command)
	}
	if _, err := os.Stat(resolved[0].Command); err != nil {
		t.Fatal(err)
	}
}

func TestFirstUseRemovesStagingAbandonedByAKilledInstaller(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	cache := filepath.Join(temp, "cache")
	harnessRoot := filepath.Join(cache, core.HarnessKindCodex.ID())
	abandoned := filepath.Join(harnessRoot, ".install-killed")
	if err := os.MkdirAll(abandoned, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(abandoned, "partial-download"), []byte("incomplete"), 0o644); err != nil {
		t.Fatal(err)
	}
	environment := harnessTestFakeNodeTools(t, temp, filepath.Join(temp, "installs"))

	managed, err := resolveAt(cache, core.HarnessKindCodex, core.ExecutionPolicyConfiguredApprovals, environment)
	if err != nil {
		t.Fatal(err)
	}
	defer managed.Release()

	if _, err := os.Stat(abandoned); err == nil {
		t.Fatal("abandoned staging kept")
	}
	if _, err := os.Stat(managed.Command); err != nil {
		t.Fatal(err)
	}
}

// Cancelling a background preparation (the daemon's quota prewarm, #1595)
// must stop npm's whole process group and publish nothing, and a later
// installation must complete from the same cache.
func TestCancellingAnInstallStopsNpmAndLeavesACacheTheNextInstallCompletes(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	cache := filepath.Join(temp, "cache")
	installCounter := filepath.Join(temp, "installs")
	environment := harnessTestFakeNodeTools(t, temp, installCounter)
	pidFile := filepath.Join(temp, "npm-child.pid")
	harnessTestExecutable(t, filepath.Join(temp, "bin/npm"), fmt.Sprintf(
		"#!/bin/sh\nif [ \"$1\" = --version ]; then echo 12.0.0; exit 0; fi\nsleep 300 &\necho $! > '%s'\nwait\n", pidFile))

	token, cancel := context.WithCancel(context.Background())
	ctx := core.WithPreparationCancellation(context.Background(), token)
	result := make(chan error, 1)
	go func() {
		managed, err := resolveAtAsync(ctx, cache, core.HarnessKindCodex, core.ExecutionPolicyConfiguredApprovals, environment)
		if managed != nil {
			managed.Release()
		}
		result <- err
	}()
	// The shell creates the file before it writes the PID, so wait for a
	// complete one, and fail if it never appears.
	readPID := func() (int, bool) {
		data, err := os.ReadFile(pidFile)
		if err != nil {
			return 0, false
		}
		pid, err := strconv.Atoi(strings.TrimSpace(string(data)))
		return pid, err == nil
	}
	for waited := 0; ; waited++ {
		if _, ok := readPID(); ok {
			break
		}
		if waited >= 500 {
			t.Fatal("npm never recorded its child's PID")
		}
		select {
		case err := <-result:
			t.Fatalf("install ended before npm started: %v", err)
		case <-time.After(20 * time.Millisecond):
		}
	}
	cancel()
	err := <-result
	if err == nil || !strings.Contains(err.Error(), "cancelled") {
		t.Fatal(err)
	}

	pid, _ := readPID()
	child := strconv.Itoa(pid)
	// `kill -0` also succeeds on a zombie, which has already stopped
	// running. Ask `ps` for the process state: no output (gone) or `Z` means
	// it is not running, whether or not anything reaped it.
	alive := func() bool {
		output, _ := exec.Command("ps", "-o", "stat=", "-p", child).Output()
		state := strings.TrimSpace(string(output))
		return state != "" && !strings.HasPrefix(state, "Z")
	}
	for range 100 {
		if !alive() {
			break
		}
		time.Sleep(50 * time.Millisecond)
	}
	if alive() {
		t.Fatal("npm's child outlived the cancelled install")
	}
	entries, err := os.ReadDir(filepath.Join(cache, core.HarnessKindCodex.ID()))
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range entries {
		if !strings.HasPrefix(entry.Name(), ".install") {
			t.Fatalf("cancelled install published %q", entry.Name())
		}
	}

	environment = harnessTestFakeNodeTools(t, temp, installCounter)
	managed, err := resolveAt(cache, core.HarnessKindCodex, core.ExecutionPolicyConfiguredApprovals, environment)
	if err != nil {
		t.Fatal(err)
	}
	defer managed.Release()
	if _, err := os.Stat(managed.Command); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: #1434: repairing an incomplete install deleted it in place and
// failed with ENOTEMPTY while an older release's collector recreated files in it.
func TestRepairOfAnIncompleteInstallSurvivesAWriterInTheOldDirectory(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	cache := filepath.Join(temp, "cache")
	install := core.ManagedHarnessInstallDir(cache, core.HarnessKindCodex)
	environment := harnessTestFakeNodeTools(t, temp, filepath.Join(temp, "installs"))
	// A tree with many entries and no receipt: what a collector that removed
	// half of a fresh installation leaves behind.
	for index := range 200 {
		directory := filepath.Join(install, fmt.Sprintf("node_modules/package-%d", index))
		if err := os.MkdirAll(directory, 0o777); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(directory, "index.js"), []byte("partial"), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	var stop atomic.Bool
	var writer sync.WaitGroup
	writer.Add(1)
	// The older collector opens `.lease` with create on every pass.
	go func() {
		defer writer.Done()
		for !stop.Load() {
			if lease, err := openLock(filepath.Join(install, core.LeaseFile)); err == nil {
				lease.Close()
			}
			os.WriteFile(filepath.Join(install, "late-file"), []byte("late"), 0o644)
		}
	}()
	managed, err := resolveAt(cache, core.HarnessKindCodex, core.ExecutionPolicyConfiguredApprovals, environment)
	stop.Store(true)
	writer.Wait()
	if err != nil {
		t.Fatal(err)
	}
	defer managed.Release()

	if _, err := os.Stat(managed.Command); err != nil {
		t.Fatal(err)
	}
	if complete, err := completeInstall(install, core.HarnessKindCodex, core.Pin(core.HarnessKindCodex)); err != nil || !complete {
		t.Fatalf("complete %v %v", complete, err)
	}
	if _, err := os.Stat(filepath.Join(install, "node_modules/package-0")); err == nil {
		t.Fatal("partial tree kept")
	}
}

// Hard-won: #1434: a directory the repair could not delete failed the launch.
func TestRepairProceedsWhenTheSetAsideTreeCannotBeDeleted(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	cache := filepath.Join(temp, "cache")
	install := core.ManagedHarnessInstallDir(cache, core.HarnessKindCodex)
	locked := filepath.Join(install, "locked")
	if err := os.MkdirAll(locked, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(locked, "file"), []byte("partial"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(locked, 0o500); err != nil {
		t.Fatal(err)
	}
	// Root ignores directory permissions, so deletion would succeed.
	canDeleteAnyway := os.Remove(filepath.Join(locked, "file")) == nil
	if !canDeleteAnyway {
		environment := harnessTestFakeNodeTools(t, temp, filepath.Join(temp, "installs"))
		managed, err := resolveAt(cache, core.HarnessKindCodex, core.ExecutionPolicyConfiguredApprovals, environment)
		if err != nil {
			t.Fatal(err)
		}
		defer managed.Release()
		if _, err := os.Stat(managed.Command); err != nil {
			t.Fatal(err)
		}
		if _, err := os.Stat(filepath.Join(install, "locked")); err == nil {
			t.Fatal("locked tree still at the install")
		}
	}
	// Allow the temporary directory to be removed.
	entries, _ := os.ReadDir(filepath.Join(cache, core.HarnessKindCodex.ID()))
	for _, entry := range entries {
		leftover := filepath.Join(cache, core.HarnessKindCodex.ID(), entry.Name(), "locked")
		if _, err := os.Stat(leftover); err == nil {
			if err := os.Chmod(leftover, 0o700); err != nil {
				t.Fatal(err)
			}
		}
	}
}

func harnessTestLockFileBlocking(t *testing.T, path string) *core.FileLock {
	t.Helper()
	file, err := openLock(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := harnessLock(file, false); err != nil {
		t.Fatal(err)
	}
	return core.NewFileLock(file)
}

// Hard-won: #1434: the collector removed a newer installation while its installer held the lock.
func TestCollectorLeavesInstallationsAloneWhileAnInstallerHoldsTheLock(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	root := filepath.Join(t.TempDir(), "codex")
	if err := os.MkdirAll(root, 0o777); err != nil {
		t.Fatal(err)
	}
	harnessTestCompleteFake(t, root, core.HarnessKindCodex, "newer", "bin/bridge")
	installer := harnessTestLockFileBlocking(t, filepath.Join(root, installLockFile))

	if err := collectObsoleteInstalls(root, "older"); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(root, "newer")); err != nil {
		t.Fatal("collected under the installer's lock")
	}

	installer.Drop()
	if err := collectObsoleteInstalls(root, "older"); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(root, "newer")); err == nil {
		t.Fatal("obsolete install kept")
	}
}

func TestObsoleteInstallWaitsForTheLastSharedLease(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	root := filepath.Join(t.TempDir(), "codex")
	if err := os.MkdirAll(root, 0o777); err != nil {
		t.Fatal(err)
	}
	harnessTestCompleteFake(t, root, core.HarnessKindCodex, "old", "bin/bridge")
	harnessTestCompleteFake(t, root, core.HarnessKindCodex, "current", "bin/bridge")

	oldLease, err := AcquireSupervisorLease(filepath.Join(root, "old", core.LeaseFile))
	if err != nil {
		t.Fatal(err)
	}
	if err := gcHarnessRoot(root, "current"); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(root, "old")); err != nil {
		t.Fatal("leased install collected")
	}

	oldLease.Drop()
	if err := gcHarnessRoot(root, "current"); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(root, "old")); err == nil {
		t.Fatal("released install kept")
	}
	if _, err := os.Stat(filepath.Join(root, "current")); err != nil {
		t.Fatal("current install collected")
	}
}

// Hard-won: dfc33b0e9: an unrelated fork extended a released installation lease.
//
// Rust holds a forked child between fork and exec with pre_exec; Go has no
// such hook, so the unrelated child here inherits the lease's open file
// description explicitly and keeps running while the lease is released.
func TestReleasedManagedInstallIsCollectedWhileAnUnrelatedChildWaitsToExec(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	cache := filepath.Join(temp, "cache")
	selected := core.Pin(core.HarnessKindGrok)
	root := filepath.Join(cache, core.HarnessKindGrok.ID())
	if err := os.MkdirAll(root, 0o777); err != nil {
		t.Fatal(err)
	}
	harnessTestCompleteFake(t, root, core.HarnessKindGrok, selected.InstallID, selected.Entrypoint)
	harnessTestExecutable(t, filepath.Join(root, selected.InstallID, "bin/agent"), "#!/bin/sh\nexit 0\n")
	managed, err := resolveAt(cache, core.HarnessKindGrok, core.ExecutionPolicyConfiguredApprovals, map[string]string{})
	if err != nil {
		t.Fatal(err)
	}
	install := filepath.Dir(managed.LeasePath)
	unrelated := exec.Command("/bin/sh", "-c", "read line")
	unrelated.ExtraFiles = []*os.File{managed.lease.File()}
	stdin, err := unrelated.StdinPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err := unrelated.Start(); err != nil {
		t.Fatal(err)
	}
	managed.Release()
	collected := gcHarnessRoot(root, "a-different-install")
	_, statErr := os.Stat(install)
	// Reap the child before asserting, including when collection fails.
	stdin.Write([]byte("\n"))
	stdin.Close()
	if err := unrelated.Wait(); err != nil {
		t.Fatal(err)
	}
	if collected != nil {
		t.Fatal(collected)
	}
	if statErr == nil {
		t.Fatal("released install was still considered in use")
	}
}

func TestSupervisorLeaseClosesTheWorkerShutdownGap(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	temp := t.TempDir()
	cache := filepath.Join(temp, "cache")
	selected := core.Pin(core.HarnessKindCodex)
	root := filepath.Join(cache, core.HarnessKindCodex.ID())
	if err := os.MkdirAll(root, 0o777); err != nil {
		t.Fatal(err)
	}
	harnessTestCompleteFake(t, root, core.HarnessKindCodex, selected.InstallID, selected.Entrypoint)
	environment := harnessTestFakeNodeTools(t, temp, filepath.Join(temp, "unused"))
	managed, err := resolveAt(cache, core.HarnessKindCodex, core.ExecutionPolicyConfiguredApprovals, environment)
	if err != nil {
		t.Fatal(err)
	}
	supervisor, err := AcquireSupervisorLease(managed.LeasePath)
	if err != nil {
		t.Fatal(err)
	}
	leasePath := managed.LeasePath
	managed.Release()

	exclusive, err := openLock(leasePath)
	if err != nil {
		t.Fatal(err)
	}
	defer exclusive.Close()
	if locked, err := harnessTryLock(exclusive, false); err != nil || locked {
		t.Fatalf("exclusive lease taken under the supervisor: %v %v", locked, err)
	}
	supervisor.Drop()
	locked, err := harnessTryLock(exclusive, false)
	if err != nil || !locked {
		t.Fatalf("supervisor lease released immediately: %v %v", locked, err)
	}
	core.NewFileLock(exclusive).Drop()
}

func TestACompleteCacheHitDoesNotExecuteTheEntrypoint(t *testing.T) {
	skipHarnessInstallOnWindows(t)
	for _, harness := range []core.HarnessKind{core.HarnessKindCodex, core.HarnessKindClaude} {
		harnessTestCheckCompleteCacheHit(t, harness)
	}
}

func harnessTestCheckCompleteCacheHit(t *testing.T, harness core.HarnessKind) {
	t.Helper()
	temp := t.TempDir()
	cache := filepath.Join(temp, "cache")
	selected := core.Pin(harness)
	root := filepath.Join(cache, harness.ID())
	if err := os.MkdirAll(root, 0o777); err != nil {
		t.Fatal(err)
	}
	harnessTestCompleteFake(t, root, harness, selected.InstallID, selected.Entrypoint)
	install := filepath.Join(root, selected.InstallID)
	marker := filepath.Join(temp, "executed")
	for _, entry := range []string{selected.Entrypoint, selected.CLIEntrypoint} {
		harnessTestExecutable(t, filepath.Join(install, entry), fmt.Sprintf("#!/bin/sh\ntouch '%s'\n", marker))
	}
	environment := harnessTestFakeNodeTools(t, temp, filepath.Join(temp, "unused"))

	managed, err := resolveAt(cache, harness, core.ExecutionPolicyConfiguredApprovals, environment)
	if err != nil {
		t.Fatal(err)
	}
	defer managed.Release()

	if _, err := os.Stat(marker); err == nil {
		t.Fatal("the entrypoint ran")
	}
	var providerEnv string
	switch harness {
	case core.HarnessKindCodex:
		providerEnv = "CODEX_PATH"
	case core.HarnessKindClaude:
		providerEnv = "CLAUDE_CODE_EXECUTABLE"
	default:
		panic("unreachable")
	}
	cli := filepath.Join(install, selected.CLIEntrypoint)
	if got, ok := managed.Environment[providerEnv]; !ok || got != cli {
		t.Fatalf("%s = %q", providerEnv, got)
	}
	if err := os.Remove(cli); err != nil {
		t.Fatal(err)
	}
	if complete, err := completeInstall(install, harness, selected); err != nil || complete {
		t.Fatalf("a complete cache must contain the pinned CLI: %v", err)
	}
}

// skipUnlessIgnoredTestsRequested is Rust's #[ignore]: the test runs only
// when MJ_RUN_IGNORED_TESTS=1 asks for it, as `cargo test -- --ignored`
// does.
func skipUnlessIgnoredTestsRequested(t *testing.T, reason string) {
	t.Helper()
	if os.Getenv("MJ_RUN_IGNORED_TESTS") != "1" {
		t.Skip(reason)
	}
}

// harnessTestValidationDirectory is a disposable directory under the Go
// module's own build output, like Rust's target/<name>.
func harnessTestValidationDirectory(t *testing.T, name string) string {
	t.Helper()
	parent, err := filepath.Abs(filepath.Join("..", "..", "bin", name))
	if err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(parent, 0o777); err != nil {
		t.Fatal(err)
	}
	temp, err := os.MkdirTemp(parent, "")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(temp) })
	return temp
}

func harnessTestRunChecked(t *testing.T, command *exec.Cmd, operation string) {
	t.Helper()
	output, err := core.RunWithInput(command, nil)
	if err != nil {
		t.Fatalf("%s: %v", operation, err)
	}
	if !output.Status.Success() {
		t.Fatalf("%s failed: %s", operation, outputSummary(&output))
	}
}

func TestGrokRealInstallLaunchesFromFinalDirectory(t *testing.T) {
	skipUnlessIgnoredTestsRequested(t, "downloads the pinned Grok artifact into a disposable cache")
	temp := harnessTestValidationDirectory(t, "grok-install-validation")
	environment, err := Discover()
	if err != nil {
		t.Fatal(err)
	}
	managed, err := resolveAt(temp, core.HarnessKindGrok, core.ExecutionPolicyConfiguredApprovals, environment)
	if err != nil {
		t.Fatal(err)
	}
	defer managed.Release()
	for _, path := range []string{managed.Command, filepath.Join(filepath.Dir(managed.Command), "agent")} {
		command := exec.Command(path, "--version")
		command.Env = harnessEnvironmentList(environment)
		harnessTestRunChecked(t, command, "verify relocated Grok launcher")
	}
}

func TestKimiRealInstallLaunchesFromFinalDirectory(t *testing.T) {
	skipUnlessIgnoredTestsRequested(t, "downloads the pinned Kimi artifact into a disposable cache")
	temp := harnessTestValidationDirectory(t, "kimi-install-validation")
	environment, err := Discover()
	if err != nil {
		t.Fatal(err)
	}
	managed, err := resolveAt(temp, core.HarnessKindKimi, core.ExecutionPolicyConfiguredApprovals, environment)
	if err != nil {
		t.Fatal(err)
	}
	defer managed.Release()
	command := exec.Command(managed.Command, "--version")
	command.Env = harnessEnvironmentList(environment)
	harnessTestRunChecked(t, command, "verify relocated Kimi executable")
}

func TestAntigravityRealInstallIsConcurrentReusableAndKeepsItsLease(t *testing.T) {
	skipUnlessIgnoredTestsRequested(t, "downloads the pinned Antigravity artifact into an isolated cache")
	temp := t.TempDir()
	environment := map[string]string{
		"HOME":                temp,
		"XDG_CACHE_HOME":      filepath.Join(temp, "cache"),
		"MJ_ANTIGRAVITY_HOME": filepath.Join(temp, "login with spaces"),
	}
	policy := core.ExecutionPolicyConfiguredApprovals
	command, args := core.FallbackLaunch(core.HarnessKindAntigravity, policy)
	resolve := func() (Located, error) {
		return Locate(context.Background(), core.HarnessRuntimePolicyManaged, core.HarnessKindAntigravity, policy, command, args, environment)
	}
	var first, second Located
	var firstErr, secondErr error
	var wait sync.WaitGroup
	wait.Add(2)
	go func() { defer wait.Done(); first, firstErr = resolve() }()
	go func() { defer wait.Done(); second, secondErr = resolve() }()
	wait.Wait()
	if firstErr != nil || secondErr != nil {
		t.Fatal(errors.Join(firstErr, secondErr))
	}
	if first.Command != second.Command {
		t.Fatalf("%q %q", first.Command, second.Command)
	}
	probe := exec.Command(first.Command, "--version")
	probe.Env = append(os.Environ(), harnessEnvironmentList(first.Environment)...)
	output, err := core.Capture(context.Background(), probe)
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() || strings.TrimSpace(string(output.Stdout)) != core.AntigravityVersion {
		t.Fatalf("%v %q", output.Status, output.Stdout)
	}
	root := filepath.Join(first.Managed.CacheRoot, "antigravity")
	if err := gcHarnessRoot(root, "a-newer-pin"); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(first.Command); err != nil {
		t.Fatal("leased install collected")
	}
	third, err := resolve()
	if err != nil || third.Command != first.Command {
		t.Fatalf("%q %v", third.Command, err)
	}
	executable := first.Command
	first.Managed.Release()
	second.Managed.Release()
	third.Managed.Release()
	if err := gcHarnessRoot(root, "a-newer-pin"); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(executable); err == nil {
		t.Fatal("released install kept")
	}
}

func TestMuseRealInstallIsVerifiedConcurrentAndReusable(t *testing.T) {
	skipUnlessIgnoredTestsRequested(t, "installs the pinned Muse runtime from its publisher")
	temp := harnessTestValidationDirectory(t, "muse-integration-inspect")
	// The adapter runs through npx, so resolution also locates the managed
	// Node cache, which needs a cache base of its own.
	environment := map[string]string{"HOME": temp, "XDG_CACHE_HOME": filepath.Join(temp, "cache")}
	resolved := make([]*ManagedHarness, 2)
	errs := make([]error, 2)
	var wait sync.WaitGroup
	for index := range 2 {
		wait.Add(1)
		go func() {
			defer wait.Done()
			resolved[index], errs[index] = resolveAt(temp, core.HarnessKindMuse, core.ExecutionPolicyConfiguredApprovals, environment)
		}()
	}
	wait.Wait()
	for index, err := range errs {
		if err != nil {
			t.Fatal(err)
		}
		defer resolved[index].Release()
	}
	first := resolved[0]
	if first.Command != resolved[1].Command || first.Command != "sh" {
		t.Fatalf("%q %q", first.Command, resolved[1].Command)
	}
	bridge, _ := core.NpmBridgeFor(core.HarnessKindMuse)
	if !slices.Equal(first.Args, []string{"-c", bridge.BootstrapScript()}) {
		t.Fatalf("args %q", first.Args)
	}
	harnessTestRunChecked(t, exec.Command(first.Environment["MUSE_CLI"], "--version"), "verify installed Muse executable")
	if !EntrypointIsExecutable(first.Environment["MUSE_CLI"]) {
		t.Fatal("Muse not executable")
	}
}
