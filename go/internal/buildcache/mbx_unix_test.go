//go:build unix

// Port of the Unix-only tests in mj-build-cache/src/mbx.rs.

package buildcache

import (
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Runs on macOS too, where the probe must work without `readlink -f`.
func TestNativeProbePrefersPathThenUserBinsAndCanonicalizesSymlinks(t *testing.T) {
	root := t.TempDir()
	pathBin := filepath.Join(root, "path-bin")
	home := filepath.Join(root, "home")
	localBin := filepath.Join(home, ".local/bin")
	cargoBin := filepath.Join(home, ".cargo/bin")
	realBin := filepath.Join(root, "real")
	mkdirAll(t, pathBin, localBin, cargoBin, realBin)
	install := func(path, version string) { writeExecutable(t, path, fakeMbxScript(version)) }
	realMbx := filepath.Join(realBin, "mbx-real")
	install(realMbx, "1.30.0")
	// Relative links, one of them reached through a symlinked directory.
	symlink(t, realBin, filepath.Join(root, "alias"))
	symlink(t, "mbx-real", filepath.Join(realBin, "mbx-hop"))
	symlink(t, "../alias/mbx-hop", filepath.Join(pathBin, "mbx"))
	install(filepath.Join(localBin, "mbx"), "1.31.0")
	install(filepath.Join(cargoBin, "mbx"), "1.32.0")
	readlink := lookPath(t, "readlink")
	// Hard-won: #1330: the probe needed `readlink -f`, which macOS before 12.3 lacks.
	writeExecutable(t, filepath.Join(pathBin, "readlink"),
		"#!/bin/sh\ncase $1 in -*) echo \"readlink: illegal option $1\" >&2; exit 1 ;; esac\nexec '"+readlink+"' \"$@\"\n")

	command := core.NewCommandSpec("/bin/sh", []string{"-c", NativeVersionScript}).
		WithPurpose("test native mbx path resolution")
	command.ClearEnv = true
	command.Env = map[string]string{"PATH": pathBin, "HOME": home}
	executor := core.ProcessExecutor{}

	probe := func() NativeMbx {
		t.Helper()
		result, err := executor.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		if result.Status != 0 {
			t.Fatalf("status %d: %s", result.Status, result.Stderr)
		}
		native, err := parseNativeProbeOutput(result.Stdout)
		if err != nil {
			t.Fatal(err)
		}
		return native
	}
	if got, want := probe(), (NativeMbx{Program: canonical(t, realMbx), Version: "1.30.0"}); got != want {
		t.Fatalf("got %#v, want %#v", got, want)
	}

	if err := os.Remove(filepath.Join(pathBin, "mbx")); err != nil {
		t.Fatal(err)
	}
	if got := probe().Program; got != canonical(t, filepath.Join(localBin, "mbx")) {
		t.Fatal(got)
	}

	if err := os.Remove(filepath.Join(localBin, "mbx")); err != nil {
		t.Fatal(err)
	}
	if got := probe().Program; got != canonical(t, filepath.Join(cargoBin, "mbx")) {
		t.Fatal(got)
	}
}

func TestCacheCopyRefreshIsAtomicAndSkipsUnchangedBinaries(t *testing.T) {
	requireLinux(t)
	root := t.TempDir()
	source := filepath.Join(root, "native mbx")
	nativeBin := filepath.Join(root, "native-bin")
	home := filepath.Join(root, "home")
	cache := filepath.Join(root, "cache with spaces")
	mkdirAll(t, nativeBin, home)
	install := func(version string) { writeExecutable(t, source, fakeMbxScript(version)) }
	native := func(version string) *NativeMbx { return &NativeMbx{Program: source, Version: version} }
	symlink(t, source, filepath.Join(nativeBin, "mbx"))
	executor := processWithEnv(map[string]string{
		"PATH": nativeBin + ":/usr/bin:/bin",
		"HOME": home,
	})

	install("1.22.0")
	first, err := SyncMbxBinaryFromNative(CacheHost{}, native("1.22.0"), cache, executor)
	if err != nil {
		t.Fatal(err)
	}
	copyPath := first.Path
	if copyPath != pathJoin(cache, ".mjolnir/bin/mbx") {
		t.Fatal(copyPath)
	}
	if readText(t, copyPath) != readText(t, source) {
		t.Fatal("the copy differs from its source")
	}
	info, err := os.Stat(copyPath)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm() != 0o755 {
		t.Fatalf("mode = %o", info.Mode().Perm())
	}
	originalInode := inode(t, copyPath)

	unchanged, err := SyncMbxBinaryFromNative(CacheHost{}, native("1.22.0"), cache, executor)
	if err != nil {
		t.Fatal(err)
	}
	if unchanged.Path != copyPath {
		t.Fatal(unchanged.Path)
	}
	if inode(t, copyPath) != originalInode {
		t.Fatal("an unchanged binary was copied again")
	}

	install("1.23.0")
	if _, err := SyncMbxBinaryFromNative(CacheHost{}, native("1.23.0"), cache, executor); err != nil {
		t.Fatal(err)
	}
	if inode(t, copyPath) == originalInode {
		t.Fatal("a changed binary was not republished")
	}
	versionCommand := core.NewCommandSpec(copyPath, []string{"--version"})
	version, err := executor.Execute(&versionCommand)
	if err != nil {
		t.Fatal(err)
	}
	if lossyTrim(version.Stdout) != "mbx 1.23.0" {
		t.Fatal(lossyTrim(version.Stdout))
	}
	entries, err := os.ReadDir(filepath.Join(cache, ".mjolnir/bin"))
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 2 {
		t.Fatalf("publication leaves only the copy and its cross-process lock file: %v", entries)
	}
}

func TestCacheSyncWithoutFlockRechecksAfterRenameAndResynchronizes(t *testing.T) {
	requireLinux(t)
	root := t.TempDir()
	tools := filepath.Join(root, "tools")
	home := filepath.Join(root, "home")
	cache := filepath.Join(root, "cache")
	oldSource := filepath.Join(root, "mbx-1.22.0")
	newSource := filepath.Join(root, "mbx-1.23.0")
	nativePath := filepath.Join(tools, "mbx")
	flipped := filepath.Join(root, "flipped")
	mkdirAll(t, tools, home)

	for _, name := range []string{"sh", "dirname", "mkdir", "readlink", "mktemp", "chmod", "wc", "mv", "rm", "ln"} {
		symlink(t, lookPath(t, name), filepath.Join(tools, name))
	}

	realCP := lookPath(t, "cp")
	writeExecutable(t, filepath.Join(tools, "cp"), `#!/bin/sh
"$MBX_TEST_REAL_CP" "$@" || exit $?
if [ ! -e "$MBX_TEST_FLIPPED" ]; then
    : > "$MBX_TEST_FLIPPED"
    rm -f -- "$MBX_TEST_NATIVE"
    ln -s -- "$MBX_TEST_NEW_SOURCE" "$MBX_TEST_NATIVE"
fi`)
	writeExecutable(t, oldSource, fakeMbxScript("1.22.0"))
	writeExecutable(t, newSource, fakeMbxScript("1.23.0"))
	symlink(t, oldSource, nativePath)

	executor := processWithEnv(map[string]string{
		"PATH":                tools,
		"HOME":                home,
		"MBX_TEST_REAL_CP":    realCP,
		"MBX_TEST_FLIPPED":    flipped,
		"MBX_TEST_NATIVE":     nativePath,
		"MBX_TEST_NEW_SOURCE": newSource,
	})

	binary, err := SyncMbxBinaryFromNative(CacheHost{}, &NativeMbx{Program: oldSource, Version: "1.22.0"}, cache, executor)
	if err != nil {
		t.Fatal(err)
	}

	if !exists(flipped) {
		t.Fatal("the test copy did not switch host versions")
	}
	if binary.Version != "1.23.0" {
		t.Fatal(binary.Version)
	}
	copied := mustRun(t, core.NewCommandSpec(binary.Path, []string{"--version"}))
	if lossyTrim(copied.Stdout) != "mbx 1.23.0" {
		t.Fatal(lossyTrim(copied.Stdout))
	}
}

func TestCrossProcessCacheSyncCannotPublishAStaleHostBinaryLast(t *testing.T) {
	requireLinux(t)
	root := t.TempDir()
	sourceDir := filepath.Join(root, "sources")
	nativeBin := filepath.Join(root, "native-bin")
	wrapperBin := filepath.Join(root, "wrapper-bin")
	home := filepath.Join(root, "home")
	cache := filepath.Join(root, "cache")
	lock := filepath.Join(cache, ".mjolnir/bin/.mbx.lock")
	ready := filepath.Join(root, "lock-ready")
	release := filepath.Join(root, "release-lock")
	mkdirAll(t, sourceDir, nativeBin, wrapperBin, home, filepath.Dir(lock))

	oldSource := filepath.Join(sourceDir, "mbx-1.22.0")
	newSource := filepath.Join(sourceDir, "mbx-1.23.0")
	writeExecutable(t, oldSource, fakeMbxScript("1.22.0"))
	writeExecutable(t, newSource, fakeMbxScript("1.23.0"))
	nativePath := filepath.Join(nativeBin, "mbx")
	symlink(t, oldSource, nativePath)

	// Mark the old process as soon as it reaches flock, then delegate to the
	// real utility. This proves it is waiting on the held lock before the
	// native install path changes.
	writeExecutable(t, filepath.Join(wrapperBin, "flock"),
		"#!/bin/sh\n[ -z \"${MBX_TEST_FLOCK_MARKER:-}\" ] || : > \"$MBX_TEST_FLOCK_MARKER\"\nexec /usr/bin/flock \"$@\"\n")

	pathValue := wrapperBin + ":" + nativeBin + ":/usr/bin:/bin"
	holderScript := `set -eu
exec 9>"$1"
flock -x 9
: > "$2"
while [ ! -e "$3" ]; do sleep 0.01; done`
	holder := core.NewCommandSpec("sh", []string{"-c", holderScript, "mbx-lock-holder", lock, ready, release}).
		WithPurpose("hold test mbx lock")
	holder.Env = map[string]string{"PATH": pathValue, "HOME": home}
	type result struct {
		output core.CommandOutput
		err    error
	}
	run := func(command core.CommandSpec, done func()) <-chan result {
		finished := make(chan result, 1)
		go func() {
			output, err := core.ProcessExecutor{}.Execute(&command)
			if done != nil {
				done()
			}
			finished <- result{output, err}
		}()
		return finished
	}
	holderDone := run(holder, nil)

	waitFor := func(path string) bool {
		deadline := time.Now().Add(3 * time.Second)
		for !exists(path) && time.Now().Before(deadline) {
			time.Sleep(10 * time.Millisecond)
		}
		return exists(path)
	}
	if !waitFor(ready) {
		os.WriteFile(release, []byte("release"), 0o666)
		<-holderDone
		t.Fatal("the test lock holder did not acquire flock")
	}

	makeSync := func(source, version string) core.CommandSpec {
		command := core.NewCommandSpec("sh", []string{
			"-c", syncMbxBinaryScript, "test-mbx-sync", source, CacheBinaryPath(cache), version, NativeVersionScript,
		}).WithPurpose("run cross-process mbx sync test")
		command.ClearEnv = true
		command.Env = map[string]string{"PATH": pathValue, "HOME": home}
		return command
	}

	oldMarker := filepath.Join(root, "old-reached-flock")
	oldCommand := makeSync(oldSource, "1.22.0")
	oldCommand.Env["MBX_TEST_FLOCK_MARKER"] = oldMarker
	var oldFinished atomic.Bool
	oldDone := run(oldCommand, func() { oldFinished.Store(true) })
	oldWaited := waitFor(oldMarker) && !oldFinished.Load()

	if err := os.Remove(nativePath); err != nil {
		t.Fatal(err)
	}
	symlink(t, newSource, nativePath)
	newDone := run(makeSync(newSource, "1.23.0"), nil)
	time.Sleep(40 * time.Millisecond)
	if err := os.WriteFile(release, []byte("release"), 0o666); err != nil {
		t.Fatal(err)
	}

	holderResult, oldResult, newResult := <-holderDone, <-oldDone, <-newDone
	for _, finished := range []result{holderResult, oldResult, newResult} {
		if finished.err != nil {
			t.Fatal(finished.err)
		}
	}
	if holderResult.output.Status != 0 {
		t.Fatalf("%#v", holderResult.output)
	}
	if !oldWaited {
		t.Fatal("the stale synchronizer did not wait on flock")
	}
	if oldResult.output.Status != 75 {
		t.Fatalf("status %d: %s", oldResult.output.Status, oldResult.output.Stderr)
	}
	if newResult.output.Status != 0 {
		t.Fatalf("status %d: %s", newResult.output.Status, newResult.output.Stderr)
	}

	version := mustRun(t, core.NewCommandSpec(CacheBinaryPath(cache), []string{"--version"}))
	if got := strings.TrimSpace(lossy(version.Stdout)); got != "mbx 1.23.0" {
		t.Fatal(got)
	}
}
