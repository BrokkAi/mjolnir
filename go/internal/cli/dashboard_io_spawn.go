// Port of mj-cli/src/dashboard/io/spawn.rs.
//
// The spawn helpers that run the dashboard's background jobs. tokio's
// UnboundedSender is a core.Unbounded; a JoinHandle<()> is a dashboardTask;
// tokio_util's DropGuard is the context.CancelFunc the caller must call
// where Rust drops the guard; Arc<AtomicBool> is *atomic.Bool; a
// tokio::sync::Semaphore shared by Arc is a buffered channel of slots that
// starts empty: sending a token acquires a permit, receiving it back
// releases it. An async Rust job is a func(ctx) here: ctx ends when the job
// is aborted or times out, as dropping the Rust future does. Rust's
// `T: Send + 'static` bounds have no Go counterpart.
//
// Rust's `#[cfg(test)] pub(crate) use mj_client::settings::save::save_at as
// save_setup_at` re-export is not declared: Go tests call client's SaveAt
// directly.

package cli

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"os"
	"slices"
	"strings"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/daemon"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// dashboardTask is a spawned dashboard job, Rust's JoinHandle<()>. Go has no
// task abort or drop: abort ends the context the job runs under, and a job
// checks it before reporting, so an aborted job reports nothing, as a Rust
// task dropped at its next await point does. A holder that Rust wraps in
// AbortOnDropHandle calls abort wherever Rust drops it.
type dashboardTask struct {
	handle *core.JoinHandle[struct{}]
	cancel context.CancelFunc
}

// spawnDashboardTask runs job on its own goroutine under a context that
// abort ends. A panic in job is reported by join, never lost.
func spawnDashboardTask(job func(ctx context.Context)) *dashboardTask {
	ctx, cancel := context.WithCancel(context.Background())
	handle := core.Spawn(func() struct{} {
		defer cancel()
		job(ctx)
		return struct{}{}
	})
	return &dashboardTask{handle: handle, cancel: cancel}
}

// abort is JoinHandle::abort: the job's context ends.
func (t *dashboardTask) abort() { t.cancel() }

// done is closed once the job has returned.
func (t *dashboardTask) done() <-chan struct{} { return t.handle.Done() }

// isFinished is JoinHandle::is_finished.
func (t *dashboardTask) isFinished() bool {
	select {
	case <-t.handle.Done():
		return true
	default:
		return false
	}
}

// join is awaiting the JoinHandle: nil once the job returned, the panic it
// died of, or ctx's error when ctx ends first.
func (t *dashboardTask) join(ctx context.Context) error {
	_, err := t.handle.Join(ctx)
	return err
}

// report hands one finished answer to the dashboard. A closed channel means
// the dashboard has already shut down, which is not a failure, so every
// helper reports through here instead of deciding that for itself.
func report[T any](operation string, updates *core.Unbounded[T], update T) {
	if err := updates.Send(update); err != nil {
		slog.Debug("dashboard background result dropped after shutdown", "operation", operation, "error", err.Error())
	}
}

// blockingResult turns a finished blocking job into the answer the
// dashboard shows. err is the job's own failure; joinErr is a panic in the
// job, which arrives here as a join error and becomes a reported failure, so
// the screen never waits forever on work that died.
func blockingResult[T any](operation string, value T, err, joinErr error) (T, error) {
	var zero T
	if joinErr != nil {
		failure := fmt.Errorf("%s task failed: %w", operation, joinErr)
		slog.Warn("dashboard background operation panicked", "operation", operation, "error", failure.Error())
		return zero, failure
	}
	if err != nil {
		slog.Warn("dashboard background operation failed", "operation", operation, "error", err.Error())
		return zero, err
	}
	return value, nil
}

// blockingOutcome is a blocking job's answer.
type blockingOutcome[T any] struct {
	value T
	err   error
}

// awaitBlocking runs work as blocking work and waits for it (Rust's
// `spawn_blocking(work).await`). joinErr is the panic work died of. ok is
// false when ctx ended first, which is the awaiting task being aborted: the
// blocking work still runs to its end, as Rust's does, but nobody waits.
func awaitBlocking[T any](ctx context.Context, work func() (T, error)) (value T, err, joinErr error, ok bool) {
	handle := core.SpawnBlocking(func() blockingOutcome[T] {
		value, err := work()
		return blockingOutcome[T]{value: value, err: err}
	})
	select {
	case <-handle.Done():
	case <-ctx.Done():
		return value, nil, nil, false
	}
	outcome, joinErr := handle.Join(context.Background())
	return outcome.value, outcome.err, joinErr, true
}

// blockOnResult is Rust's `mj_core::runtime::block_on(future)?` for a
// future that answers a Result: runtime shutdown fails the call as the
// future's own error would.
func blockOnResult[T any](future func(ctx context.Context) (T, error)) (T, error) {
	outcome, err := core.BlockOn(func(ctx context.Context) blockingOutcome[T] {
		value, err := future(ctx)
		return blockingOutcome[T]{value: value, err: err}
	})
	if err != nil {
		var zero T
		return zero, err
	}
	return outcome.value, outcome.err
}

// withDaemon runs request on a started or reused daemon's connection and
// closes it afterwards (Rust: `daemon::connect_or_start().await?` and
// dropping the client).
func withDaemon[T any](ctx context.Context, request func(daemonClient *client.DaemonClient) (T, error)) (T, error) {
	daemonClient, err := ConnectOrStart(ctx)
	if err != nil {
		var zero T
		return zero, err
	}
	defer daemonClient.Close()
	return request(daemonClient)
}

// withExistingDaemon is withDaemon on a running daemon only (Rust's
// `daemon::connect_existing()`).
func withExistingDaemon[T any](ctx context.Context, request func(daemonClient *client.DaemonClient) (T, error)) (T, error) {
	daemonClient, err := client.ConnectExisting(ctx)
	if err != nil {
		var zero T
		return zero, err
	}
	defer daemonClient.Close()
	return request(daemonClient)
}

// unit adapts a call that answers only an error to the result shape of
// the spawn helpers.
func unit(err error) (struct{}, error) { return struct{}{}, err }

// spawnIO runs one blocking job off the loop and reports its outcome on the
// dashboard's I/O channel. Errors and panics are formatted once, here, so no
// caller can quietly drop one.
func spawnIO[T any](
	operation string,
	updates *core.Unbounded[dashboardIoUpdate],
	work func() (T, error),
	toUpdate func(T, error) dashboardIoUpdate,
) *dashboardTask {
	return spawnDashboardTask(func(ctx context.Context) {
		value, err, joinErr, ok := awaitBlocking(ctx, work)
		if !ok {
			return
		}
		value, err = blockingResult(operation, value, err, joinErr)
		report(operation, updates, toUpdate(value, err))
	})
}

// spawnCriticalIO runs a user-authored mutation off the event loop and
// keeps dashboard exit pending until the mutation has reached its durable
// boundary.
func spawnCriticalIO[T any](
	tracker client.CriticalOperationTracker,
	label string,
	updates *core.Unbounded[dashboardIoUpdate],
	work func() (T, error),
	toUpdate func(T, error) dashboardIoUpdate,
) *dashboardTask {
	guard := tracker.Begin(label)
	return spawnDashboardTask(func(ctx context.Context) {
		defer guard.Release()
		value, err, joinErr, ok := awaitBlocking(ctx, work)
		if !ok {
			return
		}
		value, err = blockingResult(label, value, err, joinErr)
		report(label, updates, toUpdate(value, err))
	})
}

// spawnAsyncJob: network waits must not hold a blocking-pool thread:
// connecting to the daemon itself needs a blocking metadata read. Bound
// acknowledgement waits so a silent daemon cannot indefinitely prevent
// dashboard exit. tracker is nil for a job that does not hold exit.
func spawnAsyncJob[T any](
	tracker *client.CriticalOperationTracker,
	label string,
	updates *core.Unbounded[dashboardIoUpdate],
	timeout time.Duration,
	work func(ctx context.Context) (T, error),
	toUpdate func(T, error) dashboardIoUpdate,
) *dashboardTask {
	var guard *client.CriticalOperationGuard
	if tracker != nil {
		guard = tracker.Begin(label)
	}
	return spawnDashboardTask(func(ctx context.Context) {
		if guard != nil {
			defer guard.Release()
		}
		// The work's context ends at the deadline, which drops the work as
		// Rust's AbortOnDropHandle does when the timeout fires.
		workCtx, cancelWork := context.WithTimeout(ctx, timeout)
		defer cancelWork()
		worker := core.Spawn(func() blockingOutcome[T] {
			value, err := work(workCtx)
			return blockingOutcome[T]{value: value, err: err}
		})
		var (
			value T
			err   error
		)
		select {
		case <-worker.Done():
			outcome, joinErr := worker.Join(context.Background())
			if joinErr != nil {
				err = fmt.Errorf("%s task failed: %w", label, joinErr)
			} else {
				value, err = outcome.value, outcome.err
			}
		case <-workCtx.Done():
			if ctx.Err() != nil {
				// Aborted: the job answers nothing.
				return
			}
			err = fmt.Errorf(
				"%s: the daemon did not acknowledge the save within %d seconds; it may still complete. Reconnect to verify the saved state",
				label, int64(timeout/time.Second),
			)
		}
		if ctx.Err() != nil {
			// Aborted while the work finished: the job answers nothing.
			return
		}
		if err != nil {
			slog.Error("dashboard save was not confirmed", "operation", label, "error", err.Error())
		}
		report(label, updates, toUpdate(value, err))
	})
}

