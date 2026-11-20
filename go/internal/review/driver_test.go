// Port of mj-review/src/driver/tests.rs.

package review

import (
	"encoding/json"
	"fmt"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func driverTestSeed() core.TurnReviewSeed {
	return core.TurnReviewSeed{
		Task:         "add a retry",
		UserMessages: []core.UserMessage{core.UserMessagePrompt("add a retry")},
		Declined:     []core.ReviewOverride{},
	}
}

func driverTestChangedDelta() []core.RepoDelta {
	baseline := "base-tree"
	return []core.RepoDelta{{
		Root:         "/w/app",
		BaselineTree: &baseline,
		CurrentTree:  "new-tree",
		Patch:        "diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@\n+retry\n",
		Diffstat:     "1 file changed, 1 insertion(+)",
		ChangedLines: 1,
		Files: []core.FileLineChange{{
			Path:       "src/lib.rs",
			Insertions: 1,
		}},
		MergedIn: []string{},
	}}
}

// startReviewer is the StartReviewer request for role, if requests hold one.
func startReviewer(requests []core.ReviewRequest, role string) (core.ReviewRequestStartReviewer, bool) {
	for _, request := range requests {
		if start, ok := request.(core.ReviewRequestStartReviewer); ok && start.Role == role {
			return start, true
		}
	}
	return core.ReviewRequestStartReviewer{}, false
}

// prompted is the command a role was just prompted under, from the
// requests it produced.
func prompted(t *testing.T, requests []core.ReviewRequest, role string) string {
	t.Helper()
	start, ok := startReviewer(requests, role)
	if !ok {
		t.Fatalf("%s was not prompted in %#v", role, requests)
	}
	return start.CommandID
}

func promptText(t *testing.T, requests []core.ReviewRequest, role string) string {
	t.Helper()
	start, ok := startReviewer(requests, role)
	if !ok {
		t.Fatalf("%s was not prompted in %#v", role, requests)
	}
	return start.Prompt
}

// running drives a review to the point where the reviewer has been
// prompted.
func running(t *testing.T) (*TurnReviewDriver, string) {
	t.Helper()
	driver, requests := TurnReviewDriverStart(driverTestSeed(), "review-test", 1)
	if !reflect.DeepEqual(requests, []core.ReviewRequest{core.ReviewRequestCaptureDelta{}}) {
		t.Fatalf("%#v", requests)
	}
	requests = driver.DeltaCaptured(driverTestChangedDelta())
	commandID := prompted(t, requests, "reviewer-1")
	prompt := promptText(t, requests, "reviewer-1")
	if !strings.Contains(prompt, "git -C /w/app diff --no-ext-diff --ignore-submodules=none base-tree new-tree") {
		t.Fatal(prompt)
	}
	if strings.Contains(prompt, "+retry") {
		t.Fatal("the prompt does not embed the diff")
	}
	if !strings.Contains(prompt, "add a retry") {
		t.Fatal(prompt)
	}
	return driver, commandID
}

// roundTrip stores the driver's snapshot as JSON and restores a driver from
// it, as a daemon restart does.
func roundTrip(t *testing.T, driver *TurnReviewDriver) *TurnReviewDriver {
	t.Helper()
	stored, err := json.Marshal(driver.Snapshot())
	if err != nil {
		t.Fatal(err)
	}
	var snapshot core.ReviewDriverSnapshot
	if err := json.Unmarshal(stored, &snapshot); err != nil {
		t.Fatal(err)
	}
	restored, err := TurnReviewDriverRestore(snapshot)
	if err != nil {
		t.Fatal(err)
	}
	return restored
}

func assertRequests(t *testing.T, got, want []core.ReviewRequest) {
	t.Helper()
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("requests:\n got %#v\nwant %#v", got, want)
	}
}

func assertLastVerdict(t *testing.T, driver *TurnReviewDriver, want core.ReviewVerdict) {
	t.Helper()
	if !sameVerdict(driver.LastVerdict(), want) {
		t.Fatalf("last verdict: got %#v, want %#v", driver.LastVerdict(), want)
	}
}

// Hard-won: 2a11e18a80: a workspace without a baseline was misleadingly labeled unchanged.

