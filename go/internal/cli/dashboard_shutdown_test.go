// Port of the tests in mj-cli/src/dashboard/shutdown.rs.

package cli

import (
	"errors"
	"strings"
	"testing"
)

func TestFinalCleanupOverlapsAndManagerErrorDoesNotCancelSaves(t *testing.T) {
	savesStarted := make(chan struct{})
	managerStarted := make(chan struct{})
	releaseSaves := make(chan struct{})
	releaseManager := make(chan struct{})
	type cleanupResult struct {
		saves   timed[error]
		manager timed[error]
	}
	cleanup := make(chan cleanupResult, 1)
	go func() {
		saves, manager := overlapFinalCleanup(
			func() error {
				close(savesStarted)
				<-releaseSaves
				return nil
			},
			func() error {
				close(managerStarted)
				<-releaseManager
				return errors.New("manager shutdown failed")
			},
		)
		cleanup <- cleanupResult{saves: saves, manager: manager}
	}()

	<-savesStarted
	<-managerStarted
	close(releaseManager)
	// Rust's is_finished: the saves are still held, so cleanup cannot have
	// returned however long the manager's failure has had.
	select {
	case <-cleanup:
		t.Fatal("cleanup returned while the saves were still running")
	default:
	}
	close(releaseSaves)

	result := <-cleanup
	if result.saves.output != nil {
		t.Fatal(result.saves.output)
	}
	if result.manager.output == nil || !strings.Contains(result.manager.output.Error(), "manager shutdown failed") {
		t.Fatalf("manager output = %v", result.manager.output)
	}
	if result.saves.startedAt.After(result.manager.finishedAt) {
		t.Fatal("the saves must start before the manager finishes")
	}
	if result.manager.startedAt.After(result.saves.finishedAt) {
		t.Fatal("the manager must start before the saves finish")
	}
}