func spawnCriticalAsync[T any](
	tracker client.CriticalOperationTracker,
	label string,
	updates *core.Unbounded[dashboardIoUpdate],
	timeout time.Duration,
	work func(ctx context.Context) (T, error),
	toUpdate func(T, error) dashboardIoUpdate,
) *dashboardTask {
	return spawnAsyncJob(&tracker, label, updates, timeout, work, toUpdate)
}

func spawnBackgroundAsync[T any](
	label string,
	updates *core.Unbounded[dashboardIoUpdate],
	timeout time.Duration,
	work func(ctx context.Context) (T, error),
	toUpdate func(T, error) dashboardIoUpdate,
) *dashboardTask {
	return spawnAsyncJob(nil, label, updates, timeout, work, toUpdate)
}

// loadWorkspaceManagementEntries loads the manager's complete view from the
// daemon: one listing plus one drafts query. The per-workspace snapshot this
// replaced loaded the whole controller once per workspace for session
// previews the manager never shows.
func loadWorkspaceManagementEntries(ctx context.Context, daemonClient *client.DaemonClient) ([]tui.WorkspaceManagementEntry, error) {
	entries, err := workspaceManagementListings(ctx, daemonClient)
	if err != nil {
		return nil, err
	}
	if err := fillWorkspaceManagementDrafts(ctx, daemonClient, entries); err != nil {
		return nil, err
	}
	return entries, nil
}

// workspaceManagementListings is the workspace list alone: names and
// session counts, no drafts.
func workspaceManagementListings(ctx context.Context, daemonClient *client.DaemonClient) ([]tui.WorkspaceManagementEntry, error) {
	listings, err := daemonClient.ListWorkspaces(ctx)
	if err != nil {
		return nil, err
	}
	entries := make([]tui.WorkspaceManagementEntry, 0, len(listings))
	for _, listing := range listings {
		entries = append(entries, tui.WorkspaceManagementEntry{Workspace: listing.Workspace, Drafts: []tui.WorkspaceDraftEntry{}})
	}
	return entries, nil
}

func fillWorkspaceManagementDrafts(ctx context.Context, daemonClient *client.DaemonClient, entries []tui.WorkspaceManagementEntry) error {
	groups, err := daemonClient.ListWorkspaceDrafts(ctx)
	if err != nil {
		return err
	}
	for _, group := range groups {
		index := slices.IndexFunc(entries, func(entry tui.WorkspaceManagementEntry) bool {
			return entry.Workspace.ID == group.WorkspaceID
		})
		if index < 0 {
			continue
		}
		drafts := make([]tui.WorkspaceDraftEntry, 0, len(group.Drafts))
		for _, draft := range group.Drafts {
			drafts = append(drafts, tui.WorkspaceDraftEntry{
				ID:        draft.ID,
				SessionID: draft.SessionID,
				Source:    draft.Source,
				SavedAt:   draft.SavedAt,
				OwnerPID:  draft.OwnerPid,
			})
		}
		entries[index].Drafts = drafts
	}
	return nil
}

// spawnWorkspaceManagementOperation: tracker is nil for an operation that
// does not hold dashboard exit.
func spawnWorkspaceManagementOperation(
	generation uint64,
	label string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker *client.CriticalOperationTracker,
	work func(ctx context.Context) (workspaceManagementResult, error),
) *dashboardTask {
	toUpdate := func(result workspaceManagementResult, err error) dashboardIoUpdate {
		return dashboardIoUpdateWorkspaceManagement{generation: generation, result: result, err: err}
	}
	if tracker != nil {
		return spawnCriticalAsync(*tracker, label, updates, saveAckTimeout, work, toUpdate)
	}
	return spawnBackgroundAsync(label, updates, saveAckTimeout, work, toUpdate)
}

func spawnWorkspaceManagementLoad(generation uint64, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	namesUpdates := updates
	return spawnWorkspaceManagementOperation(
		generation,
		"loading workspaces",
		updates,
		nil,
		func(ctx context.Context) (workspaceManagementResult, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (workspaceManagementResult, error) {
				entries, err := workspaceManagementListings(ctx, daemonClient)
				if err != nil {
					return workspaceManagementResult{}, err
				}
				// Names first: the dialog opens on this answer while the
				// drafts query runs. The supervised task's own answer carries
				// the details. A closed dashboard has nobody to answer.
				_ = namesUpdates.Send(dashboardIoUpdateWorkspaceManagement{
					generation: generation,
					result: workspaceManagementResult{
						entries:        cloneWorkspaceEntries(entries),
						detailsPending: true,
					},
				})
				if err := fillWorkspaceManagementDrafts(ctx, daemonClient, entries); err != nil {
					return workspaceManagementResult{}, err
				}
				return workspaceManagementResult{entries: entries}, nil
			})
		},
	)
}

// cloneWorkspaceEntries is Rust's `entries.clone()`: the drafts are filled
// into the original afterwards, so the copy must not share its slices.
func cloneWorkspaceEntries(entries []tui.WorkspaceManagementEntry) []tui.WorkspaceManagementEntry {
	cloned := make([]tui.WorkspaceManagementEntry, len(entries))
	for index, entry := range entries {
		cloned[index] = tui.WorkspaceManagementEntry{Workspace: entry.Workspace, Drafts: slices.Clone(entry.Drafts)}
	}
	return cloned
}

// spawnWorkspacePaneSizesLoad reads layouts for workspace ids discovered
// after the dashboard opened. Loading is deliberately separate from the
// runtime snapshot so a late result cannot overwrite a pane size the local
// client already edited.
func spawnWorkspacePaneSizesLoad(workspaceIDs []string, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	return spawnBackgroundAsync(
		"load workspace pane sizes",
		updates,
		saveAckTimeout,
		func(ctx context.Context) (map[string]core.PaneSizes, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (map[string]core.PaneSizes, error) {
				layouts, err := daemonClient.WorkspaceLayouts(ctx, workspaceIDs)
				if err != nil {
					return nil, err
				}
				return maps.Clone(layouts.PaneSizes), nil
			})
		},
		func(sizes map[string]core.PaneSizes, err error) dashboardIoUpdate {
			return dashboardIoUpdateWorkspacePaneSizes{sizes: sizes, err: err}
		},
	)
}

// spawnWorkspaceLayoutsLoad reads conversation layouts for workspace ids
// discovered after the dashboard opened, for the same reason pane sizes are
// read separately.
func spawnWorkspaceLayoutsLoad(workspaceIDs []string, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	return spawnBackgroundAsync(
		"load workspace layouts",
		updates,
		saveAckTimeout,
		func(ctx context.Context) (map[string]core.ConversationLayout, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (map[string]core.ConversationLayout, error) {
				layouts, err := daemonClient.WorkspaceLayouts(ctx, workspaceIDs)
				if err != nil {
					return nil, err
				}
				return maps.Clone(layouts.ConversationLayouts), nil
			})
		},
		func(layouts map[string]core.ConversationLayout, err error) dashboardIoUpdate {
			return dashboardIoUpdateWorkspaceLayouts{layouts: layouts, err: err}
		},
	)
}

func spawnWorkspaceCreate(
	generation uint64,
	name string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	return spawnWorkspaceManagementOperation(
		generation,
		"creating workspace",
		updates,
		&tracker,
		func(ctx context.Context) (workspaceManagementResult, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (workspaceManagementResult, error) {
				workspace, err := daemonClient.CreateWorkspace(ctx, name)
				if err != nil {
					return workspaceManagementResult{}, err
				}
				entries, err := loadWorkspaceManagementEntries(ctx, daemonClient)
				if err != nil {
					return workspaceManagementResult{}, err
				}
				selected := workspace.ID
				return workspaceManagementResult{entries: entries, selectWorkspace: &selected}, nil
			})
		},
	)
}

func spawnWorkspaceRename(
	generation uint64,
	workspaceID string,
	name string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	return spawnWorkspaceManagementOperation(
		generation,
		"renaming workspace",
		updates,
		&tracker,
		func(ctx context.Context) (workspaceManagementResult, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (workspaceManagementResult, error) {
				if err := daemonClient.RenameWorkspace(ctx, workspaceID, name); err != nil {
					return workspaceManagementResult{}, err
				}
				entries, err := loadWorkspaceManagementEntries(ctx, daemonClient)
				if err != nil {
					return workspaceManagementResult{}, err
				}
				return workspaceManagementResult{entries: entries}, nil
			})
		},
	)
}

func spawnWorkspaceCloseCancel(workspaceID string, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	return spawnBackgroundAsync(
		"cancel workspace close",
		updates,
		saveAckTimeout,
		func(ctx context.Context) (struct{}, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (struct{}, error) {
				return unit(daemonClient.CancelWorkspaceClose(ctx, workspaceID))
			})
		},
		func(_ struct{}, err error) dashboardIoUpdate {
			return dashboardIoUpdateWorkspaceCloseCancelled{err: err}
		},
	)
}

