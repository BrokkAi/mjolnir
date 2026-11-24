// Port of the tests in mj-cli/src/dashboard/io.rs.

package cli

import (
	"context"
	"encoding/json"
	"fmt"
	"net"
	"net/netip"
	"os"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/controller"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/daemon"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// Hard-won: #1507: quit waited for checkpoint completion and cancelled accepted lifecycle work.
func TestLifecycleQuitProtectsSubmissionButNotDaemonCompletion(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	check(t, err)
	defer listener.Close()
	metadata := client.DaemonMetadata{
		ProtocolVersion: client.ProtocolVersion,
		Pid:             uint32(os.Getpid()),
		Address:         listenerAddress(listener),
		Token:           "lifecycle-acceptance-test",
		StartedAt:       "test",
		BuildVersion:    core.PackageVersion,
		InstanceID:      "lifecycle-acceptance-test",
		Placement:       client.DaemonPlacement{Kind: client.DaemonPlacementKindHost},
	}
	sessionID := "0123456789abcdef0123456789abcdef"
	receipt := client.LifecycleReceipt{SessionID: sessionID, OperationID: "accepted-suspension"}
	submitted := make(chan struct{})
	accept := make(chan struct{})
	waiting := make(chan struct{})
	complete := make(chan struct{})
	served := make(chan error, 1)
	go func() {
		served <- func() error {
			stream, err := listener.Accept()
			if err != nil {
				return err
			}
			defer stream.Close()
			request, err := client.ReadFrame[client.RequestEnvelope](stream)
			if err != nil {
				return err
			}
			start, ok := request.Action.(client.DaemonActionStartSessionLifecycle)
			if !ok {
				return fmt.Errorf("first request %#v, want a lifecycle start", request.Action)
			}
			if _, ok := start.Request.(client.SessionLifecycleRequestSuspend); !ok {
				return fmt.Errorf("lifecycle request %#v, want a suspend", start.Request)
			}
			close(submitted)
			<-accept
			if err := client.WriteFrame(stream, client.ResponseEnvelope{
				ProtocolVersion: request.ProtocolVersion,
				RequestID:       request.RequestID,
				Result:          client.DaemonOk[client.DaemonReply](client.DaemonReplyLifecycleAccepted{Receipt: receipt}),
			}); err != nil {
				return err
			}
			request, err = client.ReadFrame[client.RequestEnvelope](stream)
			if err != nil {
				return err
			}
			wait, ok := request.Action.(client.DaemonActionWaitSessionLifecycle)
			if !ok || wait.Receipt != receipt {
				return fmt.Errorf("second request %#v, want a wait for %#v", request.Action, receipt)
			}
			close(waiting)
			<-complete
			return client.WriteFrame(stream, client.ResponseEnvelope{
				ProtocolVersion: request.ProtocolVersion,
				RequestID:       request.RequestID,
				Result:          client.DaemonOk[client.DaemonReply](client.DaemonReplyDone{}),
			})
		}()
	}()
	tracker, _ := client.NewCriticalOperationTracker()
	cancelled := new(atomic.Bool)
	results := core.NewUnbounded[dashboardLifecycleUpdate]()
	spawnLifecycleOperation(
		lifecycleOperationRequest{
			sessionID:   sessionID,
			sessionName: "held suspend",
			kind:        tui.SessionOperationKindSuspending,
			cancelled:   cancelled,
			updates:     results,
		},
		tracker,
		func(_ *atomic.Bool, admission lifecycleSubmission) (daemon.LifecycleSuccess, error) {
			return blockOnResult(func(ctx context.Context) (daemon.LifecycleSuccess, error) {
				daemonClient, err := client.DaemonClientConnect(ctx, metadata)
				if err != nil {
					return nil, err
				}
				defer daemonClient.Close()
				receipt, err := admission.submit(ctx, daemonClient, client.SessionLifecycleRequestSuspend{
					SessionID:           sessionID,
					DiscardUntracked:    core.UntrackedConsent{Kind: core.ConsentAny},
					AbortOperations:     core.OperationConsent{Kind: core.ConsentNone},
					WithoutCheckpoint:   false,
					ConfirmNoCheckpoint: false,
				})
				if err != nil {
					return nil, err
				}
				if _, err := daemonClient.WaitSessionLifecycle(ctx, receipt); err != nil {
					return nil, err
				}
				return daemon.LifecycleSuccessClosed{}, nil
			})
		},
	)
	select {
	case <-submitted:
	case <-time.After(30 * time.Second):
		t.Fatal("the lifecycle was not submitted")
	}
	if len(tracker.Blockers()) == 0 {
		t.Fatal("quit must retain an unaccepted request")
	}
	tracker.CancelAll()
	if cancelled.Load() {
		t.Fatal("quitting must not cancel the lifecycle")
	}
	close(accept)
	select {
	case <-waiting:
	case <-time.After(30 * time.Second):
		t.Fatal("the lifecycle was not followed")
	}
	if blockers := tracker.Blockers(); len(blockers) != 0 {
		t.Fatalf("daemon completion must not hold quit: %v", blockers)
	}
	if _, ok, _ := results.TryRecv(); ok {
		t.Fatal("the operation is still executing")
	}
	close(complete)
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	result, ok := results.Recv(ctx)
	if !ok {
		t.Fatal("the lifecycle reported nothing")
	}
	if _, closed := result.success.(daemon.LifecycleSuccessClosed); result.failure != nil || !closed {
		t.Fatalf("lifecycle result %#v, failure %v", result.success, result.failure)
	}
	check(t, <-served)
}

func TestSavedBundleWaitsForRuntimeIdentityWithoutInstallingOldConfig(t *testing.T) {
	completion := dashboardIoUpdateCreatedBundle{created: createdBundleUpdate{
		bundleID: "created",
		bundle:   core.ProjectBundle{PrimaryRepo: "created", Repositories: []core.ProjectRepository{}},
	}}
	ctrl := controller.Controller{Config: core.DefaultConfig(), State: core.NewStateState()}
	if !awaitingRuntime(completion, &ctrl) {
		t.Fatal("a bundle the feed has not delivered must wait")
	}
	ctrl.Config.Theme = core.ThemeChoiceFixed{Value: core.UiThemeLight}
	ctrl.Config.Bundles["created"] = core.ProjectBundle{PrimaryRepo: "created", Repositories: []core.ProjectRepository{}}
	if awaitingRuntime(completion, &ctrl) {
		t.Fatal("the delivered bundle must end the wait")
	}
	if ctrl.Config.Theme != core.ThemeChoice(core.ThemeChoiceFixed{Value: core.UiThemeLight}) {
		t.Fatalf("theme %#v, want the runtime's light theme", ctrl.Config.Theme)
	}
	replaced := ctrl.Config.Bundles["created"]
	replaced.PrimaryRepo = "replaced"
	ctrl.Config.Bundles["created"] = replaced
	if !awaitingRuntime(completion, &ctrl) {
		t.Fatal("the same id cannot authorize a different saved project")
	}
}

func TestImportedSessionFollowupWaitsForBothRuntimeRecordAndBundle(t *testing.T) {
	session := lifecycleSession("imported", "workspace", core.SessionStateStopped)
	completion := dashboardIoUpdateImportedSessionApplied{applied: importedDashboardSessionApply{
		verdict:         client.SessionVerdict{},
		harness:         "codex",
		nativeSessionID: "native",
		session:         session,
		bundle:          core.ProjectBundle{PrimaryRepo: "project", Repositories: []core.ProjectRepository{}},
	}}
	ctrl := controller.Controller{Config: core.DefaultConfig(), State: core.NewStateState()}
	if !awaitingRuntime(completion, &ctrl) {
		t.Fatal("an import the feed has not delivered must wait")
	}
	newer := session
	newer.SessionTitleOverride = pointer("newer runtime title")
	ctrl.State.Sessions.Insert(newer.ID, newer)
	if !awaitingRuntime(completion, &ctrl) {
		t.Fatal("the record alone must not end the wait")
	}
	ctrl.Config.Bundles[session.BundleID] = core.ProjectBundle{PrimaryRepo: "project", Repositories: []core.ProjectRepository{}}
	if awaitingRuntime(completion, &ctrl) {
		t.Fatal("the delivered bundle must end the wait")
	}
	if title := ctrl.State.Sessions.GetShared("imported").ListedTitle(); title != "newer runtime title" {
		t.Fatalf("title %q, want the runtime's newer title", title)
	}
}

func TestUnconfirmedRegistrationDoesNotOfferToReplayAcceptedCreation(t *testing.T) {
	completion := dashboardIoUpdateCreateSession{update: dashboardCreateSessionUpdateRegistered{
		registered: &registeredDashboardSession{
			retryLaunch:             tui.DashboardActionNone{},
			session:                 lifecycleSession("created", "workspace", core.SessionStateProvisioning),
			rememberedContainerSize: nil,
			cancelled:               new(atomic.Bool),
		},
	}}
	if _, ok := runtimeWaitExpired(completion).(dashboardIoUpdateRuntimeConfirmationMissing); !ok {
		t.Fatalf("expired registration %#v, want RuntimeConfirmationMissing", runtimeWaitExpired(completion))
	}
}

// lifecycleTestState is a State holding records (Rust's `State { sessions,
// ..State::default() }`).
func lifecycleTestState(records ...core.SessionRecord) core.StateState {
	state := core.NewStateState()
	for _, record := range records {
		state.Sessions.Insert(record.ID, record)
	}
	return state
}

// Hard-won: fa50a0ac: launch verification found lifecycle notices identifying titled sessions by ID.
func TestNoticesNameATitledSessionByItsTitleAndOthersByShortID(t *testing.T) {
	titled := lifecycleSession("5e0fb24c-titled", "default", core.SessionStateStopped)
	titled.SessionTitleOverride = pointer("Fix the parser")
	untitled := lifecycleSession("a1a8109b-untitled", "default", core.SessionStateStopped)
	state := lifecycleTestState(titled, untitled)
	for _, want := range []struct{ id, name string }{
		{"5e0fb24c-titled", "Fix the parser"},
		{"a1a8109b-untitled", "a1a8109b"},
		{"0badc0de-gone", "0badc0de"},
	} {
		if got := sessionNoticeName(&state, want.id); got != want.name {
			t.Fatalf("notice name of %s: %q, want %q", want.id, got, want.name)
		}
	}
}

// A session the dashboard created has only the title it was created with
// ("project via fake"), which the session list shows; "Launching" and "is
// ready" named it by id (launch finding R5-5).
// Hard-won: b67f7811: lifecycle notices lost the creation title when the dashboard record reloaded.
func TestNoticesNameAnUnnamedSessionByTheTitleItWasCreatedWith(t *testing.T) {
	created := lifecycleSession("036b869b-created", "default", core.SessionStateRunning)
	created.Title = "project via fake"
	state := lifecycleTestState(created)
	if got := sessionNoticeName(&state, "036b869b-created"); got != "project via fake" {
		t.Fatalf("notice name %q, want the creation title", got)
	}
}

// The daemon's runtime snapshot drops a destroyed session before the
// lifecycle reload lands, so the completion notice cannot find the record
// ("Permanently destroyed suspended session 5590965c" for "gamma", launch
// finding R5-4). It uses the name taken when the operation began; a record
// that is still there wins, because it has the newest name.
// Hard-won: b67f7811: a destroyed record disappeared before its completion notice could name it.
func TestALifecycleNoticeKeepsTheNameTakenWhenTheOperationBegan(t *testing.T) {
	empty := core.NewStateState()
	if got := lifecycleNoticeName(&empty, "5590965c-gamma", pointer("gamma")); got != "gamma" {
		t.Fatalf("name %q, want the name taken at start", got)
	}
	if got := lifecycleNoticeName(&empty, "5590965c-gamma", nil); got != "5590965c" {
		t.Fatalf("name %q, want the short id", got)
	}
	renamed := lifecycleSession("5590965c-gamma", "default", core.SessionStateStopped)
	renamed.SessionTitleOverride = pointer("gamma two")
	state := lifecycleTestState(renamed)
	if got := lifecycleNoticeName(&state, "5590965c-gamma", pointer("gamma")); got != "gamma two" {
		t.Fatalf("name %q, want the record's newer name", got)
	}
}

// setupTestJSON is serde_json::to_string of a config.
func setupTestJSON(t *testing.T, config core.Config) string {
	t.Helper()
	encoded, err := json.Marshal(config)
	check(t, err)
	return string(encoded)
}

// setupTestValue is serde_json::to_value of a config, as a generic object.
func setupTestValue(t *testing.T, config core.Config) map[string]any {
	t.Helper()
	var value map[string]any
	check(t, json.Unmarshal([]byte(setupTestJSON(t, config)), &value))
	return value
}

// setupTestText is a generic JSON value's text (Rust's Value::to_string).
func setupTestText(t *testing.T, value any) string {
	t.Helper()
	encoded, err := json.Marshal(value)
	check(t, err)
	return string(encoded)
}

func setupTestRead(t *testing.T, path string) string {
	t.Helper()
	contents, err := os.ReadFile(path)
	check(t, err)
	return string(contents)
}

func setupTestLoad(t *testing.T, path string) core.Config {
	t.Helper()
	config, err := core.ConfigLoadFrom(path)
	check(t, err)
	return config
}

// Hard-won: 8d16bf96: configured API-key references were treated as missing and blocked unrelated Settings saves.
func TestSetupSaveResolvesAPIKeyReferencesAndRejectsMissingKeysBeforeWriting(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "config.toml")
	home := filepath.Join(directory, "codex")
	check(t, os.Mkdir(home, 0o755))
	// A key name no test runner exports: a profile that does not set its
	// provider's key inherits it from the process environment.
	check(t, os.WriteFile(
		filepath.Join(home, "config.toml"),
		[]byte("model_provider = 'deepseek'\n[model_providers.deepseek]\nname = 'DeepSeek'\nbase_url = 'https://api.deepseek.com/v1'\nenv_key = 'MJ_TEST_SETUP_PROVIDER_KEY'\nwire_api = 'responses'\n"),
		0o644,
	))
	check(t, os.WriteFile(filepath.Join(directory, "secrets.toml"), []byte("MJ_TEST_SETUP_PROVIDER_KEY = 'test-secret'\n"), 0o644))
	state := core.NewStateState()
	core.WithSecretResolver(core.SecretResolverBeside(path), func() struct{} {
		value := setupTestValue(t, core.DefaultConfig())
		value["profiles"] = map[string]any{"deepseek": map[string]any{
			"kind": "codex", "home": home,
			"environment": map[string]any{"MJ_TEST_SETUP_PROVIDER_KEY": map[string]any{"from_secret": "MJ_TEST_SETUP_PROVIDER_KEY"}},
		}}
		var original core.Config
		check(t, json.Unmarshal([]byte(setupTestText(t, value)), &original))
		check(t, original.SaveTo(path))
		originalJSON := setupTestJSON(t, original)
		value["notify"] = map[string]any{"bell": !original.Notify.Bell}
		saved, err := client.SaveAt(path, originalJSON, setupTestText(t, value), &state)
		check(t, err)
		if saved.Notify.Bell != !original.Notify.Bell {
			t.Fatalf("bell %v, want %v", saved.Notify.Bell, !original.Notify.Bell)
		}
		if key, _ := saved.Profiles["deepseek"].Environment.Get("MJ_TEST_SETUP_PROVIDER_KEY"); key != "test-secret" {
			t.Fatalf("provider key %q, want the resolved secret", key)
		}
		before := setupTestRead(t, path)
		if strings.Contains(before, "test-secret") {
			t.Fatalf("the secret was written to config.toml:\n%s", before)
		}
		if !strings.Contains(before, "from_secret") {
			t.Fatalf("the secret reference was not written:\n%s", before)
		}

		value["profiles"].(map[string]any)["deepseek"].(map[string]any)["environment"] = map[string]any{}
		_, err = client.SaveAt(path, originalJSON, setupTestText(t, value), &state)
		if err == nil || !strings.Contains(err.Error(), "authenticates with MJ_TEST_SETUP_PROVIDER_KEY") {
			t.Fatalf("missing key error %v", err)
		}
		if after := setupTestRead(t, path); after != before {
			t.Fatalf("a refused save wrote config.toml:\n%s", after)
		}

		// A profile the edit leaves alone does not hold up the save, even
		// when it cannot start because its secret is gone.
		check(t, os.WriteFile(filepath.Join(directory, "secrets.toml"), nil, 0o644))
		unready := setupTestLoad(t, path)
		if unready.Profiles["deepseek"].EnsureReady("deepseek") == nil {
			t.Fatal("a profile without its secret must not be ready")
		}
		unreadyJSON := setupTestJSON(t, unready)
		value = setupTestValue(t, unready)
		value["notify"] = map[string]any{"bell": !unready.Notify.Bell}
		saved, err = client.SaveAt(path, unreadyJSON, setupTestText(t, value), &state)
		check(t, err)
		if saved.Notify.Bell != !unready.Notify.Bell {
			t.Fatalf("bell %v, want %v", saved.Notify.Bell, !unready.Notify.Bell)
		}
		if after := setupTestRead(t, path); !strings.Contains(after, "from_secret") {
			t.Fatalf("the secret reference was lost:\n%s", after)
		}
		return struct{}{}
	})
}