func TestFindingsAreTheVerdictWithoutASecondPass(t *testing.T) {
	driver, commandID := running(t)
	requests := driver.RoleTurnCompleted(commandID, "child-1", "[P1] src/lib.rs:1 -- unbounded retry loop")
	// Findings reap the reviewer and start nothing else.
	assertRequests(t, requests, []core.ReviewRequest{core.ReviewRequestCloseReviewer{Role: "reviewer-1"}})
	if driver.Finished() {
		t.Fatal("findings await mailbox delivery")
	}
	assertLastVerdict(t, driver, core.ReviewVerdictFindings{
		Synthesis: "Reviewer 1 (session child-1)\n[P1] src/lib.rs:1 -- unbounded retry loop",
		Evidence:  core.ReviewPassEvidence{},
	})
	want := []core.RoleStatus{{
		Role:  "reviewer-1",
		Label: "Reviewer 1",
		State: core.RoleStateFindings,
	}}
	if !reflect.DeepEqual(driver.Roles(), want) {
		t.Fatalf("a verdict keeps the completed role state available to surfaces: %#v", driver.Roles())
	}
}

func TestAnEmptyReportFailsTheReviewAndLeavesTheBaselineAlone(t *testing.T) {
	driver, commandID := running(t)
	driver.RoleTurnCompleted(commandID, "child-1", "  \n ")
	if driver.LastVerdict() == nil {
		t.Fatal("the failed verdict is retained")
	}
	failed, ok := driver.LastVerdict().(core.ReviewVerdictFailed)
	if !ok {
		t.Fatalf("an empty report must fail the review, got %#v", driver.Phase())
	}
	if !strings.Contains(failed.Reason, "empty report") {
		t.Fatal(failed.Reason)
	}
}

// The primary is told what it is getting: one reviewer's findings that
// nobody checked, which it verifies against source before acting.

func TestCancellingLeavesTheBaselineSoTheNextReviewCoversBothTurns(t *testing.T) {
	driver, _ := running(t)
	requests := driver.Cancel()
	assertRequests(t, requests, []core.ReviewRequest{
		core.ReviewRequestCloseReviewer{Role: "reviewer-1"},
		core.ReviewRequestClose{},
	})
	if phase := driver.Phase(); phase != (core.TurnReviewPhaseResolved{Resolution: core.ResolutionCancelled}) {
		t.Fatalf("%#v", phase)
	}
	if len(driver.Cancel()) != 0 {
		t.Fatal("cancelling twice is inert")
	}
}

func TestACompletionForAnotherCommandIsIgnored(t *testing.T) {
	driver, _ := running(t)
	if len(driver.RoleTurnCompleted("turn-review-reviewer-999", "child-1", "No findings.")) != 0 {
		t.Fatal("a replayed completion for another command cannot advance the review")
	}
	if driver.Finished() {
		t.Fatal("the review finished")
	}
}

func TestARequestFailureClosesWithoutAdvancingCoverage(t *testing.T) {
	driver, _ := running(t)
	requests := driver.RequestFailed("the reviewer could not start")
	assertRequests(t, requests, []core.ReviewRequest{core.ReviewRequestCloseReviewer{Role: "reviewer-1"}})
	if _, ok := driver.LastVerdict().(core.ReviewVerdictFailed); !ok {
		t.Fatalf("%#v", driver.LastVerdict())
	}
	if driver.Finished() {
		t.Fatal("failure awaits mailbox delivery")
	}
	restored := roundTrip(t, driver)
	assertLastVerdict(t, restored, driver.LastVerdict())
	assertRequests(t, restored.RecoveryRequests(), requests)
}

func TestAnEmptyPushSnapshotCanResumeVerdictDelivery(t *testing.T) {
	driver, _ := TurnReviewDriverStart(driverTestSeed(), "empty-push", 1)
	deltas := driverTestChangedDelta()
	deltas[0].CurrentTree = *deltas[0].BaselineTree
	deltas[0].Patch = ""
	deltas[0].Files = deltas[0].Files[:0]
	deltas[0].ChangedLines = 0
	driver.EmptyPushDeltaCaptured(deltas)
	restored := roundTrip(t, driver)
	assertLastVerdict(t, restored, core.ReviewVerdictClean{})
	if len(restored.AwaitedCommands()) != 0 {
		t.Fatalf("%#v", restored.AwaitedCommands())
	}
}

