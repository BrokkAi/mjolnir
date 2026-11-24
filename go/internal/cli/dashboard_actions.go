// Port of mj-cli/src/dashboard/actions.rs.
//
// What the dashboard does with the work its key handling asks for.
//
// Every arm here either updates UI state directly or starts a background
// job, including SetupLogin, whose terminal connection runs in a background
// task.

package cli

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/daemon"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// mbxInstallCompletion is a finished mbx install with the build cache
// preview read after it. Each error is the failure text of its half (Rust's
// Result<_, String>).
type mbxInstallCompletion struct {
	install             string
	installError        *string
	preview             *core.BuildCachePreview
	previewError        *string
	installMbxAvailable bool
}

// chatToggle is a chat-scoped toggle the host's keys can run.
type chatToggle uint8

const (
	chatToggleTranscriptRendering chatToggle = iota
	chatToggleDictation
)

// applyChatToggle applies a toggle to the conversation on screen, or says
// there is none.
//
// The keys that run these live in the dashboard's command registry, so they
// answer from every surface, including a pane that has never opened a chat.
func applyChatToggle(dashboard *tui.DashboardState, activeChat *chat.ActiveChat, toggle chatToggle) {
	if activeChat == nil {
		dashboard.SetNotice("No conversation is open.")
		return
	}
	switch toggle {
	case chatToggleTranscriptRendering:
		activeChat.ToggleTranscriptRendering()
	case chatToggleDictation:
		if reason, refused := activeChat.ToggleDictation(); refused {
			dashboard.SetNotice(reason)
		}
	}
}

// copiedSessionIDNotice is what the notice bar says after the session
// menu's Copy session ID. The clipboard gets the full ID; the notice names
// the short form the rest of the dashboard uses.
func copiedSessionIDNotice(sessionID string) string {
	return fmt.Sprintf("Copied session ID %s", core.ShortID(sessionID))
}

// withDaemon runs request against a daemon client this process connects to,
// or starts, and closes it after.
func withDaemon[T any](ctx context.Context, connect func(context.Context) (*client.DaemonClient, error), request func(*client.DaemonClient) (T, error)) (T, error) {
	daemonClient, err := connect(ctx)
	if err != nil {
		var zero T
		return zero, err
	}
	defer daemonClient.Close()
	return request(daemonClient)
}

// errorText is anyhow's `{error:#}` for a Go error.
func errorText(err error) string { return err.Error() }

// waitLifecycleReceipt submits request through the admission and waits for
// the daemon to finish it.
func waitLifecycleReceipt(ctx context.Context, admission lifecycleSubmission, request client.SessionLifecycleRequest) (client.DaemonReply, error) {
	daemonClient, err := ConnectOrStart(ctx)
	if err != nil {
		return nil, err
	}
	defer daemonClient.Close()
	receipt, err := admission.submit(ctx, daemonClient, request)
	if err != nil {
		return nil, err
	}
	return daemonClient.WaitSessionLifecycle(ctx, receipt)
}

func startSuspension(c *dashboardContext, sessionID string, consent core.SuspendConsent) {
	c.dashboard.SetNotice(fmt.Sprintf("Suspending %s…", c.sessionNoticeName(sessionID)))
	request := c.beginLifecycleOperation(sessionID, tui.SessionOperationKindSuspending)
	markActiveChatRetiring(c.chats[sessionID], sessionID)
	spawnLifecycleOperation(request, c.criticalOperations, func(_ *atomic.Bool, admission lifecycleSubmission) (daemon.LifecycleSuccess, error) {
		if _, err := waitLifecycleReceipt(backgroundContext(), admission, client.SessionLifecycleRequestSuspend{
			SessionID:           sessionID,
			DiscardUntracked:    consent.DiscardUntracked,
			AbortOperations:     consent.AbortOperations,
			WithoutCheckpoint:   consent.WithoutCheckpoint,
			ConfirmNoCheckpoint: consent.ConfirmNoCheckpoint,
		}); err != nil {
			return nil, err
		}
		return daemon.LifecycleSuccessClosed{}, nil
	})
}

// backgroundContext is the context a blocking lifecycle worker runs its
// daemon calls under (Rust's block_on on the shared runtime).
func backgroundContext() context.Context { return context.Background() }