func TestSetupSaveRefusesToRemoveAnActiveSessionsTargetWithoutWriting(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "config.toml")
	original := core.DefaultConfig()
	original.Targets["podman"] = core.TargetTemplateLocalBare{}
	check(t, original.SaveTo(path))
	before, err := os.ReadFile(path)
	check(t, err)
	session := lifecycleSession("session-1", "default", core.SessionStateRunning)
	state := lifecycleTestState(session)
	updated := setupTestClone(t, original)
	updated.Targets = map[string]core.TargetTemplate{}
	_, err = client.SaveAt(path, setupTestJSON(t, original), setupTestJSON(t, updated), &state)
	if err == nil || !strings.Contains(err.Error(), "running session") {
		t.Fatalf("removal error %v", err)
	}
	if after, err := os.ReadFile(path); err != nil || string(after) != string(before) {
		t.Fatalf("a refused save wrote config.toml (%v):\n%s", err, after)
	}
	// An unrelated preference remains editable even while a session needs
	// repair.
	updated = setupTestClone(t, original)
	updated.Advanced.DetailedActivityClocks = true
	_, err = client.SaveAt(path, setupTestJSON(t, original), setupTestJSON(t, updated), &state)
	check(t, err)
	if !setupTestLoad(t, path).Advanced.DetailedActivityClocks {
		t.Fatal("the unrelated preference was not saved")
	}
}

