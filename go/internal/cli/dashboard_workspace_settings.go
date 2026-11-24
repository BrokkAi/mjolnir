// Port of mj-cli/src/dashboard/workspace_settings.rs.
//
// Ordered background saves of dashboard settings.
//
// The dashboard persists workspace support-pane sizes, conversation layouts,
// and the shared sidebar size. They need the same behaviour, so the
// coordinator is generic over the stored value and carries a short setting
// name for its notices.

package cli

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"reflect"
	"slices"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const workspaceSettingsFlushTimeout = 5 * time.Second

const (
	workspaceSettingsPaneSizes = "workspace pane sizes"
	workspaceSettingsLayout    = "workspace layout"
)

// settingSnapshot is one publication to the save task. Its maps are cloned
// from the coordinator's when published and never edited afterwards, which
// is what Rust's clone on borrow gives the task.
type settingSnapshot[T any] struct {
	requested map[string]T
	baseline  map[string]T
}

// workspaceSettingPersistence saves the latest requested value of each
// workspace in order, one save at a time.
//
// Rust takes T's Default and PartialEq from its bounds; Go cannot compare or
// default every T (ConversationLayout holds maps), so the constructor is
// given zero and equal.
type workspaceSettingPersistence[T any] struct {
	settingName string
	requested   map[string]T
	baseline    map[string]T
	// sender is the watch sender; nil once finish dropped it.
	sender *core.Watch[settingSnapshot[T]]
	// senderDropped is closed when finish drops the sender, which the save
	// task reads as the watch closing (core.Watch has no close).
	senderDropped chan struct{}
	// receiverGone is closed once the save task has returned, which drops
	// its watch receiver: a later publish has no one to read it.
	receiverGone <-chan struct{}
	task         *core.JoinHandle[error]
	// abort ends the save task's context (JoinHandle::abort on Drop).
	abort   context.CancelFunc
	failure *string
	notices chat.Notices
	zero    func() T
	equal   func(a, b T) bool
}

// startSidebarSizePersistence is Rust's
// `WorkspaceSettingPersistence::<PaneSize>::start_sidebar_size`.
func startSidebarSizePersistence(initial core.PaneSize, notices chat.Notices) *workspaceSettingPersistence[core.PaneSize] {
	return newWorkspaceSettingPersistence(
		"sidebar size",
		map[string]core.PaneSize{"global": initial},
		notices,
		// PaneSize's Rust Default is Standard.
		func() core.PaneSize { return core.PaneSizeStandard },
		func(a, b core.PaneSize) bool { return a == b },
		func(ctx context.Context, _ string, size core.PaneSize) error {
			daemonClient, err := client.ConnectExisting(ctx)
			if err != nil {
				return err
			}
			defer daemonClient.Close()
			return daemonClient.SaveSidebarSize(ctx, size)
		},
	)
}

// startPaneSizesPersistence is Rust's
// `WorkspaceSettingPersistence::<PaneSizes>::start_pane_sizes`.
func startPaneSizesPersistence(initial map[string]core.PaneSizes, notices chat.Notices) *workspaceSettingPersistence[core.PaneSizes] {
	return newWorkspaceSettingPersistence(
		workspaceSettingsPaneSizes,
		initial,
		notices,
		core.NewPaneSizes,
		func(a, b core.PaneSizes) bool { return a == b },
		func(ctx context.Context, workspaceID string, sizes core.PaneSizes) error {
			daemonClient, err := client.ConnectExisting(ctx)
			if err != nil {
				return err
			}
			defer daemonClient.Close()
			return daemonClient.SaveWorkspacePaneSizes(ctx, workspaceID, sizes)
		},
	)
}

// startLayoutsPersistence is Rust's
// `WorkspaceSettingPersistence::<ConversationLayout>::start_layouts`.
func startLayoutsPersistence(initial map[string]core.ConversationLayout, notices chat.Notices) *workspaceSettingPersistence[core.ConversationLayout] {
	return newWorkspaceSettingPersistence(
		workspaceSettingsLayout,
		initial,
		notices,
		core.NewConversationLayout,
		conversationLayoutEqual,
		func(ctx context.Context, workspaceID string, layout core.ConversationLayout) error {
			daemonClient, err := client.ConnectExisting(ctx)
			if err != nil {
				return err
			}
			defer daemonClient.Close()
			return daemonClient.SaveWorkspaceLayout(ctx, workspaceID, layout)
		},
	)
}