func TestTheReviewerPromptCarriesTheSeedDeclinedFindingsAndNoCorrectivePass(t *testing.T) {
	seed := driverTestSeed()
	seed.Declined = []core.ReviewOverride{{
		Finding: "[P1] src/lib.rs:1 -- no bound",
		Reason:  "The caller bounds it",
	}}
	driver, _ := TurnReviewDriverStart(seed, "review-test", 1)
	requests := driver.DeltaCaptured(driverTestChangedDelta())
	prompt := promptText(t, requests, "reviewer-1")
	if strings.Contains(prompt, "corrective pass") {
		t.Fatal(prompt)
	}
	if !strings.Contains(prompt, "Finding: [P1] src/lib.rs:1 -- no bound") {
		t.Fatal(prompt)
	}
	if !strings.Contains(prompt, "Reason: The caller bounds it") {
		t.Fatal(prompt)
	}
}

func TestOutOfOrderReportsRemainInLaunchOrderAndPartialFindingsKeepCoverageOpen(t *testing.T) {
	driver, _ := TurnReviewDriverStart(driverTestSeed(), "group-1", 3)
	requests := driver.DeltaCaptured(driverTestChangedDelta())
	first := prompted(t, requests, "reviewer-1")
	third := prompted(t, requests, "reviewer-3")
	if len(driver.RoleTurnCompleted(third, "session-three", "third finding")) != 0 {
		t.Fatal("one report cannot settle the group")
	}
	sessionTwo := "session-two"
	if len(driver.RoleFailed("reviewer-2", &sessionTwo, "harness exited")) != 0 {
		t.Fatal("one failure cannot settle the group")
	}
	driver = roundTrip(t, driver)
	if len(driver.RoleTurnCompleted(third, "session-three", "duplicate")) != 0 {
		t.Fatal("a settled role cannot report again")
	}
	requests = driver.RoleTurnCompleted(first, "session-one", "first finding")
	if len(requests) != 3 {
		t.Fatalf("%#v", requests)
	}
	findings, ok := driver.LastVerdict().(core.ReviewVerdictFindings)
	if !ok {
		t.Fatal("reports must retain findings")
	}
	synthesis := findings.Synthesis
	if !(strings.Index(synthesis, "session-one") < strings.Index(synthesis, "session-two")) {
		t.Fatal(synthesis)
	}
	if !(strings.Index(synthesis, "session-two") < strings.Index(synthesis, "session-three")) {
		t.Fatal(synthesis)
	}
	if !(strings.Contains(synthesis, "first finding") && strings.Contains(synthesis, "third finding")) {
		t.Fatal(synthesis)
	}
	if len(findings.Evidence.ReviewerFailures) != 1 {
		t.Fatalf("%#v", findings.Evidence)
	}
	if driver.LastVerdict().IsComplete() {
		t.Fatal("a failed reviewer keeps coverage open")
	}
}

func TestEveryCleanSlotIsRequiredBeforeTheGroupAdvances(t *testing.T) {
	driver, _ := TurnReviewDriverStart(driverTestSeed(), "group-clean", 2)
	requests := driver.DeltaCaptured(driverTestChangedDelta())
	first := prompted(t, requests, "reviewer-1")
	second := prompted(t, requests, "reviewer-2")
	if len(driver.RoleTurnCompleted(second, "session-two", "No findings.")) != 0 {
		t.Fatal("one clean report cannot settle the group")
	}
	if driver.Verdict() != nil {
		t.Fatalf("%#v", driver.Verdict())
	}
	driver.RoleTurnCompleted(first, "session-one", "No findings.")
	assertLastVerdict(t, driver, core.ReviewVerdictClean{})
}