// setupTestClone is Rust's Config::clone: a copy whose maps are its own, so
// editing the copy's targets, profiles or projects leaves the original as it
// was. Map values are plain values that edits replace rather than mutate.
func setupTestClone(t *testing.T, config core.Config) core.Config {
	t.Helper()
	clone := config
	clone.Profiles = maps.Clone(config.Profiles)
	clone.Bundles = maps.Clone(config.Bundles)
	clone.Machines = maps.Clone(config.Machines)
	clone.Targets = maps.Clone(config.Targets)
	clone.DefaultTargets = maps.Clone(config.DefaultTargets)
	clone.DisabledTargets = maps.Clone(config.DisabledTargets)
	return clone
}

// Launch finding R3-3: saving only the Jev checkbox wrote the built-in
// `[targets.docker]` and `[targets.podman]` blocks into config.toml. The
// dialog edits the effective config, which includes them; the file must
// keep only what the user set.
// Hard-won: f15ebc51: saving unrelated Setup settings persisted built-in targets and made Doctor misreport Docker.
func TestSetupSaveDoesNotWriteBuiltInTargetsTheUserNeverConfigured(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "config.toml")
	check(t, core.DefaultConfig().SaveTo(path))
	original := core.DefaultConfig().WithLocalTargets()
	edited := setupTestClone(t, original)
	edited.Jev.Enabled = false
	state := core.NewStateState()
	saved, err := client.SaveAt(path, setupTestJSON(t, original), setupTestJSON(t, edited), &state)
	check(t, err)
	if saved.Jev.Enabled {
		t.Fatal("the Jev edit was not saved")
	}
	text := setupTestRead(t, path)
	if !strings.Contains(text, "[jev]") {
		t.Fatalf("config.toml has no [jev]:\n%s", text)
	}
	if strings.Contains(text, "[targets.") {
		t.Fatalf("config.toml has built-in targets:\n%s", text)
	}
	if targets := setupTestLoad(t, path).Targets; len(targets) != 0 {
		t.Fatalf("targets %v, want none", targets)
	}
}

