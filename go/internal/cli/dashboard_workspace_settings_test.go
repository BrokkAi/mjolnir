// Port of mj-cli/src/dashboard/workspace_settings.rs tests.
//
// Rust's #[tokio::test] runs these on a current-thread runtime, where a task
// runs until its next await before the test resumes; each Go test runs in a
// synctest bubble and calls synctest.Wait where Rust relies on that. The
// paused-clock test uses the bubble's clock.

package cli

import (
	"context"
	"errors"
	"reflect"
	"strings"
	"sync"
	"testing"
	"testing/synctest"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func workspaceSettingsLayoutOf(sessions core.PaneSize) core.PaneSizes {
	sizes := core.NewPaneSizes()
	sizes.Sessions = sessions
	return sizes
}

func workspaceSettingsInitial(workspaceID string) map[string]core.PaneSizes {
	return map[string]core.PaneSizes{workspaceID: core.NewPaneSizes()}
}

func paneSizesEqual(a, b core.PaneSizes) bool { return a == b }

// workspaceSettingsSave is one save call the test answers: Rust's
// (workspace id, sizes, oneshot sender).
type workspaceSettingsSave struct {
	workspaceID string
	sizes       core.PaneSizes
	complete    chan error
}

// recordingPaneSizes is the persistence over a save that hands each call to
// the test and waits for the test's answer.
func recordingPaneSizes(t *testing.T) (*workspaceSettingPersistence[core.PaneSizes], *core.Unbounded[workspaceSettingsSave]) {
	t.Helper()
	calls := core.NewUnbounded[workspaceSettingsSave]()
	persistence := newWorkspaceSettingPersistence(
		workspaceSettingsPaneSizes,
		workspaceSettingsInitial("workspace-a"),
		chat.NewNotices(),
		core.NewPaneSizes,
		paneSizesEqual,
		func(ctx context.Context, workspaceID string, sizes core.PaneSizes) error {
			complete := make(chan error, 1)
			if err := calls.Send(workspaceSettingsSave{workspaceID: workspaceID, sizes: sizes, complete: complete}); err != nil {
				panic(err)
			}
			select {
			case err := <-complete:
				return err
			case <-ctx.Done():
				return ctx.Err()
			}
		},
	)
	return persistence, calls
}

func nextWorkspaceSettingsSave(t *testing.T, calls *core.Unbounded[workspaceSettingsSave]) workspaceSettingsSave {
	t.Helper()
	call, ok := calls.Recv(context.Background())
	if !ok {
		t.Fatal("the save calls ended")
	}
	return call
}

func TestRapidChangesAreSerializedAndFlushTheLatestLayout(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		persistence, calls := recordingPaneSizes(t)
		first := workspaceSettingsLayoutOf(core.PaneSizeMinimized)
		middle := workspaceSettingsLayoutOf(core.PaneSizeMaximized)
		latest := core.NewPaneSizes()
		persistence.update("workspace-a", first)
		call := nextWorkspaceSettingsSave(t, calls)
		if call.workspaceID != "workspace-a" {
			t.Fatalf("workspace %q", call.workspaceID)
		}
		if call.sizes != first {
			t.Fatalf("sizes %+v", call.sizes)
		}
		persistence.update("workspace-a", middle)
		persistence.update("workspace-a", latest)
		synctest.Wait()
		if _, ok, _ := calls.TryRecv(); ok {
			t.Fatal("saves must not overlap")
		}
		finish := core.Spawn(persistence.finish)
		call.complete <- nil
		call = nextWorkspaceSettingsSave(t, calls)
		if call.workspaceID != "workspace-a" {
			t.Fatalf("workspace %q", call.workspaceID)
		}
		if call.sizes != latest {
			t.Fatalf("sizes %+v", call.sizes)
		}
		call.complete <- nil
		err, joinErr := finish.Join(context.Background())
		if joinErr != nil {
			t.Fatal(joinErr)
		}
		if err != nil {
			t.Fatal(err)
		}
		// Rust's recv() answers None once the finished task dropped the
		// save closure: no further save can arrive.
		if call, ok, _ := calls.TryRecv(); ok {
			t.Fatalf("an extra save %+v", call)
		}
	})
}

