// Port of mj-store/src/database/tests.rs, continued (turn reviews and the
// turn record a projection page carries).

package store

import (
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/review"
)

func TestOldReviewRowsLoadWithoutFindingsAndRecoveryKeepsNewFindings(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	connection := dtOpen(t, database)
	dtExec(t, connection,
		"INSERT INTO turn_review_state(session_id, baselines, reviewed_through_ordinal) VALUES (?1, ?2, 0)",
		"session-1", "{}")
	dtOK(t, connection.Close())
	state := dtMust(turnReviewStateIn(database, "session-1"))
	dtEqual(t, len(state.PushFindings), 0)
	dtEqual(t, len(state.OpenReviews), 0)
	state.PushFindings = []core.PushFindingsRecord{{
		ReviewID:  "review-1",
		Trees:     map[string][2]string{"/workspace/app": {"tree-1", "head-1"}},
		Synthesis: "fix the regression",
	}}
	state.OpenReviews = []core.OpenReview{{Active: dtPtr("interrupted")}}
	dtOK(t, saveTurnReviewStateIn(database, "session-1", &state))
	dtEqual(t, dtMust(clearInterruptedTurnReviewsIn(database)), []string{"session-1"})
	restored := dtMust(turnReviewStateIn(database, "session-1"))
	dtEqual(t, restored.PushFindings, state.PushFindings)
	dtEqual(t, len(restored.OpenReviews), 0)
}

func dtReviewSeed() core.TurnReviewSeed {
	return core.TurnReviewSeed{
		Task:         "inspect the parser",
		UserMessages: []core.UserMessage{},
		Declined:     []core.ReviewOverride{},
	}
}

func dtRepoDelta(root string) core.RepoDelta {
	return core.RepoDelta{
		Root:         root,
		BaseCommit:   dtPtr("basecommit"),
		HeadCommit:   dtPtr("headcommit"),
		BaselineTree: dtPtr("before"),
		CurrentTree:  "after",
		Patch:        "+one",
	}
}