func TestAggregateKeepsLaterReportsAfterMoreThanSixtyFourKibibytes(t *testing.T) {
	driver, _ := TurnReviewDriverStart(driverTestSeed(), "large-group", 3)
	requests := driver.DeltaCaptured(driverTestChangedDelta())
	for index := 1; index <= 3; index++ {
		command := prompted(t, requests, fmt.Sprintf("reviewer-%d", index))
		report := fmt.Sprintf("report-%d: %s", index, strings.Repeat("x", 31_000))
		driver.RoleTurnCompleted(command, fmt.Sprintf("session-%d", index), report)
	}
	findings, ok := driver.LastVerdict().(core.ReviewVerdictFindings)
	if !ok {
		t.Fatal("expected aggregate findings")
	}
	if len(findings.Synthesis) <= 64*1024 {
		t.Fatalf("synthesis is %d bytes", len(findings.Synthesis))
	}
	for index := 1; index <= 3; index++ {
		if !strings.Contains(findings.Synthesis, fmt.Sprintf("report-%d:", index)) {
			t.Fatalf("report %d is missing", index)
		}
		if !strings.Contains(findings.Synthesis, fmt.Sprintf("session-%d", index)) {
			t.Fatalf("session %d is missing", index)
		}
	}
}

func TestRestartReplaysOriginalPromptOnlyForUnsettledRoles(t *testing.T) {
	driver, capture := TurnReviewDriverStart(driverTestSeed(), "replay-group", 2)
	assertRequests(t, driver.RecoveryRequests(), capture)
	starts := driver.DeltaCaptured(driverTestChangedDelta())
	first := prompted(t, starts, "reviewer-1")
	if len(driver.RoleTurnCompleted(first, "session-one", "No findings.")) != 0 {
		t.Fatal("one clean report cannot settle the group")
	}
	restored := roundTrip(t, driver)
	assertRequests(t, restored.RecoveryRequests(), []core.ReviewRequest{starts[1]})
	if len(restored.RoleTurnCompleted(first, "session-one", "late duplicate")) != 0 {
		t.Fatal("a settled role cannot report again")
	}
	second := prompted(t, starts, "reviewer-2")
	settled := restored.RoleTurnCompleted(second, "session-two", "No findings.")
	restored = roundTrip(t, restored)
	assertRequests(t, restored.RecoveryRequests(), settled)
	assertLastVerdict(t, restored, core.ReviewVerdictClean{})
}

func writingDriver(id string) (*TurnReviewDriver, []core.ReviewRequest) {
	driver, _ := TurnReviewDriverStart(driverTestSeed(), id, 2)
	requests := driver.DeltaCapturedWithWriter(driverTestChangedDelta(), true)
	return driver, requests
}

func startedRoles(requests []core.ReviewRequest) []string {
	roles := []string{}
	for _, request := range requests {
		if start, ok := request.(core.ReviewRequestStartReviewer); ok {
			roles = append(roles, start.Role)
		}
	}
	return roles
}

func lastRequest(requests []core.ReviewRequest) core.ReviewRequest {
	if len(requests) == 0 {
		return nil
	}
	return requests[len(requests)-1]
}