func TestRapidWorkspaceSwitchesRetainEachLatestLayout(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		persistence, calls := recordingPaneSizes(t)
		persistence.remember("workspace-b", core.NewPaneSizes())
		aFirst := workspaceSettingsLayoutOf(core.PaneSizeMinimized)
		aLatest := workspaceSettingsLayoutOf(core.PaneSizeMaximized)
		bLatest := workspaceSettingsLayoutOf(core.PaneSizeMinimized)
		persistence.update("workspace-a", aFirst)
		call := nextWorkspaceSettingsSave(t, calls)
		if call.workspaceID != "workspace-a" || call.sizes != aFirst {
			t.Fatalf("save %q %+v", call.workspaceID, call.sizes)
		}

		// Both changes arrive while A is still being written. A's newer value
		// and B's value must survive in the one watch snapshot.
		persistence.update("workspace-b", bLatest)
		persistence.update("workspace-a", aLatest)
		synctest.Wait()
		if _, ok, _ := calls.TryRecv(); ok {
			t.Fatal("saves must not overlap")
		}
		call.complete <- nil

		call = nextWorkspaceSettingsSave(t, calls)
		if call.workspaceID != "workspace-a" || call.sizes != aLatest {
			t.Fatalf("save %q %+v", call.workspaceID, call.sizes)
		}
		call.complete <- nil
		call = nextWorkspaceSettingsSave(t, calls)
		if call.workspaceID != "workspace-b" || call.sizes != bLatest {
			t.Fatalf("save %q %+v", call.workspaceID, call.sizes)
		}
		call.complete <- nil
		if err := persistence.finish(); err != nil {
			t.Fatal(err)
		}
		if call, ok, _ := calls.TryRecv(); ok {
			t.Fatalf("an extra save %+v", call)
		}
	})
}

func TestFailedSavesAreReportedAndTheLatestChoiceIsRetriedOnExit(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		notices := chat.NewNotices()
		type attempt struct {
			workspaceID string
			sizes       core.PaneSizes
		}
		var mu sync.Mutex
		var attempts []attempt
		failed := make(chan struct{})
		first := true
		persistence := newWorkspaceSettingPersistence(
			workspaceSettingsPaneSizes,
			workspaceSettingsInitial("workspace-a"),
			notices,
			core.NewPaneSizes,
			paneSizesEqual,
			func(_ context.Context, workspaceID string, sizes core.PaneSizes) error {
				mu.Lock()
				attempts = append(attempts, attempt{workspaceID: workspaceID, sizes: sizes})
				isFirst := first
				first = false
				mu.Unlock()
				if isFirst {
					close(failed)
					return errors.New("database unavailable")
				}
				return nil
			},
		)
		desired := workspaceSettingsLayoutOf(core.PaneSizeMinimized)
		persistence.update("workspace-a", desired)
		<-failed
		// Rust's task reports the failure before its next await, which is
		// before the test resumes.
		synctest.Wait()
		if notice, ok := notices.Current(); !ok || !strings.Contains(notice, "database unavailable") {
			t.Fatalf("notice %q, %v", notice, ok)
		}
		if err := persistence.finish(); err != nil {
			t.Fatal(err)
		}
		mu.Lock()
		defer mu.Unlock()
		want := []attempt{
			{workspaceID: "workspace-a", sizes: desired},
			{workspaceID: "workspace-a", sizes: desired},
		}
		if !reflect.DeepEqual(attempts, want) {
			t.Fatalf("attempts %+v", attempts)
		}
	})
}