// spawnWorkspaceClose: the daemon owns stopping. Detaching the UI must not
// wait for checkpoints, and ordinary save acknowledgement deadlines are too
// short for them.
func spawnWorkspaceClose(generation uint64, workspaceID string, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	return spawnDashboardTask(func(ctx context.Context) {
		closingWorkspaceID := workspaceID
		// The close is its own task: aborting this one stops only the
		// report, as dropping a Rust JoinHandle does not abort its task.
		work := core.Spawn(func() blockingOutcome[struct{}] {
			closeCtx := context.Background()
			_, err := withDaemon(closeCtx, func(daemonClient *client.DaemonClient) (struct{}, error) {
				return unit(daemonClient.CloseWorkspace(closeCtx, workspaceID))
			})
			return blockingOutcome[struct{}]{err: err}
		})
		outcome, joinErr := work.Join(ctx)
		if ctx.Err() != nil {
			return
		}
		_, err := blockingResult("closing workspace", struct{}{}, outcome.err, joinErr)
		report[dashboardIoUpdate]("closing workspace", updates, dashboardIoUpdateWorkspaceClosed{
			generation:  generation,
			workspaceID: closingWorkspaceID,
			err:         err,
		})
	})
}

func spawnWorkspaceDraftRecovery(
	generation uint64,
	draftID string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	return spawnWorkspaceManagementOperation(
		generation,
		"recovering workspace draft",
		updates,
		&tracker,
		func(ctx context.Context) (workspaceManagementResult, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (workspaceManagementResult, error) {
				if err := daemonClient.RecoverDraft(ctx, draftID); err != nil {
					return workspaceManagementResult{}, err
				}
				entries, err := loadWorkspaceManagementEntries(ctx, daemonClient)
				if err != nil {
					return workspaceManagementResult{}, err
				}
				return workspaceManagementResult{entries: entries}, nil
			})
		},
	)
}

const saveAckTimeout = 15 * time.Second

// spawnCancellableIO is like spawnCriticalIO, with a cooperative
// cancellation flag for work that can own a subprocess while the dashboard
// is shutting down.
func spawnCancellableIO[T any](
	tracker client.CriticalOperationTracker,
	label string,
	updates *core.Unbounded[dashboardIoUpdate],
	work func(cancelled *atomic.Bool) (T, error),
	toUpdate func(T, error) dashboardIoUpdate,
) *dashboardTask {
	_, task := spawnCancellableIOWithToken(tracker, label, updates, work, toUpdate)
	return task
}

func spawnCancellableIOWithToken[T any](
	tracker client.CriticalOperationTracker,
	label string,
	updates *core.Unbounded[dashboardIoUpdate],
	work func(cancelled *atomic.Bool) (T, error),
	toUpdate func(T, error) dashboardIoUpdate,
) (*atomic.Bool, *dashboardTask) {
	cancelled := new(atomic.Bool)
	guard := tracker.BeginCancellable(label, cancelled)
	worker := spawnDashboardTask(func(ctx context.Context) {
		defer guard.Release()
		value, err, joinErr, ok := awaitBlocking(ctx, func() (T, error) { return work(cancelled) })
		if !ok {
			return
		}
		value, err = blockingResult(label, value, err, joinErr)
		report(label, updates, toUpdate(value, err))
	})
	return cancelled, worker
}

// spawnFirstRunSetup discovers and saves first-run configuration without
// blocking terminal input.
func spawnFirstRunSetup(updates *core.Unbounded[dashboardIoUpdate], tracker client.CriticalOperationTracker) *dashboardTask {
	progress := updates
	return spawnCancellableIO(
		tracker,
		"first-run setup",
		updates,
		func(_ *atomic.Bool) (*[]string, error) {
			// The daemon owns its configuration and its environment. In
			// container placement this discovery runs inside the container,
			// not against the client's host home.
			return blockOnResult(func(ctx context.Context) (*[]string, error) {
				return withDaemon(ctx, func(daemonClient *client.DaemonClient) (*[]string, error) {
					// The welcome takes the keyboard, so it opens only once the
					// daemon has said this installation needs setup. An
					// existing installation picks up agents installed since
					// its last launch through the startup availability check.
					due, err := daemonClient.FirstRunSetupDue(ctx)
					if err != nil {
						return nil, err
					}
					if !due {
						return nil, nil
					}
					report[dashboardIoUpdate]("first-run setup", progress, dashboardIoUpdateFirstRunStarted{})
					// Setup decides again under its lock: another dashboard
					// may have finished it since the daemon answered.
					identity, err := daemonClient.Identity(ctx)
					if err != nil {
						return nil, err
					}
					var projectDirectory *string
					if daemonClient.SharesClientFilesystem(identity) {
						if directory, err := os.Getwd(); err == nil {
							projectDirectory = &directory
						}
					}
					summary, err := daemonClient.RunFirstRunSetup(ctx, projectDirectory)
					if err != nil {
						return nil, err
					}
					if summary == nil {
						return nil, nil
					}
					report[dashboardIoUpdate]("first-run setup", progress, dashboardIoUpdateFirstRunConfigured{report: *summary})
					// Setup is saved by now. A daemon handoff abandons the
					// check, so its failure is reported as a check, not as
					// failed setup.
					problems, err := daemonClient.RunDoctor(ctx)
					if err != nil {
						problems = []string{fmt.Sprintf("Could not check prerequisites: %s. Run `mj doctor` to check them.", err.Error())}
					}
					return &problems, nil
				})
			})
		},
		func(problems *[]string, err error) dashboardIoUpdate {
			update := dashboardIoUpdateFirstRunChecked{err: err}
			if problems != nil {
				update.checked = true
				update.problems = *problems
			}
			return update
		},
	)
}

// spawnAvailabilityCheck asks the daemon to check launch availability.
// Results arrive on the feed.
func spawnAvailabilityCheck(updates *core.Unbounded[dashboardIoUpdate]) {
	spawnBackgroundAsync(
		"asking the daemon to check availability",
		updates,
		saveAckTimeout,
		func(ctx context.Context) (struct{}, error) {
			return withExistingDaemon(ctx, func(daemonClient *client.DaemonClient) (struct{}, error) {
				reply, err := daemonClient.Request(ctx, client.DaemonActionCheckAvailability{})
				if err != nil {
					return struct{}{}, err
				}
				if _, done := reply.(client.DaemonReplyDone); !done {
					return struct{}{}, errors.New("daemon returned an unexpected availability acknowledgement")
				}
				return struct{}{}, nil
			})
		},
		func(_ struct{}, err error) dashboardIoUpdate {
			return dashboardIoUpdateAvailabilityCheckRequested{err: err}
		},
	)
}

// spawnProfileLoginStatuses reads saved-profile sign-in status off the
// dashboard's loop.
func spawnProfileLoginStatuses(generation uint64, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	return spawnIO(
		"reading profile sign-in status",
		updates,
		func() (map[string]core.LoginStatus, error) {
			reply, err := machineRequest(client.DaemonActionProfileLoginStatuses{}, new(atomic.Bool))
			if err != nil {
				return nil, err
			}
			statuses, ok := reply.(client.DaemonReplyProfileLoginStatuses)
			if !ok {
				return nil, errors.New("daemon returned an unexpected profile login status reply")
			}
			return statuses.Statuses, nil
		},
		func(statuses map[string]core.LoginStatus, err error) dashboardIoUpdate {
			return dashboardIoUpdateProfileLoginStatuses{generation: generation, statuses: statuses, err: err}
		},
	)
}

func spawnSetupDiscovery(
	generation uint64,
	scope tui.DetectScope,
	machines []client.DaemonPair[string, core.Machine],
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	label := "detecting runtimes"
	if scope == tui.DetectScopeProfiles {
		label = "detecting agent profiles"
	}
	return spawnCancellableIO(
		tracker,
		label,
		updates,
		func(cancelled *atomic.Bool) (tui.SetupDetection, error) {
			reply, err := machineRequest(client.DaemonActionDiscoverSetup{Scope: scope, Machines: machines}, cancelled)
			if err != nil {
				return tui.SetupDetection{}, err
			}
			discovery, ok := reply.(client.DaemonReplySetupDiscovery)
			if !ok {
				return tui.SetupDetection{}, errors.New("daemon returned an unexpected discovery reply")
			}
			return discovery.Detection, nil
		},
		func(detection tui.SetupDetection, err error) dashboardIoUpdate {
			return dashboardIoUpdateSetupDiscovered{generation: generation, detection: detection, err: err}
		},
	)
}

func spawnSetupSave(
	generation uint64,
	original string,
	updated string,
	loginProfile *string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	return spawnCriticalAsync(
		tracker,
		"saving setup",
		updates,
		saveAckTimeout,
		func(ctx context.Context) (core.Config, error) {
			// The daemon checks the edit against the stored sessions it
			// would disturb; only it opens the store.
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (core.Config, error) {
				return daemonClient.SaveSetup(ctx, original, updated)
			})
		},
		func(config core.Config, err error) dashboardIoUpdate {
			return dashboardIoUpdateSetupSaved{generation: generation, config: config, err: err, loginProfile: loginProfile}
		},
	)
}

func spawnQuotaPreferencesSave(
	prefer []string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	return spawnCriticalAsync(
		tracker,
		"saving load-balancing preferences",
		updates,
		saveAckTimeout,
		func(ctx context.Context) (struct{}, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (struct{}, error) {
				return unit(daemonClient.SetQuotaPreferences(ctx, prefer))
			})
		},
		func(_ struct{}, err error) dashboardIoUpdate {
			return dashboardIoUpdateQuotaPreferencesSaved{err: err}
		},
	)
}