func TestABriefWriterRunsAloneAndItsBriefReachesEveryReviewerPrompt(t *testing.T) {
	driver, requests := writingDriver("brief-written")
	if len(requests) != 1 {
		t.Fatalf("only the writer starts first: %#v", requests)
	}
	writer, ok := requests[0].(core.ReviewRequestStartBriefWriter)
	if !ok {
		t.Fatalf("only the writer starts first: %#v", requests)
	}
	if writer.CommandID != core.BriefWriterCommandID("brief-written") {
		t.Fatal(writer.CommandID)
	}
	if !strings.Contains(writer.Prompt, "base-tree new-tree") {
		t.Fatal(writer.Prompt)
	}
	if !strings.Contains(writer.Prompt, "add a retry") {
		t.Fatal(writer.Prompt)
	}
	if _, ok := driver.Phase().(core.TurnReviewPhaseWritingBrief); !ok {
		t.Fatalf("%#v", driver.Phase())
	}
	if want := [][2]string{{core.BriefWriterRole, writer.CommandID}}; !reflect.DeepEqual(driver.AwaitedCommands(), want) {
		t.Fatalf("%#v", driver.AwaitedCommands())
	}
	// A reviewer report or failure cannot settle a role before it starts.
	if len(driver.RoleFailed("reviewer-1", nil, "early")) != 0 {
		t.Fatal("a reviewer failed before it started")
	}

	started := driver.BriefWritten("## Paths\n- `a.rs:1` calls `b.rs:2`")
	if roles := startedRoles(started); !slices.Equal(roles, []string{"reviewer-1", "reviewer-2"}) {
		t.Fatalf("%#v", roles)
	}
	if lastRequest(started) != (core.ReviewRequestCloseBriefWriter{}) {
		t.Fatalf("%#v", started)
	}
	for _, role := range []string{"reviewer-1", "reviewer-2"} {
		text := promptText(t, started, role)
		if !strings.Contains(text, "## Relations computed from the repository") {
			t.Fatal(text)
		}
		if !strings.Contains(text, "`a.rs:1` calls `b.rs:2`") {
			t.Fatal(text)
		}
	}
	if _, ok := driver.Phase().(core.TurnReviewPhaseRunning); !ok {
		t.Fatalf("%#v", driver.Phase())
	}
	if len(driver.AwaitedCommands()) != 2 {
		t.Fatalf("%#v", driver.AwaitedCommands())
	}
	// A second handback changes nothing.
	if len(driver.BriefWritten("again")) != 0 {
		t.Fatal("a second handback restarted the reviewers")
	}
	if len(driver.BriefFailed("late")) != 0 {
		t.Fatal("a late failure restarted the reviewers")
	}
}

func TestAFailedOrEmptyBriefStartsTheReviewersWithoutOneAndSaysWhy(t *testing.T) {
	failure := "the writer ended without a handback"
	for _, c := range []struct {
		failure *string
		reason  string
	}{
		{&failure, "the writer ended without a handback"},
		{nil, "empty brief"},
	} {
		driver, _ := writingDriver("brief-failed")
		var requests []core.ReviewRequest
		if c.failure != nil {
			requests = driver.BriefFailed(*c.failure)
		} else {
			requests = driver.BriefWritten("  \n")
		}
		if len(requests) == 0 {
			t.Fatalf("the notice comes first: %#v", requests)
		}
		noted, ok := requests[0].(core.ReviewRequestNoteBriefSkipped)
		if !ok {
			t.Fatalf("the notice comes first: %#v", requests)
		}
		rest := requests[1:]
		if !strings.Contains(noted.Reason, c.reason) {
			t.Fatal(noted.Reason)
		}
		if roles := startedRoles(rest); !slices.Equal(roles, []string{"reviewer-1", "reviewer-2"}) {
			t.Fatalf("%#v", roles)
		}
		if lastRequest(rest) != (core.ReviewRequestCloseBriefWriter{}) {
			t.Fatalf("%#v", rest)
		}
		if strings.Contains(promptText(t, rest, "reviewer-1"), "Relations computed") {
			t.Fatal("a failed brief reached the reviewers")
		}
		// The review then settles like any other.
		for index, role := range []string{"reviewer-1", "reviewer-2"} {
			driver.RoleTurnCompleted(prompted(t, rest, role), fmt.Sprintf("s%d", index), "No findings.")
		}
		assertLastVerdict(t, driver, core.ReviewVerdictClean{})
	}
}

func TestCancellingWhileTheWriterWorksClosesItAndStartsNoReviewer(t *testing.T) {
	driver, _ := writingDriver("brief-cancelled")
	requests := driver.Cancel()
	if !slices.Contains(requests, core.ReviewRequest(core.ReviewRequestCloseBriefWriter{})) {
		t.Fatalf("%#v", requests)
	}
	if lastRequest(requests) != (core.ReviewRequestClose{}) {
		t.Fatalf("%#v", requests)
	}
	if len(startedRoles(requests)) != 0 {
		t.Fatalf("%#v", requests)
	}
	if !driver.Finished() {
		t.Fatal("the review did not finish")
	}
	// The writer's late answer cannot start reviewers in a cancelled review.
	if len(driver.BriefWritten("late")) != 0 {
		t.Fatal("a late brief started reviewers")
	}
	if len(driver.BriefFailed("late")) != 0 {
		t.Fatal("a late failure started reviewers")
	}
}