// newWorkspaceSettingPersistence is Rust's `with_save`: it starts the one
// save task, which calls save for each workspace whose latest requested
// value differs from what was last saved, or whose last save failed. save
// runs under the task's context, which ends when the task is aborted, as
// Rust drops the save future.
func newWorkspaceSettingPersistence[T any](
	settingName string,
	initial map[string]T,
	notices chat.Notices,
	zero func() T,
	equal func(a, b T) bool,
	save func(ctx context.Context, workspaceID string, value T) error,
) *workspaceSettingPersistence[T] {
	if initial == nil {
		initial = map[string]T{}
	}
	sender := core.NewWatch(settingSnapshot[T]{
		requested: maps.Clone(initial),
		baseline:  maps.Clone(initial),
	})
	senderDropped := make(chan struct{})
	// The receiver marks the initial snapshot seen, as watch::channel does,
	// before the task could miss a publication.
	receiver := newWatchReceiver(sender, senderDropped)
	ctx, abort := context.WithCancel(context.Background())
	task := core.Spawn(func() error {
		defer abort()
		return saveSettingSnapshots(ctx, settingName, receiver, notices, zero, equal, save)
	})
	return &workspaceSettingPersistence[T]{
		settingName:   settingName,
		requested:     maps.Clone(initial),
		baseline:      maps.Clone(initial),
		sender:        sender,
		senderDropped: senderDropped,
		receiverGone:  task.Done(),
		task:          task,
		abort:         abort,
		notices:       notices,
		zero:          zero,
		equal:         equal,
	}
}

// saveSettingSnapshots is the save task. It writes each workspace whose
// latest requested value differs from what it last saved, or whose last
// save failed, one save at a time and in workspace order. Once the sender is
// dropped it returns the first failure still standing. When ctx ends (abort)
// it returns at once, and nothing reads its result.
func saveSettingSnapshots[T any](
	ctx context.Context,
	settingName string,
	receiver *watchReceiver[settingSnapshot[T]],
	reports chat.Notices,
	zero func() T,
	equal func(a, b T) bool,
	save func(ctx context.Context, workspaceID string, value T) error,
) error {
	persisted := map[string]T{}
	failures := map[string]string{}
	// The initial snapshot is the database baseline. Newly remembered
	// workspaces add their own baseline before their first update.
	for receiver.changed(ctx) {
		snapshot := receiver.borrowAndUpdate()
		// A deleted workspace must not keep an old failed save alive until
		// shutdown. The latest snapshot is authoritative for both retry and
		// final-error reporting.
		maps.DeleteFunc(persisted, func(workspaceID string, _ T) bool {
			_, kept := snapshot.requested[workspaceID]
			return !kept
		})
		maps.DeleteFunc(failures, func(workspaceID string, _ string) bool {
			_, kept := snapshot.requested[workspaceID]
			return !kept
		})
		for _, workspaceID := range slices.Sorted(maps.Keys(snapshot.requested)) {
			value := snapshot.requested[workspaceID]
			previous, ok := persisted[workspaceID]
			if !ok {
				baseline, ok := snapshot.baseline[workspaceID]
				if !ok {
					baseline = zero()
				}
				previous = baseline
				persisted[workspaceID] = baseline
			}
			_, retrying := failures[workspaceID]
			if equal(previous, value) && !retrying {
				continue
			}
			err := save(ctx, workspaceID, value)
			if ctx.Err() != nil {
				// Aborted: Rust drops the save future here.
				return nil
			}
			if err == nil {
				persisted[workspaceID] = value
				delete(failures, workspaceID)
				continue
			}
			// The workspace may have been deleted while the save was in
			// flight. Do not report that expected failure or make shutdown
			// fail in that case.
			if _, live := receiver.borrow().requested[workspaceID]; live {
				message := fmt.Sprintf("Could not save %s for %s: %v", settingName, workspaceID, err)
				slog.Warn(message)
				reports.SetFailure(message)
				failures[workspaceID] = message
			} else {
				delete(failures, workspaceID)
			}
		}
	}
	if len(failures) == 0 {
		return nil
	}
	return errors.New(failures[slices.Sorted(maps.Keys(failures))[0]])
}

// remember adds a layout loaded for a workspace without treating it as a
// user mutation. A duplicate remember is deliberately ignored so a late load
// cannot reset a layout the user already changed.
func (w *workspaceSettingPersistence[T]) remember(workspaceID string, value T) {
	if _, ok := w.requested[workspaceID]; ok {
		return
	}
	w.requested[workspaceID] = value
	w.baseline[workspaceID] = value
	w.publish()
}

