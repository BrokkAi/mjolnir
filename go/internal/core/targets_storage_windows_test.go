//go:build windows

// Port of the Windows test in mj-core/src/targets/storage.rs.

package core

import (
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

func TestWindowsStorageMeasuresMissingPathsAtAnExistingAncestor(t *testing.T) {
	directory := t.TempDir()
	missing := filepath.Join(directory, "not/yet/created")
	paths := []string{
		missing,
		directory,
		// Inside Docker Desktop's VM, which Windows cannot measure.
		"/var/lib/docker",
	}
	filesystems := MeasureWindowsFilesystems(paths)
	if len(filesystems) != 1 {
		t.Fatalf("%+v", filesystems)
	}
	if !strings.HasPrefix(strings.ToLower(missing), strings.ToLower(filesystems[0].Mount)) {
		t.Fatalf("%+v", filesystems)
	}
	if filesystems[0].AvailableBytes > filesystems[0].TotalBytes {
		t.Fatalf("%+v", filesystems[0])
	}
	if !slices.Equal(filesystems[0].Paths, paths[:2]) {
		t.Fatalf("%v", filesystems[0].Paths)
	}
}