func spawnSpinnerStyleSave(
	style core.SpinnerStyle,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	return spawnCriticalIO(
		tracker,
		"saving spinner style",
		updates,
		func() (core.Config, error) {
			return blockOnResult(func(ctx context.Context) (core.Config, error) {
				return withDaemon(ctx, func(daemonClient *client.DaemonClient) (core.Config, error) {
					reply, err := daemonClient.Request(ctx, client.DaemonActionSaveSpinnerStyle{Style: style})
					if err != nil {
						return core.Config{}, err
					}
					if _, done := reply.(client.DaemonReplyDone); !done {
						return core.Config{}, fmt.Errorf("unexpected spinner save reply: %s", client.DaemonReplyDebug(reply))
					}
					reply, err = daemonClient.Request(ctx, client.DaemonActionConfiguration{})
					if err != nil {
						return core.Config{}, err
					}
					configuration, ok := reply.(client.DaemonReplyConfiguration)
					if !ok {
						return core.Config{}, fmt.Errorf("unexpected configuration reply: %s", client.DaemonReplyDebug(reply))
					}
					return configuration.Config, nil
				})
			})
		},
		func(config core.Config, err error) dashboardIoUpdate {
			return dashboardIoUpdateSpinnerStyleSaved{config: config, err: err}
		},
	)
}

// spawnProjectSourceResolution resolves one raw checkout's Git origin off
// the event loop. Each session is independent, so callers can launch these
// concurrently and redraw as the answers arrive.
func spawnProjectSourceResolution(
	sessionID string,
	sessionName string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	reportedSessionID := sessionID
	return spawnCancellableIO(
		tracker,
		fmt.Sprintf("resolving project for %s", sessionName),
		updates,
		func(_ *atomic.Bool) (core.ProjectSourceIdentity, error) {
			return blockOnResult(func(ctx context.Context) (core.ProjectSourceIdentity, error) {
				return withDaemon(ctx, func(daemonClient *client.DaemonClient) (core.ProjectSourceIdentity, error) {
					return daemonClient.SessionProjectSource(ctx, sessionID)
				})
			})
		},
		func(source core.ProjectSourceIdentity, err error) dashboardIoUpdate {
			return dashboardIoUpdateProjectSource{sessionID: reportedSessionID, source: source, err: err}
		},
	)
}

// lifecycleOperationRequest is what every session lifecycle operation needs
// to run off the loop.
type lifecycleOperationRequest struct {
	sessionID string
	// sessionName is the session's name as notices give it, for the
	// operation's label.
	sessionName string
	kind        tui.SessionOperationKind
	cancelled   *atomic.Bool
	updates     *core.Unbounded[dashboardLifecycleUpdate]
}

// lifecycleSubmission keeps client ownership until the daemon confirms
// lifecycle admission. Rust shares an `Arc<Mutex<Option<guard>>>` that the
// first release takes; the copies here share one guard, whose Release is
// idempotent, so the first release is the one that counts.
type lifecycleSubmission struct {
	guard *client.CriticalOperationGuard
}

func (s lifecycleSubmission) submit(
	ctx context.Context,
	daemonClient *client.DaemonClient,
	request client.SessionLifecycleRequest,
) (client.LifecycleReceipt, error) {
	receipt, err := daemonClient.StartSessionLifecycle(ctx, request)
	if err != nil {
		return client.LifecycleReceipt{}, err
	}
	s.release()
	return receipt, nil
}

func (s lifecycleSubmission) release() {
	s.guard.Release()
}

// spawnLifecycleOperation protects submission, then follows daemon-owned
// work without holding quit.
func spawnLifecycleOperation(
	request lifecycleOperationRequest,
	tracker client.CriticalOperationTracker,
	work func(cancelled *atomic.Bool, submission lifecycleSubmission) (daemon.LifecycleSuccess, error),
) {
	submission := lifecycleSubmission{guard: tracker.Begin(fmt.Sprintf(
		"submitting %s for session %s",
		strings.ToLower(request.kind.Label()), request.sessionName,
	))}
	core.Spawn(func() struct{} {
		// Failed or panicking submission must report its uncertainty before
		// quit is released; successful submission releases at the receipt.
		workerSubmission := submission
		joined := core.SpawnBlocking(func() blockingOutcome[daemon.LifecycleSuccess] {
			success, err := work(request.cancelled, workerSubmission)
			return blockingOutcome[daemon.LifecycleSuccess]{value: success, err: err}
		})
		outcome, joinErr := joined.Join(context.Background())
		update := dashboardLifecycleUpdate{sessionID: request.sessionID, operation: request.cancelled}
		switch {
		case joinErr != nil:
			update.failure = &dashboardLifecycleFailure{
				kind:    dashboardLifecycleFailed,
				message: fmt.Sprintf("session lifecycle operation task failed: %s", joinErr.Error()),
			}
		case outcome.err != nil:
			failure := dashboardLifecycleFailureFrom(outcome.err)
			update.failure = &failure
		default:
			update.success = outcome.value
		}
		report("session lifecycle operation", request.updates, update)
		submission.release()
		return struct{}{}
	})
}

// spawnMaterializedSessionProjection: permits is the projection slots the
// dashboard shares (Rust: Arc<Semaphore>).
func spawnMaterializedSessionProjection(
	projection materializedProjection,
	viewedThroughEventOrdinal uint64,
	previous tui.MaterializedProjectionCache,
	updates *core.Unbounded[dashboardIoUpdate],
	permits chan struct{},
) {
	sessionID := projection.materialized.SessionID
	core.Spawn(func() struct{} {
		// The dashboard never closes its permits, so Rust's "worker
		// stopped" answer for a closed semaphore has no Go counterpart.
		permits <- struct{}{}
		joined := core.SpawnBlocking(func() tui.PreparedMaterializedSessionDetail {
			return tui.PreparedMaterializedSessionDetailFromAssessedMaterialized(
				projection.materialized,
				projection.assessment,
				viewedThroughEventOrdinal,
				previous,
			)
		})
		detail, joinErr := joined.Join(context.Background())
		<-permits
		update := dashboardIoUpdateMaterializedSessionProjection{sessionID: sessionID}
		if joinErr != nil {
			update.err = fmt.Errorf("session projection task failed: %w", joinErr)
		} else {
			update.detail = &detail
		}
		report[dashboardIoUpdate]("session projection", updates, update)
		return struct{}{}
	})
}

// spawnStoppedSubagentTranscript loads the stored tail of a stopped
// sub-agent's conversation: as much as a live chat keeps, off the event
// loop.
func spawnStoppedSubagentTranscript(sessionID string, updates *core.Unbounded[dashboardIoUpdate]) {
	reportedSessionID := sessionID
	spawnBackgroundAsync(
		"load a stopped sub-agent's transcript",
		updates,
		saveAckTimeout,
		func(ctx context.Context) (*core.MaterializedSession, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (*core.MaterializedSession, error) {
				return daemonClient.StoredProjectionTail(ctx, sessionID, chat.TailSeedItems)
			})
		},
		func(session *core.MaterializedSession, err error) dashboardIoUpdate {
			return dashboardIoUpdateStoppedSubagentTranscript{sessionID: reportedSessionID, session: session, err: err}
		},
	)
}

// superviseWithPermit holds one of permits while a background job runs,
// the way Rust's summary readers hold an owned permit around an
// AbortOnDropHandle. Aborting the supervisor aborts the job; a job that died
// is logged as the supervisor's failure.
func superviseWithPermit(permits chan struct{}, failure string, start func() *dashboardTask) *dashboardTask {
	return spawnDashboardTask(func(ctx context.Context) {
		select {
		case permits <- struct{}{}:
		case <-ctx.Done():
			return
		}
		defer func() { <-permits }()
		task := start()
		if err := task.join(ctx); err != nil {
			task.abort()
			if ctx.Err() == nil {
				slog.Error(failure, "error", err.Error())
			}
		}
	})
}

func spawnNotificationMessageRead(
	request tui.NotificationReadRequest,
	generation uint64,
	permits chan struct{},
	updates *core.Unbounded[dashboardIoUpdate],
) *dashboardTask {
	sessionID := request.SessionID
	source := request.Source
	return superviseWithPermit(permits, "notification read supervisor failed", func() *dashboardTask {
		return spawnBackgroundAsync(
			"read notification message",
			updates,
			saveAckTimeout,
			func(ctx context.Context) (*string, error) {
				return withDaemon(ctx, func(daemonClient *client.DaemonClient) (*string, error) {
					var (
						item client.StoredTranscriptItem
						err  error
					)
					switch source := source.(type) {
					case tui.NotificationReadSourceItem:
						item, err = daemonClient.StoredTranscriptItem(ctx, sessionID, source.Value)
					case tui.NotificationReadSourceThrough:
						item, err = daemonClient.StoredAgentMessageThrough(ctx, sessionID, source.Value)
					}
					if err != nil {
						return nil, err
					}
					found, ok := item.(client.StoredTranscriptItemFound)
					if !ok {
						return nil, nil
					}
					text, ok := core.MaterializedAgentMessageText([]core.TranscriptItem{*found.Item}, found.Item.StableID)
					if !ok {
						return nil, nil
					}
					return &text, nil
				})
			},
			func(text *string, err error) dashboardIoUpdate {
				return dashboardIoUpdateNotificationMessageRead{request: request, generation: generation, text: text, err: err}
			},
		)
	})
}

