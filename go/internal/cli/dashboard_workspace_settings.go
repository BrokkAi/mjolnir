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
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const workspaceSettingsFlushTimeout = 5 * time.Second

const (
	workspaceSettingsPaneSizes = "workspace pane sizes"
	workspaceSettingsLayout    = "workspace layout"
)

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
	task          *core.JoinHandle[error]
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
	panic("unported: mj-cli/src/dashboard/workspace_settings.rs WorkspaceSettingPersistence::start_sidebar_size")
}

// startPaneSizesPersistence is Rust's
// `WorkspaceSettingPersistence::<PaneSizes>::start_pane_sizes`.
func startPaneSizesPersistence(initial map[string]core.PaneSizes, notices chat.Notices) *workspaceSettingPersistence[core.PaneSizes] {
	panic("unported: mj-cli/src/dashboard/workspace_settings.rs WorkspaceSettingPersistence::start_pane_sizes")
}

// startLayoutsPersistence is Rust's
// `WorkspaceSettingPersistence::<ConversationLayout>::start_layouts`.
func startLayoutsPersistence(initial map[string]core.ConversationLayout, notices chat.Notices) *workspaceSettingPersistence[core.ConversationLayout] {
	panic("unported: mj-cli/src/dashboard/workspace_settings.rs WorkspaceSettingPersistence::start_layouts")
}

// newWorkspaceSettingPersistence is Rust's `with_save`: it starts the one
// save task, which calls save for each workspace whose latest requested
// value differs from what was last saved, or whose last save failed.
func newWorkspaceSettingPersistence[T any](
	settingName string,
	initial map[string]T,
	notices chat.Notices,
	zero func() T,
	equal func(a, b T) bool,
	save func(ctx context.Context, workspaceID string, value T) error,
) *workspaceSettingPersistence[T] {
	panic("unported: mj-cli/src/dashboard/workspace_settings.rs WorkspaceSettingPersistence::with_save")
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
	panic("unported: mj-cli/src/dashboard/workspace_settings.rs WorkspaceSettingPersistence::publish")
}

func (w *workspaceSettingPersistence[T]) isRunning() bool {
	return w.task != nil
}

// wait supervises the save task: it returns once the task ended (after a
// panic or a failed final save, reported to the notices) or ctx ended, and
// blocks until ctx ends when no task runs.
func (w *workspaceSettingPersistence[T]) wait(ctx context.Context) {
	panic("unported: mj-cli/src/dashboard/workspace_settings.rs WorkspaceSettingPersistence::wait")
}

// finish is called after handing back the terminal. Retry unsuccessful
// latest choices, drain every workspace's pending change, and bound the
// flush by workspaceSettingsFlushTimeout.
func (w *workspaceSettingPersistence[T]) finish() error {
	panic("unported: mj-cli/src/dashboard/workspace_settings.rs WorkspaceSettingPersistence::finish")
}

// drop is Rust's Drop: abort a save task still running.
func (w *workspaceSettingPersistence[T]) drop() {
	if w.task != nil && w.abort != nil {
		w.abort()
	}
}
