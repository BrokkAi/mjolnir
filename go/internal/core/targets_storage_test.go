// Port of the tests in mj-core/src/targets/storage.rs.

package core

import (
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"testing"
	"time"
)

// Hard-won: 540c9202: A full target filesystem stopped all workers and recovery repeatedly wrote to the same disk.
// Hard-won: #1620: an errno name inside a path, such as /work/enospc-repro, read as a full disk.
func TestRecognisesAFullDiskInEveryToolSWords(t *testing.T) {
	for _, text := range []string{
		// scp to a full home directory, as on precision-3260.
		"scp: write remote \".local/share/hel/workers/abc/hel.prepared-upgrade-stage-1.next\": No space left on device",
		// A worker's own exit reason.
		"relay coordinator failed: append journal: No space left on device (os error 28)",
		// podman cp and docker cp.
		"Error: copying to container: write /var/lib/hel/workers/x/hel: no space left on device",
		"Error response from daemon: ENOSPC: no space left on device, write",
		"cp: error writing '/home/u/.cache/mjolnir/uploads/x': Disk quota exceeded",
		"write failed (os error 28)",
		"write: EDQUOT",
	} {
		if !ReportsNoSpace(text) {
			t.Fatal(text)
		}
	}
	for _, text := range []string{
		"Permission denied (publickey)",
		"ssh: connect to host precision-3260 port 22: Connection timed out",
		"No such file or directory",
		"error 280 while writing",
		"cat: /work/enospc-repro/log: Permission denied",
		"open /srv/edquot_tests: No such file or directory",
	} {
		if ReportsNoSpace(text) {
			t.Fatal(text)
		}
	}
}

// Hard-won: 540c9202: A full target filesystem made repeated recovery writes fail without identifying the affected path.
func TestANoSpaceFailureNamesThePathItCouldNotWrite(t *testing.T) {
	cases := []struct {
		text string
		path string
		ok   bool
	}{
		{"scp: write remote \".local/share/hel/workers/abc/hel.prepared-upgrade-stage-1.next\": No space left on device",
			".local/share/hel/workers/abc/hel.prepared-upgrade-stage-1.next", true},
		{"cp: error writing '/home/u/Projects/x/y': No space left on device", "/home/u/Projects/x/y", true},
		{"Error: write /var/lib/hel/workers/x/hel: no space left on device", "/var/lib/hel/workers/x/hel", true},
		{"Error response from daemon: ENOSPC: no space left on device, write", "", false},
	}
	for _, c := range cases {
		path, ok := NoSpacePath(c.text)
		if path != c.path || ok != c.ok {
			t.Fatalf("%q: got %q, %v", c.text, path, ok)
		}
	}
}

// precision-3260: `~/Projects` is its own filesystem beside `/`. Paths on
// one filesystem make one record, and a write is judged by the filesystem it
// lands on.
// Hard-won: 540c9202: Root and Projects were separate filesystems, but workers stopped when only one filled.
func TestStorageLinesKeepOneRecordPerFilesystemAndPlaceEachPath(t *testing.T) {
	output := []byte("home=/home/jonathan\n" +
		"storage=0\t491134172\t467026656\t/\t.local/share/hel/workers\n" +
		"storage=0\t491134172\t467026656\t/\t/tmp\n" +
		"storage=41943040\t976762584\t900000000\t/home/jonathan/Projects\t~/Projects\n" +
		"garbage\n")
	home, filesystems := ParseStorageLines(output)
	if home == nil || *home != "/home/jonathan" {
		t.Fatalf("home %v", home)
	}
	if len(filesystems) != 2 || filesystems[0].Mount != "/" {
		t.Fatalf("%+v", filesystems)
	}
	if !slices.Equal(filesystems[0].Paths, []string{"/home/jonathan/.local/share/hel/workers", "/tmp"}) {
		t.Fatalf("%v", filesystems[0].Paths)
	}
	if filesystems[0].ReservedBytes != (491134172-467026656)*1024 {
		t.Fatalf("reserved %d", filesystems[0].ReservedBytes)
	}
	sampled := uint64(1)
	view := TargetStorageViewEvaluate("precision-3260", home, filesystems, &sampled,
		func(*FilesystemSpace) *string { return nil }, nil)
	worker := ".local/share/hel/workers/abc/hel.next"
	clone := "/home/jonathan/Projects/app/.mj/clones/abc"
	if got := view.FilesystemFor(worker); got == nil || got.Mount != "/" {
		t.Fatalf("%+v", got)
	}
	if got := view.FilesystemFor(clone); got == nil || got.Mount != "/home/jonathan/Projects" {
		t.Fatalf("%+v", got)
	}
	if _, refused := view.RefuseWrite(worker, 100<<20, "stage"); !refused {
		t.Fatal("a write to the full filesystem was not refused")
	}
	if _, refused := view.RefuseWrite(clone, 100<<20, "restore"); refused {
		t.Fatal("a write to the healthy filesystem was refused")
	}
	// A path under nothing measured is not judged.
	if view.FilesystemFor("/srv/elsewhere") != nil {
		t.Fatal("an unmeasured path was placed")
	}
	if _, refused := view.RefuseWrite("/srv/elsewhere", 1, "write"); refused {
		t.Fatal("an unmeasured path was refused")
	}
	problem, ok := view.ProblemFor([]string{clone, worker})
	if !ok || !strings.HasPrefix(problem,
		"disk full: precision-3260 has 0 B free on / (the filesystem reserves 24.69 GB more for root)") {
		t.Fatal(problem)
	}
	if got := view.FilesystemLines(); !slices.Equal(got, []string{
		"/: 0 B free, 24.69 GB reserved for root (full)",
		"/home/jonathan/Projects: 42.95 GB free, 35.66 GB reserved for root",
	}) {
		t.Fatalf("%q", got)
	}
}