func spawnStoredSessionSummary(
	sessionID string,
	viewedThroughEventOrdinal uint64,
	generation uint64,
	permits chan struct{},
	updates *core.Unbounded[dashboardIoUpdate],
) *dashboardTask {
	reportedSessionID := sessionID
	return superviseWithPermit(permits, "stored summary supervisor failed", func() *dashboardTask {
		return spawnBackgroundAsync(
			"load stored session summary",
			updates,
			saveAckTimeout,
			func(ctx context.Context) (tui.PreparedMaterializedSessionSummary, error) {
				summary, err := withDaemon(ctx, func(daemonClient *client.DaemonClient) (*core.MaterializedSessionSummary, error) {
					return daemonClient.StoredSessionSummary(ctx, sessionID)
				})
				if err != nil {
					return tui.PreparedMaterializedSessionSummary{}, err
				}
				if summary == nil {
					return tui.PreparedMaterializedSessionSummary{}, fmt.Errorf("session %s has no stored projection", sessionID)
				}
				// Only CPU preparation needs a blocking thread. Acquiring one
				// while awaiting the connection can starve its metadata read.
				prepared, joinErr := core.SpawnBlocking(func() tui.PreparedMaterializedSessionSummary {
					return tui.PreparedMaterializedSessionSummaryFromMaterialized(*summary, viewedThroughEventOrdinal)
				}).Join(ctx)
				if joinErr != nil {
					return tui.PreparedMaterializedSessionSummary{}, fmt.Errorf("prepare stored session summary task failed: %w", joinErr)
				}
				return prepared, nil
			},
			func(summary tui.PreparedMaterializedSessionSummary, err error) dashboardIoUpdate {
				return dashboardIoUpdateStoredSessionSummary{sessionID: reportedSessionID, generation: generation, summary: summary, err: err}
			},
		)
	})
}

func spawnLifecycleReload(reload lifecycleReload, updates *core.Unbounded[dashboardIoUpdate]) {
	spawnBackgroundAsync(
		"reload lifecycle state",
		updates,
		saveAckTimeout,
		storedSessionMetadata,
		func(metadata dashboardMetadata, err error) dashboardIoUpdate {
			return dashboardIoUpdateLifecycleReloaded{reloaded: &lifecycleReloaded{reload: reload, metadata: metadata, err: err}}
		},
	)
}

func spawnDashboardRename(
	sessionID string,
	sessionName string,
	title string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	renamedSessionID := sessionID
	requestedTitle := title
	spawnCriticalAsync(
		tracker,
		fmt.Sprintf("renaming session %s", sessionName),
		updates,
		saveAckTimeout,
		func(ctx context.Context) (string, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (string, error) {
				return daemonClient.SetSessionTitle(ctx, renamedSessionID, requestedTitle)
			})
		},
		func(saved string, err error) dashboardIoUpdate {
			return dashboardIoUpdateRenameSession{title: title, saved: saved, err: err}
		},
	)
}

func spawnDashboardNotificationsIgnored(
	sessionID string,
	sessionName string,
	ignored bool,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	spawnCriticalAsync(
		tracker,
		fmt.Sprintf("changing notification setting for %s", sessionName),
		updates,
		saveAckTimeout,
		func(ctx context.Context) (struct{}, error) {
			return withExistingDaemon(ctx, func(daemonClient *client.DaemonClient) (struct{}, error) {
				return unit(daemonClient.SetSessionNotificationsIgnored(ctx, sessionID, ignored))
			})
		},
		func(_ struct{}, err error) dashboardIoUpdate {
			return dashboardIoUpdateNotificationsIgnored{ignored: ignored, err: err}
		},
	)
}

func spawnDashboardChangeWorkspace(
	sessionID string,
	sessionName string,
	workspaceID string,
	workspaceName string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	movedSessionID := sessionID
	spawnCriticalAsync(
		tracker,
		fmt.Sprintf("moving session %s to a workspace", sessionName),
		updates,
		saveAckTimeout,
		func(ctx context.Context) (struct{}, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (struct{}, error) {
				return unit(daemonClient.SetSessionWorkspace(ctx, movedSessionID, workspaceID))
			})
		},
		func(_ struct{}, err error) dashboardIoUpdate {
			return dashboardIoUpdateChangeWorkspace{sessionID: sessionID, workspaceName: workspaceName, err: err}
		},
	)
}

type configRenameRequest struct {
	what    string
	oldID   string
	newID   string
	profile bool
}

func spawnConfigRename(
	request configRenameRequest,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	spawnCriticalAsync(
		tracker,
		fmt.Sprintf("renaming %s", request.what),
		updates,
		saveAckTimeout,
		func(ctx context.Context) (dashboardMetadata, error) {
			return withExistingDaemon(ctx, func(daemonClient *client.DaemonClient) (dashboardMetadata, error) {
				var err error
				if request.profile {
					err = daemonClient.RenameProfile(ctx, request.oldID, request.newID)
				} else {
					err = daemonClient.RenameTarget(ctx, request.oldID, request.newID)
				}
				if err != nil {
					return dashboardMetadata{}, err
				}
				stored, err := daemonClient.StoredSessionMetadata(ctx)
				if err != nil {
					return dashboardMetadata{}, err
				}
				return dashboardMetadataFrom(stored), nil
			})
		},
		func(metadata dashboardMetadata, err error) dashboardIoUpdate {
			return dashboardIoUpdateConfigRename{what: request.what, metadata: metadata, err: err}
		},
	)
}

// spawnQuotaRefreshRequest asks the daemon, the only process that probes
// quota, to probe now. The request returns once the daemon has accepted it;
// the reports arrive through the runtime feed.
func spawnQuotaRefreshRequest(updates *core.Unbounded[dashboardIoUpdate]) {
	spawnBackgroundAsync(
		"asking the daemon to refresh quota",
		updates,
		saveAckTimeout,
		func(ctx context.Context) (struct{}, error) {
			return withExistingDaemon(ctx, func(daemonClient *client.DaemonClient) (struct{}, error) {
				return unit(daemonClient.RefreshQuota(ctx))
			})
		},
		func(_ struct{}, err error) dashboardIoUpdate {
			return dashboardIoUpdateQuotaRefreshRequested{err: err}
		},
	)
}

func spawnCapacityRefreshRequest(updates *core.Unbounded[dashboardIoUpdate]) {
	spawnBackgroundAsync(
		"asking the daemon to refresh capacity",
		updates,
		saveAckTimeout,
		func(ctx context.Context) (struct{}, error) {
			return withExistingDaemon(ctx, func(daemonClient *client.DaemonClient) (struct{}, error) {
				return unit(daemonClient.RefreshCapacity(ctx))
			})
		},
		func(_ struct{}, err error) dashboardIoUpdate {
			return dashboardIoUpdateCapacityRefreshRequested{err: err}
		},
	)
}

// spawnQuotaProbeRequest asks the daemon to probe one profile's quota now,
// even a disabled profile's. The answer arrives through the runtime feed
// like any other.
func spawnQuotaProbeRequest(profileID string, updates *core.Unbounded[dashboardIoUpdate]) {
	completion := profileID
	spawnBackgroundAsync(
		fmt.Sprintf("asking the daemon to refresh %s's quota", profileID),
		updates,
		saveAckTimeout,
		func(ctx context.Context) (struct{}, error) {
			return withExistingDaemon(ctx, func(daemonClient *client.DaemonClient) (struct{}, error) {
				return unit(daemonClient.ProbeQuotaProfiles(ctx, []string{profileID}, true))
			})
		},
		func(_ struct{}, err error) dashboardIoUpdate {
			return dashboardIoUpdateQuotaProbeRequested{profileID: completion, err: err}
		},
	)
}

// containerSettingsRequest is what the container editor asks the
// controller to persist.
type containerSettingsRequest struct {
	sessionID          string
	resourceAllocation *core.SessionResourceAllocation
	additionalMounts   []core.AdditionalMount
	mountHistory       []string
}

// containerSettingsOutcome is Rust's inner
// `Result<DashboardMetadata, ContainerUpgradePreview>`: the saved metadata,
// or the upgrade the daemon wants confirmed first.
type containerSettingsOutcome struct {
	metadata       dashboardMetadata
	upgradePreview *core.ContainerUpgradePreview
}

func spawnDashboardContainerSettings(
	request containerSettingsRequest,
	sessionName string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	sessionID := request.sessionID
	spawnCriticalAsync(
		tracker,
		fmt.Sprintf("saving container settings for %s", sessionName),
		updates,
		saveAckTimeout,
		func(ctx context.Context) (containerSettingsOutcome, error) {
			var allocation core.SessionResourceAllocation
			if request.resourceAllocation != nil {
				allocation = *request.resourceAllocation
			}
			preview, err := withDaemon(ctx, func(daemonClient *client.DaemonClient) (*core.ContainerUpgradePreview, error) {
				return daemonClient.SetSessionContainerSettings(ctx, request.sessionID, allocation, request.additionalMounts, request.mountHistory)
			})
			if err != nil {
				return containerSettingsOutcome{}, err
			}
			if preview != nil {
				return containerSettingsOutcome{upgradePreview: preview}, nil
			}
			// Return a fresh durable snapshot so the dashboard can update
			// its state without synchronously reloading the database while
			// it is applying the worker result.
			metadata, err := storedSessionMetadata(ctx)
			if err != nil {
				return containerSettingsOutcome{}, err
			}
			return containerSettingsOutcome{metadata: metadata}, nil
		},
		func(outcome containerSettingsOutcome, err error) dashboardIoUpdate {
			return dashboardIoUpdateContainerSettings{
				sessionID:      sessionID,
				metadata:       outcome.metadata,
				upgradePreview: outcome.upgradePreview,
				err:            err,
			}
		},
	)
}

// namedSession is a session by its id and by the name notices give it, for
// background jobs whose label names the session they work on.
type namedSession struct {
	id   string
	name string
}