// Launch campaign finding A-3: choosing "Follows the terminal" in Settings
// removes `symbols` from `[advanced]` instead of leaving the old value.
func TestSetupSaveRemovesTheSymbolsKeyWhenItReturnsToUnset(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "config.toml")
	original := core.DefaultConfig()
	ascii := core.SymbolSetAscii
	original.Advanced.Symbols = &ascii
	check(t, original.SaveTo(path))
	if !strings.Contains(setupTestRead(t, path), "symbols") {
		t.Fatal("the symbols key was not written")
	}
	edited := setupTestClone(t, original)
	edited.Advanced.Symbols = nil
	state := core.NewStateState()
	_, err := client.SaveAt(path, setupTestJSON(t, original), setupTestJSON(t, edited), &state)
	check(t, err)
	if text := setupTestRead(t, path); strings.Contains(text, "symbols") {
		t.Fatalf("the symbols key was kept:\n%s", text)
	}
	if symbols := setupTestLoad(t, path).Advanced.Symbols; symbols != nil {
		t.Fatalf("symbols %v, want none", *symbols)
	}
}

func TestSettingsCanOverrideAnImplicitLocalTargetWithoutASetupFile(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "config.toml")
	original := core.DefaultConfig().WithLocalTargets()
	edited := setupTestClone(t, original)
	target, ok := edited.Targets["docker"]
	if !ok {
		t.Fatal("the local targets have no docker target")
	}
	if docker, ok := target.(core.TargetTemplateLocalDocker); ok {
		docker.ContainerTemplate.Image = "example.test/custom:latest"
		edited.Targets["docker"] = docker
	}
	state := core.NewStateState()
	saved, err := client.SaveAt(path, setupTestJSON(t, original), setupTestJSON(t, edited), &state)
	check(t, err)
	if !reflect.DeepEqual(saved.Targets["docker"], edited.Targets["docker"]) {
		t.Fatalf("saved docker target %#v, want %#v", saved.Targets["docker"], edited.Targets["docker"])
	}
	if loaded := setupTestLoad(t, path).Targets["docker"]; !reflect.DeepEqual(loaded, edited.Targets["docker"]) {
		t.Fatalf("loaded docker target %#v, want %#v", loaded, edited.Targets["docker"])
	}
}

