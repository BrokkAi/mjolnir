// Port of the tests in mj-cli/src/dashboard/startup_prompts.rs.

package cli

import (
	"context"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

func TestARecallCannotOvertakeItsAdmissionAndAnotherSessionRemainsIndependent(t *testing.T) {
	first := newSessionMailbox()
	defer first.stop()
	independent := newSessionMailbox()
	defer independent.stop()
	entered := make(chan struct{})
	release := make(chan struct{})
	recalled := make(chan struct{})
	if err := first.admit(func(context.Context) {
		close(entered)
		<-release
	}); err != nil {
		t.Fatal(err)
	}
	if err := first.admit(func(context.Context) { close(recalled) }); err != nil {
		t.Fatal(err)
	}
	<-entered
	select {
	case <-recalled:
		t.Fatal("the recall overtook its admission")
	default:
	}
	other := make(chan struct{})
	if err := independent.admit(func(context.Context) { close(other) }); err != nil {
		t.Fatal(err)
	}
	<-other
	close(release)
	<-recalled
}

// heldMailbox is a mailbox whose worker never runs, as a task spawned on a
// Tokio test's runtime is never polled while the test does not await: what
// is admitted stays queued and reaches no daemon.
func heldMailbox() *sessionMailbox {
	return &sessionMailbox{sender: make(chan saveJob, sessionMailboxCapacity), stop: func() {}}
}

func standbyComposer(sessionID string, config *core.Config) *chat.ChatState {
	return chat.ChatStateStandby(sessionID, config, chat.SessionHeaderIdentity{}, chat.NewNotices())
}

func pressKey(code rune) tea.KeyPressMsg { return tea.KeyPressMsg{Code: code} }

func waitingPrompt(prompt chat.StartupPrompt, inheritedDraft *string, recalled bool, order uint64) *trackedPromptWaiting {
	settlement, settle := newSettlement()
	return &trackedPromptWaiting{
		prompt:         prompt,
		inheritedDraft: inheritedDraft,
		recalled:       recalled,
		order:          order,
		settlement:     settlement,
		settle:         settle,
	}
}

func standbyDraft(t *testing.T, dashboard *tui.DashboardState, sessionID string) string {
	t.Helper()
	draft, ok := dashboard.StandbyPromptDraft(sessionID)
	if !ok {
		t.Fatalf("no standby draft for %s", sessionID)
	}
	return draft
}

func TestDurableRefusalsRestoreImagesOnceWithoutDuplicatingRecalledInput(t *testing.T) {
	config := core.DefaultConfig()
	dashboard := tui.NewDashboardState(config, core.NewStateState(), map[string]client.ProfileQuota{})
	reference, err := core.NewAttachmentRef([]byte("\x89PNG\r\n\x1a\ninstalled"), "image/png", 2, 2)
	if err != nil {
		t.Fatal(err)
	}
	image, err := chat.ClipboardImageFromReference(reference)
	if err != nil {
		t.Fatal(err)
	}
	payload := chat.PromptPayloadWithImage("original ", image)
	composer := standbyComposer("session", &config)
	composer.SetDraft(payload.EncodeDraft())
	composer.HandleKey(pressKey(tea.KeyEnter))
	prompt, ok := composer.LatestStartupPrompt()
	if !ok {
		t.Fatal("Enter queued no startup prompt")
	}
	savedOriginalMove := "saved-original-move"
	prompt.MoveOperationID = &savedOriginalMove
	composer.Paste("newer text")
	dashboard.AdoptStandbyComposer("session", composer.TakeLiveComposer())
	var pending startupPrompts
	inheritedBeforeAdmission := "inherited-before-admission"
	pending.insertPending("session", prompt.CommandID, waitingPrompt(prompt, &inheritedBeforeAdmission, false, 0))
	if pending.admissionFailed("session", prompt.CommandID, "lost enqueue acknowledgement", false) {
		t.Fatal("an uncertain admission must not restore")
	}
	if got := standbyDraft(t, dashboard, "session"); got != "newer text" {
		t.Fatalf("uncertain admission must not restore a resendable duplicate: draft %q", got)
	}
	if known, _, ok := pending.knownPrompt("session", prompt.CommandID); !ok || known.CommandID != prompt.CommandID {
		t.Fatalf("known prompt = %#v, %v", known, ok)
	}
	pending.retainUnconfirmed(dashboard, map[string]*chat.ActiveChat{})
	saved := standbyDraft(t, dashboard, "session")
	reopened := standbyComposer("session", &config)
	reopened.SetDraft(saved)
	retries := reopened.StartupRetryPrompts()
	if len(retries) != 1 || !retries[0].Prompt.Equal(&prompt) ||
		retries[0].InheritedDraft == nil || *retries[0].InheritedDraft != inheritedBeforeAdmission {
		t.Fatalf("bounded shutdown must preserve exact ID, payload, images and Move origin: %#v", retries)
	}
	// A lost reply may already have an accepted receipt. Resume the
	// complete original request and settle it without returning a second
	// sendable draft.
	var conflict error = client.StartupPromptIdentityConflict{}
	if admissionFailureIsDefinite(conflict) {
		t.Fatal("an identity conflict is not a definite refusal")
	}
	if !admissionFailureIsDefinite(client.DaemonRefusal{Message: "not admitted"}) {
		t.Fatal("a daemon refusal is definite")
	}
	if pending.admissionFailed("session", prompt.CommandID, conflict.Error(), admissionFailureIsDefinite(conflict)) {
		t.Fatal("a conflict must not restore")
	}
	retries = reopened.StartupRetryPrompts()
	retry := retries[len(retries)-1]
	acceptedDashboard := tui.NewDashboardState(config, core.NewStateState(), map[string]client.ProfileQuota{})
	acceptedDashboard.AdoptStandbyComposer("session", reopened.TakeLiveComposer())
	accepted := startupPrompts{mailboxes: map[string]*sessionMailbox{"session": heldMailbox()}}
	updates := core.NewUnbounded[dashboardIoUpdate]()
	tracker, _ := client.NewCriticalOperationTracker()
	accepted.resume(namedSession{id: "session", name: "Session name"}, retry.Prompt, retry.InheritedDraft, updates, tracker)
	if waiting, ok := accepted.pending["session"][prompt.CommandID].(*trackedPromptWaiting); !ok ||
		waiting.inheritedDraft == nil || *waiting.inheritedDraft != inheritedBeforeAdmission {
		t.Fatalf("resumed prompt = %#v", accepted.pending["session"][prompt.CommandID])
	}
	accepted.applyOutcomes("session",
		map[string]client.StartupPromptOutcome{prompt.CommandID: client.StartupPromptOutcomeAccepted{}},
		acceptedDashboard, map[string]*chat.ActiveChat{})
	if got := standbyDraft(t, acceptedDashboard, "session"); got != "newer text" {
		t.Fatalf("accepted draft = %q", got)
	}
	if previews := acceptedDashboard.StandbyStartupPrompts("session"); len(previews) != 0 {
		t.Fatalf("startup previews = %#v", previews)
	}
	if !accepted.isSettled("session", prompt.CommandID) {
		t.Fatal("the accepted prompt must settle")
	}

	// Reopen the actual backend recovery representation: receipt
	// consumption and payload merge happen atomically, so terminal polling
	// cannot append A twice.
	withoutReceipt, err := core.RemoveStartupReceipt(saved, prompt.CommandID)
	if err != nil {
		t.Fatal(err)
	}
	recoveredSaved, err := core.MergeDrafts(withoutReceipt, payload.EncodeDraft())
	if err != nil {
		t.Fatal(err)
	}
	recoveredOwner := standbyComposer("session", &config)
	recoveredOwner.SetDraft(recoveredSaved)
	if retries := recoveredOwner.StartupRetryPrompts(); len(retries) != 0 {
		t.Fatalf("retry prompts = %#v", retries)
	}
	recoveredPayload, err := chat.PromptPayloadDecodeDraft(recoveredOwner.Draft())
	if err != nil {
		t.Fatal(err)
	}
	if recoveredPayload.Text != "newer text\n\noriginal [image 1]" {
		t.Fatalf("recovered text = %q", recoveredPayload.Text)
	}
	if len(recoveredPayload.Images) != 1 {
		t.Fatalf("recovered images = %d", len(recoveredPayload.Images))
	}
	recoveredDashboard := tui.NewDashboardState(config, core.NewStateState(), map[string]client.ProfileQuota{})
	recoveredDashboard.AdoptStandbyComposer("session", recoveredOwner.TakeLiveComposer())
	recoveredBefore := standbyDraft(t, recoveredDashboard, "session")
	var fresh startupPrompts
	fresh.applyOutcomes("session", map[string]client.StartupPromptOutcome{
		prompt.CommandID: client.StartupPromptOutcomeRefused{
			EncodedDraft: payload.EncodeDraft(),
			Reason:       "already recovered atomically",
		},
	}, recoveredDashboard, map[string]*chat.ActiveChat{})
	if got := standbyDraft(t, recoveredDashboard, "session"); got != recoveredBefore {
		t.Fatalf("late terminal receipt cannot append the already recovered payload again: %q", got)
	}

	outcomes := map[string]client.StartupPromptOutcome{
		prompt.CommandID: client.StartupPromptOutcomeRefused{
			EncodedDraft: payload.EncodeDraft(),
			Reason:       "destination move cancelled",
		},
	}
	chats := map[string]*chat.ActiveChat{}
	pending.applyOutcomes("session", outcomes, dashboard, chats)
	recovered := standbyDraft(t, dashboard, "session")
	pending.applyOutcomes("session", outcomes, dashboard, chats)
	if got := standbyDraft(t, dashboard, "session"); got != recovered {
		t.Fatalf("second refusal changed the draft to %q", got)
	}
	decoded, err := chat.PromptPayloadDecodeDraft(recovered)
	if err != nil {
		t.Fatal(err)
	}
	if decoded.Text != "newer text\n\noriginal [image 1]" {
		t.Fatalf("recovered text = %q", decoded.Text)
	}
	if len(decoded.Images) != 1 {
		t.Fatalf("recovered images = %d", len(decoded.Images))
	}
	if previews := dashboard.StandbyStartupPrompts("session"); len(previews) != 0 {
		t.Fatalf("startup previews = %#v", previews)
	}
	if !pending.isSettled("session", prompt.CommandID) {
		t.Fatal("the refused prompt must settle")
	}

	recalledComposer := standbyComposer("recalled", &config)
	recalledComposer.SetDraft("edit me")
	recalledComposer.HandleKey(pressKey(tea.KeyEnter))
	recalledPrompt, ok := recalledComposer.LatestStartupPrompt()
	if !ok {
		t.Fatal("Enter queued no startup prompt")
	}
	recalledComposer.HandleKey(pressKey(tea.KeyUp))
	recalledComposer.Paste(" now")
	dashboard.AdoptStandbyComposer("recalled", recalledComposer.TakeLiveComposer())
	pending.insertPending("recalled", recalledPrompt.CommandID, waitingPrompt(recalledPrompt, nil, true, 1))
	if pending.admissionFailed("recalled", recalledPrompt.CommandID, "explicit admission refusal", true) {
		t.Fatal("a recalled prompt must not restore")
	}
	pending.applyOutcomes("recalled", map[string]client.StartupPromptOutcome{
		recalledPrompt.CommandID: client.StartupPromptOutcomeRefused{
			EncodedDraft: recalledPrompt.Payload.EncodeDraft(),
			Reason:       "not admitted",
		},
	}, dashboard, chats)
	if got := standbyDraft(t, dashboard, "recalled"); got != "edit me now" {
		t.Fatalf("recalled draft = %q", got)
	}
	if !pending.isSettled("recalled", recalledPrompt.CommandID) {
		t.Fatal("the recalled prompt must settle")
	}
	// The last part of the Rust test drives a quick reply through
	// populated_dashboard, the dashboard tests' fixture in
	// mj-cli/src/dashboard/tests.rs, which is dash-rest's to port.
	t.Run("quick reply receipts settle their own line", func(t *testing.T) {
		t.Skip("needs populatedDashboard from mj-cli/src/dashboard/tests.rs (dash-rest, not yet ported); run quickReplyReceiptsSettleTheirOwnLine(t, populatedDashboard) once it lands")
		quickReplyReceiptsSettleTheirOwnLine(t, nil)
	})
}

// quickReplyReceiptsSettleTheirOwnLine is the quick-reply half of
// durable_refusals_restore_images_once_without_duplicating_recalled_input:
// a quick-reply receipt must settle its own line, never restore into or
// clear the real conversation's independent composer draft.
func quickReplyReceiptsSettleTheirOwnLine(t *testing.T, populatedDashboard func() *tui.DashboardState) {
	for _, refused := range []bool{false, true} {
		for _, cardGone := range []bool{false, true} {
			for _, occupied := range []bool{false, true} {
				dashboard := populatedDashboard()
				dashboard.SetSessionAttentionFor("session-1", core.SessionAttentionImportant)
				dismissals := core.SnapshotMap[string, core.NotificationDismissal]{}
				dismissals.Insert("session-1", core.NotificationDismissalReadThrough{Value: 1})
				dashboard.SetNotificationDismissals(dismissals)
				notices := chat.NewNotices()
				dashboard.ShareNotices(notices)
				if occupied {
					dashboard.SeedStandbyPrompt("session-1", "Keep the real composer")
				}
				dashboard.FocusSessions()
				for range 4 {
					dashboard.HandleKey(pressKey(tea.KeyTab))
				}
				for _, ch := range "Quick response" {
					dashboard.HandleKey(tea.KeyPressMsg{Code: ch, Text: string(ch)})
				}
				reply, ok := dashboard.HandleKey(pressKey(tea.KeyEnter)).(tui.DashboardActionNotificationReply)
				if !ok {
					t.Fatal("quick reply submission")
				}
				sessionID, prompt := reply.SessionID, reply.Prompt
				var pending startupPrompts
				pending.insertPending(sessionID, prompt.CommandID, waitingPrompt(prompt, nil, false, 0))
				var outcome client.StartupPromptOutcome = client.StartupPromptOutcomeAccepted{}
				if refused {
					outcome = client.StartupPromptOutcomeRefused{
						EncodedDraft: prompt.Payload.EncodeDraft(),
						Reason:       "reply rejected",
					}
				}
				if cardGone {
					dashboard.SetSessionAttentionFor(sessionID, core.SessionAttentionWorking)
					if dashboard.HasNotificationForSession(sessionID) {
						t.Fatal("the card must be gone")
					}
				}
				pending.applyOutcomes(sessionID, map[string]client.StartupPromptOutcome{prompt.CommandID: outcome}, dashboard, map[string]*chat.ActiveChat{})
				if !pending.isSettled(sessionID, prompt.CommandID) {
					t.Fatal("the quick reply must settle")
				}
				saved, _ := dashboard.StandbyPromptDraft(sessionID)
				decoded, err := chat.PromptPayloadDecodeDraft(saved)
				if err != nil {
					t.Fatal(err)
				}
				want := ""
				switch {
				case occupied:
					want = "Keep the real composer"
				case refused && cardGone:
					want = "Quick response"
				}
				if decoded.Text != want {
					t.Fatalf("refused=%v cardGone=%v occupied=%v: draft %q, want %q", refused, cardGone, occupied, decoded.Text, want)
				}
				if refused {
					found := false
					for _, notice := range notices.History() {
						if notice.Failure && strings.Contains(notice.Text, "Quick reply to ") &&
							strings.Contains(notice.Text, "reply rejected") && strings.Contains(notice.Text, "Quick response") {
							found = true
						}
					}
					if !found {
						t.Fatal("the refusal must be reported with the unsent text")
					}
				}
				if cardGone {
					continue
				}
				next := dashboard.HandleKey(pressKey(tea.KeyEnter))
				if refused {
					again, ok := next.(tui.DashboardActionNotificationReply)
					if !ok {
						t.Fatal("refused reply stays available to retry")
					}
					if again.Prompt.Payload.Text != "Quick response" {
						t.Fatalf("retried text = %q", again.Prompt.Payload.Text)
					}
				} else if _, ok := next.(tui.DashboardActionOpen); !ok {
					t.Fatalf("success cleared the quick-reply line: got %#v", next)
				}
			}
		}
	}
}

func TestUnadmittedMovePromptsWaitForExactOriginAndReturnToDraftOnCancel(t *testing.T) {
	var pending startupPrompts
	updates := core.NewUnbounded[dashboardIoUpdate]()
	tracker, _ := client.NewCriticalOperationTracker()
	moveA := "move-a"
	pending.beginMove("session", &moveA)
	prompt := chat.StartupPrompt{
		CommandID:       "held-a",
		Payload:         chat.PromptPayloadText("original queued input"),
		MoveOperationID: &moveA,
	}
	pending.enqueue(namedSession{id: "session", name: "Session name"}, prompt, nil, updates, tracker)
	completed := true
	pending.observeMove("session", "historical-move", &completed)
	state := core.NewStateState()
	pending.flushHeld(&state, updates, tracker)
	if _, ok, _ := updates.TryRecv(); ok {
		t.Fatal("a historical Move must not release the new prompt")
	}
	if blockers := tracker.Blockers(); len(blockers) != 1 {
		t.Fatalf("local admission remains tracked for bounded quit: %v", blockers)
	}
	pending.finishMove("session", nil)
	pending.flushHeld(&state, updates, tracker)
	received, ok, _ := updates.TryRecv()
	if !ok {
		t.Fatal("expected local refusal")
	}
	refusal, ok := received.(dashboardIoUpdateStartupPromptOutcomes)
	if !ok {
		t.Fatalf("expected local refusal, got %#v", received)
	}
	if refusal.err != nil {
		t.Fatal(refusal.err)
	}
	dashboard := tui.NewDashboardState(core.DefaultConfig(), core.NewStateState(), map[string]client.ProfileQuota{})
	dashboard.SeedStandbyPrompt("session", "newer draft")
	pending.applyOutcomes(refusal.sessionID, refusal.outcomes, dashboard, map[string]*chat.ActiveChat{})
	if got := standbyDraft(t, dashboard, "session"); got != "newer draft\n\noriginal queued input" {
		t.Fatalf("draft = %q", got)
	}
	if blockers := tracker.Blockers(); len(blockers) != 0 {
		t.Fatalf("blockers = %v", blockers)
	}
	if pending.originAdmission("session", nil) != originAdmissionSend {
		t.Fatal("a prompt without an origin is sent")
	}

	moveB := "move-b"
	pending.beginMove("session", &moveB)
	prompt = chat.StartupPrompt{
		CommandID:       "held-b",
		Payload:         chat.PromptPayloadText("recalled before admission"),
		MoveOperationID: &moveB,
	}
	pending.enqueue(namedSession{id: "session", name: "Session name"}, prompt, nil, updates, tracker)
	pending.withdraw(namedSession{id: "session", name: "Session name"}, prompt, updates, tracker)
	received, ok, _ = updates.TryRecv()
	withdrawn, isWithdrawn := received.(dashboardIoUpdateStartupPromptWithdrawn)
	if !ok || !isWithdrawn || withdrawn.err != nil || !withdrawn.withdrawn {
		t.Fatalf("expected a local withdrawal, got %#v", received)
	}
	if blockers := tracker.Blockers(); len(blockers) != 0 {
		t.Fatalf("blockers = %v", blockers)
	}
}

func TestTransportBeforeAdmissionRetriesTheSamePayloadBeforeRecallCanRun(t *testing.T) {
	// Rust pauses Tokio's clock and advances it; this clock is advanced by
	// firing the backoff the retry asked for.
	requested := make(chan time.Duration, 1)
	fire := make(chan time.Time)
	original := admissionBackoffTimer
	admissionBackoffTimer = func(backoff time.Duration) <-chan time.Time {
		requested <- backoff
		return fire
	}
	t.Cleanup(func() { admissionBackoffTimer = original })

	mailbox := newSessionMailbox()
	defer mailbox.stop()
	independent := newSessionMailbox()
	defer independent.stop()
	type attempt struct {
		number int
		prompt chat.StartupPrompt
	}
	attempts := make(chan attempt, 4)
	recovered := make(chan struct{})
	var attemptCount atomic.Int64
	originalMove := "original-move"
	prompt := chat.StartupPrompt{
		CommandID:       "retry-stable",
		Payload:         chat.PromptPayloadText("keep exact payload"),
		MoveOperationID: &originalMove,
	}
	retried := prompt
	tracker, _ := client.NewCriticalOperationTracker()
	guard := tracker.Begin("queued exact-ID retry")
	if err := mailbox.admit(func(context.Context) {
		settlement, settle := newSettlement()
		defer settle()
		retryAdmission(settlement, func(context.Context) bool {
			number := int(attemptCount.Add(1) - 1)
			attempts <- attempt{number: number, prompt: retried}
			if number == 0 {
				return false
			}
			<-recovered
			return true
		})
		guard.Release()
	}); err != nil {
		t.Fatal(err)
	}
	recalled := make(chan struct{})
	if err := mailbox.admit(func(context.Context) { close(recalled) }); err != nil {
		t.Fatal(err)
	}
	first := <-attempts
	if !first.prompt.Equal(&prompt) {
		t.Fatalf("first attempt sent %#v", first.prompt)
	}
	select {
	case <-recalled:
		t.Fatal("recall ran before the admission resolved")
	default:
	}
	if blockers := tracker.Blockers(); len(blockers) != 1 {
		t.Fatalf("blockers = %v", blockers)
	}
	done := make(chan struct{})
	if err := independent.admit(func(context.Context) { close(done) }); err != nil {
		t.Fatal(err)
	}
	<-done
	// tokio::time::advance(1 s): the retry waits exactly the first backoff.
	if backoff := <-requested; backoff != time.Second {
		t.Fatalf("first backoff = %v", backoff)
	}
	fire <- time.Time{}
	second := <-attempts
	if second.number != 1 {
		t.Fatalf("second attempt number = %d", second.number)
	}
	if !second.prompt.Equal(&prompt) {
		t.Fatalf("second attempt sent %#v", second.prompt)
	}
	select {
	case <-recalled:
		t.Fatal("recall must remain behind the unresolved exact-ID admission")
	default:
	}
	close(recovered)
	<-recalled
	if blockers := tracker.Blockers(); len(blockers) != 0 {
		t.Fatalf("blockers = %v", blockers)
	}
}