// spawnTerminalDraftPersist is one revisioned owner write; the cache
// sequences and coalesces each session.
func spawnTerminalDraftPersist(
	clientID string,
	session namedSession,
	revision uint64,
	draft core.DetachedSessionDraft,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	sessionID := session.id
	persistedSessionID := sessionID
	return spawnCriticalAsync(
		tracker,
		fmt.Sprintf("saving draft for %s", session.name),
		updates,
		saveAckTimeout,
		func(ctx context.Context) (struct{}, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (struct{}, error) {
				return unit(daemonClient.PersistTerminalDraft(ctx, clientID, persistedSessionID, uint32(os.Getpid()), revision, draft))
			})
		},
		func(_ struct{}, err error) dashboardIoUpdate {
			return dashboardIoUpdateTerminalDraft{sessionID: sessionID, revision: revision, err: err}
		},
	)
}

func spawnReadReceiptPersist(
	clientID string,
	workspaceID string,
	session namedSession,
	through uint64,
	retryDelay time.Duration,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	sessionID := session.id
	persistedSessionID := sessionID
	spawnCriticalAsync(
		tracker,
		fmt.Sprintf("saving read status for %s", session.name),
		updates,
		saveAckTimeout,
		func(ctx context.Context) (uint64, error) {
			// Give projection persistence or a reconnect time to catch up.
			delay := time.NewTimer(retryDelay)
			select {
			case <-delay.C:
			case <-ctx.Done():
				delay.Stop()
				return 0, ctx.Err()
			}
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (uint64, error) {
				return daemonClient.PersistReadReceipt(ctx, clientID, workspaceID, persistedSessionID, through)
			})
		},
		func(saved uint64, err error) dashboardIoUpdate {
			return dashboardIoUpdateReadReceipt{sessionID: sessionID, through: saved, err: err}
		},
	)
}

func spawnClipboardRead(target tui.DashboardPasteTarget, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	return spawnIO(
		"read clipboard",
		updates,
		chat.ReadText,
		func(text string, err error) dashboardIoUpdate {
			return dashboardIoUpdateClipboardText{target: target, text: text, err: err}
		},
	)
}

// spawnClipboardWrite writes copied text to the desktop clipboard off the
// render loop.
func spawnClipboardWrite(text string, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	return spawnIO(
		"write clipboard",
		updates,
		func() (struct{}, error) { return unit(chat.WriteText(text)) },
		func(_ struct{}, err error) dashboardIoUpdate {
			return dashboardIoUpdateClipboardWritten{err: err}
		},
	)
}

func spawnCreateBundle(
	sources []string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	spawnAsyncJob(
		&tracker,
		"creating project",
		updates,
		60*time.Second,
		func(ctx context.Context) (createdBundleUpdate, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (createdBundleUpdate, error) {
				created, err := daemonClient.CreateProject(ctx, sources)
				if err != nil {
					return createdBundleUpdate{}, err
				}
				return createdBundleUpdate{bundle: created.Project.Bundle, bundleID: created.BundleID}, nil
			})
		},
		func(created createdBundleUpdate, err error) dashboardIoUpdate {
			return dashboardIoUpdateCreatedBundle{created: created, err: err}
		},
	)
}

func spawnRemoveBundle(
	bundleID string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	removed := bundleID
	spawnCriticalAsync(
		tracker,
		"removing project",
		updates,
		60*time.Second,
		func(ctx context.Context) (struct{}, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (struct{}, error) {
				return unit(daemonClient.RemoveProject(ctx, bundleID))
			})
		},
		func(_ struct{}, err error) dashboardIoUpdate {
			return dashboardIoUpdateRemovedBundle{bundleID: removed, err: err}
		},
	)
}

func spawnImportedSessionApply(
	imported dashboardImportSuccess,
	pending pendingDashboardImport,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	spawnCriticalAsync(
		tracker,
		"saving imported session",
		updates,
		saveAckTimeout,
		func(ctx context.Context) (importedDashboardSessionApply, error) {
			// The staged archive lives until this save finishes, as Rust's
			// TempDir does inside the job.
			if imported.archiveDirectory != "" {
				defer func() {
					if err := os.RemoveAll(imported.archiveDirectory); err != nil {
						slog.Warn("could not remove the imported session's staged archive", "path", imported.archiveDirectory, "error", err.Error())
					}
				}()
			}
			published, err := withDaemon(ctx, func(daemonClient *client.DaemonClient) (*client.SessionRecordWithVerdict, error) {
				return daemonClient.SessionRecordWithVerdict(ctx, imported.sessionID)
			})
			if err != nil {
				return importedDashboardSessionApply{}, err
			}
			if published == nil {
				return importedDashboardSessionApply{}, errors.New("imported session disappeared")
			}
			bundle, ok := imported.controller.Config.Bundles[published.Record.BundleID]
			if !ok {
				return importedDashboardSessionApply{}, errors.New("import worker did not return its session bundle")
			}
			return importedDashboardSessionApply{
				harness:         imported.harness,
				nativeSessionID: pending.nativeSessionID,
				session:         published.Record,
				verdict:         published.Verdict,
				bundle:          bundle,
			}, nil
		},
		func(applied importedDashboardSessionApply, err error) dashboardIoUpdate {
			return dashboardIoUpdateImportedSessionApplied{applied: applied, err: err}
		},
	)
}

// spawnResumeCandidates loads the stopped sessions the resume dialog lists.
// The runtime feed carries only live sessions.
func spawnResumeCandidates(discoveryID uint64, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	return spawnBackgroundAsync(
		"load suspended sessions",
		updates,
		saveAckTimeout,
		func(ctx context.Context) (client.ResumeCandidates, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (client.ResumeCandidates, error) {
				return daemonClient.ResumeCandidates(ctx)
			})
		},
		func(candidates client.ResumeCandidates, err error) dashboardIoUpdate {
			return dashboardIoUpdateResumeCandidates{discoveryID: discoveryID, candidates: candidates, err: err}
		},
	)
}

// spawnResumeRecord fetches the whole record of the session picked in the
// resume dialog.
func spawnResumeRecord(sessionID string, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	requested := sessionID
	return spawnBackgroundAsync(
		"load session to resume",
		updates,
		saveAckTimeout,
		func(ctx context.Context) (*client.SessionRecordWithVerdict, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (*client.SessionRecordWithVerdict, error) {
				return daemonClient.SessionRecordWithVerdict(ctx, requested)
			})
		},
		func(record *client.SessionRecordWithVerdict, err error) dashboardIoUpdate {
			return dashboardIoUpdateResumeRecord{sessionID: sessionID, record: record, err: err}
		},
	)
}

func spawnCheckpointArchiveSizeRefresh(
	generation uint64,
	targets map[string]string,
	updates *core.Unbounded[dashboardIoUpdate],
) {
	core.SpawnBlocking(func() struct{} {
		var sizes map[string]client.DaemonResult[*uint64]
		reply, err := machineRequest(client.DaemonActionCheckpointArchiveSizes{Paths: maps.Clone(targets)}, new(atomic.Bool))
		if answered, ok := reply.(client.DaemonReplyCheckpointArchiveSizes); err == nil && ok {
			sizes = answered.Sizes
		} else {
			failure := ""
			if err != nil {
				failure = err.Error()
			} else {
				failure = fmt.Sprintf("unexpected archive size reply: %s", client.DaemonReplyDebug(reply))
			}
			sizes = make(map[string]client.DaemonResult[*uint64], len(targets))
			for id := range targets {
				sizes[id] = client.DaemonErr[*uint64](failure)
			}
		}
		report[dashboardIoUpdate]("checkpoint archive sizes", updates, dashboardIoUpdateCheckpointArchiveSizes{
			generation: generation,
			targets:    targets,
			sizes:      sizes,
		})
		return struct{}{}
	})
}

// goRecipeSave is the fast-start setup a new session saves before it
// registers: Rust's `Option<(PathBuf, GoRecipe, bool)>` argument of
// spawn_dashboard_create_session (directory, recipe, global).
type goRecipeSave struct {
	directory string
	recipe    core.GoRecipe
	global    bool
}