func TestAStalledSaveBoundsExitAndIsCancelled(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		started := make(chan struct{})
		cancelled := make(chan struct{})
		persistence := newWorkspaceSettingPersistence(
			workspaceSettingsPaneSizes,
			workspaceSettingsInitial("workspace-a"),
			chat.NewNotices(),
			core.NewPaneSizes,
			paneSizesEqual,
			func(ctx context.Context, _ string, _ core.PaneSizes) error {
				// Rust takes each sender once; a second call panics.
				close(started)
				defer close(cancelled)
				<-ctx.Done()
				return ctx.Err()
			},
		)
		persistence.update("workspace-a", workspaceSettingsLayoutOf(core.PaneSizeMinimized))
		<-started
		before := time.Now()
		err := persistence.finish()
		if elapsed := time.Since(before); elapsed != workspaceSettingsFlushTimeout {
			t.Fatalf("finish took %v", elapsed)
		}
		if err == nil || !strings.Contains(err.Error(), "Timed out") {
			t.Fatalf("finish = %v", err)
		}
		// The save future must be dropped.
		<-cancelled
	})
}

func TestAnUnsuccessfulFinalSaveIsReturnedAfterReportingTheFailure(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		notices := chat.NewNotices()
		persistence := newWorkspaceSettingPersistence(
			workspaceSettingsPaneSizes,
			workspaceSettingsInitial("workspace-a"),
			notices,
			core.NewPaneSizes,
			paneSizesEqual,
			func(context.Context, string, core.PaneSizes) error {
				return errors.New("workspace was deleted")
			},
		)
		persistence.update("workspace-a", workspaceSettingsLayoutOf(core.PaneSizeMinimized))
		err := persistence.finish()
		if err == nil || !strings.Contains(err.Error(), "workspace was deleted") {
			t.Fatalf("finish = %v", err)
		}
		if notice, ok := notices.Current(); !ok || !strings.Contains(notice, "workspace was deleted") {
			t.Fatalf("notice %q, %v", notice, ok)
		}
	})
}

func TestAPanickingSaveIsSupervisedAndReportedOnExit(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		notices := chat.NewNotices()
		persistence := newWorkspaceSettingPersistence(
			workspaceSettingsPaneSizes,
			workspaceSettingsInitial("workspace-a"),
			notices,
			core.NewPaneSizes,
			paneSizesEqual,
			func(context.Context, string, core.PaneSizes) error {
				panic("save panic")
			},
		)
		persistence.update("workspace-a", workspaceSettingsLayoutOf(core.PaneSizeMinimized))
		persistence.wait(context.Background())
		if persistence.isRunning() {
			t.Fatal("the panicked task is still supervised as running")
		}
		if notice, ok := notices.Current(); !ok || !strings.Contains(notice, "save task failed") {
			t.Fatalf("notice %q, %v", notice, ok)
		}
		if persistence.finish() == nil {
			t.Fatal("finish reports the panicked save")
		}
	})
}

func TestForgettingWorkspaceWhileSaveFailsDoesNotFailFinish(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		notices := chat.NewNotices()
		started := make(chan struct{})
		released := make(chan struct{})
		persistence := newWorkspaceSettingPersistence(
			workspaceSettingsPaneSizes,
			workspaceSettingsInitial("workspace-a"),
			notices,
			core.NewPaneSizes,
			paneSizesEqual,
			func(context.Context, string, core.PaneSizes) error {
				// Rust takes each channel once; a second call panics.
				close(started)
				<-released
				return errors.New("workspace was deleted")
			},
		)
		persistence.update("workspace-a", workspaceSettingsLayoutOf(core.PaneSizeMinimized))
		<-started
		persistence.forget("workspace-a")
		close(released)
		if err := persistence.finish(); err != nil {
			t.Fatal(err)
		}
		if notice, ok := notices.Current(); ok {
			t.Fatalf("notice %q", notice)
		}
	})
}