func TestReviewPartialsCommitWithOutcomesAndAreNotRepeatedAfterReload(t *testing.T) {
	database := dtDatabase(t, "review.sqlite3")
	owner := "async-review-owner"
	record := session(owner, "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	driver, _ := review.TurnReviewDriverStart(dtReviewSeed(), "async-review", 3)
	driver.DeltaCaptured([]core.RepoDelta{dtRepoDelta("/workspace/app"), dtRepoDelta("/workspace/other")})
	registrations := []core.ReviewerRegistration{}
	for index := 1; index <= 3; index++ {
		registrations = append(registrations, core.ReviewerRegistration{
			Role:           fmt.Sprintf("reviewer-%d", index),
			RequestKey:     fmt.Sprintf("review-key-%d", index),
			ChildSessionID: dtPtr(fmt.Sprintf("review-child-%d", index)),
		})
	}
	state := core.TurnReviewState{
		OpenReviews: []core.OpenReview{{
			Active: dtPtr("async-review"),
			ActiveGroup: &core.ActiveReviewGroup{
				Driver:        driver.Snapshot(),
				Reviewers:     make([]core.ResolvedReviewSettings, 3),
				Registrations: registrations,
			},
		}},
	}
	dtOK(t, saveTurnReviewStateIn(database, owner, &state))
	command := driver.Snapshot().Roles[0].CommandID
	driver.RoleTurnCompleted(command, "review-child-1", "No findings.")
	state.OpenReviews[0].ActiveGroup.Driver = driver.Snapshot()
	dtOK(t, saveTurnReviewStateIn(database, owner, &state))
	beforeFindings := dtCloneReviewState(state)
	connection := dtOpen(t, database)
	outbox := func() []core.MailboxEvent {
		events := []core.MailboxEvent{}
		for _, row := range dtQueryColumn[string](t, connection, "SELECT event_json FROM mailbox_outbox ORDER BY event_key") {
			var event core.MailboxEvent
			dtOK(t, json.Unmarshal([]byte(row), &event))
			events = append(events, event)
		}
		return events
	}
	dtEqual(t, len(outbox()), 0, "clean outcomes do not send partials")
	dtExec(t, connection, `CREATE TRIGGER fail_second_review_partial BEFORE INSERT ON mailbox_outbox
             WHEN (SELECT COUNT(*) FROM mailbox_outbox) > 0
             BEGIN SELECT RAISE(ABORT, 'outbox unavailable'); END;`)
	command = driver.Snapshot().Roles[1].CommandID
	driver.RoleTurnCompleted(command, "review-child-2", "The parser drops queued input.")
	state.OpenReviews[0].ActiveGroup.Driver = driver.Snapshot()
	err := dtErr(t, saveTurnReviewStateIn(database, owner, &state))
	dtTrue(t, strings.Contains(err.Error(), "outbox unavailable"), err)
	dtEqual(t, dtMust(turnReviewStateIn(database, owner)), beforeFindings)
	dtEqual(t, len(outbox()), 0, "the first repository's message rolls back too")
	dtExec(t, connection, "DROP TRIGGER fail_second_review_partial")
	dtOK(t, saveTurnReviewStateIn(database, owner, &state))
	messages := outbox()
	dtEqual(t, len(messages), 2, "one partial per reviewed repository")
	for index := range messages {
		text := core.RenderMailboxEvent(&messages[index])
		dtTrue(t, strings.HasPrefix(text, "Review 2/3 — partial result"), text)
		dtTrue(t, strings.Contains(text, "The parser drops queued input."), text)
		dtTrue(t, strings.Contains(text, "basecommit..headcommit"), text)
	}
	restored := dtMust(turnReviewStateIn(database, owner))
	dtOK(t, saveTurnReviewStateIn(database, owner, &restored))
	dtEqual(t, outbox(), messages, "reloading and saving sends no duplicates")
	restoredDriver := dtMust(review.TurnReviewDriverRestore(restored.OpenReviews[0].ActiveGroup.Driver))
	command = restoredDriver.Snapshot().Roles[2].CommandID
	restoredDriver.RoleTurnCompleted(command, "review-child-3", "No findings.")
	restored.OpenReviews[0].ActiveGroup.Driver = restoredDriver.Snapshot()
	dtOK(t, saveTurnReviewStateIn(database, owner, &restored))
	dtEqual(t, outbox(), messages, "the final result uses the existing final delivery")
}

// dtCloneReviewState is TurnReviewState::clone: a copy whose open review
// groups later edits of the original cannot reach.
func dtCloneReviewState(state core.TurnReviewState) core.TurnReviewState {
	clone := state
	clone.OpenReviews = make([]core.OpenReview, len(state.OpenReviews))
	for index, open := range state.OpenReviews {
		if open.ActiveGroup != nil {
			group := *open.ActiveGroup
			open.ActiveGroup = &group
		}
		clone.OpenReviews[index] = open
	}
	return clone
}

// Revision 145 moves a session's open review and findings record into lists.
// A store an older build left with a restored review group, a findings record
// and an interrupted marker must keep all three, with the marker still
// cleared through its own path, and refuse that older build afterwards.
func TestReviewStateMigrationMovesTheOpenReviewAndFindingsIntoLists(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	for _, id := range []string{"session-group", "session-interrupted"} {
		record := session(id, "project-1")
		dtOK(t, SaveSessionTo(database, &record))
	}
	driver, _ := review.TurnReviewDriverStart(dtReviewSeed(), "group-1", 1)
	state := core.TurnReviewState{
		PushFindings: []core.PushFindingsRecord{{
			ReviewID:  "earlier",
			Trees:     map[string][2]string{"/workspace": {"tree", "head"}},
			Synthesis: "a finding",
		}},
		OpenReviews: []core.OpenReview{{
			Active: dtPtr("group-1"),
			ActiveGroup: &core.ActiveReviewGroup{
				Driver:    driver.Snapshot(),
				Reviewers: make([]core.ResolvedReviewSettings, 1),
				Registrations: []core.ReviewerRegistration{{
					Role:           "reviewer-1",
					RequestKey:     "key-1",
					ChildSessionID: dtPtr("child-1"),
				}},
			},
		}},
	}
	state.DeclineFindings([]core.ReviewOverride{{Finding: "F", Reason: "R"}}, []string{"/workspace"})
	dtOK(t, saveTurnReviewStateIn(database, "session-group", &state))
	interrupted := core.TurnReviewState{
		OpenReviews: []core.OpenReview{{Active: dtPtr("review-preparing")}},
	}
	dtOK(t, saveTurnReviewStateIn(database, "session-interrupted", &interrupted))

	connection := dtMust(OpenConnection(database))
	restorePre145ReviewColumns(t, connection)
	restorePre153TranscriptLayout(t, connection)
	dtExec(t, connection, `DELETE FROM schema_migrations WHERE version >= 145;
             UPDATE schema_compatibility SET minimum_compatible_version = 144 WHERE singleton = 1;
             PRAGMA user_version = 144;`)
	var active, group, findings *string
	dtOK(t, connection.QueryRow(
		"SELECT active, active_group, push_findings FROM turn_review_state WHERE session_id='session-group'",
	).Scan(&active, &group, &findings))
	dtEqual(t, active, dtPtr("group-1"))
	dtTrue(t, group != nil && strings.HasPrefix(*group, "{"), "an older build stored one group object")
	dtTrue(t, findings != nil && strings.HasPrefix(*findings, "{"), "an older build stored one findings object")
	dtOK(t, connection.Close())
	forgetVerifiedSchema(database)

	dtEqual(t, dtMust(turnReviewStateIn(database, "session-group")), state)
	dtEqual(t, dtMust(activeReviewGroupsIn(database)), []ActiveReviewGroupState{{SessionID: "session-group", State: state}})
	dtEqual(t, dtMust(clearInterruptedTurnReviewsIn(database)), []string{"session-interrupted"})
	dtEqual(t, dtMust(turnReviewStateIn(database, "session-interrupted")), core.TurnReviewState{})
	dtEqual(t, dtMust(turnReviewStateIn(database, "session-group")), state)
	schema := dtMust(readSchemaState(dtOpen(t, database)))
	dtEqual(t, schema.minimumCompatible, dtPtr(minimumCompatibleVersion))
	dtTrue(t, schema.ensureSupportedBy(144) != nil)
}

func TestActiveReviewGroupSurvivesInterruptedSweepWithRegisteredChildren(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-group", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	driver, _ := review.TurnReviewDriverStart(dtReviewSeed(), "group-1", 2)
	group := core.ActiveReviewGroup{
		Driver:    driver.Snapshot(),
		Reviewers: make([]core.ResolvedReviewSettings, 2),
		Registrations: []core.ReviewerRegistration{
			{Role: "reviewer-1", RequestKey: "key-1", ChildSessionID: dtPtr("child-1")},
			{Role: "reviewer-2", RequestKey: "key-2"},
		},
	}
	state := core.TurnReviewState{
		OpenReviews: []core.OpenReview{{Active: dtPtr("group-1"), ActiveGroup: &group}},
	}
	dtOK(t, saveTurnReviewStateIn(database, "session-group", &core.TurnReviewState{}))
	dtOK(t, saveTurnReviewStateIn(database, "session-group", &state))
	dtEqual(t, dtMust(activeReviewGroupsIn(database)), []ActiveReviewGroupState{{SessionID: "session-group", State: state}})
	dtEqual(t, len(dtMust(clearInterruptedTurnReviewsIn(database))), 0)
	dtEqual(t, dtMust(turnReviewStateIn(database, "session-group")), state)
}

// Hard-won: ee65b804b: a follow-up for a never-reviewed session had no row to mark and was never durable.

// A finished review keeps each reviewer's model, effort, profile, handback and
// verdict after its child session is destroyed and its report pruned.
func TestAFinishedReviewKeepsEachReviewersOutputAfterItsSessionIsDestroyed(t *testing.T) {
	database := dtDatabase(t, "review-reports.sqlite3")
	owner := "review-owner"
	ownerRecord := session(owner, "project-1")
	dtOK(t, SaveSessionTo(database, &ownerRecord))
	children := []string{"review-child-1", "review-child-2"}
	for _, child := range children {
		record := session(child, "project-1")
		dtOK(t, SaveSessionTo(database, &record))
	}
	driver, _ := review.TurnReviewDriverStart(dtReviewSeed(), "finished-review", 2)
	driver.DeltaCaptured([]core.RepoDelta{dtRepoDelta("/workspace/app")})
	var commands []string
	for _, slot := range driver.Snapshot().Roles {
		commands = append(commands, slot.CommandID)
	}
	for index, message := range []string{"The parser drops queued input.", "No findings."} {
		dtMust(recordSubagentHandbackTo(database, children[index], &core.SubagentHandback{
			CommandID:    fmt.Sprintf("prompt-%d", index),
			Message:      message,
			RecordedAtMs: 100 + int64(index),
		}))
	}
	driver.RoleTurnCompleted(commands[0], children[0], "The parser drops queued input.")
	driver.RoleTurnCompleted(commands[1], children[1], "No findings.")
	reviewers := []core.ResolvedReviewSettings{
		{
			Profile: "codex",
			Main: core.ReviewModelSettings{
				Model:  dtPtr("gpt-6-terra"),
				Effort: dtPtr("high"),
			},
		},
		{
			Profile: "claude",
			Main: core.ReviewModelSettings{
				Model:  dtPtr("claude-sonnet"),
				Effort: dtPtr("medium"),
			},
			Autoconfigured: true,
		},
	}
	registrations := []core.ReviewerRegistration{}
	for index, child := range children {
		registrations = append(registrations, core.ReviewerRegistration{
			Role:           fmt.Sprintf("reviewer-%d", index+1),
			RequestKey:     fmt.Sprintf("review-key-%d", index),
			ChildSessionID: dtPtr(child),
		})
	}
	state := core.TurnReviewState{
		OpenReviews: []core.OpenReview{{
			Active: dtPtr("finished-review"),
			ActiveGroup: &core.ActiveReviewGroup{
				Driver:        driver.Snapshot(),
				Reviewers:     reviewers,
				Registrations: registrations,
			},
		}},
	}
	dtOK(t, saveReviewerOutcomeIn(database, owner, &state, []AcceptedReport{
		{ReviewID: "finished-review", Role: "reviewer-1", Text: "The parser drops queued input."},
		{ReviewID: "finished-review", Role: "reviewer-2", Text: "No findings."},
	}))
	turn := core.MaterializedTurnOutcome{
		Usage: &core.TokenUsage{
			Scope:        core.UsageScopeTurn,
			TotalTokens:  120,
			InputTokens:  100,
			OutputTokens: 20,
		},
		CommandID:         "prompt-0",
		AcceptedOrdinal:   dtPtr(uint64(1)),
		TurnStartPosition: dtPtr(uint64(1)),
		CompletedOrdinal:  1,
		CompletedAtMs:     200,
		Outcome:           core.TurnOutcomeKindCompleted{StopReason: "EndTurn"},
	}
	connection := dtOpen(t, database)
	dtExec(t, connection, `INSERT INTO session_turn_usage(
                 session_id, command_id, completed_ordinal, turn_start_position, body
             ) VALUES (?1, ?2, ?3, ?4, ?5)`,
		children[0], turn.CommandID, int64(turn.CompletedOrdinal), int64(*turn.TurnStartPosition),
		dtMust(serializeTurnOutcome(&turn)))
	dtOK(t, connection.Close())
	closed := state
	closed.OpenReviews = nil
	// A reviewer's child can record another command's handback afterwards. The
	// text the review settled from is the authority and must not be replaced.
	dtMust(recordSubagentHandbackTo(database, children[0], &core.SubagentHandback{
		CommandID:    "a-later-command",
		Message:      "Findings from a later task.",
		RecordedAtMs: 300,
	}))
	dtOK(t, saveTurnReviewStateIn(database, owner, &state))
	dtOK(t, saveReviewerOutcomeIn(database, owner, &state, nil))
	dtOK(t, closeTurnReviewStateIn(database, owner, "finished-review", &closed, core.FinishedReviewResolutionCompleted))
	// The reviewer sessions and their reports are gone, as they are after the
	// review host's destroy path.
	for _, child := range children {
		dtOK(t, deleteSessionFrom(database, child))
	}
	connection = dtOpen(t, database)
	dtExec(t, connection, "DELETE FROM subagent_sessions")
	dtExec(t, connection, "DELETE FROM subagent_handbacks")

	finished := dtMust(finishedReviewIn(database, owner, nil))
	if finished == nil {
		t.Fatal("the finished review survives its reviewer sessions")
	}
	dtEqual(t, finished.ReviewID, "finished-review")
	dtEqual(t, finished.Resolution, core.FinishedReviewResolutionCompleted)
	dtEqual(t, len(finished.Roles), 2)
	first := finished.Roles[0]
	dtEqual(t, first.Role, "reviewer-1")
	dtEqual(t, first.Label, "Reviewer 1")
	dtEqual(t, first.ChildSessionID, dtPtr(children[0]))
	dtEqual(t, first.Profile, "codex")
	dtEqual(t, first.Model, dtPtr("gpt-6-terra"))
	dtEqual(t, first.Effort, dtPtr("high"))
	dtEqual(t, first.Handback, dtPtr("The parser drops queued input."),
		"a later handback for another command never replaces the accepted one")
	_, findingsVerdict := first.Verdict.(core.ReviewVerdictFindings)
	dtTrue(t, findingsVerdict)
	dtTrue(t, first.Turn != nil && first.Turn.Usage != nil && first.Turn.Usage.TotalTokens == 120)
	second := finished.Roles[1]
	dtEqual(t, second.Profile, "claude")
	dtEqual(t, second.Model, dtPtr("claude-sonnet"))
	dtEqual(t, second.Handback, dtPtr("No findings."))
	dtEqual(t, second.Verdict, core.ReviewVerdict(core.ReviewVerdictClean{}))
	dtTrue(t, second.Turn == nil)
	dtTrue(t, dtMust(finishedReviewIn(database, owner, dtPtr("finished-review"))) != nil)
	dtTrue(t, dtMust(finishedReviewIn(database, owner, dtPtr("missing"))) == nil)
	// Destroying the requesting session removes its reports with it, so a
	// destroyed session cannot leave unreachable handbacks behind.
	dtOK(t, deleteSessionFrom(database, owner))
	remaining := dtQuery[int64](t, dtOpen(t, database), "SELECT COUNT(*) FROM review_reports")
	dtEqual(t, remaining, int64(0))
}

// A reviewer that ended without handing back fails its role. Its child may
// still hold a handback from an earlier command, which the review host
// rejected as this role's outcome; the finished report must not present that
// text as this review's findings.
func TestAFailedReviewerKeepsNoHandbackFromAnotherCommand(t *testing.T) {
	database := dtDatabase(t, "failed-reviewer.sqlite3")
	owner := "failed-review-owner"
	child := "failed-review-child"
	for _, id := range []string{owner, child} {
		record := session(id, "project-1")
		dtOK(t, SaveSessionTo(database, &record))
	}
	dtMust(recordSubagentHandbackTo(database, child, &core.SubagentHandback{
		CommandID:    "another-command",
		Message:      "Findings from an earlier task.",
		RecordedAtMs: 100,
	}))
	driver, _ := review.TurnReviewDriverStart(dtReviewSeed(), "failed-review", 1)
	driver.DeltaCaptured([]core.RepoDelta{dtRepoDelta("/workspace/app")})
	driver.RoleFailed("reviewer-1", dtPtr(child), "the reviewer's session ended before it handed back")
	state := core.TurnReviewState{
		OpenReviews: []core.OpenReview{{
			Active: dtPtr("failed-review"),
			ActiveGroup: &core.ActiveReviewGroup{
				Driver:    driver.Snapshot(),
				Reviewers: make([]core.ResolvedReviewSettings, 1),
				Registrations: []core.ReviewerRegistration{{
					Role:           "reviewer-1",
					RequestKey:     "review-key",
					ChildSessionID: dtPtr(child),
				}},
			},
		}},
	}
	dtOK(t, closeTurnReviewStateIn(database, owner, "failed-review", &state, core.FinishedReviewResolutionFailed))
	finished := dtMust(finishedReviewIn(database, owner, nil))
	if finished == nil {
		t.Fatal("the failed review is recorded")
	}
	dtEqual(t, finished.Resolution, core.FinishedReviewResolutionFailed)
	dtEqual(t, len(finished.Roles), 1)
	_, failed := finished.Roles[0].Verdict.(core.ReviewVerdictFailed)
	dtTrue(t, failed)
	dtTrue(t, finished.Roles[0].Handback == nil, "a rejected handback is not this review's output")
}

// A review cancelled after its reviewer reached a verdict must be reported as
// cancelled. The host clears the active group and passes its own decision,
// while the stored group still carries the verdict it never delivered.
func TestACancelledReviewKeepsTheHostDecisionOverItsStoredVerdict(t *testing.T) {
	database := dtDatabase(t, "cancelled-review.sqlite3")
	owner := "cancelled-review-owner"
	child := "cancelled-review-child"
	for _, id := range []string{owner, child} {
		record := session(id, "project-1")
		dtOK(t, SaveSessionTo(database, &record))
	}
	driver, _ := review.TurnReviewDriverStart(dtReviewSeed(), "cancelled-review", 1)
	driver.DeltaCaptured([]core.RepoDelta{dtRepoDelta("/workspace/app")})
	command := driver.Snapshot().Roles[0].CommandID
	driver.RoleTurnCompleted(command, child, "No findings.")
	state := core.TurnReviewState{
		OpenReviews: []core.OpenReview{{
			Active: dtPtr("cancelled-review"),
			ActiveGroup: &core.ActiveReviewGroup{
				Driver:    driver.Snapshot(),
				Reviewers: make([]core.ResolvedReviewSettings, 1),
				Registrations: []core.ReviewerRegistration{{
					Role:           "reviewer-1",
					RequestKey:     "review-key",
					ChildSessionID: dtPtr(child),
				}},
			},
		}},
	}
	dtOK(t, saveTurnReviewStateIn(database, owner, &state))
	// The verdict is in the stored driver; the host decides to cancel it.
	cleared := state
	cleared.OpenReviews = nil
	dtOK(t, closeTurnReviewStateIn(database, owner, "cancelled-review", &cleared, core.FinishedReviewResolutionCancelled))
	finished := dtMust(finishedReviewIn(database, owner, nil))
	if finished == nil {
		t.Fatal("the cancelled review is recorded")
	}
	dtEqual(t, finished.Resolution, core.FinishedReviewResolutionCancelled)
	dtEqual(t, len(finished.Roles), 1)
	dtEqual(t, finished.Roles[0].Verdict, core.ReviewVerdict(core.ReviewVerdictClean{}),
		"the reviewer's own outcome is still recorded")
}

// Hard-won: ee65b804b: a follow-up for a never-reviewed session had no row to mark and was never durable.

// A projection page must carry the whole turn record, not just the transcript
// rows: the wait endpoint reads the outcome back from these columns long after
// the session's actor is gone.
func TestAProjectionPagePersistsTheTurnOutcomeAndTheQueueAcceptanceOrdinal(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))

	running := core.MaterializedExecutionState(core.MaterializedExecutionStateRunning{StartedAtMs: 90})
	activeTurn := &core.MaterializedTurn{
		CommandID:         "prompt-1",
		AcceptedOrdinal:   dtPtr(uint64(1)),
		TurnStartPosition: 2,
		StartedAtMs:       90,
	}
	started := core.MaterializedSessionMutation{
		Execution:  &running,
		ActiveTurn: &activeTurn,
		QueuedPrompts: &[]core.MaterializedQueuedPrompt{{
			CommandID:       "prompt-2",
			Kind:            core.QueuedCommandKindPrompt{},
			Content:         []any{map[string]any{"type": "text", "text": "next"}},
			QueuedAtMs:      105,
			AcceptedOrdinal: dtPtr(uint64(3)),
		}},
	}
	dtMust(applyProjectionEventTo(database, "session-1", 1, core.RelayEventGenesisDigest, EventDigest(1), &started))

	reloaded := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	dtTrue(t, reloaded.ActiveTurn != nil)
	dtEqual(t, reloaded.ActiveTurn.AcceptedOrdinal, dtPtr(uint64(1)))
	dtEqual(t, reloaded.QueuedPrompts[0].AcceptedOrdinal, dtPtr(uint64(3)))
	turnState := dtMust(loadMaterializedTurnOutcomeFrom(database, "session-1"))
	dtTrue(t, turnState != nil && turnState.ActiveTurn != nil)
	dtEqual(t, turnState.ActiveTurn.TurnStartPosition, uint64(2))

	idle := core.MaterializedExecutionState(core.MaterializedExecutionStateIdle{})
	var noTurn *core.MaterializedTurn
	completed := core.MaterializedSessionMutation{
		Execution:  &idle,
		ActiveTurn: &noTurn,
		LastTurnOutcome: &core.MaterializedTurnOutcome{
			CommandID:         "prompt-1",
			AcceptedOrdinal:   dtPtr(uint64(1)),
			TurnStartPosition: dtPtr(uint64(2)),
			CompletedOrdinal:  2,
			CompletedAtMs:     400,
			Outcome:           core.TurnOutcomeKindCompleted{StopReason: "EndTurn"},
		},
	}
	dtMust(applyProjectionEventTo(database, "session-1", 2, EventDigest(1), EventDigest(2), &completed))

	turnState = dtMust(loadMaterializedTurnOutcomeFrom(database, "session-1"))
	dtTrue(t, turnState != nil)
	dtEqual(t, turnState.Execution, core.MaterializedExecutionState(core.MaterializedExecutionStateIdle{}))
	dtTrue(t, turnState.ActiveTurn == nil, "a completed turn is no longer running")
	outcome := turnState.LastTurnOutcome
	if outcome == nil {
		t.Fatal("the finished turn's outcome")
	}
	dtEqual(t, outcome.AcceptedOrdinal, dtPtr(uint64(1)))
	dtEqual(t, outcome.Outcome, core.TurnOutcomeKind(core.TurnOutcomeKindCompleted{StopReason: "EndTurn"}))
}