// spawnDashboardCreateSession: registering a session and provisioning it
// are one job with two answers: the dashboard shows the session as soon as
// it exists, then follows the launch, so this stays separate from
// spawnLifecycleOperation. Only the registration holds dashboard exit: once
// the daemon has the record, the launch is the daemon's. goSave is nil when
// no fast-start setup is saved.
func spawnDashboardCreateSession(
	action tui.DashboardAction,
	goSave *goRecipeSave,
	updates *core.Unbounded[dashboardIoUpdate],
	lifecycleUpdates *core.Unbounded[dashboardLifecycleUpdate],
	tracker client.CriticalOperationTracker,
) {
	cancelled := new(atomic.Bool)
	guard := tracker.BeginCancellable("creating session", cancelled)
	core.SpawnBlocking(func() struct{} {
		retryLaunch := action
		launch, ok := action.(tui.DashboardActionCreateSession)
		if !ok {
			// Rust's let-else returns here and drops the guard.
			guard.Release()
			return struct{}{}
		}
		registered, err := func() (*registeredDashboardSession, error) {
			if goSave != nil {
				if err := core.GoPreferencesSaveRecipe(core.GoPreferencesPath(), goSave.directory, goSave.recipe, goSave.global); err != nil {
					return nil, fmt.Errorf("save fast-start setup; session has not been started: %w", err)
				}
			}
			// The new-session preflight reads configuration and runs git in
			// the environment that owns the project paths. A client in
			// another placement cannot do that itself.
			if launch.ProjectDirectory == nil {
				repairs, err := blockOnResult(func(ctx context.Context) ([]core.LocalRemoteRepair, error) {
					return withDaemon(ctx, func(daemonClient *client.DaemonClient) ([]core.LocalRemoteRepair, error) {
						return daemonClient.NewSessionRemoteRepairs(ctx, launch.BundleID, launch.TargetTemplateID)
					})
				})
				if err != nil {
					return nil, err
				}
				if len(repairs) != 0 {
					if err := updates.Send(dashboardIoUpdateCreateSession{update: dashboardCreateSessionUpdateRemoteRepair{
						bundleID: launch.BundleID,
						repairs:  repairs,
						retry:    action,
					}}); err != nil {
						return nil, fmt.Errorf("dashboard closed during remote repair preparation: %w", err)
					}
					return nil, nil
				}
			}
			if cancelled.Load() {
				return nil, errors.New("operation cancelled")
			}
			title := ""
			if launch.ProfileID != "" {
				title = core.DefaultSessionTitle(launch.ProjectDirectory, launch.BundleID, launch.ProfileID)
			}
			var review core.SessionReview
			if launch.Review != nil {
				review = *launch.Review
			}
			var provider core.QuotaProvider
			if launch.Provider != nil {
				provider = *launch.Provider
			}
			session, err := blockOnResult(func(ctx context.Context) (client.RegisteredSession, error) {
				return withDaemon(ctx, func(daemonClient *client.DaemonClient) (client.RegisteredSession, error) {
					return daemonClient.StartCreateSession(ctx, client.CreateSessionRequest{
						Oracle:                launch.Oracle,
						OracleChoice:          launch.OracleChoice,
						RememberLaunchChoices: true,
						RememberContainerSize: launch.RememberContainerSize,
						Review:                review,
						NoProjectMemory:       launch.Carried.NoProjectMemory,
						NoGithubEvents:        launch.Carried.NoGithubEvents,
						NoGithubCredentials:   launch.Carried.NoGithubCredentials,
						NotificationsIgnored:  false,
						At:                    nil,
						Branch:                nil,
						Base:                  nil,
						NoLaterHistory:        false,
						Subagents:             launch.Subagents,
						CreateManagedWorktree: launch.CreateManagedWorktree,
						InitialPrompt:         nil,
						WorkspaceID:           launch.WorkspaceID,
						ProfileID:             launch.ProfileID,
						Provider:              provider,
						BundleID:              launch.BundleID,
						ProjectDirectory:      launch.ProjectDirectory,
						TargetTemplateID:      launch.TargetTemplateID,
						AdditionalMounts:      launch.AdditionalMounts,
						// Local changes are never part of isolated creation;
						// the compatibility field is intentionally ignored.
						ResourceAllocation:   launch.ResourceAllocation,
						Title:                title,
						SessionTitleOverride: nil,
					})
				})
			})
			if err != nil {
				return nil, err
			}
			return &registeredDashboardSession{
				retryLaunch:             retryLaunch,
				session:                 session.Session,
				rememberedContainerSize: session.RememberedContainerSize,
				cancelled:               cancelled,
			}, nil
		}()
		if err != nil {
			report[dashboardIoUpdate]("creating session", updates, dashboardIoUpdateCreateSession{update: dashboardCreateSessionUpdateFailed{
				retryLaunch: retryLaunch,
				message:     err.Error(),
			}})
		}
		if registered == nil {
			guard.Release()
			return struct{}{}
		}
		sessionID := registered.session.ID
		report[dashboardIoUpdate]("creating session", updates, dashboardIoUpdateCreateSession{update: dashboardCreateSessionUpdateRegistered{registered: registered}})
		guard.Release()
		followDaemonCreation("creating session", sessionID, lifecycleUpdates, cancelled)
		return struct{}{}
	})
}

// followDaemonCreation reports how a session the daemon has accepted
// finishes launching.
//
// The daemon owns the launch from registration on, and it continues after
// this terminal exits. Callers therefore release their exit guard before
// calling this, so quitting never waits for a launch or its rollback.
func followDaemonCreation(
	operation string,
	sessionID string,
	lifecycleUpdates *core.Unbounded[dashboardLifecycleUpdate],
	cancelled *atomic.Bool,
) {
	success, err := blockOnResult(func(ctx context.Context) (daemon.LifecycleSuccess, error) {
		return awaitDaemonCreation(ctx, sessionID, cancelled)
	})
	update := dashboardLifecycleUpdate{sessionID: sessionID, operation: cancelled}
	if err != nil {
		failure := dashboardLifecycleFailureFrom(err)
		update.failure = &failure
	} else {
		update.success = success
	}
	report(operation, lifecycleUpdates, update)
}

// awaitDaemonCreation waits for the daemon to finish creating a session,
// across daemon restarts.
//
// A daemon that stops mid-create does not fail it: the next daemon finishes
// it from its durable record, and any daemon answers `WaitCreateSession`
// from that record. A lost connection therefore means "ask again", never
// "failed". The wait is read-only, so repeating it is safe;
// `StartCreateSession` is never repeated. Each retry follows a lost
// connection and waits a quarter second, and the loop ends with the
// daemon's answer or the caller's cancellation.
func awaitDaemonCreation(ctx context.Context, sessionID string, cancelled *atomic.Bool) (daemon.LifecycleSuccess, error) {
	return followCreateWait(ctx, sessionID, cancelled, ConnectOrStart)
}

// followCreateWaitRetryDelay is follow_create_wait's RETRY_DELAY: the pause
// after a lost connection before the wait is asked again.
const followCreateWaitRetryDelay = 250 * time.Millisecond

// followCreateWaitCancellationPoll is how often follow_create_wait looks at
// the cancellation flag while it waits.
const followCreateWaitCancellationPoll = 50 * time.Millisecond

// followCreateWait is the loop of awaitDaemonCreation, with the way it
// reaches a daemon given, so a test can answer it from a fake daemon.
func followCreateWait(
	ctx context.Context,
	sessionID string,
	cancelled *atomic.Bool,
	connect func(ctx context.Context) (*client.DaemonClient, error),
) (daemon.LifecycleSuccess, error) {
	// Every wait below, including the connection and the backoff, is
	// watched beside the cancellation flag, so a cancel ends the follow
	// promptly.
	cancellation := func(ctx context.Context) <-chan struct{} {
		stopped := make(chan struct{})
		go func() {
			ticker := time.NewTicker(followCreateWaitCancellationPoll)
			defer ticker.Stop()
			for !cancelled.Load() {
				select {
				case <-ticker.C:
				case <-ctx.Done():
					return
				}
			}
			close(stopped)
		}()
		return stopped
	}
	for {
		// One attempt, with no reconnect inside the client: a daemon that
		// stops drops the connection, and this loop starts the next daemon
		// through connect.
		attemptCtx, stopAttempt := context.WithCancel(ctx)
		attempt := make(chan error, 1)
		go func() {
			daemonClient, err := connect(attemptCtx)
			if err != nil {
				attempt <- err
				return
			}
			defer daemonClient.Close()
			attempt <- daemonClient.WaitCreateSession(attemptCtx, sessionID)
		}()
		var err error
		select {
		case err = <-attempt:
		case <-cancellation(attemptCtx):
			stopAttempt()
			return nil, errors.New("operation cancelled")
		case <-ctx.Done():
			stopAttempt()
			return nil, ctx.Err()
		}
		stopAttempt()
		if err == nil {
			return daemon.LifecycleSuccessCreated{}, nil
		}
		// Only a connection that dropped after the wait was sent means "ask
		// again". A refusal, a failure to connect or start a daemon (which
		// waits out a handoff before it fails) and a protocol mismatch are
		// real answers and end the follow with their own error.
		if !errors.As(err, new(client.DaemonTransportLost)) {
			return nil, err
		}
		slog.Info("lost the daemon while a session was being created; waiting again", "session_id", sessionID, "error", err.Error())
		backoffCtx, stopBackoff := context.WithCancel(ctx)
		delay := time.NewTimer(followCreateWaitRetryDelay)
		select {
		case <-delay.C:
			stopBackoff()
		case <-cancellation(backoffCtx):
			delay.Stop()
			stopBackoff()
			return nil, errors.New("operation cancelled")
		case <-ctx.Done():
			delay.Stop()
			stopBackoff()
			return nil, ctx.Err()
		}
	}
}

// supervisedWikiTask keeps each read or follower behind its own join
// boundary. A panic becomes a visible failure without cancelling the
// sibling search; ending ctx still cancels its worker.
func supervisedWikiTask[T any](
	ctx context.Context,
	operation string,
	work func(ctx context.Context) (T, error),
) (T, error) {
	workCtx, cancelWork := context.WithCancel(ctx)
	defer cancelWork()
	worker := core.Spawn(func() blockingOutcome[T] {
		value, err := work(workCtx)
		return blockingOutcome[T]{value: value, err: err}
	})
	select {
	case <-worker.Done():
	case <-ctx.Done():
		// The supervisor was dropped; the deferred cancel stops the worker.
		var zero T
		return zero, ctx.Err()
	}
	outcome, joinErr := worker.Join(context.Background())
	return blockingResult(operation, outcome.value, outcome.err, joinErr)
}