func (w *workspaceSettingPersistence[T]) forget(workspaceID string) {
	_, requestedRemoved := w.requested[workspaceID]
	_, baselineRemoved := w.baseline[workspaceID]
	delete(w.requested, workspaceID)
	delete(w.baseline, workspaceID)
	if requestedRemoved || baselineRemoved {
		w.publish()
	}
}

func (w *workspaceSettingPersistence[T]) update(workspaceID string, value T) {
	if current, ok := w.requested[workspaceID]; ok && w.equal(current, value) {
		return
	}
	// Callers normally remember a lazy-loaded workspace first. A default
	// baseline keeps an unexpected early update deterministic and lets the
	// first non-default choice be persisted rather than discarded.
	if _, ok := w.baseline[workspaceID]; !ok {
		w.baseline[workspaceID] = w.zero()
	}
	w.requested[workspaceID] = value
	w.publish()
}

// publish sends the current snapshot to the save task, or reports that the
// task has stopped.
func (w *workspaceSettingPersistence[T]) publish() {
	if w.sender == nil {
		return
	}
	// watch::Sender::send fails once the receiver is gone, which is when
	// the save task has returned.
	select {
	case <-w.receiverGone:
		w.notices.SetFailure(fmt.Sprintf("Could not save %s: background saver stopped", w.settingName))
		return
	default:
	}
	w.sender.SendReplace(settingSnapshot[T]{
		requested: maps.Clone(w.requested),
		baseline:  maps.Clone(w.baseline),
	})
}

func (w *workspaceSettingPersistence[T]) isRunning() bool {
	return w.task != nil
}

// wait supervises the save task: it returns once the task ended (after a
// panic or a failed final save, reported to the notices) or ctx ended, and
// blocks until ctx ends when no task runs. Returning on ctx leaves the task
// to a later wait, as dropping Rust's wait future does.
func (w *workspaceSettingPersistence[T]) wait(ctx context.Context) {
	if w.task == nil {
		<-ctx.Done()
		return
	}
	select {
	case <-w.task.Done():
	case <-ctx.Done():
		return
	}
	result, joinErr := w.task.Join(context.Background())
	if joinErr != nil {
		result = fmt.Errorf("%s save task failed: %w", w.settingName, joinErr)
	}
	w.task = nil
	w.failure = nil
	if result != nil {
		message := result.Error()
		w.failure = &message
		slog.Warn(message)
		w.notices.SetFailure(message)
	}
}

// finish is called after handing back the terminal. Retry unsuccessful
// latest choices, drain every workspace's pending change, and bound the
// flush by workspaceSettingsFlushTimeout.
func (w *workspaceSettingPersistence[T]) finish() error {
	// Rust's finish consumes self, so every exit drops it, which aborts a
	// save task still running.
	defer w.drop()
	if w.sender != nil {
		requested, baseline := w.requested, w.baseline
		w.requested, w.baseline = map[string]T{}, map[string]T{}
		w.sender.SendReplace(settingSnapshot[T]{requested: requested, baseline: baseline})
		w.sender = nil
		close(w.senderDropped)
	}
	if w.isRunning() {
		ctx, cancel := context.WithTimeout(context.Background(), workspaceSettingsFlushTimeout)
		w.wait(ctx)
		cancel()
		if w.isRunning() {
			return fmt.Errorf(
				"Timed out saving %s after %d seconds; the latest choice may not have been saved",
				w.settingName,
				int(workspaceSettingsFlushTimeout/time.Second),
			)
		}
	}
	if w.failure == nil {
		return nil
	}
	failure := *w.failure
	w.failure = nil
	return errors.New(failure)
}

// drop is Rust's Drop: abort a save task still running.
func (w *workspaceSettingPersistence[T]) drop() {
	if w.task != nil && w.abort != nil {
		w.abort()
	}
}

// conversationLayoutEqual is ConversationLayout's derived PartialEq: an empty
// map equals a nil one, as two empty BTreeMaps are equal.
func conversationLayoutEqual(a, b core.ConversationLayout) bool {
	browseEqual := (a.Browse == nil) == (b.Browse == nil) && (a.Browse == nil || *a.Browse == *b.Browse)
	return a.Focus == b.Focus &&
		browseEqual &&
		maps.Equal(a.Sessions, b.Sessions) &&
		maps.Equal(a.Pins, b.Pins) &&
		reflect.DeepEqual(a.Root, b.Root)
}
