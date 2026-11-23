//go:build linux

// Port of the Linux-only test in mj-checkpoint/src/snapshot.rs.

package checkpoint

import (
	"os"
	"path/filepath"
	"syscall"
	"testing"
)

// Hard-won: #1635: snapshot packing failed with EXDEV when repository and capture directory were on different filesystems.
func TestSnapshotAndEstimatePackAcrossDifferentFilesystems(t *testing.T) {
	device := func(path string) (uint64, bool) {
		info, err := os.Stat(path)
		if err != nil {
			return 0, false
		}
		stat, ok := info.Sys().(*syscall.Stat_t)
		return uint64(stat.Dev), ok && info.IsDir()
	}
	systemTemp := os.TempDir()
	systemDev, _ := device(systemTemp)
	var other string
	for _, candidate := range []string{"/dev/shm", "/var/tmp", "/tmp"} {
		dev, ok := device(candidate)
		if !ok || dev == systemDev {
			continue
		}
		created, err := os.MkdirTemp(candidate, "mj-snapshot-exdev-")
		if err == nil {
			other = created
			break
		}
	}
	if other == "" {
		t.Logf("skipping cross-filesystem snapshot test: no writable mount distinct from %s", systemTemp)
		return
	}
	t.Cleanup(func() { os.RemoveAll(other) })
	if dev, _ := device(other); dev == systemDev {
		t.Fatal("other mount shares the system temp device")
	}

	origin := filepath.Join(other, "origin.git")
	checkout := filepath.Join(other, "checkout")
	snapGit(other, "init", "--bare", "--initial-branch=main", origin)
	snapGit(other, "clone", origin, checkout)
	snapMustDo(os.WriteFile(filepath.Join(checkout, "file"), []byte("origin\n"), 0o644))
	snapGit(checkout, "add", "file")
	snapGit(checkout, "commit", "-m", "base")
	snapGit(checkout, "push", "origin", "main")
	captureDir := snapMust(os.MkdirTemp(systemTemp, ".tmp"))
	t.Cleanup(func() { os.RemoveAll(captureDir) })
	otherDev, _ := device(other)
	captureDev, _ := device(captureDir)
	if otherDev == captureDev {
		t.Fatal("capture directory shares the repository device")
	}
	packBase := filepath.Join(captureDir, "snapshot")
	clean := snapMust(CaptureWorkspaceSnapshot(fileGit{}, checkout, CheckoutOwnershipManaged(), packBase, anyConsent(), nil))
	if clean.Pack != nil {
		t.Fatal("clean capture has a pack")
	}
	for _, entry := range snapMust(os.ReadDir(captureDir)) {
		if extension, ok := checkpointExtension(entry.Name()); ok && extension == "pack" {
			t.Fatalf("pack left behind: %s", entry.Name())
		}
	}
	if total := snapMust(EstimateDelta(fileGit{}, checkout, CheckoutOwnershipManaged(), nil)).TotalBytes; total == nil || *total != 0 {
		t.Fatalf("clean estimate %v", total)
	}

	snapMustDo(os.WriteFile(filepath.Join(checkout, "committed"), []byte("committed change\n"), 0o644))
	snapGit(checkout, "add", "committed")
	snapGit(checkout, "commit", "-m", "committed change")
	snapMustDo(os.WriteFile(filepath.Join(checkout, "file"), []byte("working change\n"), 0o644))
	estimate := snapMust(EstimateDelta(fileGit{}, checkout, CheckoutOwnershipManaged(), nil))
	snapshot := snapMust(CaptureWorkspaceSnapshot(fileGit{}, checkout, CheckoutOwnershipManaged(), packBase, anyConsent(), nil))
	packLen := uint64(snapMust(os.Stat(*snapshot.Pack)).Size())
	if *estimate.TotalBytes != packLen {
		t.Fatalf("estimate %d != pack %d", *estimate.TotalBytes, packLen)
	}

	root := &snapRepo{temp: other, origin: origin, checkout: checkout}
	restored := root.restore(snapshot)
	if snapGit(restored, "rev-parse", "HEAD^{tree}") != snapGit(root.checkout, "rev-parse", "HEAD^{tree}") {
		t.Fatal("restored tree differs")
	}
	if got := string(snapMust(os.ReadFile(filepath.Join(restored, "file")))); got != "working change\n" {
		t.Fatalf("file %q", got)
	}
	snapGit(restored, "fsck", "--full")
}
