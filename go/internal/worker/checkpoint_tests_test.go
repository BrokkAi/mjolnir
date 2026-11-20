// Port of mj-worker/src/checkpoint_tests.rs (`cfg(all(test, unix))`).

//go:build unix

package worker

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	checkpointTestsSession = "018f9dd2-a3b4-7c8d-9000-123456789abc"
	checkpointTestsNative  = "0190aabb-ccdd-7eef-9000-abcdef012345"
)

func checkpointTestsMust(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func TestLiveMuseCheckpointRestoreRelocatesNativeContextAndPreservesQueuedWork(t *testing.T) {
	required := func(name string) string {
		value, ok := os.LookupEnv(name)
		if !ok {
			t.Skip("requires pinned MJ_MUSE_ACP_TEST_BINARY, MJ_MUSE_TEST_BINARY and authenticated MJ_MUSE_TEST_HOME; uses two provider turns")
		}
		return value
	}
	adapter := required("MJ_MUSE_ACP_TEST_BINARY")
	muse := required("MJ_MUSE_TEST_BINARY")
	account := required("MJ_MUSE_TEST_HOME")
	temp := t.TempDir()
	spec, _ := checkpointTestsFixture(t, temp)
	spec.Session.HarnessKind = core.HarnessKindMuse
	spec.Session.ProfileID = "muse-smoke"
	spec.HarnessHome = filepath.Join(temp, "original/muse")
	checkpointTestsMust(t, os.MkdirAll(spec.HarnessHome, 0o755))
	checkpointTestsCopy(t, filepath.Join(account, "auth.json"), filepath.Join(spec.HarnessHome, "auth.json"))
	cwd := filepath.Join(spec.WorkspaceRoot, "app")
	token := "MUSE_" + filepath.Base(temp)
	prompt := fmt.Sprintf("Remember this exact token for the next turn: %s. Reply with only that token. Do not use tools.", token)
	sessionID, reply := nativeMuseTurn(t, adapter, muse, spec.HarnessHome, cwd, nil, prompt)
	if !strings.Contains(reply, token) {
		t.Fatal("provider did not acknowledge the smoke token")
	}
	spec.Session.NativeSessionID = sessionID
	content, err := jsonvalue.Marshal(jsonvalue.Object{{Key: "type", Value: "text"}, {Key: "text", Value: prompt}})
	checkpointTestsMust(t, err)
	spec.CanonicalSession.Transcript[0].Body = core.CanonicalTranscriptBodyUser{Content: []json.RawMessage{content}}
	_, err = checkpoint.ExportCheckpoint(&spec)
	checkpointTestsMust(t, err)
	archive, err := checkpoint.ReadArchiveVerified(spec.OutputPath)
	checkpointTestsMust(t, err)
	for _, payload := range archive.Manifest.Payloads {
		if filepath.Base(payload.Path) == "auth.json" {
			t.Fatal("credentials entered the archive")
		}
	}
	restoredHome := filepath.Join(temp, "restored/muse")
	restoredRelay := filepath.Join(temp, "restored-relay")
	restoredWorkspace := filepath.Join(temp, "restored-workspace")
	checkpointTestsMust(t, os.MkdirAll(restoredWorkspace, 0o755))
	testSupportGit(restoredWorkspace, "clone", "--quiet", cwd, "app")
	checkpointTestsMust(t, checkpoint.RestoreCheckpoint(&checkpoint.CheckpointRestoreSpec{
		ArchivePath:         spec.OutputPath,
		WorkspaceRoot:       restoredWorkspace,
		RelayRoot:           restoredRelay,
		HarnessHome:         restoredHome,
		RestoreRepositories: true,
		RestoreNative:       true,
		QueuePolicy:         checkpoint.QueueRestorePolicyRestore,
	}, checkpoint.SystemGit{}))
	if _, err := os.Stat(filepath.Join(restoredHome, "auth.json")); err == nil {
		t.Fatal("credentials entered the archive")
	}
	if seed := checkpointTestsRestoredSeed(t, restoredRelay); checkpointTestsJSON(t, seed.QueuedPrompts) != checkpointTestsJSON(t, spec.CanonicalSession.QueuedPrompts) {
		t.Fatalf("queued prompts = %+v", seed.QueuedPrompts)
	}
	checkpointTestsCopy(t, filepath.Join(spec.HarnessHome, "auth.json"), filepath.Join(restoredHome, "auth.json"))
	resumed, reply := nativeMuseTurn(t, adapter, muse, restoredHome, filepath.Join(restoredWorkspace, "app"), &sessionID,
		"What exact token did I ask you to remember in the previous turn? Reply only with the token. Do not use tools.")
	if resumed != sessionID {
		t.Fatalf("resumed %s, expected %s", resumed, sessionID)
	}
	if !strings.Contains(reply, token) {
		t.Fatal("native context was not restored")
	}
}

func checkpointTestsCopy(t *testing.T, from, to string) {
	t.Helper()
	data, err := os.ReadFile(from)
	checkpointTestsMust(t, err)
	checkpointTestsMust(t, os.WriteFile(to, data, 0o600))
}

// checkpointTestsFixture is checkpoint_tests::fixture: a Codex session with
// one native rollout and one repository "app" with a base commit and an
// origin, exported to worker/source.hel.zip. It returns the spec and that
// output path.
func checkpointTestsFixture(t *testing.T, temp string) (checkpoint.CheckpointExportSpec, string) {
	t.Helper()
	workerRoot := filepath.Join(temp, "worker")
	checkpointTestsMust(t, os.MkdirAll(workerRoot, 0o755))
	harnessHome := filepath.Join(temp, "codex")
	native := filepath.Join(harnessHome, "sessions/2026/08/09")
	checkpointTestsMust(t, os.MkdirAll(native, 0o755))
	checkpointTestsMust(t, os.WriteFile(filepath.Join(native, "rollout-"+checkpointTestsNative+".jsonl"), []byte("native"), 0o644))
	workspace := filepath.Join(temp, "workspace")
	repository := filepath.Join(workspace, "app")
	checkpointTestsMust(t, os.MkdirAll(repository, 0o755))
	testSupportGit(repository, "init")
	testSupportGit(repository, "config", "user.email", "hel@example.test")
	testSupportGit(repository, "config", "user.name", "Hel Test")
	checkpointTestsMust(t, os.WriteFile(filepath.Join(repository, "README.md"), []byte("hello"), 0o644))
	testSupportGit(repository, "add", ".")
	testSupportGit(repository, "commit", "-m", "base")
	testSupportGit(repository, "remote", "add", "origin", "https://github.com/example/app.git")
	base := testSupportGit(repository, "rev-parse", "HEAD")
	output := filepath.Join(workerRoot, "source.hel.zip")
	lastActivity := int64(1)
	title := "test"
	return checkpoint.CheckpointExportSpec{
		ProtocolVersion: checkpoint.CheckpointExportProtocolVersion,
		Session: core.SessionManifest{
			ID:              checkpointTestsSession,
			Title:           "test",
			HarnessKind:     core.HarnessKindCodex,
			ProfileID:       "codex-1",
			NativeSessionID: checkpointTestsNative,
			CreatedAt:       "2026-08-09T00:00:00Z",
			CheckpointedAt:  "2026-08-09T00:01:00Z",
			HelVersion:      "0.1.0",
			RelayVersion:    "0.1.0",
			AdapterVersion:  "test",
		},
		Target: core.TargetManifest{
			TemplateID: "local",
			TargetKind: "podman",
			Details:    map[string]string{},
		},
		Bundle: core.BundleManifest{
			ID:                "bundle",
			PrimaryRepository: "app",
		},
		RelayRoot:     workerRoot,
		HarnessHome:   harnessHome,
		WorkspaceRoot: workspace,
		Repositories: []checkpoint.CheckpointRepositorySpec{{
			ID:                  "app",
			RelativeDestination: "app",
			Capture:             checkpoint.CheckpointRepositoryCapture{Mode: checkpoint.CheckpointRepositoryCaptureDeltaFrom, BaseCommit: base},
		}},
		CanonicalSession: core.CanonicalSessionSnapshot{
			EventFrontier:       1,
			EventFrontierDigest: strings.Repeat("a", 64),
			Session: core.CanonicalSessionState{
				Execution:        core.CanonicalExecutionState{State: core.CanonicalExecutionStateIdle},
				LastActivityAtMs: &lastActivity,
				SessionTitle:     &title,
			},
			Transcript: []core.CanonicalTranscriptItem{{
				StableID:        "user-1",
				Position:        1,
				CreatedAtMs:     1,
				LastChangedAtMs: 1,
				Body: core.CanonicalTranscriptBodyUser{
					Content: []json.RawMessage{json.RawMessage(`{"type":"text","text":"hello"}`)},
				},
			}},
			QueuedPrompts: []core.CanonicalQueuedPrompt{{
				CommandID:  "queued-1",
				Kind:       core.CanonicalQueuedCommandKind{Kind: core.CanonicalQueuedCommandKindPrompt},
				Content:    []json.RawMessage{json.RawMessage(`{"type":"text","text":"next"}`)},
				QueuedAtMs: 2,
			}},
		},
		OutputPath: output,
	}, output
}

func checkpointTestsRestoredSeed(t *testing.T, relayRoot string) core.RestoredRelaySeed {
	t.Helper()
	data, err := os.ReadFile(core.RestoredRelaySeedPath(relayRoot))
	checkpointTestsMust(t, err)
	var seed core.RestoredRelaySeed
	checkpointTestsMust(t, json.Unmarshal(data, &seed))
	return seed
}

// Hard-won: 055d8a772 review: recovery of one component must preserve newer parent state and current files.
func TestCheckoutRecoveryPublishesOnlyTheStagedCheckoutAndPreservesANewCurrentDirectory(t *testing.T) {
	temp := t.TempDir()
	source, archivePath := checkpointTestsFixture(t, temp)
	testSupportGit(source.WorkspaceRoot, "clone", "--quiet", "app", "second")
	source.Repositories = append(source.Repositories, checkpoint.CheckpointRepositorySpec{
		ID:                  "second",
		RelativeDestination: "second",
		Capture: checkpoint.CheckpointRepositoryCapture{
			Mode:       checkpoint.CheckpointRepositoryCaptureDeltaFrom,
			BaseCommit: testSupportGit(filepath.Join(source.WorkspaceRoot, "second"), "rev-parse", "HEAD"),
		},
	})
	checkpointTestsMust(t, os.WriteFile(filepath.Join(source.WorkspaceRoot, "second/README.md"), []byte("archived second repository work"), 0o644))
	_, err := checkpoint.ExportCheckpoint(&source)
	checkpointTestsMust(t, err)
	stage := filepath.Join(temp, "checkout-stage")
	checkpointTestsMust(t, os.Mkdir(stage, 0o755))
	testSupportGit(stage, "clone", "--quiet", filepath.Join(source.WorkspaceRoot, "app"), "app")
	testSupportGit(stage, "clone", "--quiet", filepath.Join(source.WorkspaceRoot, "second"), "second")
	parentNative := filepath.Join(temp, "current-native")
	checkpointTestsMust(t, os.Mkdir(parentNative, 0o755))
	checkpointTestsMust(t, os.WriteFile(filepath.Join(parentNative, "sentinel"), []byte("newer native state"), 0o644))
	parentRelay := filepath.Join(temp, "current-relay")
	checkpointTestsMust(t, os.Mkdir(parentRelay, 0o755))
	checkpointTestsMust(t, os.WriteFile(filepath.Join(parentRelay, core.RelayStateFile), []byte("current relay"), 0o644))
	destination := filepath.Join(temp, "checkout-current")
	verified, err := checkpoint.VerifyArchiveStreaming(archivePath)
	checkpointTestsMust(t, err)
	spec := checkpoint.CheckoutRecoverySpec{
		Version:             1,
		CheckpointSha256:    verified.ArchiveSha256,
		StagingRoot:         stage,
		StagedCheckout:      filepath.Join(stage, "app"),
		DestinationCheckout: filepath.Join(destination, "app"),
		AdditionalCheckouts: []checkpoint.CheckoutRecoveryPublication{{
			StagedCheckout:      filepath.Join(stage, "second"),
			DestinationCheckout: filepath.Join(destination, "second"),
		}},
		Restore: checkpoint.CheckpointRestoreSpec{
			ArchivePath:         archivePath,
			WorkspaceRoot:       stage,
			RelayRoot:           parentRelay,
			HarnessHome:         parentNative,
			RestoreRepositories: true,
			RestoreNative:       false,
			QueuePolicy:         checkpoint.QueueRestorePolicyRestore,
		},
	}
	specPath := filepath.Join(temp, "checkout-recovery.json")
	encoded, err := json.Marshal(spec)
	checkpointTestsMust(t, err)
	checkpointTestsMust(t, os.WriteFile(specPath, encoded, 0o644))
	checkpointTestsMust(t, RestoreCheckoutFromSpecFile(specPath))
	if info, err := os.Stat(filepath.Join(destination, "app/.git")); err != nil || !info.IsDir() {
		t.Fatal("the primary checkout was not published")
	}
	for _, staged := range []string{"app", "second"} {
		if _, err := os.Stat(filepath.Join(stage, staged)); err == nil {
			t.Fatalf("staged %s was left behind", staged)
		}
	}
	checkpointTestsExpectFile(t, filepath.Join(destination, "second/README.md"), "archived second repository work")
	checkpointTestsMust(t, os.WriteFile(filepath.Join(destination, "app/current-work"), []byte("new current files"), 0o644))
	// An interrupted caller can retry after publication; it must not replay
	// the archive over the now-current checkout, even without its old
	// staging.
	checkpointTestsMust(t, RestoreCheckoutFromSpecFile(specPath))
	checkpointTestsExpectFile(t, filepath.Join(destination, "app/current-work"), "new current files")
	checkpointTestsExpectFile(t, filepath.Join(parentNative, "sentinel"), "newer native state")
	checkpointTestsExpectFile(t, filepath.Join(parentRelay, core.RelayStateFile), "current relay")
}

// checkpointTestsJSON is value's wire form, which compares values as Rust's
// derived PartialEq does.
func checkpointTestsJSON(t *testing.T, value any) string {
	t.Helper()
	encoded, err := json.Marshal(value)
	checkpointTestsMust(t, err)
	return string(encoded)
}

func checkpointTestsExpectFile(t *testing.T, path string, expected string) {
	t.Helper()
	data, err := os.ReadFile(path)
	checkpointTestsMust(t, err)
	if !bytes.Equal(data, []byte(expected)) {
		t.Fatalf("%s = %q, want %q", path, data, expected)
	}
}

func checkpointTestsParentMail(key string, wake bool) core.MailboxEvent {
	return core.MailboxEvent{
		Key:         key,
		Source:      "parent",
		Wake:        wake,
		CreatedAtMs: 1,
		Body:        core.MailboxEventBodyParentMessage{Text: "message " + key},
	}
}

// Mail the worker accepted before a checkpoint cut, but had not handed to
// the agent, is archived from the relay at that cut and arrives exactly once
// from whichever relay restores it, in its order. Queue policies that defer
// mail leave it for the controller's admission, while a queued prompt's
// claimed mail is bound to its command ID before that prompt can start.
func TestAcceptedMailCrossesACheckpointIntoTheRestoredRelayExactlyOnce(t *testing.T) {
	deliver := func(event core.MailboxEvent) core.RelayCommand {
		return core.RelayCommandDeliverMailboxEvent{Event: event}
	}
	configured := func(root string) *DurableRelay {
		relay, err := DurableRelayOpen(root, checkpointTestsSession, "1.0.0")
		checkpointTestsMust(t, err)
		_, err = relay.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
		checkpointTestsMust(t, err)
		return relay
	}
	temp := t.TempDir()

	// The source delivered one waking message and holds two that wait for
	// the agent's next prompt.
	source := configured(filepath.Join(temp, "source-relay"))
	submitRelay(source, "deliver-delivered", deliver(checkpointTestsParentMail("delivered", true)))
	wake, err := source.ClaimPendingCommands(true)
	checkpointTestsMust(t, err)
	if _, ok := wake[0].Command.(core.RelayCommandMailboxWake); !ok {
		t.Fatalf("claimed %#v", wake[0].Command)
	}
	_, err = source.RecordCommandCompleted(wake[0].CommandID, core.RelayCommandOutcomePrompt{StopReason: "end_turn"})
	checkpointTestsMust(t, err)
	submitRelay(source, "deliver-first", deliver(checkpointTestsParentMail("first", false)))
	submitRelay(source, "deliver-second", deliver(checkpointTestsParentMail("second", false)))
	cut := readyCheckpoint(source, "suspend-barrier")
	// The request and its answer are decoded from their wire form: the relay
	// protocol owns ReadCheckpointMailbox (protocol 75).
	readAtCut := func(relay *DurableRelay) map[string]json.RawMessage {
		params, err := json.Marshal(map[string]any{"barrier_command_id": "suspend-barrier", "cut": cut})
		checkpointTestsMust(t, err)
		request, err := core.UnmarshalRelayRequestJSON([]byte(fmt.Sprintf(`{"method":"read_checkpoint_mailbox","params":%s}`, params)))
		checkpointTestsMust(t, err)
		body, err := json.Marshal(relay.Handle(relayRequest("read-mail", request)))
		checkpointTestsMust(t, err)
		var fields map[string]json.RawMessage
		checkpointTestsMust(t, json.Unmarshal(body, &fields))
		return fields
	}
	response := readAtCut(source)
	var payload struct {
		Type string `json:"type"`
		Data struct {
			Mailbox core.CanonicalMailbox `json:"mailbox"`
		} `json:"data"`
	}
	if string(response["result"]) != `"ok"` || json.Unmarshal(response["payload"], &payload) != nil || payload.Type != "checkpoint_mailbox" {
		t.Fatalf("the relay refused to report its mail at the ready cut: %s", response)
	}
	mailbox := payload.Data.Mailbox
	pending := []core.MailboxEvent{checkpointTestsParentMail("first", false), checkpointTestsParentMail("second", false)}
	if got, want := checkpointTestsJSON(t, mailbox.PendingEvents), checkpointTestsJSON(t, pending); got != want {
		t.Fatalf("pending = %s, want %s", got, want)
	}
	if _, ok := mailbox.DeliveredEventKeys["delivered"]; !ok {
		t.Fatalf("delivered = %v", mailbox.DeliveredEventKeys)
	}
	// Mail accepted after the cut moves the relay past it, as it would refuse
	// a Close there; the cut's mail can no longer be read.
	submitRelay(source, "deliver-late", deliver(checkpointTestsParentMail("late", false)))
	refused := readAtCut(source)
	var failure core.RelayProtocolError
	if string(refused["result"]) != `"error"` || json.Unmarshal(refused["error"], &failure) != nil {
		t.Fatalf("the relay reported mail for a cut it has moved past: %s", refused)
	}
	if failure.Code != core.RelayErrorCodeInvalidState {
		t.Fatalf("code = %s", failure.Code)
	}

	// The fixture's queued prompt carries claimed mail, as a recovery prompt
	// does after a worker restart.
	spec, _ := checkpointTestsFixture(t, temp)
	claimed := "claimed mail for the queued prompt"
	archived := mailbox
	archived.PromptContexts = map[string]string{"queued-1": claimed}
	spec.CanonicalSession.Mailbox = &archived
	_, err = checkpoint.ExportCheckpoint(&spec)
	checkpointTestsMust(t, err)
	verified, err := checkpoint.ReadArchiveVerified(spec.OutputPath)
	checkpointTestsMust(t, err)
	if verified.Manifest.SchemaVersion != core.ArchiveSchemaVersionMailbox {
		t.Fatalf("schema = %d", verified.Manifest.SchemaVersion)
	}
	restore := func(name string, queuePolicy checkpoint.QueueRestorePolicy) *DurableRelay {
		workspace := filepath.Join(temp, name+"-workspace")
		checkpointTestsMust(t, os.MkdirAll(workspace, 0o755))
		testSupportGit(workspace, "clone", "--quiet", filepath.Join(spec.WorkspaceRoot, "app"), "app")
		relayRoot := filepath.Join(temp, name+"-relay")
		checkpointTestsMust(t, checkpoint.RestoreCheckpoint(&checkpoint.CheckpointRestoreSpec{
			ArchivePath:   spec.OutputPath,
			WorkspaceRoot: workspace,
			RelayRoot:     relayRoot,
			HarnessHome:   filepath.Join(temp, name+"-home"),
			RestoreNative: true,
			QueuePolicy:   queuePolicy,
		}, checkpoint.SystemGit{}))
		return configured(relayRoot)
	}
	pendingKeys := func(relay *DurableRelay) []string {
		return relay.OperationalState().UndeliveredMailboxEventKeys
	}
	expectKeys := func(relay *DurableRelay, keys ...string) {
		t.Helper()
		if got := pendingKeys(relay); !reflect.DeepEqual(got, keys) && !(len(got) == 0 && len(keys) == 0) {
			t.Fatalf("pending keys = %q, want %q", got, keys)
		}
	}
	hiddenContext := func(command core.ClaimedRelayCommand) string {
		if command.HiddenPromptContext == nil {
			return ""
		}
		return *command.HiddenPromptContext
	}

	// The restored worker owns the mail at once. The queued prompt starts
	// with its claimed mail and leaves the waiting messages for the next one.
	// A producer's retry of a message under any command ID is acknowledged
	// without a second copy, whether it is pending or was delivered.
	restored := restore("restored", checkpoint.QueueRestorePolicyRestore)
	expectKeys(restored, "first", "second")
	started, err := restored.ClaimPendingCommands(true)
	checkpointTestsMust(t, err)
	if started[0].CommandID != "queued-1" || !strings.Contains(hiddenContext(started[0]), claimed) {
		t.Fatalf("started %+v", started[0])
	}
	submitRelay(restored, "retry-first", deliver(checkpointTestsParentMail("first", false)))
	submitRelay(restored, "retry-delivered", deliver(checkpointTestsParentMail("delivered", true)))
	expectKeys(restored, "first", "second")

	// Deferred: nothing pending until the controller admits it. The prompt it
	// admits under its original ID finds its claimed mail before it starts.
	deferred := restore("deferred", checkpoint.QueueRestorePolicyDefer)
	expectKeys(deferred)
	if started, err := deferred.ClaimPendingCommands(true); err != nil || len(started) != 0 {
		t.Fatalf("started %+v, %v", started, err)
	}
	submitRelay(deferred, "queued-1", prompt("next"))
	started, err = deferred.ClaimPendingCommands(true)
	checkpointTestsMust(t, err)
	if !strings.Contains(hiddenContext(started[0]), claimed) {
		t.Fatalf("started %+v", started[0])
	}
	for _, event := range pending {
		submitRelay(deferred, "admit-"+event.Key, deliver(event))
		submitRelay(deferred, "again-"+event.Key, deliver(event))
	}
	expectKeys(deferred, "first", "second")

	// A discarded queue takes its claimed mail with it; pending mail still
	// waits for the controller, and delivered keys still deduplicate.
	discarded := restore("discarded", checkpoint.QueueRestorePolicyDiscardDeferringMail)
	expectKeys(discarded)
	submitRelay(discarded, "retry-delivered", deliver(checkpointTestsParentMail("delivered", true)))
	expectKeys(discarded)
	submitRelay(discarded, "queued-1", prompt("next"))
	started, err = discarded.ClaimPendingCommands(true)
	checkpointTestsMust(t, err)
	if started[0].CommandID != "queued-1" || started[0].HiddenPromptContext != nil {
		t.Fatalf("started %+v", started[0])
	}
}