func TestAnUnattributedFailureCountsEveryFilesystemFullUntilMeasured(t *testing.T) {
	home, filesystems := ParseStorageLines([]byte("home=/h\nstorage=41943040\t99999999\t1\t/\t/tmp\n"))
	sampled := uint64(1)
	failure := "write failed: No space left on device"
	view := TargetStorageViewEvaluate("host", home, filesystems, &sampled,
		func(*FilesystemSpace) *string { return nil }, &failure)
	if _, ok := view.ProblemFor([]string{"/tmp/x"}); !ok {
		t.Fatal("no problem reported")
	}
}

func TestStorageProbeScriptMeasuresMissingPathsAtAnExistingAncestor(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("POSIX shell")
	}
	directory := t.TempDir()
	missing := filepath.Join(directory, "not/yet/created")
	output, err := exec.Command("sh", "-c", StorageProbeScript, "mj-storage", missing, directory).Output()
	if err != nil {
		t.Fatal(err)
	}
	home, filesystems := ParseStorageLines(output)
	if home == nil {
		t.Fatal("no home")
	}
	if len(filesystems) != 1 {
		t.Fatalf("%q", output)
	}
	if filesystems[0].TotalBytes == 0 || len(filesystems[0].Paths) != 2 {
		t.Fatalf("%+v", filesystems[0])
	}
}

// recordedQuotaOutput is Linux quota-tools 4.11 on a tmpfs mounted with
// `usrquota,grpquota`, for a user over a 2000 KiB soft limit, under a 4000
// KiB hard one.
const recordedQuotaOutput = "Disk quotas for user u1 (uid 1000): \n" +
	"     Filesystem  blocks   quota   limit   grace   files   quota   limit   grace\n" +
	"         MOUNT    3000*   2000    4000 GRACE       2       0       0       0\n" +
	"Disk quotas for group u1 (gid 1000): \n" +
	"     Filesystem  blocks   quota   limit   grace   files   quota   limit   grace\n" +
	"         MOUNT    3000       0       0       0       1       0       0       0\n"

// Hard-won: #1620: `df` cannot see a disk quota, so a worker stopped by one was restarted into it.
func TestStorageProbeScriptMeasuresTheUserSDiskQuota(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("POSIX shell")
	}
	directory := t.TempDir()
	tools := filepath.Join(directory, "tools")
	if err := os.Mkdir(tools, 0o755); err != nil {
		t.Fatal(err)
	}
	output := filepath.Join(directory, "quota-output")
	// Answers for whichever mount `-f` names, as the real tool does.
	fake := "#!/bin/sh\nfor last; do :; done\nsed \"s|MOUNT|$last|\" '" + output + "'\n"
	if err := os.WriteFile(filepath.Join(tools, "quota"), []byte(fake), 0o755); err != nil {
		t.Fatal(err)
	}
	probe := func(grace string) FilesystemSpace {
		t.Helper()
		if err := os.WriteFile(output, []byte(strings.ReplaceAll(recordedQuotaOutput, "GRACE", grace)), 0o644); err != nil {
			t.Fatal(err)
		}
		command := exec.Command("sh", "-c", StorageProbeScript, "mj-storage", directory)
		command.Env = append(os.Environ(), "PATH="+tools+":"+os.Getenv("PATH"))
		stdout, err := command.Output()
		if err != nil {
			t.Fatalf("%v: %q", err, stdout)
		}
		_, filesystems := ParseStorageLines(stdout)
		if len(filesystems) != 1 {
			t.Fatalf("%q", stdout)
		}
		return filesystems[0]
	}

	// Within the grace period the hard limit leaves 1000 KiB.
	withinGrace := probe("4102444800")
	if !withinGrace.QuotaLimited || withinGrace.AvailableBytes != 1000*1024 {
		t.Fatalf("within grace %+v", withinGrace)
	}

	// Once the grace period has run out the soft limit stops writes.
	expired := probe("1000000000")
	if !expired.QuotaLimited || expired.AvailableBytes != 0 {
		t.Fatalf("expired %+v", expired)
	}
}

// Hard-won: a6ecbe614: without `dirname` on PATH, the probe of a missing path spun forever and outlived its daemon.
func TestStorageProbeScriptEndsWhenTheHostLacksDirname(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("POSIX shell")
	}
	directory := t.TempDir()
	shell := ""
	for _, candidate := range []string{"/bin/sh", "/usr/bin/sh"} {
		if info, err := os.Stat(candidate); err == nil && info.Mode().IsRegular() {
			shell = candidate
			break
		}
	}
	if shell == "" {
		t.Fatal("find a POSIX sh")
	}
	tools := filepath.Join(directory, "tools")
	if err := os.Mkdir(tools, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(shell, filepath.Join(tools, "sh")); err != nil {
		t.Fatal(err)
	}
	child := exec.Command(shell, "-c", StorageProbeScript, "mj-storage", filepath.Join(directory, "not/yet/created"))
	child.Env = []string{"PATH=" + tools}
	if err := child.Start(); err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() { done <- child.Wait() }()
	select {
	case <-done:
	case <-time.After(10 * time.Second):
		child.Process.Kill()
		<-done
		t.Fatal("the storage probe kept walking a path it could not shorten")
	}
}