func TestSetupSaveMergesUnrelatedEditsAndRefusesConflictsOrInvalidValues(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "config.toml")
	original := core.DefaultConfig()
	check(t, original.SaveTo(path))
	edited := setupTestClone(t, original)
	edited.SessionsSide = core.SessionsSideRight
	edited.Theme = core.ThemeChoiceFixed{Value: core.UiThemeLight}
	_, _, err := core.ConfigUpdateTo(path, func(current *core.Config) (struct{}, error) {
		current.Advanced.DetailedActivityClocks = true
		return struct{}{}, nil
	})
	check(t, err)
	originalJSON := setupTestJSON(t, original)
	state := core.NewStateState()
	saved, err := client.SaveAt(path, originalJSON, setupTestJSON(t, edited), &state)
	check(t, err)
	if saved.SessionsSide != core.SessionsSideRight {
		t.Fatalf("sessions side %v, want right", saved.SessionsSide)
	}
	if saved.Theme != core.ThemeChoice(core.ThemeChoiceFixed{Value: core.UiThemeLight}) {
		t.Fatalf("theme %#v, want light", saved.Theme)
	}
	if !saved.Advanced.DetailedActivityClocks {
		t.Fatal("the other client's edit was lost")
	}
	if loaded := setupTestLoad(t, path); !reflect.DeepEqual(loaded, saved) {
		t.Fatalf("loaded config differs from the saved one:\n%#v\nwant\n%#v", loaded, saved)
	}

	conflicting := setupTestClone(t, original)
	conflicting.Phone.Bind = "127.0.0.1:1234"
	_, _, err = core.ConfigUpdateTo(path, func(current *core.Config) (struct{}, error) {
		current.Phone.Bind = "127.0.0.1:5678"
		return struct{}{}, nil
	})
	check(t, err)
	before, err := os.ReadFile(path)
	check(t, err)
	_, err = client.SaveAt(path, originalJSON, setupTestJSON(t, conflicting), &state)
	if err == nil || !strings.Contains(err.Error(), "another client") {
		t.Fatalf("conflict error %v", err)
	}
	invalid := setupTestClone(t, original)
	invalid.Review.Reviewers = []core.SubagentChoice{{Model: "", Effort: nil}}
	if _, err := client.SaveAt(path, originalJSON, setupTestJSON(t, invalid), &state); err == nil {
		t.Fatal("an invalid reviewer was saved")
	}
	if after, err := os.ReadFile(path); err != nil || string(after) != string(before) {
		t.Fatalf("a refused save wrote config.toml (%v):\n%s", err, after)
	}
}