func TestAGroupWideFailureWhileTheWriterWorksClosesTheWriter(t *testing.T) {
	driver, _ := writingDriver("brief-group-failure")
	requests := driver.RequestFailed("the worker is gone")
	if !slices.Contains(requests, core.ReviewRequest(core.ReviewRequestCloseBriefWriter{})) {
		t.Fatalf("%#v", requests)
	}
	if len(startedRoles(requests)) != 0 {
		t.Fatalf("%#v", requests)
	}
}

// Restart during WritingBrief relaunches or reattaches the writer, never the
// reviewers; restart after the brief starts the reviewers once and not the
// writer.
func TestARestartDuringTheBriefReplaysTheWriterAndNeverStartsReviewersEarly(t *testing.T) {
	driver, requests := writingDriver("brief-restart")
	restored := roundTrip(t, driver)
	if _, ok := restored.Phase().(core.TurnReviewPhaseWritingBrief); !ok {
		t.Fatalf("%#v", restored.Phase())
	}
	assertRequests(t, restored.RecoveryRequests(), requests)
	if restored.Status() != "writing a relation brief for the reviewers…" {
		t.Fatal(restored.Status())
	}

	restored.BriefWritten("## Paths\n- none")
	again := roundTrip(t, restored)
	replay := again.RecoveryRequests()
	if roles := startedRoles(replay); !slices.Equal(roles, []string{"reviewer-1", "reviewer-2"}) {
		t.Fatalf("%#v", roles)
	}
	for _, request := range replay {
		switch request.(type) {
		case core.ReviewRequestStartReviewer, core.ReviewRequestCloseBriefWriter:
		default:
			t.Fatalf("the finished writer is not started again: %#v", replay)
		}
	}
	if !strings.Contains(promptText(t, replay, "reviewer-1"), "## Paths\n- none") {
		t.Fatal(promptText(t, replay, "reviewer-1"))
	}
}

// Hard-won: 5fc728617: a persisted writer outcome lost its cleanup on restart.
func TestARestartAfterEitherWriterOutcomeReplaysItsCloseOnce(t *testing.T) {
	for _, handback := range []bool{true, false} {
		driver, _ := writingDriver("writer-close-restart")
		if handback {
			driver.BriefWritten("## Paths\n- `a.rs:1` calls `b.rs:2`")
		} else {
			driver.BriefFailed("writer stopped")
		}
		restored := roundTrip(t, driver)
		replay := restored.RecoveryRequests()
		closes, notes := 0, 0
		for _, request := range replay {
			switch request := request.(type) {
			case core.ReviewRequestCloseBriefWriter:
				closes++
			case core.ReviewRequestNoteBriefSkipped:
				if request.Reason == "writer stopped" {
					notes++
				}
			}
		}
		if closes != 1 {
			t.Fatalf("handback=%v: %d closes in %#v", handback, closes, replay)
		}
		if roles := startedRoles(replay); !slices.Equal(roles, []string{"reviewer-1", "reviewer-2"}) {
			t.Fatalf("%#v", roles)
		}
		wantNotes := 0
		if !handback {
			wantNotes = 1
		}
		if notes != wantNotes {
			t.Fatalf("handback=%v: %d notices in %#v", handback, notes, replay)
		}
	}
}

// Hard-won: 5fc728617: the reviewer's clean verdict was accepted as a relation brief.
func TestALeakedCleanReviewerHandbackSkipsTheBrief(t *testing.T) {
	driver, _ := writingDriver("writer-clean-verdict")
	requests := driver.BriefWritten("  No findings.\n")
	if len(requests) == 0 {
		t.Fatal("no requests")
	}
	if _, ok := requests[0].(core.ReviewRequestNoteBriefSkipped); !ok {
		t.Fatalf("%#v", requests)
	}
	if strings.Contains(promptText(t, requests, "reviewer-1"), "Relations computed") {
		t.Fatal("the clean verdict reached the reviewers as a brief")
	}
	if driver.Snapshot().Seed.Brief != nil {
		t.Fatalf("%q", *driver.Snapshot().Seed.Brief)
	}
}