// applyDashboardAction carries out one dashboard action.
func applyDashboardAction(ctx context.Context, c *dashboardContext, action tui.DashboardAction) error {
	c.cancelStalePathInput()
	// A fast-start launch bypasses the new-session key, so it meets the Git
	// identity check here.
	if launch, ok := action.(tui.DashboardActionGoLaunch); ok {
		action = c.dashboard.GateLaunchOnGitIdentity(launch)
	}
	switch action := action.(type) {
	case tui.DashboardActionSetOperatorAway:
		away := action.Away
		revision := c.dashboard.OperatorPresenceRevision()
		spawnCriticalAsync(c.criticalOperations, "changing operator presence", c.dashboardIoTx, 30*time.Second,
			func(ctx context.Context) (struct{}, error) {
				return withDaemon(ctx, ConnectOrStart, func(daemonClient *client.DaemonClient) (struct{}, error) {
					reply, err := daemonClient.Request(ctx, client.DaemonActionSetOperatorAway{Away: away})
					if err != nil {
						return struct{}{}, err
					}
					if _, done := reply.(client.DaemonReplyDone); done {
						return struct{}{}, nil
					}
					return struct{}{}, fmt.Errorf("unexpected operator-presence reply %#v", reply)
				})
			},
			func(_ struct{}, err error) dashboardIoUpdate {
				return dashboardIoUpdateOperatorAway{away: away, revision: revision, err: err}
			})
	case nil, tui.DashboardActionNone:
	case tui.DashboardActionCheckGitIdentity:
		// The daemon reads Git in the environment its sessions inherit.
		updates := c.dashboardIoTx
		go func() {
			identity, err := withDaemon(backgroundContext(), ConnectOrStart, func(daemonClient *client.DaemonClient) (client.GitIdentity, error) {
				return daemonClient.GitIdentity(backgroundContext())
			})
			report("reading the Git identity", updates, dashboardIoUpdate(dashboardIoUpdateGitIdentityChecked{identity: identity, err: err}))
		}()
	case tui.DashboardActionSaveGitIdentity:
		name, email := action.Name, action.Email
		spawnCriticalAsync(c.criticalOperations, "saving the Git identity", c.dashboardIoTx, saveAckTimeout,
			func(ctx context.Context) (client.GitIdentity, error) {
				return withDaemon(ctx, ConnectOrStart, func(daemonClient *client.DaemonClient) (client.GitIdentity, error) {
					return daemonClient.SetGitIdentity(ctx, name, email)
				})
			},
			func(identity client.GitIdentity, err error) dashboardIoUpdate {
				return dashboardIoUpdateGitIdentitySaved{identity: identity, err: err}
			})
	case tui.DashboardActionCopyNativeSessionId:
		return c.copyText(action.NativeSessionID, "Copied session ID")
	case tui.DashboardActionCopySessionId:
		return c.copyText(action.SessionID, copiedSessionIDNotice(action.SessionID))
	case tui.DashboardActionToggleTranscriptRendering:
		dashboard, visible := c.dashboardAndVisibleChat()
		applyChatToggle(dashboard, visible, chatToggleTranscriptRendering)
	case tui.DashboardActionToggleDictation:
		dashboard, visible := c.dashboardAndVisibleChat()
		applyChatToggle(dashboard, visible, chatToggleDictation)
	case tui.DashboardActionGoPrepareProject:
		if mode := c.dashboard.GoMode(); mode != nil {
			spawnCreateBundle([]string{mode.Directory}, c.dashboardIoTx, c.criticalOperations)
		}
	case tui.DashboardActionGoLaunch:
		if mode := c.dashboard.GoMode(); mode != nil {
			directory := mode.Directory
			c.cancelStartupSession()
			workspaceID, _ := c.dashboard.ActiveWorkspaceID()
			retry := tui.DashboardActionGoLaunch{Recipe: action.Recipe}
			recipe := action.Recipe
			c.dashboard.SetNotice("Preparing session…")
			config := c.controller.Config
			spawnCriticalIO(c.criticalOperations, "preparing fast-start session", c.dashboardIoTx,
				func() (core.GoRecipe, error) { return resolveGoRecipe(&config, directory, recipe) },
				func(resolved core.GoRecipe, err error) dashboardIoUpdate {
					return dashboardIoUpdateGoPrepared{workspaceID: workspaceID, retry: retry, recipe: resolved, err: err}
				})
		}
	case tui.DashboardActionSaveSpinnerStyle:
		spawnSpinnerStyleSave(action.Style, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionQuitDetach:
		c.requestShutdown()
	case tui.DashboardActionSelectWorkspace:
		// An explicit choice outranks opening a workspace created earlier.
		c.pendingWorkspaceSelection = nil
		if active, ok := c.dashboard.ActiveWorkspaceID(); !ok || active != action.WorkspaceID {
			workspaceID := action.WorkspaceID
			c.selectWorkspace(&workspaceID)
		}
	case tui.DashboardActionLoadNativeAgentHistory:
		updates := c.dashboardIoTx
		owner, child, before := action.Owner, action.Child, action.Before
		go func() {
			page, err := withDaemon(backgroundContext(), ConnectOrStart, func(daemonClient *client.DaemonClient) (core.NativeAgentHistoryPage, error) {
				return daemonClient.NativeAgentHistory(backgroundContext(), owner, child, before)
			})
			if sendErr := updates.Send(dashboardIoUpdateNativeAgentHistory{owner: owner, child: child, page: page, err: err}); sendErr != nil {
				slog.Warn("native agent history receiver closed", "error", sendErr)
			}
		}()
	case tui.DashboardActionStopNativeAgent:
		updates := c.dashboardIoTx
		owner, child := action.Owner, action.Child
		go func() {
			_, err := withDaemon(backgroundContext(), client.ConnectExisting, func(daemonClient *client.DaemonClient) (struct{}, error) {
				return struct{}{}, daemonClient.StopBackgroundTask(backgroundContext(), owner, "native-agent:"+child)
			})
			if sendErr := updates.Send(dashboardIoUpdateNativeAgentStopped{owner: owner, child: child, err: err}); sendErr != nil {
				slog.Warn("native agent stop result receiver closed", "error", sendErr)
			}
		}()
	case tui.DashboardActionOpenSubagents:
		c.openSubagents(action.ParentID)
	case tui.DashboardActionDismissNotification:
		spawnNotificationDismissal(c, action)
	case tui.DashboardActionDismissNotifications:
		// Every dismissal is sent at once with its own deadline, so a slow
		// session cannot discard the others' results; all come back in one
		// update.
		guard := c.criticalOperations.Begin("notification dismissals")
		updates := c.dashboardIoTx
		dismissals := action.Dismissals
		go func() {
			defer guard.Release()
			results := make([]notificationDismissalResult, len(dismissals))
			var group sync.WaitGroup
			for index, dismissal := range dismissals {
				group.Add(1)
				go func() {
					defer group.Done()
					deadline := 30 * time.Second
					ctx, cancel := context.WithTimeout(backgroundContext(), deadline)
					defer cancel()
					done := make(chan notificationDismissalResult, 1)
					go func() {
						ordinal, err := requestNotificationDismissal(ctx, dismissal.First, dismissal.Second)
						done <- notificationDismissalResult{ordinal: ordinal, err: err}
					}()
					var result notificationDismissalResult
					select {
					case result = <-done:
					case <-ctx.Done():
						result.err = fmt.Errorf("the daemon did not acknowledge the dismissal within %d seconds; it may still complete", int(deadline.Seconds()))
					}
					result.sessionID, result.target = dismissal.First, dismissal.Second
					results[index] = result
				}()
			}
			group.Wait()
			for _, result := range results {
				if result.err != nil {
					slog.Error("notification dismissal was not confirmed", "session_id", result.sessionID, "error", result.err)
				}
			}
			if err := updates.Send(dashboardIoUpdateNotificationsDismissed{results: results}); err != nil {
				slog.Warn("notification dismissal results receiver closed", "error", err)
			}
		}()
	case tui.DashboardActionInterruptAll:
		updates := c.dashboardIoTx
		sessionID := action.SessionID
		go func() {
			result, err := withDaemon(backgroundContext(), client.ConnectExisting, func(daemonClient *client.DaemonClient) (client.InterruptAllResult, error) {
				reply, err := daemonClient.Request(backgroundContext(), client.DaemonActionInterruptAll{SessionID: sessionID})
				if err != nil {
					return client.InterruptAllResult{}, err
				}
				if interrupted, ok := reply.(client.DaemonReplyInterruptedAll); ok {
					return interrupted.Result, nil
				}
				return client.InterruptAllResult{}, fmt.Errorf("unexpected Interrupt All reply: %#v", reply)
			})
			if updates.Send(dashboardIoUpdateInterruptAllFinished{result: result, err: err}) != nil {
				slog.Debug("dashboard closed before Interrupt All finished")
			}
		}()
	case tui.DashboardActionInterruptTurn:
		// The chat is what knows the turn and sends the interrupt, so the
		// command reaches only a conversation this terminal has open.
		var notice string
		if open := c.chats[action.SessionID]; open == nil {
			notice = "Open the conversation to interrupt its turn."
		} else if !open.InterruptTurn() {
			notice = "No turn to interrupt."
		}
		if notice != "" {
			c.dashboard.SetNotice(notice)
		}
	case tui.DashboardActionExitSubagentWorkspace:
		parentID, hasParent := c.dashboard.SubagentParentID()
		c.dashboard.CloseSubagentWorkspace()
		if hasParent {
			c.openChatSession(parentID)
		}
	case tui.DashboardActionLoadWorkspaceManagement:
		spawnWorkspaceManagementLoad(action.Generation, c.dashboardIoTx)
	case tui.DashboardActionCreateWorkspace:
		spawnWorkspaceCreate(action.Generation, action.Name, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionRenameWorkspace:
		spawnWorkspaceRename(action.Generation, action.WorkspaceID, action.Name, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionCancelWorkspaceClose:
		spawnWorkspaceCloseCancel(action.WorkspaceID, c.dashboardIoTx)
	case tui.DashboardActionCloseWorkspace:
		spawnWorkspaceClose(action.Generation, action.WorkspaceID, c.dashboardIoTx)
	case tui.DashboardActionRecoverWorkspaceDraft:
		spawnWorkspaceDraftRecovery(action.Generation, action.DraftID, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionOpenConfig:
		c.dashboard.BeginSetup()
	case tui.DashboardActionDiscoverSetup:
		spawnSetupDiscovery(action.Generation, action.Scope, action.Machines, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionCheckAvailability:
		spawnAvailabilityCheck(c.dashboardIoTx)
	case tui.DashboardActionResolveContainerPath:
		inputContext := c.dashboard.PathInputContext()
		session, ok := c.controller.State.Sessions.Get(action.SessionID)
		if !ok {
			return errors.New("Session no longer exists")
		}
		targetID, source := session.TargetTemplateID, action.Source
		cancelled, _ := spawnCancellableIOWithToken(c.criticalOperations, "resolving attached directory", c.dashboardIoTx,
			func(cancelled *atomic.Bool) (string, error) {
				reply, err := machineRequest(client.DaemonActionResolveContainerPath{TargetID: targetID, Source: source}, cancelled)
				if err != nil {
					return "", err
				}
				if resolved, ok := reply.(client.DaemonReplyResolvedPath); ok {
					return resolved.Path, nil
				}
				return "", fmt.Errorf("unexpected path reply: %#v", reply)
			},
			func(path string, err error) dashboardIoUpdate {
				return dashboardIoUpdateContainerPathResolved{context: inputContext, path: path, err: err}
			})
		c.trackPathInput(cancelled)
	case tui.DashboardActionResolveSetupPath:
		generation, draft, path, value, machine := action.Generation, action.Draft, action.Path, action.Value, action.Machine
		cancelled, _ := spawnCancellableIOWithToken(c.criticalOperations, "resolving setup path", c.dashboardIoTx,
			func(cancelled *atomic.Bool) (string, error) {
				reply, err := machineRequest(client.DaemonActionResolveSetupPath{Machine: machine, Source: value}, cancelled)
				if err != nil {
					return "", err
				}
				if resolved, ok := reply.(client.DaemonReplyResolvedPath); ok {
					return resolved.Path, nil
				}
				return "", fmt.Errorf("unexpected setup path reply: %#v", reply)
			},
			func(resolved string, err error) dashboardIoUpdate {
				return dashboardIoUpdateSetupPathResolved{generation: generation, draft: draft, path: path, value: value, resolved: resolved, err: err}
			})
		c.trackPathInput(cancelled)
	case tui.DashboardActionPreviewBuildCache:
		// A stale answer is dropped by its key, so the job is not tied to
		// the path-input job that a later edit would cancel.
		generation, key, machine := action.Generation, action.Key, action.Machine
		spawnCancellableIOWithToken(c.criticalOperations, "previewing the build cache", c.dashboardIoTx,
			func(cancelled *atomic.Bool) (client.DaemonReplyBuildCachePreview, error) {
				reply, err := machineRequest(client.DaemonActionPreviewBuildCache{Machine: machine}, cancelled)
				if err != nil {
					return client.DaemonReplyBuildCachePreview{}, err
				}
				if preview, ok := reply.(client.DaemonReplyBuildCachePreview); ok {
					return preview, nil
				}
				return client.DaemonReplyBuildCachePreview{}, fmt.Errorf("unexpected build cache reply: %#v", reply)
			},
			func(preview client.DaemonReplyBuildCachePreview, err error) dashboardIoUpdate {
				if err != nil {
					return dashboardIoUpdateBuildCachePreviewed{generation: generation, key: key, err: err}
				}
				return dashboardIoUpdateBuildCachePreviewed{generation: generation, key: key, preview: preview.Preview, installMbxAvailable: preview.Available}
			})
	case tui.DashboardActionInstallMbx:
		if !c.dashboard.MbxInstallStarted(action.Generation, action.Key, action.MachineID) {
			return nil
		}
		generation, key, machineID, machine := action.Generation, action.Key, action.MachineID, action.Machine
		spawnCancellableIOWithToken(c.criticalOperations, "installing mbx", c.dashboardIoTx,
			func(cancelled *atomic.Bool) (mbxInstallCompletion, error) {
				var completion mbxInstallCompletion
				reply, err := machineRequest(client.DaemonActionInstallMbx{Machine: machine}, cancelled)
				if err == nil {
					if installed, ok := reply.(client.DaemonReplyMbxInstalled); ok {
						completion.install = installed.Message
					} else {
						err = fmt.Errorf("unexpected mbx install reply: %#v", reply)
					}
				}
				if err != nil {
					failure := fmt.Sprintf("mbx installation interrupted — run Install again: %s", errorText(err))
					completion.installError = &failure
				}
				previewReply, err := machineRequest(client.DaemonActionPreviewBuildCache{Machine: machine}, cancelled)
				switch preview := previewReply.(type) {
				case client.DaemonReplyBuildCachePreview:
					if err == nil {
						completion.preview = preview.Preview
						completion.installMbxAvailable = preview.Available
						break
					}
					failure := errorText(err)
					completion.previewError = &failure
				default:
					var failure string
					if err != nil {
						failure = errorText(err)
					} else {
						failure = fmt.Sprintf("unexpected build cache reply: %#v", previewReply)
					}
					completion.previewError = &failure
				}
				return completion, nil
			},
			func(completion mbxInstallCompletion, err error) dashboardIoUpdate {
				if err != nil {
					failure := errorText(err)
					skipped := "preview refresh was skipped because the background task failed"
					completion = mbxInstallCompletion{installError: &failure, previewError: &skipped}
				}
				update := dashboardIoUpdateMbxInstalled{
					generation:          generation,
					key:                 key,
					machineID:           machineID,
					installed:           completion.install,
					preview:             completion.preview,
					installMbxAvailable: completion.installMbxAvailable,
				}
				if completion.installError != nil {
					update.err = errors.New(*completion.installError)
				}
				if completion.previewError != nil {
					update.previewErr = errors.New(*completion.previewError)
				}
				return update
			})
	case tui.DashboardActionPreviewArchiveSpace:
		// Walks the sessions directory, so it belongs in the blocking pool;
		// a stale answer is dropped by its days value.
		generation, olderThanDays := action.Generation, action.OlderThanDays
		spawnIO("estimating archive space", c.dashboardIoTx,
			func() (core.ArchiveSpacePreview, error) {
				return withDaemon(backgroundContext(), ConnectOrStart, func(daemonClient *client.DaemonClient) (core.ArchiveSpacePreview, error) {
					return daemonClient.ArchiveSpacePreview(backgroundContext(), olderThanDays)
				})
			},
			func(preview core.ArchiveSpacePreview, err error) dashboardIoUpdate {
				return dashboardIoUpdateArchiveSpacePreviewed{generation: generation, olderThanDays: olderThanDays, preview: preview, err: err}
			})
	case tui.DashboardActionSaveSetup:
		spawnSetupSave(action.Generation, action.Original, action.Updated, nil, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionSaveSetupAndLogin:
		profileID := action.ProfileID
		spawnSetupSave(action.Generation, action.Original, action.Updated, &profileID, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionOpenManagedSetup:
		c.dashboard.BeginManagedSetup()
	case tui.DashboardActionSetupLogin:
		login, err := startActiveLogin(action.ProfileID, action.Generation)
		if err != nil {
			c.dashboard.SetupLoginFinished(action.Generation, action.ProfileID, tui.SetupLoginOutcomeFailed{Value: errorText(err)})
		} else {
			c.login = login
		}
	// One key refreshes both panes, so it runs both requests rather than
	// leaving the user to focus each pane in turn.
	case tui.DashboardActionRefreshAll:
		// Revisit unanswered archive paths even when the dialog's rows did
		// not change.
		c.checkpointArchiveRecords.Clear()
		c.controllerChanged = true
		c.requestQuotaRefresh()
		spawnCapacityRefreshRequest(c.dashboardIoTx)
		c.dashboard.SetNotice(quotaRefreshNotice)
	case tui.DashboardActionLoadWebAccess, tui.DashboardActionRecoverWebViewer, tui.DashboardActionInspectWebListener:
		spawnWebRequest(c, action)
	case tui.DashboardActionRestartDaemon:
		spawnDaemonRestart(c)
	case tui.DashboardActionWarmProfileCapabilities:
		key, encoded := action.Key, action.Config
		spawnBackgroundAsync("starting profile capability hydration", c.dashboardIoTx, 30*time.Second,
			func(ctx context.Context) (struct{}, error) {
				var config core.Config
				if err := json.Unmarshal([]byte(encoded), &config); err != nil {
					return struct{}{}, err
				}
				return withDaemon(ctx, client.ConnectExisting, func(daemonClient *client.DaemonClient) (struct{}, error) {
					return struct{}{}, daemonClient.WarmProfileCapabilities(ctx, config)
				})
			},
			func(_ struct{}, err error) dashboardIoUpdate {
				return dashboardIoUpdateProfileHydration{key: key, err: err}
			})
	case tui.DashboardActionProbeGitStatus:
		c.spawnGitProbe(action.SessionID)
	case tui.DashboardActionCancelWebAccess:
		if c.webRequestCancel != nil {
			c.webRequestCancel()
			c.webRequestCancel = nil
		}
		c.webRequestGeneration++
	case tui.DashboardActionLoadMountHistory, tui.DashboardActionRefreshProjects:
		retry := false
		if refresh, ok := action.(tui.DashboardActionRefreshProjects); ok {
			retry = refresh.Retry
		}
		c.dashboard.SetNotice("Refreshing recent projects…")
		historyTx := c.dashboardIoTx
		var pickerContext *string
		if picker, ok := c.dashboard.ProjectCatalogContext(); ok {
			pickerContext = &picker
		}
		historyContext := pickerContext
		spawnAsyncJob(nil, "refreshing projects", c.dashboardIoTx, 125*time.Second,
			func(ctx context.Context) (core.ProjectCatalogView, error) {
				metadata, err := withDaemon(ctx, ConnectOrStart, func(daemonClient *client.DaemonClient) (client.StoredSessionMetadata, error) {
					return daemonClient.StoredSessionMetadata(ctx)
				})
				var history map[string][]string
				if err == nil {
					history = map[string][]string{}
					for key, value := range metadata.MountHistory.All() {
						history[key] = append([]string(nil), (*value)...)
					}
				}
				// Publish the history before requesting discovery. A late
				// history snapshot cannot erase locations discovery adds.
				if sendErr := historyTx.Send(dashboardIoUpdateMountHistory{context: historyContext, history: history, err: err}); sendErr != nil {
					return core.ProjectCatalogView{}, errors.New("dashboard closed during project refresh")
				}
				daemonClient, err := ConnectOrStart(ctx)
				if err != nil {
					return core.ProjectCatalogView{}, err
				}
				defer daemonClient.Close()
				view, err := daemonClient.ProjectCatalog(ctx, true, retry)
				for err == nil && view.Status.State == core.ProjectCatalogStateRefreshing {
					select {
					case <-ctx.Done():
						return core.ProjectCatalogView{}, ctx.Err()
					case <-time.After(250 * time.Millisecond):
					}
					view, err = daemonClient.ProjectCatalog(ctx, false, false)
				}
				return view, err
			},
			func(view core.ProjectCatalogView, err error) dashboardIoUpdate {
				return dashboardIoUpdateProjectCatalog{context: pickerContext, catalog: view, err: err}
			})
	case tui.DashboardActionCheckProjectDirectories:
		for _, path := range action.Paths {
			label := fmt.Sprintf("checking project directory %s", path)
			spawnCancellableIO(c.criticalOperations, label, c.dashboardIoTx,
				func(cancelled *atomic.Bool) (*bool, error) {
					reply, err := machineRequest(client.DaemonActionCheckProjectDirectories{Paths: []string{path}}, cancelled)
					if err != nil {
						return nil, err
					}
					directories, ok := reply.(client.DaemonReplyProjectDirectories)
					if !ok {
						return nil, fmt.Errorf("unexpected directory reply: %#v", reply)
					}
					if len(directories.Directories) == 0 {
						return nil, nil
					}
					exists := directories.Directories[0].Second
					return &exists, nil
				},
				func(exists *bool, err error) dashboardIoUpdate {
					if err != nil {
						exists = nil
					}
					return dashboardIoUpdateProjectDirectory{path: path, exists: exists}
				})
		}
	case tui.DashboardActionTestTarget:
		targetID := action.TargetID
		cancelled, _ := spawnCancellableIOWithToken(c.criticalOperations, fmt.Sprintf("testing target %s", targetID), c.dashboardIoTx,
			func(cancelled *atomic.Bool) (struct{}, error) {
				reply, err := machineRequest(client.DaemonActionTestTarget{TargetID: targetID}, cancelled)
				if err != nil {
					return struct{}{}, err
				}
				if _, done := reply.(client.DaemonReplyDone); done {
					return struct{}{}, nil
				}
				return struct{}{}, fmt.Errorf("unexpected target test reply: %#v", reply)
			},
			func(_ struct{}, err error) dashboardIoUpdate {
				return dashboardIoUpdateTargetTest{targetID: targetID, err: err}
			})
		c.targetTestCancel = cancelled
	case tui.DashboardActionCancelTargetTest:
		if cancelled := c.targetTestCancel; cancelled != nil {
			c.targetTestCancel = nil
			cancelled.Store(true)
		}
	case tui.DashboardActionRenameProfile:
		spawnConfigRename(configRenameRequest{
			what:    fmt.Sprintf("profile %s to %s", action.OldID, action.NewID),
			oldID:   action.OldID,
			newID:   action.NewID,
			profile: true,
		}, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionRenameTarget:
		spawnConfigRename(configRenameRequest{
			what:  fmt.Sprintf("target %s to %s", action.OldID, action.NewID),
			oldID: action.OldID,
			newID: action.NewID,
		}, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionPasteFromClipboard:
		if !c.clipboardReadInFlight {
			c.clipboardReadInFlight = true
			c.dashboard.SetNotice("Reading clipboard…")
			spawnClipboardRead(c.dashboard.PasteTarget(), c.dashboardIoTx)
		}
	case tui.DashboardActionMarkAllRead:
		receipts := make([]sessionThrough, 0, len(action.Receipts))
		for _, receipt := range action.Receipts {
			receipts = append(receipts, sessionThrough{sessionID: receipt.First, through: receipt.Second})
		}
		c.acknowledgeDashboardSessions(receipts)
	case tui.DashboardActionOpenResumeDialog:
		c.startResumeDiscovery()
	case tui.DashboardActionOpenFilteredResumeDialog:
		c.startResumeDiscoveryWithFilter(&resumeDiscoveryFilter{query: action.Query, state: action.State})
	case tui.DashboardActionSearchArchivedSessions:
		if c.wikiSearchTask != nil {
			c.wikiSearchTask.cancel()
			c.wikiSearchTask = nil
		}
		cancel := spawnWikiSearch(action.RequestID, action.Query, wikiSearchDebounce, c.dashboardIoTx)
		c.wikiSearchTask = &generationTask{generation: action.RequestID, cancel: cancel}
	case tui.DashboardActionLoadResumeRecord:
		spawnResumeRecord(action.SessionID, c.dashboardIoTx)
	case tui.DashboardActionLoadArchivedBrief:
		spawnWikiBrief(action.WikiID, c.dashboardIoTx)
	case tui.DashboardActionLoadArchivedHits:
		spawnWikiHits(action.WikiID, action.Query, c.dashboardIoTx)
	case tui.DashboardActionRestoreArchivedSession:
		c.dashboard.SetNotice("Restoring the archived session…")
		spawnDashboardRestoreSession(client.WikiRestoreRequest{
			WikiID:           action.WikiID,
			WorkspaceID:      action.WorkspaceID,
			ProfileID:        action.ProfileID,
			TargetTemplateID: action.TargetTemplateID,
			AdditionalMounts: []core.AdditionalMount{},
		}, action, c.dashboardIoTx, c.lifecycleUpdatesTx, c.criticalOperations)
	case tui.DashboardActionImportSession:
		if _, ok := c.dashboard.ActiveWorkspaceID(); !ok {
			c.dashboard.SetNotice("Select a workspace before importing a session.")
		} else {
			c.startImport(pendingDashboardImport{
				profileID:       action.ProfileID,
				nativeSessionID: action.NativeSessionID,
				displayTitle:    action.DisplayTitle,
			}, dashboardImportSafety{})
		}
	case tui.DashboardActionCancelImport:
		if active := c.activeImport; active != nil {
			active.cancelled.Store(true)
			c.dashboard.UpdateImportProgress(1, nil, "Cancelling upload…")
		}
	case tui.DashboardActionConfirmImportBundle:
		pending := c.pendingImport
		c.pendingImport = nil
		if pending == nil {
			c.dashboard.FinishImport()
			c.dashboard.SetNotice("Import confirmation expired.")
			return nil
		}
		if !action.Accepted {
			c.dashboard.FinishImport()
			c.dashboard.SetNotice("Import cancelled; no Mjolnir files were changed.")
			return nil
		}
		if _, ok := c.dashboard.ActiveWorkspaceID(); !ok {
			c.dashboard.FinishImport()
			c.dashboard.SetNotice("Select a workspace before importing a session.")
			return nil
		}
		projectRoot := action.ProjectRoot
		c.startImport(*pending, dashboardImportSafety{
			createManagedWorktree: action.CreateManagedWorktree,
			projectRoot:           &projectRoot,
			accepted:              true,
		})
	case tui.DashboardActionNotificationReply:
		c.startupPrompts.enqueue(
			namedSession{
				name: sessionNoticeName(&c.controller.State, action.Card.SessionID),
				id:   action.SessionID,
			},
			action.Prompt,
			nil,
			c.dashboardIoTx,
			c.criticalOperations,
		)
	case tui.DashboardActionQueueStartupPrompt:
		// The daemon clears the saved draft only when it still holds the
		// text this prompt was typed from; anything else stays put.
		var inheritedDraft *string
		if session, ok := c.controller.State.Sessions.Get(action.SessionID); ok && session.DraftInput == action.Prompt.Payload.EncodeDraft() {
			draft := session.DraftInput
			inheritedDraft = &draft
		}
		c.startupPrompts.enqueue(
			namedSession{name: sessionNoticeName(&c.controller.State, action.SessionID), id: action.SessionID},
			action.Prompt,
			inheritedDraft,
			c.dashboardIoTx,
			c.criticalOperations,
		)
	case tui.DashboardActionWithdrawStartupPrompt:
		c.startupPrompts.withdraw(
			namedSession{name: sessionNoticeName(&c.controller.State, action.SessionID), id: action.SessionID},
			action.Prompt,
			c.dashboardIoTx,
			c.criticalOperations,
		)
	case tui.DashboardActionRenameSession:
		c.dashboard.SetNotice("Renaming session…")
		spawnDashboardRename(action.SessionID, c.sessionNoticeName(action.SessionID), action.Title, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionSetSessionNotificationsIgnored:
		spawnDashboardNotificationsIgnored(action.SessionID, c.sessionNoticeName(action.SessionID), action.Ignored, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionPreferLoadBalancingProfile:
		// The most recently preferred profile wins among quota-equals.
		prefer := []string{action.ProfileID}
		for _, id := range c.controller.Config.Quota.Prefer {
			if id != action.ProfileID {
				prefer = append(prefer, id)
			}
		}
		spawnQuotaPreferencesSave(prefer, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionClearLoadBalancingPreferences:
		spawnQuotaPreferencesSave([]string{}, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionProbeQuotaProfile:
		c.dashboard.SetNotice(fmt.Sprintf("Refreshing %s's quota…", action.ProfileID))
		spawnQuotaProbeRequest(action.ProfileID, c.dashboardIoTx)
	case tui.DashboardActionChangeWorkspace:
		c.dashboard.SetNotice("Moving session…")
		spawnDashboardChangeWorkspace(action.SessionID, c.sessionNoticeName(action.SessionID), action.WorkspaceID, action.WorkspaceName, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionSaveContainerSettings:
		c.dashboard.SetNotice("Saving container size and mounts…")
		resourceAllocation := action.ResourceAllocation
		spawnDashboardContainerSettings(containerSettingsRequest{
			sessionID:          action.SessionID,
			resourceAllocation: &resourceAllocation,
			additionalMounts:   action.AdditionalMounts,
			mountHistory:       action.MountHistory,
		}, c.sessionNoticeName(action.SessionID), c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionDiscoverProjects:
		pickerContext, request := action.Context, action.Request
		cancelled, _ := spawnCancellableIOWithToken(c.criticalOperations, "finding projects", c.dashboardIoTx,
			func(cancelled *atomic.Bool) (core.ProjectDiscovery, error) {
				reply, err := machineRequest(client.DaemonActionDiscoverProjects{Request: request}, cancelled)
				if err != nil {
					return core.ProjectDiscovery{}, err
				}
				if discovery, ok := reply.(client.DaemonReplyProjectDiscovery); ok {
					return discovery.Discovery, nil
				}
				return core.ProjectDiscovery{}, fmt.Errorf("unexpected discovery reply: %#v", reply)
			},
			func(discovery core.ProjectDiscovery, err error) dashboardIoUpdate {
				return dashboardIoUpdateProjectDiscovery{context: pickerContext, discovery: discovery, err: err}
			})
		c.trackCompletion(cancelled)
	case tui.DashboardActionCompletePath:
		inputContext := c.dashboard.PathInputContext()
		host, kind, prefix := action.Host, action.Kind, action.Prefix
		cancelled, _ := spawnCancellableIOWithToken(c.criticalOperations, "completing path", c.dashboardIoTx,
			func(cancelled *atomic.Bool) (core.PathCompletion, error) {
				reply, err := machineRequest(client.DaemonActionCompletePath{Host: host, Prefix: prefix, Kind: kind}, cancelled)
				if err != nil {
					return core.PathCompletion{}, err
				}
				if completion, ok := reply.(client.DaemonReplyPathCompletion); ok {
					return completion.Completion, nil
				}
				return core.PathCompletion{}, fmt.Errorf("unexpected completion reply: %#v", reply)
			},
			func(completion core.PathCompletion, err error) dashboardIoUpdate {
				return dashboardIoUpdatePathCompletions{context: inputContext, prefix: prefix, completion: completion, err: err}
			})
		c.trackCompletion(cancelled)
	case tui.DashboardActionValidateMountSource:
		inputContext := c.dashboard.PathInputContext()
		c.dashboard.SetNotice("Checking attached directory…")
		targetID, source := action.TargetTemplateID, action.Source
		cancelled, _ := spawnCancellableIOWithToken(c.criticalOperations, "validating attached directory", c.dashboardIoTx,
			func(cancelled *atomic.Bool) (client.DaemonReplyMountValidation, error) {
				reply, err := machineRequest(client.DaemonActionValidateMountSource{TargetID: targetID, Source: source}, cancelled)
				if err != nil {
					return client.DaemonReplyMountValidation{}, err
				}
				if validation, ok := reply.(client.DaemonReplyMountValidation); ok {
					return validation, nil
				}
				return client.DaemonReplyMountValidation{}, fmt.Errorf("unexpected mount reply: %#v", reply)
			},
			func(validation client.DaemonReplyMountValidation, err error) dashboardIoUpdate {
				return dashboardIoUpdateMountValidation{context: inputContext, source: source, resolved: validation.Path, forced: validation.Forced, err: err}
			})
		c.trackPathInput(cancelled)
	case tui.DashboardActionValidateSessionMounts:
		c.dashboard.SetNotice("Checking attached directories…")
		generation := c.dashboard.SessionPreflightGeneration()
		targetID, mounts, launch := action.TargetTemplateID, action.Mounts, action.Launch
		spawnCancellableIO(c.criticalOperations, "validating session directories", c.dashboardIoTx,
			func(cancelled *atomic.Bool) (*client.DaemonPair[string, string], error) {
				reply, err := machineRequest(client.DaemonActionValidateSessionMounts{TargetID: targetID, Mounts: mounts}, cancelled)
				if err != nil {
					return nil, err
				}
				validation, ok := reply.(client.DaemonReplySessionMountValidation)
				if !ok {
					return nil, fmt.Errorf("unexpected mounts reply: %#v", reply)
				}
				if validation.Failure == nil {
					return nil, nil
				}
				return &client.DaemonPair[string, string]{First: validation.Failure[0], Second: validation.Failure[1]}, nil
			},
			func(failure *client.DaemonPair[string, string], err error) dashboardIoUpdate {
				return dashboardIoUpdateSessionMountValidation{generation: generation, launch: launch, failure: failure, err: err}
			})
	case tui.DashboardActionPreflightResumeRepositories:
		return startResumeRepositoryPreflight(c, action.Launch)
	case tui.DashboardActionConfirmRawConversion:
		startPreflightedSessionLaunch(c, action.Launch, action.Receipt)
	case tui.DashboardActionReplaceResumeRepositoryOrigin:
		c.dashboard.SetNotice("Checking replacement origin…")
		submittedRepositoryID := action.RepositoryID
		generation := c.dashboard.SessionPreflightGeneration()
		sessionID, repositoryID, replacement, launch := action.SessionID, action.RepositoryID, action.Replacement, action.Launch
		spawnCancellableIO(c.criticalOperations, fmt.Sprintf("updating repository for %s", c.sessionNoticeName(sessionID)), c.dashboardIoTx,
			func(*atomic.Bool) (resumeRepositoryPreflightApply, error) {
				preflight, err := withDaemon(backgroundContext(), ConnectOrStart, func(daemonClient *client.DaemonClient) (core.ResumeRepositorySourcePreflight, error) {
					return daemonClient.ReplaceResumeRepositoryOrigin(backgroundContext(), sessionID, repositoryID, replacement)
				})
				return resumeRepositoryPreflightApply{preflight: preflight}, err
			},
			func(apply resumeRepositoryPreflightApply, err error) dashboardIoUpdate {
				return dashboardIoUpdateResumeRepositoryPreflight{generation: generation, launch: launch, submittedRepositoryID: &submittedRepositoryID, apply: apply, err: err}
			})
	case tui.DashboardActionValidateProjectDirectory:
		inputContext := c.dashboard.PathInputContext()
		c.dashboard.SetNotice("Checking project directory…")
		targetID, directory := action.TargetTemplateID, action.Directory
		type projectValidation struct {
			resolved       string
			options        core.ManagedWorktreeOptions
			reviewTurnsOff bool
		}
		cancelled, _ := spawnCancellableIOWithToken(c.criticalOperations, "validating project directory", c.dashboardIoTx,
			func(cancelled *atomic.Bool) (projectValidation, error) {
				reply, err := machineRequest(client.DaemonActionValidateProjectDirectory{TargetID: targetID, Directory: directory}, cancelled)
				if err != nil {
					return projectValidation{}, err
				}
				preflight, ok := reply.(client.DaemonReplyNewSessionPreflight)
				if !ok {
					return projectValidation{}, fmt.Errorf("unexpected project preflight reply: %#v", reply)
				}
				if preflight.Preflight.ProjectDirectory == nil {
					return projectValidation{}, errors.New("bare preflight returned no directory")
				}
				return projectValidation{
					resolved:       *preflight.Preflight.ProjectDirectory,
					options:        preflight.Preflight.ManagedWorktree,
					reviewTurnsOff: preflight.Preflight.ReviewNotice != nil,
				}, nil
			},
			func(validation projectValidation, err error) dashboardIoUpdate {
				return dashboardIoUpdateProjectValidation{
					context: inputContext, directory: directory, resolved: validation.resolved,
					options: validation.options, reviewTurnsOff: validation.reviewTurnsOff, err: err,
				}
			})
		c.trackPathInput(cancelled)
	case tui.DashboardActionPreflightCreateSession:
		startCreateSessionPreflight(c, action.Launch)
	case tui.DashboardActionRepairRepositoryRemotes:
		c.dashboard.SetNotice("Repairing Git tracking…")
		var generation *uint64
		if _, preflight := action.Retry.(tui.DashboardActionPreflightCreateSession); preflight {
			current := c.dashboard.SessionPreflightGeneration()
			generation = &current
			c.cancelSessionPreflight()
			c.dashboard.BeginRemoteSessionPreflight(current)
		}
		bundleID, repairs, retry := action.BundleID, action.Repairs, action.Retry
		cancelled, _ := spawnCancellableIOWithToken(c.criticalOperations, "repairing Git tracking", c.dashboardIoTx,
			func(*atomic.Bool) (struct{}, error) {
				return withDaemon(backgroundContext(), ConnectOrStart, func(daemonClient *client.DaemonClient) (struct{}, error) {
					return struct{}{}, daemonClient.RepairRepositoryRemotes(backgroundContext(), bundleID, repairs)
				})
			},
			func(_ struct{}, err error) dashboardIoUpdate {
				return dashboardIoUpdateRepositoryRemotesRepaired{generation: generation, retry: retry, err: err}
			})
		if generation != nil {
			c.sessionPreflightCancel = &sessionPreflight{generation: *generation, cancelled: cancelled}
		}
	case tui.DashboardActionCreateSession:
		startSessionLaunch(c, action)
	case tui.DashboardActionPreviewContainerUpgrade:
		c.dashboard.SetNotice("Checking container upgrade…")
		updates := c.dashboardIoTx
		sessionID, upgradeOnly := action.SessionID, action.UpgradeOnly
		go func() {
			preview, err := withDaemon(backgroundContext(), ConnectOrStart, func(daemonClient *client.DaemonClient) (core.ContainerUpgradePreview, error) {
				return daemonClient.PreviewContainerUpgrade(backgroundContext(), sessionID)
			})
			report("checking container upgrade", updates, dashboardIoUpdate(dashboardIoUpdateContainerUpgradePreview{
				sessionID: sessionID, upgradeOnly: upgradeOnly, preview: preview, err: err,
			}))
		}()
	case tui.DashboardActionUpgradeContainer:
		updates := c.dashboardIoTx
		guard := c.criticalOperations.Begin("submitting container upgrade")
		request := action.Request
		go func() {
			_, err := withDaemon(backgroundContext(), ConnectOrStart, func(daemonClient *client.DaemonClient) (struct{}, error) {
				return struct{}{}, daemonClient.UpgradeContainer(backgroundContext(), request)
			})
			guard.Release()
			report("queueing container upgrade", updates, dashboardIoUpdate(dashboardIoUpdateContainerUpgradeQueued{err: err}))
		}()
	case tui.DashboardActionRestartSession:
		session, ok := c.controller.State.Sessions.Get(action.SessionID)
		if !ok {
			return nil
		}
		request := c.beginLifecycleOperation(action.SessionID, tui.SessionOperationKindResuming)
		c.dashboard.SetNotice("Restarting session…")
		sessionID, acknowledge := action.SessionID, action.AcknowledgeContainerRecreation
		spawnLifecycleOperation(request, c.criticalOperations, func(_ *atomic.Bool, admission lifecycleSubmission) (daemon.LifecycleSuccess, error) {
			if _, err := waitLifecycleReceipt(backgroundContext(), admission, client.SessionLifecycleRequestRestart{
				SessionID: sessionID, AcknowledgeContainerRecreation: acknowledge,
			}); err != nil {
				return nil, err
			}
			return daemon.LifecycleSuccessResumed{ProfileID: session.LastProfile, TargetID: session.TargetTemplateID}, nil
		})
	case tui.DashboardActionNotificationSkip:
	case tui.DashboardActionOpen:
		c.openChatSession(action.SessionID)
	case tui.DashboardActionPinSession:
		if _, occupied := c.dashboard.PaneSession(action.Pane); !occupied {
			c.pinSessionIn(action.SessionID, action.Pane)
		} else {
			c.dashboard.SetNotice("That pane is no longer empty.")
		}
	case tui.DashboardActionUnpinSession:
		c.unpinSession(action.SessionID)
	case tui.DashboardActionSplitConversation:
		c.splitConversationPane(action.Pane, action.Direction)
	case tui.DashboardActionOpenSessionInSplit:
		c.openSessionInSplit(action.SessionID, action.Direction)
	case tui.DashboardActionSplitPane:
		c.splitEmptyPane(action.Direction)
	case tui.DashboardActionClosePane:
		c.closePane(action.Pane)
	case tui.DashboardActionConversationPanesChanged:
		if action.FocusMoved {
			// The keyboard is in a different conversation now, so the
			// in-flight-attach report and any selection follow it.
			c.syncOpeningSession()
			c.selection.Clear()
		}
		c.saveActiveWorkspaceLayout()
	case tui.DashboardActionResumeSession:
		startSessionLaunch(c, action)
	case tui.DashboardActionMoveSession:
		if action.PreparationRequestID != nil {
			startMovePreparation(c, action)
			return nil
		}
		return startMove(c, action)
	case tui.DashboardActionRetryMove:
		startMoveRetry(c, action.Operation)
	case tui.DashboardActionResumeMove:
		operation := action.Operation
		workspaceID := c.workspaceID
		if operation.RecoverySession != nil {
			workspaceID = operation.RecoverySession.WorkspaceID
		}
		startSessionLaunch(c, tui.DashboardActionResumeSession{
			WorkspaceID:        workspaceID,
			SessionID:          operation.Selection.SessionID,
			ProfileID:          operation.SourceProfileID,
			TargetTemplateID:   operation.SourceTargetTemplateID,
			AdditionalMounts:   operation.SourceAdditionalMounts,
			ResourceAllocation: operation.SourceResourceAllocation,
			DiscardQueue:       true,
		})
	case tui.DashboardActionSuspendPreflight:
		updates := c.dashboardIoTx
		sessionID := action.SessionID
		go func() {
			preflight, err := withDaemon(backgroundContext(), ConnectOrStart, func(daemonClient *client.DaemonClient) (core.SuspendPreflight, error) {
				return daemonClient.SuspendPreflight(backgroundContext(), sessionID, false, false)
			})
			report("checking workspace before suspend", updates, dashboardIoUpdate(dashboardIoUpdateSuspendPreflight{sessionID: sessionID, preflight: preflight, err: err}))
		}()
	case tui.DashboardActionSuspend:
		startSuspension(c, action.SessionID, action.Consent)
	case tui.DashboardActionCreateBundle:
		c.dashboard.SetNotice("Preparing project…")
		spawnCreateBundle(action.Sources, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionRemoveBundle:
		spawnRemoveBundle(action.BundleID, c.dashboardIoTx, c.criticalOperations)
	case tui.DashboardActionDestroyStopped:
		request := c.beginLifecycleOperation(action.SessionID, tui.SessionOperationKindDestroying)
		markActiveChatRetiring(c.chats[action.SessionID], action.SessionID)
		sessionID := action.SessionID
		spawnLifecycleOperation(request, c.criticalOperations, func(_ *atomic.Bool, admission lifecycleSubmission) (daemon.LifecycleSuccess, error) {
			if _, err := waitLifecycleReceipt(backgroundContext(), admission, client.SessionLifecycleRequestDestroyStopped{SessionID: sessionID}); err != nil {
				return nil, err
			}
			return daemon.LifecycleSuccessDestroyedStopped{}, nil
		})
	case tui.DashboardActionForceDestroy:
		request := c.beginLifecycleOperation(action.SessionID, tui.SessionOperationKindDestroying)
		markActiveChatRetiring(c.chats[action.SessionID], action.SessionID)
		sessionID := action.SessionID
		spawnLifecycleOperation(request, c.criticalOperations, func(_ *atomic.Bool, admission lifecycleSubmission) (daemon.LifecycleSuccess, error) {
			if _, err := waitLifecycleReceipt(backgroundContext(), admission, client.SessionLifecycleRequestForceDestroy{SessionID: sessionID}); err != nil {
				return nil, err
			}
			return daemon.LifecycleSuccessForceDestroyed{}, nil
		})
	case tui.DashboardActionCancelOperation:
		if operation := c.lifecycleOperations[action.SessionID]; operation != nil {
			operation.cancelled.Store(true)
		}
		sessionID := action.SessionID
		updates := c.dashboardIoTx
		go func() {
			_, err := withDaemon(backgroundContext(), ConnectOrStart, func(daemonClient *client.DaemonClient) (struct{}, error) {
				return struct{}{}, daemonClient.CancelLifecycle(backgroundContext(), sessionID)
			})
			report("cancelling a lifecycle operation", updates, dashboardIoUpdate(dashboardIoUpdateLifecycleCancellation{sessionID: sessionID, err: err}))
		}()
		c.dashboard.SetNotice(fmt.Sprintf("Cancelling %s for %s…", strings.ToLower(action.Kind.Label()), c.sessionNoticeName(sessionID)))
	}
	return nil
}

// startMove submits a prepared move the person confirmed.
func startMove(c *dashboardContext, action tui.DashboardActionMoveSession) error {
	sessionID := action.SessionID
	preparation := c.dashboard.TakeMovePreparation(sessionID)
	if preparation == nil {
		c.dashboard.SetFailureNotice("Move confirmation expired; prepare the move again.")
		return nil
	}
	c.dashboard.SetNotice(fmt.Sprintf("Moving %s…", c.sessionNoticeName(sessionID)))
	operationID := preparation.OperationID
	c.startupPrompts.beginMove(sessionID, &operationID)
	request := c.beginLifecycleOperation(sessionID, tui.SessionOperationKindMoving)
	c.dashboard.SetStandbyMoveOrigin(sessionID, composerMoveOrigin(&c.startupPrompts, sessionID))
	prepared, queue := *preparation, action.Queue
	spawnLifecycleOperation(request, c.criticalOperations, func(cancelled *atomic.Bool, admission lifecycleSubmission) (daemon.LifecycleSuccess, error) {
		outcome, err := runMove(cancelled, admission, sessionID, core.MoveSessionRequest{
			Preparation:             prepared,
			Queue:                   queue,
			AcknowledgeInterruption: true,
		})
		if err != nil {
			return nil, err
		}
		if outcome.Outcome == "failed" || outcome.Outcome == "cancelled" {
			return nil, fmt.Errorf("move %s: %s", outcome.Outcome, moveOutcomeDetail(outcome))
		}
		return daemon.LifecycleSuccessMoved{Outcome: outcome}, nil
	})
	return nil
}

// startMoveRetry prepares and submits a retried move, or reopens recovery
// for an operation whose queue was not admitted yet.
func startMoveRetry(c *dashboardContext, operation core.MoveOperation) {
	sessionID := operation.Selection.SessionID
	if session, ok := c.controller.State.Sessions.Get(sessionID); !operation.QueueAdmissionStarted && ok && session.State.IsActive() {
		c.dashboard.BeginMoveRecovery(operation)
		return
	}
	c.dashboard.SetNotice(fmt.Sprintf("Retrying move for %s…", c.sessionNoticeName(sessionID)))
	c.startupPrompts.beginMove(sessionID, nil)
	request := c.beginLifecycleOperation(sessionID, tui.SessionOperationKindMoving)
	c.dashboard.SetStandbyMoveOrigin(sessionID, composerMoveOrigin(&c.startupPrompts, sessionID))
	preparedUpdates := c.dashboardIoTx
	spawnLifecycleOperation(request, c.criticalOperations, func(cancelled *atomic.Bool, admission lifecycleSubmission) (daemon.LifecycleSuccess, error) {
		selection := operation.Selection
		queue := operation.Queue
		ctx := backgroundContext()
		preparation, err := withDaemon(ctx, ConnectOrStart, func(daemonClient *client.DaemonClient) (core.MovePreparation, error) {
			return daemonClient.PrepareMoveSession(ctx, selection)
		})
		if err != nil {
			return nil, err
		}
		report("binding move prompt origin", preparedUpdates, dashboardIoUpdate(dashboardIoUpdateMoveOriginPrepared{
			sessionID:       sessionID,
			moveOperationID: preparation.OperationID,
			operation:       cancelled,
		}))
		outcome, err := runMove(cancelled, admission, sessionID, core.MoveSessionRequest{
			Preparation:             preparation,
			Queue:                   &queue,
			AcknowledgeInterruption: true,
		})
		if err != nil {
			return nil, err
		}
		if outcome.Outcome == "failed" || outcome.Outcome == "cancelled" {
			return nil, fmt.Errorf("move retry %s (operation %s): %s", outcome.Outcome, operation.OperationID, moveOutcomeDetail(outcome))
		}
		return daemon.LifecycleSuccessMoved{Outcome: outcome}, nil
	})
}

// moveOutcomeDetail is a failed move's error, or a stand-in when it named
// none.
func moveOutcomeDetail(outcome core.MoveOutcome) string {
	if outcome.Error != nil {
		return *outcome.Error
	}
	return "no further details"
}

// composerMoveOrigin is the move a standby composer's prompts wait for.
func composerMoveOrigin(prompts *startupPrompts, sessionID string) *chat.ComposerMoveOrigin {
	generation, id, ok := prompts.moveOrigin(sessionID)
	if !ok {
		return nil
	}
	return &chat.ComposerMoveOrigin{Generation: generation, ID: id}
}

// runMove submits a move and waits for it, asking the daemon to cancel it
// once the person cancels the operation.
func runMove(cancelled *atomic.Bool, admission lifecycleSubmission, sessionID string, request core.MoveSessionRequest) (core.MoveOutcome, error) {
	ctx, stop := context.WithCancel(backgroundContext())
	defer stop()
	daemonClient, err := ConnectOrStart(ctx)
	if err != nil {
		return core.MoveOutcome{}, err
	}
	defer daemonClient.Close()
	receipt, err := admission.submit(ctx, daemonClient, client.SessionLifecycleRequestMove{Request: request})
	if err != nil {
		return core.MoveOutcome{}, err
	}
	type waited struct {
		reply client.DaemonReply
		err   error
	}
	done := make(chan waited, 1)
	go func() {
		reply, err := daemonClient.WaitSessionLifecycle(ctx, receipt)
		done <- waited{reply: reply, err: err}
	}()
	poll := time.NewTicker(50 * time.Millisecond)
	defer poll.Stop()
	for {
		if cancelled.Load() {
			stop()
			cancelCtx, cancelDone := context.WithTimeout(backgroundContext(), 10*time.Second)
			_, err := withDaemon(cancelCtx, ConnectOrStart, func(other *client.DaemonClient) (struct{}, error) {
				return struct{}{}, other.CancelLifecycle(cancelCtx, sessionID)
			})
			timedOut := cancelCtx.Err() != nil
			cancelDone()
			if timedOut {
				return core.MoveOutcome{}, fmt.Errorf("request move cancellation: %w", context.DeadlineExceeded)
			}
			if err != nil {
				return core.MoveOutcome{}, err
			}
			return core.MoveOutcome{}, fmt.Errorf("move cancellation requested for %s", sessionID)
		}
		select {
		case result := <-done:
			if result.err != nil {
				return core.MoveOutcome{}, result.err
			}
			return lifecycleMoveOutcome(result.reply)
		case <-poll.C:
		}
	}
}

// startMovePreparation begins read-only preparation from either wizard
// input or an attachment reply.
func startMovePreparation(c *dashboardContext, action tui.DashboardAction) {
	move, ok := action.(tui.DashboardActionMoveSession)
	if !ok || move.PreparationRequestID == nil {
		return
	}
	requestID := *move.PreparationRequestID
	updates := c.dashboardIoTx
	profileID, targetTemplateID, additionalMounts := move.ProfileID, move.TargetTemplateID, move.AdditionalMounts
	selection := core.MoveSelection{
		Oracle:                  move.Oracle,
		OracleChoice:            move.OracleChoice,
		Subagents:               move.Subagents,
		Consent:                 move.Consent,
		SkipKeepPaths:           move.SkipKeepPaths,
		SessionID:               move.SessionID,
		ProfileID:               &profileID,
		TargetTemplateID:        &targetTemplateID,
		AdditionalMounts:        &additionalMounts,
		ResourceAllocation:      move.ResourceAllocation,
		ClearResourceAllocation: move.ClearResourceAllocation,
	}
	sessionID := move.SessionID
	go func() {
		preparation, err := withDaemon(backgroundContext(), ConnectOrStart, func(daemonClient *client.DaemonClient) (core.MovePreparation, error) {
			return daemonClient.PrepareMoveSession(backgroundContext(), selection)
		})
		report("preparing a move", updates, dashboardIoUpdate(dashboardIoUpdateMovePrepared{
			sessionID: sessionID, requestID: requestID, preparation: preparation, err: err,
		}))
	}()
}

// resumeLaunchDestination is the session and target of a resume launch.
func resumeLaunchDestination(action tui.DashboardAction) (sessionID string, targetID string, err error) {
	if resume, ok := action.(tui.DashboardActionResumeSession); ok {
		return resume.SessionID, resume.TargetTemplateID, nil
	}
	return "", "", errors.New("repository preflight did not receive a resume launch")
}

func startResumeRepositoryPreflight(c *dashboardContext, launch tui.DashboardAction) error {
	generation := c.dashboard.SessionPreflightGeneration()
	sessionID, targetID, err := resumeLaunchDestination(launch)
	if err != nil {
		return err
	}
	c.dashboard.SetNotice("Checking checkpoint repository history…")
	spawnIO("checking checkpoint repositories", c.dashboardIoTx,
		func() (resumeRepositoryPreflightApply, error) {
			preflight, err := withDaemon(backgroundContext(), ConnectOrStart, func(daemonClient *client.DaemonClient) (core.ResumeRepositorySourcePreflight, error) {
				return daemonClient.ResumeRepositoryPreflight(backgroundContext(), sessionID, targetID)
			})
			return resumeRepositoryPreflightApply{preflight: preflight}, err
		},
		func(apply resumeRepositoryPreflightApply, err error) dashboardIoUpdate {
			return dashboardIoUpdateResumeRepositoryPreflight{generation: generation, launch: launch, apply: apply, err: err}
		})
	return nil
}

// startCreateSessionPreflight resolves the network plan for an isolated
// creation while the wizard remains visible. Every Git and network
// operation is supervised by the cancellable blocking worker; the terminal
// event loop only receives the finished plan.
func startCreateSessionPreflight(c *dashboardContext, launch tui.DashboardAction) {
	create, ok := launch.(tui.DashboardActionCreateSession)
	if !ok {
		failCreateSessionPreflight(c, "invalid session preflight request")
		return
	}
	target, ok := c.controller.Config.Targets[create.TargetTemplateID]
	if !ok {
		failCreateSessionPreflight(c, fmt.Sprintf("unknown target template %q", create.TargetTemplateID))
		return
	}
	if core.IsBareProjectTarget(target) {
		// Raw local targets have no network source plan and are validated
		// by the existing project-directory flow.
		c.dashboard.FinishSessionMountPreflight()
		startSessionLaunch(c, launch)
		return
	}
	request := client.NewSessionPreflightRequest{
		ProfileID:        create.ProfileID,
		WorkspaceID:      create.WorkspaceID,
		BundleID:         create.BundleID,
		TargetID:         create.TargetTemplateID,
		AdditionalMounts: create.AdditionalMounts,
		RemoteRepairs:    []core.LocalRemoteRepair{},
	}
	generation := c.dashboard.SessionPreflightGeneration()
	c.cancelSessionPreflight()
	c.dashboard.BeginRemoteSessionPreflight(generation)
	cancelled, _ := spawnCancellableIOWithToken(c.criticalOperations, "resolving session repositories", c.dashboardIoTx,
		func(cancelled *atomic.Bool) (remotePreflightOutcome, error) {
			reply, err := machineRequest(client.DaemonActionPreflightNewSession{Request: request}, cancelled)
			if err != nil {
				return nil, err
			}
			if preflight, ok := reply.(client.DaemonReplyNewSessionPreflight); ok {
				return remotePreflightOutcomeFrom(preflight.Preflight), nil
			}
			return nil, fmt.Errorf("unexpected new-session preflight reply: %#v", reply)
		},
		func(outcome remotePreflightOutcome, err error) dashboardIoUpdate {
			return dashboardIoUpdateRemotePreflight{generation: generation, launch: launch, outcome: outcome, err: err}
		})
	c.sessionPreflightCancel = &sessionPreflight{generation: generation, cancelled: cancelled}
}

// failCreateSessionPreflight ends a creation check that could not start, so
// the review offers Retry instead of waiting on a check that is not
// running.
func failCreateSessionPreflight(c *dashboardContext, failure string) {
	generation := c.dashboard.SessionPreflightGeneration()
	c.dashboard.ApplyRemoteSessionPreflight(generation, nil, errors.New(failure))
	c.dashboard.SetFailureNotice(failure)
}

func startSessionLaunch(c *dashboardContext, action tui.DashboardAction) {
	startSessionLaunchWithRepositoryPreflight(c, action, nil)
}

func startPreflightedSessionLaunch(c *dashboardContext, action tui.DashboardAction, repositoryPreflight core.ResumeRepositorySourceReceipt) {
	startSessionLaunchWithRepositoryPreflight(c, action, &repositoryPreflight)
}

func startSessionLaunchWithRepositoryPreflight(c *dashboardContext, action tui.DashboardAction, repositoryPreflight *core.ResumeRepositorySourceReceipt) {
	var workspaceAvailable bool
	switch launch := action.(type) {
	case tui.DashboardActionCreateSession:
		workspaceAvailable = launch.WorkspaceID != ""
	case tui.DashboardActionResumeSession:
		workspaceAvailable = launch.WorkspaceID != ""
	default:
		_, workspaceAvailable = c.dashboard.ActiveWorkspaceID()
	}
	if !workspaceAvailable {
		c.dashboard.SetNotice("Select a workspace before starting a session.")
		return
	}
	switch launch := action.(type) {
	case tui.DashboardActionCreateSession:
		if repositoryPreflight != nil {
			panic("a create launch carries no repository preflight")
		}
		c.dashboard.SetNotice(tui.DashboardActionAnnotateLaunchNotice(launch, "Preparing session launch…"))
		var goSave *goRecipeSave
		if remembered := c.dashboard.RememberGoLaunch(launch); remembered != nil {
			goSave = &goRecipeSave{directory: remembered.Directory, recipe: remembered.Recipe, global: remembered.SaveAsDefault}
		}
		spawnDashboardCreateSession(launch, goSave, c.dashboardIoTx, c.lifecycleUpdatesTx, c.criticalOperations)
	case tui.DashboardActionResumeSession:
		c.dashboard.SetNotice(resumeProgressNotice(c.sessionNoticeName(launch.SessionID), launch.ProfileID, launch.TargetTemplateID))
		// Resume places historical work in the workspace where the explicit
		// request began, even if another tab is selected later.
		request := c.beginLifecycleOperation(launch.SessionID, tui.SessionOperationKindResuming)
		c.dashboard.SetResumeDestination(launch.SessionID, launch.ProfileID, launch.TargetTemplateID)
		additionalMounts := launch.AdditionalMounts
		resume := client.ResumeSessionRequest{
			SessionID:           launch.SessionID,
			WorkspaceID:         launch.WorkspaceID,
			ProfileID:           launch.ProfileID,
			TargetTemplateID:    launch.TargetTemplateID,
			AdditionalMounts:    &additionalMounts,
			ResourceAllocation:  launch.ResourceAllocation,
			DiscardQueue:        launch.DiscardQueue,
			RepositoryPreflight: repositoryPreflight,
		}
		spawnLifecycleOperation(request, c.criticalOperations, func(_ *atomic.Bool, admission lifecycleSubmission) (daemon.LifecycleSuccess, error) {
			if _, err := waitLifecycleReceipt(backgroundContext(), admission, client.SessionLifecycleRequestResume{Request: resume}); err != nil {
				return nil, err
			}
			return daemon.LifecycleSuccessResumed{ProfileID: resume.ProfileID, TargetID: resume.TargetTemplateID}, nil
		})
	default:
		panic("mount preflight only carries create or resume actions")
	}
}

// markActiveChatRetiring tells the open chat, if it is this session's, that
// the session is being retired on purpose. The stop closes the relay feed,
// and the chat must read that as the expected end rather than a lost actor
// to chase.
func markActiveChatRetiring(activeChat *chat.ActiveChat, sessionID string) {
	if activeChat != nil && activeChat.SessionID() == sessionID {
		activeChat.SetSessionRetiring(true)
	}
}

// beginLifecycleOperation marks a session busy in the UI and hands back
// what runs its operation off the loop. The daemon owns lifecycle/recovery
// serialization.
func (c *dashboardContext) beginLifecycleOperation(sessionID string, kind tui.SessionOperationKind) lifecycleOperationRequest {
	c.cancelChatOpenFor(sessionID)
	c.dashboard.BeginSessionOperation(sessionID, kind, nil)
	// Park this session's composer without changing navigation. A Move can
	// belong to an unfocused pane; its draft must stay with it.
	if kind == tui.SessionOperationKindLaunching || kind == tui.SessionOperationKindResuming || kind == tui.SessionOperationKindMoving {
		if !c.dashboard.HasStandbyComposer(sessionID) {
			if active := c.chats[sessionID]; active != nil {
				if composer := active.TakeLiveComposer(); composer != nil {
					c.dashboard.AdoptStandbyComposer(sessionID, composer)
				}
			}
		}
		if kind != tui.SessionOperationKindMoving {
			c.dashboard.SetStandbyMoveOrigin(sessionID, nil)
		}
		// Resume is an explicit request to open a suspended conversation.
		if kind == tui.SessionOperationKindResuming {
			c.dashboard.SelectActiveSession(sessionID)
			c.dashboard.FocusPrompt()
		}
	}
	cancelled := new(atomic.Bool)
	noticeName := c.sessionNoticeName(sessionID)
	c.lifecycleOperations[sessionID] = &activeLifecycleOperation{
		retirement: new(chatRetirementCapture(sessionID, c.chats, c.attachments)),
		cancelled:  cancelled,
		kind:       kind,
		noticeName: noticeName,
	}
	return lifecycleOperationRequest{
		sessionID:   sessionID,
		sessionName: c.sessionNoticeName(sessionID),
		kind:        kind,
		cancelled:   cancelled,
		updates:     c.lifecycleUpdatesTx,
	}
}

// spawnDaemonRestart restarts the daemon from this surface, off the event
// loop.
//
// Stopping and starting a daemon takes seconds and holds a file lock, so it
// can never run on the render loop. The outcome is a sentence for the
// notice bar rather than a silent success, for the same reason `mj daemon
// restart` now names the build that came up: a restart that lands on
// someone else's build must not look like the one that was asked for.
func spawnDaemonRestart(c *dashboardContext) {
	updates := c.dashboardIoTx
	guard := c.criticalOperations.Begin("restarting the Mjolnir daemon")
	c.dashboard.SetNotice("Restarting the Mjolnir daemon...")
	go func() {
		defer guard.Release()
		update := dashboardIoUpdateDaemonRestarted{}
		restarted, err := RestartDaemon(backgroundContext())
		switch {
		case err != nil:
			update.err = fmt.Errorf("Could not restart the Mjolnir daemon: %w", err)
		case restarted.RunsThisBuild != nil && *restarted.RunsThisBuild:
			update.sentence = fmt.Sprintf("Mjolnir daemon restarted as PID %d, running this build.", restarted.Pid)
		default:
			update.sentence = fmt.Sprintf("Mjolnir daemon restarted as PID %d.", restarted.Pid)
		}
		if err := updates.Send(update); err != nil {
			slog.Warn("daemon restart outcome dropped because the surface closed", "error", err)
		}
	}()
}

// spawnDaemonPlacement moves the daemon where a saved `[daemon]` setting
// says, off the event loop.
//
// Startup owns that decision: it compares where the daemon runs with the
// configuration and hands off to a daemon launched in the right place, so
// workers keep running. This only asks it now rather than at the next
// start.
func spawnDaemonPlacement(c *dashboardContext) {
	updates := c.dashboardIoTx
	guard := c.criticalOperations.Begin("moving the Mjolnir daemon")
	c.dashboard.SetNotice("Moving the Mjolnir daemon where Settings say...")
	go func() {
		defer guard.Release()
		update := dashboardIoUpdateDaemonRestarted{sentence: "The Mjolnir daemon runs where Settings say."}
		daemonClient, err := ConnectOrStart(backgroundContext())
		if err != nil {
			update = dashboardIoUpdateDaemonRestarted{err: fmt.Errorf("Could not move the Mjolnir daemon: %w", err)}
		} else {
			daemonClient.Close()
		}
		if err := updates.Send(update); err != nil {
			slog.Warn("daemon move outcome dropped because the surface closed", "error", err)
		}
	}()
}

// spawnWebRequest owns each web viewer dialog request, cancels stale
// polling, and observes background task failures.
func spawnWebRequest(c *dashboardContext, action tui.DashboardAction) {
	if c.webRequestCancel != nil {
		c.webRequestCancel()
	}
	c.webRequestCancel = nil
	c.webRequestGeneration++
	generation := c.webRequestGeneration
	cancellation, cancel := contextWithCancel()
	c.webRequestCancel = cancel
	updates := c.dashboardIoTx
	_, inspecting := action.(tui.DashboardActionInspectWebListener)
	task := core.Spawn(func() error {
		daemonClient, err := client.ConnectExisting(cancellation)
		if err != nil {
			return err
		}
		defer daemonClient.Close()
		switch action := action.(type) {
		case tui.DashboardActionRecoverWebViewer:
			if err := daemonClient.RecoverWebViewer(cancellation, action.Recovery); err != nil {
				// Another client may have recovered it since this dialog
				// loaded.
				access, accessErr := daemonClient.WebAccess(cancellation)
				if accessErr != nil {
					return accessErr
				}
				if _, failed := access.(client.WebViewerAccessFailed); failed {
					return err
				}
			}
		case tui.DashboardActionInspectWebListener:
			processes, err := daemonClient.InspectWebListener(cancellation)
			if cancellation.Err() != nil {
				return nil
			}
			return updates.Send(dashboardIoUpdateWebListeners{generation: generation, processes: processes, err: err})
		}
		for {
			access, err := daemonClient.WebAccess(cancellation)
			if err != nil {
				return err
			}
			_, starting := access.(client.WebViewerAccessStarting)
			if cancellation.Err() != nil {
				return nil
			}
			if err := updates.Send(dashboardIoUpdateWebAccess{generation: generation, access: access}); err != nil {
				return err
			}
			if !starting {
				return nil
			}
			select {
			case <-cancellation.Done():
				return nil
			case <-time.After(250 * time.Millisecond):
			}
		}
	})
	go func() {
		result, joinErr := task.Join(backgroundContext())
		if cancellation.Err() != nil {
			// Cancelled: the request was dropped on purpose.
			if joinErr == nil && result != nil {
				slog.Warn("web viewer request failed during cancellation", "error", result)
			} else if joinErr != nil {
				slog.Warn("web viewer request task failed during cancellation", "error", joinErr)
			}
			return
		}
		err := result
		if joinErr != nil {
			err = fmt.Errorf("web viewer request task failed: %w", joinErr)
		}
		if err == nil {
			return
		}
		message := fmt.Sprintf("Could not complete the web viewer request: %s", errorText(err))
		slog.Warn("web viewer request failed", "error", message)
		var update dashboardIoUpdate = dashboardIoUpdateWebAccessError{generation: generation, message: message}
		if inspecting {
			update = dashboardIoUpdateWebListeners{generation: generation, err: errors.New(message)}
		}
		report("web viewer request", updates, update)
	}()
}

// contextWithCancel is a fresh CancellationToken with its drop guard.
func contextWithCancel() (context.Context, context.CancelFunc) {
	return context.WithCancel(backgroundContext())
}

func lifecycleMoveOutcome(reply client.DaemonReply) (core.MoveOutcome, error) {
	if outcome, ok := reply.(client.DaemonReplyMoveOutcome); ok {
		return outcome.Outcome, nil
	}
	return core.MoveOutcome{}, errors.New("daemon returned an unexpected move reply")
}

func spawnNotificationDismissal(c *dashboardContext, action tui.DashboardAction) {
	dismissal, ok := action.(tui.DashboardActionDismissNotification)
	if !ok {
		return
	}
	tracker := c.criticalOperations
	sessionID, target := dismissal.SessionID, dismissal.Target
	spawnAsyncJob(&tracker, "notification dismissal", c.dashboardIoTx, 30*time.Second,
		func(ctx context.Context) (uint64, error) { return requestNotificationDismissal(ctx, sessionID, target) },
		func(ordinal uint64, err error) dashboardIoUpdate {
			return dashboardIoUpdateNotificationDismissed{sessionID: sessionID, target: target, ordinal: ordinal, err: err}
		})
}

// dispatchRestart is the test-only dispatch_restart: one restart request
// with its container acknowledgement.
func dispatchRestart(ctx context.Context, daemonClient *client.DaemonClient, sessionID string, acknowledgeContainerRecreation bool) error {
	return daemonClient.RestartSessionWithAck(ctx, sessionID, acknowledgeContainerRecreation)
}

// requestNotificationDismissal asks the daemon to dismiss one session's
// notification; the single and the workspace-wide dismissal both send it.
func requestNotificationDismissal(ctx context.Context, sessionID string, target core.NotificationDismissal) (uint64, error) {
	daemonClient, err := client.ConnectExisting(ctx)
	if err != nil {
		return 0, err
	}
	defer daemonClient.Close()
	reply, err := daemonClient.Request(ctx, client.DaemonActionDismissNotification{SessionID: sessionID, Target: target})
	if err != nil {
		return 0, err
	}
	if ordinal, ok := reply.(client.DaemonReplyOrdinal); ok {
		return ordinal.Ordinal, nil
	}
	return 0, fmt.Errorf("unexpected dismissal reply: %#v", reply)
}
