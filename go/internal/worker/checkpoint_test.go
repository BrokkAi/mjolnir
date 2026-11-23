// Port of the recovery_tests of mj-worker/src/checkpoint.rs
// (`cfg(all(test, unix))`).

//go:build unix

package worker

import (
	"os"
	"path/filepath"
	"testing"
)

func TestCheckoutPublicationNeverReplacesADirectoryCreatedByAnotherWriter(t *testing.T) {
	temp := t.TempDir()
	staged := filepath.Join(temp, "staged")
	current := filepath.Join(temp, "current")
	checkpointTestsMust(t, os.Mkdir(staged, 0o755))
	checkpointTestsMust(t, os.WriteFile(filepath.Join(staged, "file"), []byte("archived"), 0o644))
	checkpointTestsMust(t, os.Mkdir(current, 0o755))
	checkpointTestsMust(t, os.WriteFile(filepath.Join(current, "file"), []byte("new current work"), 0o644))
	if published, err := renameDirectoryWithoutReplacement(staged, current); err != nil || published {
		t.Fatalf("published = %v, %v", published, err)
	}
	checkpointTestsExpectFile(t, filepath.Join(current, "file"), "new current work")
	checkpointTestsExpectFile(t, filepath.Join(staged, "file"), "archived")
	absent := filepath.Join(temp, "absent")
	if published, err := renameDirectoryWithoutReplacement(staged, absent); err != nil || !published {
		t.Fatalf("published = %v, %v", published, err)
	}
	checkpointTestsExpectFile(t, filepath.Join(absent, "file"), "archived")
	if _, err := os.Stat(staged); err == nil {
		t.Fatal("the staged directory must move")
	}
}