// Hard-won: 0bd61a289: summary hydration exhausted the blocking pool while awaiting nested metadata reads.
// Hard-won: 99572842f: an unowned daemon fixture took the slow Windows stale-record path.
func TestStoredSummaryBurstsLeaveTheBlockingPoolAvailable(t *testing.T) {
	if rerunInIsolatedChild(t, "MJ_TEST_SUMMARY_BLOCKING_POOL") {
		return
	}
	// A running daemon owns its store. Without that ownership, startup
	// probes this record's process and endpoint before refusing it.
	guard, err := controller.ControllerStoreGuardAcquire()
	check(t, err)
	defer guard.Drop()
	// Refuse before startup can launch anything. Even this refusal needs
	// connection metadata, just as successful summary reads do.
	metadata := client.DaemonMetadata{
		ProtocolVersion: client.ProtocolVersion + 1,
		Pid:             uint32(os.Getpid()),
		Address:         netip.MustParseAddrPort("127.0.0.1:1"),
		Token:           "summary-pool-test",
		StartedAt:       "2026-11-04T00:00:00Z",
		BuildVersion:    core.PackageVersion,
		InstanceID:      "",
		Placement:       client.DaemonPlacement{Kind: client.DaemonPlacementKindHost},
	}
	encoded, err := json.Marshal(metadata)
	check(t, err)
	check(t, os.WriteFile(client.MetadataPath(), encoded, 0o600))
	// Rust builds a runtime with a single blocking thread to show the
	// network waits never hold one. Go has no bounded blocking pool; the
	// burst and the blocking work that must still run are the same.
	updates := core.NewUnbounded[dashboardIoUpdate]()
	permits := make(chan struct{}, 4)
	for id := range 64 {
		spawnStoredSessionSummary(strconv.Itoa(id), 0, uint64(id), permits, updates)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	for range 64 {
		update, ok := updates.Recv(ctx)
		if !ok {
			t.Fatal("summary network waits must not starve connection metadata reads")
		}
		summary, ok := update.(dashboardIoUpdateStoredSessionSummary)
		if !ok {
			t.Fatalf("expected a summary result, got %#v", update)
		}
		if summary.err == nil {
			t.Fatal("the newer daemon must be refused")
		}
		if !strings.Contains(summary.err.Error(), "the daemon uses a newer protocol") {
			t.Fatalf("the refusal must come from the recorded daemon's protocol: %v", summary.err)
		}
	}
	// Attachment renewal and chat preparation need this pool too.
	if _, err := core.SpawnBlocking(func() struct{} { return struct{}{} }).Join(ctx); err != nil {
		t.Fatalf("summary network waits must not starve connection metadata reads: %v", err)
	}
}

func TestAsynchronousSavesLeaveTheBlockingPoolAvailableForConnectionMetadata(t *testing.T) {
	// Rust builds a runtime with a single blocking thread to show the
	// network waits never hold one. Go has no bounded blocking pool; the
	// saves, the blocking work that must still run, and the quit blockers
	// are the same.
	tracker, _ := client.NewCriticalOperationTracker()
	updates := core.NewUnbounded[dashboardIoUpdate]()
	started := make(chan struct{}, 64)
	release := make(chan struct{})
	tasks := make([]*dashboardTask, 0, 64)
	for id := range 64 {
		tasks = append(tasks, spawnCriticalAsync(
			tracker,
			fmt.Sprintf("saving draft %d", id),
			updates,
			2*time.Second,
			func(ctx context.Context) (struct{}, error) {
				// The real connection needs a blocking metadata read.
				if _, err := core.SpawnBlocking(func() struct{} { return struct{}{} }).Join(ctx); err != nil {
					return struct{}{}, err
				}
				started <- struct{}{}
				<-release
				return struct{}{}, nil
			},
			func(_ struct{}, err error) dashboardIoUpdate {
				return dashboardIoUpdateTerminalDraft{sessionID: strconv.Itoa(id), revision: 1, err: err}
			},
		))
	}
	deadline, cancel := context.WithTimeout(context.Background(), time.Second)
	for range 64 {
		select {
		case <-started:
		case <-deadline.Done():
			cancel()
			t.Fatal("network waits must not starve blocking work")
		}
	}
	// Chat preparation and other UI work can still use the pool.
	ready, err := core.SpawnBlocking(func() string { return "chat ready" }).Join(deadline)
	cancel()
	if err != nil || ready != "chat ready" {
		t.Fatalf("network waits must not starve blocking work: %q, %v", ready, err)
	}
	close(release)
	for _, task := range tasks {
		check(t, task.join(context.Background()))
	}
	for range 64 {
		update, ok, _ := updates.TryRecv()
		draft, isDraft := update.(dashboardIoUpdateTerminalDraft)
		if !ok || !isDraft || draft.err != nil {
			t.Fatalf("save result %#v, want a saved draft", update)
		}
	}
	if blockers := tracker.Blockers(); len(blockers) != 0 {
		t.Fatalf("saves must release quit blockers: %v", blockers)
	}
}

func TestUnresponsiveSaveReportsUncertainDurabilityAndReleasesQuit(t *testing.T) {
	tracker, _ := client.NewCriticalOperationTracker()
	updates := core.NewUnbounded[dashboardIoUpdate]()
	task := spawnCriticalAsync(
		tracker,
		"saving draft",
		updates,
		20*time.Millisecond,
		func(ctx context.Context) (struct{}, error) {
			// Rust's pending future: nothing but being dropped ends it.
			<-ctx.Done()
			return struct{}{}, ctx.Err()
		},
		func(_ struct{}, err error) dashboardIoUpdate {
			return dashboardIoUpdateTerminalDraft{sessionID: "muse", revision: 1, err: err}
		},
	)
	if blockers := tracker.Blockers(); len(blockers) != 1 {
		t.Fatalf("blockers %v, want the save", blockers)
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	check(t, task.join(ctx))
	update, ok := updates.Recv(ctx)
	draft, isDraft := update.(dashboardIoUpdateTerminalDraft)
	if !ok || !isDraft || draft.err == nil {
		t.Fatalf("missing save failure: %#v", update)
	}
	if !strings.Contains(draft.err.Error(), "did not acknowledge") {
		t.Fatalf("failure %q", draft.err)
	}
	if !strings.Contains(draft.err.Error(), "may still complete") {
		t.Fatalf("failure %q", draft.err)
	}
	if blockers := tracker.Blockers(); len(blockers) != 0 {
		t.Fatalf("blockers %v, want none", blockers)
	}
}

// clipboardWrittenFailure is the failure of the one ClipboardWritten report
// queued on updates.
func clipboardWrittenFailure(t *testing.T, updates *core.Unbounded[dashboardIoUpdate], missing string) error {
	t.Helper()
	update, ok, _ := updates.TryRecv()
	written, isWritten := update.(dashboardIoUpdateClipboardWritten)
	if !ok || !isWritten || written.err == nil {
		t.Fatal(missing)
	}
	return written.err
}

// A job that dies must still answer, or the screen waits forever on work
// that is never coming back.
// Hard-won: ceb1f1b0: a blocking job panic sent no completion and left the screen waiting forever.
func TestAPanickingBlockingJobStillReportsItsFailure(t *testing.T) {
	updates := core.NewUnbounded[dashboardIoUpdate]()
	task := spawnIO(
		"write clipboard",
		updates,
		func() (struct{}, error) { panic("boom") },
		func(_ struct{}, err error) dashboardIoUpdate { return dashboardIoUpdateClipboardWritten{err: err} },
	)
	check(t, task.join(context.Background()))
	failure := clipboardWrittenFailure(t, updates, "a panicking job reported nothing")
	if !strings.Contains(failure.Error(), "write clipboard") || !strings.Contains(failure.Error(), "boom") {
		t.Fatalf("failure %q", failure)
	}
}

// Hard-won: ceb1f1b0: a critical blocking job panic left both the operation and quit blocker unresolved.
func TestAPanickingCriticalJobReportsItsFailureAndReleasesQuit(t *testing.T) {
	tracker, _ := client.NewCriticalOperationTracker()
	updates := core.NewUnbounded[dashboardIoUpdate]()
	task := spawnCriticalIO(
		tracker,
		"writing the clipboard",
		updates,
		func() (struct{}, error) { panic("boom") },
		func(_ struct{}, err error) dashboardIoUpdate { return dashboardIoUpdateClipboardWritten{err: err} },
	)
	check(t, task.join(context.Background()))
	failure := clipboardWrittenFailure(t, updates, "a panicking critical job reported nothing")
	if !strings.Contains(failure.Error(), "writing the clipboard") || !strings.Contains(failure.Error(), "boom") {
		t.Fatalf("failure %q", failure)
	}
	if blockers := tracker.Blockers(); len(blockers) != 0 {
		t.Fatalf("a panicking job must not block quit: %v", blockers)
	}
}

// Hard-won: ceb1f1b0: a cancellable blocking job panic left no failure update and retained quit admission.
func TestAPanickingCancellableJobReportsItsFailureAndReleasesQuit(t *testing.T) {
	tracker, _ := client.NewCriticalOperationTracker()
	updates := core.NewUnbounded[dashboardIoUpdate]()
	task := spawnCancellableIO(
		tracker,
		"writing the clipboard",
		updates,
		func(*atomic.Bool) (struct{}, error) { panic("boom") },
		func(_ struct{}, err error) dashboardIoUpdate { return dashboardIoUpdateClipboardWritten{err: err} },
	)
	check(t, task.join(context.Background()))
	failure := clipboardWrittenFailure(t, updates, "a panicking cancellable job reported nothing")
	if !strings.Contains(failure.Error(), "writing the clipboard") || !strings.Contains(failure.Error(), "boom") {
		t.Fatalf("failure %q", failure)
	}
	if blockers := tracker.Blockers(); len(blockers) != 0 {
		t.Fatalf("a panicking job must not block quit: %v", blockers)
	}
}

func TestQuickGithubBundleUsesCollisionSuffixAndReusesMatchingSource(t *testing.T) {
	t.Skip("needs mj-controller (not yet ported): create_quick_bundle_in_config")
}

func lifecycleSession(id, workspaceID string, state core.SessionState) core.SessionRecord {
	return core.SessionRecord{
		OracleChoice:              nil,
		Oracle:                    true,
		NotificationsIgnored:      false,
		Project:                   nil,
		TargetRuntime:             nil,
		LaunchBase:                nil,
		LaunchBranch:              nil,
		Checkout:                  nil,
		Publication:               nil,
		BuildCache:                nil,
		ContainerWorkspace:        nil,
		Subagents:                 nil,
		CreateManagedWorktree:     nil,
		WorkspaceID:               workspaceID,
		Archived:                  false,
		ID:                        id,
		Title:                     id,
		HarnessKind:               core.HarnessKindCodex,
		LastProfile:               "codex",
		BundleID:                  "project",
		ProjectDirectory:          nil,
		ManagedWorktree:           nil,
		Review:                    nil,
		NoProjectMemory:           false,
		NoGithubEvents:            false,
		NoGithubCredentials:       false,
		TargetTemplateID:          "podman",
		ResourceAllocation:        nil,
		AdditionalMounts:          []core.AdditionalMount{},
		State:                     state,
		Target:                    nil,
		NativeSessionID:           nil,
		ACPSessionTitle:           nil,
		SessionTitleOverride:      nil,
		CreatedAt:                 "2026-09-04T00:00:00Z",
		UpdatedAt:                 "2026-09-04T00:00:00Z",
		ViewedThroughEventOrdinal: 0,
		DraftInput:                "",
		LastError:                 nil,
		DismissedFailure:          nil,
		FailureGeneration:         0,
		LastCheckpointError:       nil,
		Checkpoint:                nil,
	}
}
