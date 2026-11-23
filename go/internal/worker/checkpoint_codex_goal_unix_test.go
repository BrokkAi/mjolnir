// Port of the `cfg(unix)` tests of mj-worker/src/checkpoint/codex_goal.rs.

//go:build unix

package worker

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
)

func TestNativeGoalRestoreDoesNotFollowADatabaseSymlink(t *testing.T) {
	source := t.TempDir()
	target := t.TempDir()
	codexGoalTestFixture(t, source, "selected", "original")
	artifact, artifactBytes := codexGoalTestCollect(t, source, "selected")
	if err := os.Symlink(filepath.Join(source, codexGoalDatabase), filepath.Join(target, codexGoalDatabase)); err != nil {
		t.Fatal(err)
	}
	if _, _, err := codexGoalCollect(target, "selected"); err == nil {
		t.Fatal("collect followed a database symlink")
	}
	if err := codexGoalRestore(target, "selected", artifact.RelativePath, artifactBytes); err == nil {
		t.Fatal("restore followed a database symlink")
	}
}

func TestWorkerCheckpointEntrypointsRestoreNativeGoalAccounting(t *testing.T) {
	temp := t.TempDir()
	spec, archivePath := checkpointTestsFixture(t, temp)
	expected := codexGoalTestFixture(t, spec.HarnessHome, spec.Session.NativeSessionID, "keep working")
	encoded, err := json.Marshal(spec)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := ExportFromSpecReader(bytes.NewReader(encoded)); err != nil {
		t.Fatal(err)
	}
	targetHome := filepath.Join(temp, "restored-home")
	restoreSpec := checkpoint.CheckpointRestoreSpec{
		ArchivePath:   archivePath,
		WorkspaceRoot: spec.WorkspaceRoot,
		RelayRoot:     filepath.Join(temp, "restored-relay"),
		HarnessHome:   targetHome,
		RestoreNative: true,
		QueuePolicy:   checkpoint.QueueRestorePolicyRestore,
	}
	restorePath := filepath.Join(temp, "restore.json")
	encoded, err = json.Marshal(restoreSpec)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(restorePath, encoded, 0o644); err != nil {
		t.Fatal(err)
	}
	if err := RestoreFromSpecFile(restorePath); err != nil {
		t.Fatal(err)
	}
	database := codexGoalTestOpen(t, filepath.Join(targetHome, codexGoalDatabase))
	codexGoalTestExpect(t, database, spec.Session.NativeSessionID, &expected)
}