// spawnWikiSearch: calling the returned cancel (Rust: dropping the guard)
// cancels debounce and network waits.
func spawnWikiSearch(
	requestID uint64,
	query string,
	delay time.Duration,
	updates *core.Unbounded[dashboardIoUpdate],
) context.CancelFunc {
	ctx, cancel := context.WithCancel(context.Background())
	core.Spawn(func() struct{} {
		if sleepContext(ctx, delay) != nil {
			return struct{}{}
		}
		liveQuery := query
		archive := core.Spawn(func() struct{} {
			page, err := supervisedWikiTask(ctx, "searching the session archive", func(ctx context.Context) (client.WikiSearchPage, error) {
				return withDaemon(ctx, func(daemonClient *client.DaemonClient) (client.WikiSearchPage, error) {
					return daemonClient.WikiSearch(ctx, query, wikiSearchLimit)
				})
			})
			if ctx.Err() == nil {
				report[dashboardIoUpdate]("searching the session archive", updates, dashboardIoUpdateWikiRows{requestID: requestID, page: page, err: err})
			}
			return struct{}{}
		})
		live := core.Spawn(func() struct{} {
			page, err := supervisedWikiTask(ctx, "searching live session conversations", func(ctx context.Context) (client.SessionTextSearchPage, error) {
				if strings.TrimSpace(liveQuery) == "" {
					return client.SessionTextSearchPage{}, nil
				}
				return withDaemon(ctx, func(daemonClient *client.DaemonClient) (client.SessionTextSearchPage, error) {
					return daemonClient.SessionTextSearch(ctx, liveQuery)
				})
			})
			if ctx.Err() == nil {
				report[dashboardIoUpdate]("searching live session conversations", updates, dashboardIoUpdateResumeTextMatches{requestID: requestID, page: page, err: err})
			}
			return struct{}{}
		})
		<-archive.Done()
		<-live.Done()
		return struct{}{}
	})
	return cancel
}

func spawnWikiSync(dialog bool, episode uint64, updates *core.Unbounded[dashboardIoUpdate]) context.CancelFunc {
	ctx, cancel := context.WithCancel(context.Background())
	core.Spawn(func() struct{} {
		answers := updates
		_, err := supervisedWikiTask(ctx, "syncing the session index", func(ctx context.Context) (struct{}, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (struct{}, error) {
				return unit(followWikiSync(ctx, daemonClient, func(status client.WikiSyncStatus) {
					report[dashboardIoUpdate]("syncing the session index", answers, dashboardIoUpdateWikiSync{
						dialog:  dialog,
						episode: episode,
						status:  status,
					})
				}))
			})
		})
		if err != nil && ctx.Err() == nil {
			report[dashboardIoUpdate]("syncing the session index", updates, dashboardIoUpdateWikiSync{
				dialog:  dialog,
				episode: episode,
				err:     err,
			})
		}
		return struct{}{}
	})
	return cancel
}

// followWikiSync: one episode belongs to the daemon that accepted its
// request.
func followWikiSync(
	ctx context.Context,
	daemonClient *client.DaemonClient,
	answer func(client.WikiSyncStatus),
) error {
	status, err := daemonClient.WikiSyncRequest(ctx)
	if err != nil {
		return err
	}
	if status.Target == nil {
		return errors.New("sync request returned no target")
	}
	target := *status.Target
	for {
		status.Target = &target
		answer(status)
		if status.Finished >= target {
			return nil
		}
		next, err := daemonClient.WikiSyncWatch(ctx, status.Epoch, status.Version)
		if err != nil {
			return err
		}
		if next.Epoch != status.Epoch {
			return errors.New("session index daemon changed; reopen to sync again")
		}
		status = next
	}
}

// spawnSessionsTextSearch searches the conversations of the live sessions
// for the Sessions filter's text, after the typing pause.
//
// The wait and the daemon call are in a task, so the render loop never
// waits on the index. A task whose request id is no longer the newest when
// it wakes stops without asking the daemon, and the dashboard drops an
// answer that names an older request.
func spawnSessionsTextSearch(requestID uint64, query string, updates *core.Unbounded[dashboardIoUpdate]) context.CancelFunc {
	ctx, cancel := context.WithCancel(context.Background())
	core.Spawn(func() struct{} {
		if sleepContext(ctx, wikiSearchDebounce) != nil {
			return struct{}{}
		}
		page, err := supervisedWikiTask(ctx, "searching session conversations", func(ctx context.Context) (client.SessionTextSearchPage, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (client.SessionTextSearchPage, error) {
				return daemonClient.SessionTextSearch(ctx, query)
			})
		})
		if ctx.Err() == nil {
			report[dashboardIoUpdate]("searching session conversations", updates, dashboardIoUpdateSessionTextMatches{requestID: requestID, page: page, err: err})
		}
		return struct{}{}
	})
	return cancel
}

// wikiSearchDebounce is how long the dialog waits after a keystroke before
// asking the index.
const wikiSearchDebounce = 250 * time.Millisecond

// wikiSearchLimit is how many archived sessions one search asks for.
const wikiSearchLimit = 50

// wikiBriefChars is how much of an archived transcript the preview pane
// asks for. It is a few lines tall, so a whole briefing would be wasted
// work.
const wikiBriefChars = 4_000

func spawnWikiBrief(wikiID string, updates *core.Unbounded[dashboardIoUpdate]) {
	core.Spawn(func() struct{} {
		ctx := context.Background()
		brief, err := withDaemon(ctx, func(daemonClient *client.DaemonClient) (string, error) {
			return daemonClient.WikiBrief(ctx, wikiID, wikiBriefChars)
		})
		report[dashboardIoUpdate]("reading an archived transcript", updates, dashboardIoUpdateWikiBrief{wikiID: wikiID, brief: brief, err: err})
		return struct{}{}
	})
}

// wikiHitsContext is how many messages of context the hit preview asks for
// on each side of a matching message: enough to see what the match
// answered.
const wikiHitsContext = 1

// wikiHitsChars is how much of one message the hit preview asks for. The
// pane is scrollable but hit-centred, so a long message is shown around its
// match.
const wikiHitsChars = 2_000

func spawnWikiHits(wikiID string, query string, updates *core.Unbounded[dashboardIoUpdate]) {
	core.Spawn(func() struct{} {
		ctx := context.Background()
		hits, err := withDaemon(ctx, func(daemonClient *client.DaemonClient) (*client.WikiHitTranscript, error) {
			return daemonClient.WikiHits(ctx, wikiID, query, wikiHitsContext, wikiHitsChars)
		})
		report[dashboardIoUpdate]("reading an archived transcript's matches", updates, dashboardIoUpdateWikiHits{
			wikiID: wikiID,
			query:  query,
			hits:   hits,
			err:    err,
		})
		return struct{}{}
	})
}

// spawnDashboardRestoreSession starts a session from an archived transcript
// and follows it through creation the same way a new session is followed.
func spawnDashboardRestoreSession(
	request client.WikiRestoreRequest,
	retryLaunch tui.DashboardAction,
	updates *core.Unbounded[dashboardIoUpdate],
	lifecycleUpdates *core.Unbounded[dashboardLifecycleUpdate],
	tracker client.CriticalOperationTracker,
) {
	cancelled := new(atomic.Bool)
	guard := tracker.BeginCancellable("restoring archived session", cancelled)
	core.SpawnBlocking(func() struct{} {
		registered, err := blockOnResult(func(ctx context.Context) (client.RegisteredSession, error) {
			return withDaemon(ctx, func(daemonClient *client.DaemonClient) (client.RegisteredSession, error) {
				return daemonClient.WikiRestore(ctx, request)
			})
		})
		if err != nil {
			report[dashboardIoUpdate]("restoring archived session", updates, dashboardIoUpdateCreateSession{update: dashboardCreateSessionUpdateFailed{
				retryLaunch: retryLaunch,
				message:     err.Error(),
			}})
			guard.Release()
			return struct{}{}
		}
		sessionID := registered.Session.ID
		report[dashboardIoUpdate]("restoring archived session", updates, dashboardIoUpdateCreateSession{update: dashboardCreateSessionUpdateRegistered{
			registered: &registeredDashboardSession{
				retryLaunch:             retryLaunch,
				session:                 registered.Session,
				rememberedContainerSize: registered.RememberedContainerSize,
				cancelled:               cancelled,
			},
		}})
		guard.Release()
		followDaemonCreation("restoring archived session", sessionID, lifecycleUpdates, cancelled)
		return struct{}{}
	})
}

// machineRequestCancellationPoll is how often a machine request looks at
// its cancellation flag.
const machineRequestCancellationPoll = 10 * time.Millisecond

// machineRequest: each disposable read owns a connection. Superseding the
// input closes that connection, allowing the daemon to stop its executor
// and reap its children.
func machineRequest(action client.DaemonAction, cancelled *atomic.Bool) (client.DaemonReply, error) {
	return blockOnResult(func(ctx context.Context) (client.DaemonReply, error) {
		requestCtx, stopRequest := context.WithCancel(ctx)
		defer stopRequest()
		request := make(chan blockingOutcome[client.DaemonReply], 1)
		go func() {
			reply, err := withDaemon(requestCtx, func(daemonClient *client.DaemonClient) (client.DaemonReply, error) {
				return daemonClient.Request(requestCtx, action)
			})
			request <- blockingOutcome[client.DaemonReply]{value: reply, err: err}
		}()
		ticker := time.NewTicker(machineRequestCancellationPoll)
		defer ticker.Stop()
		for {
			select {
			case outcome := <-request:
				return outcome.value, outcome.err
			case <-ticker.C:
				if cancelled.Load() {
					return nil, errors.New("machine request cancelled")
				}
			}
		}
	})
}
