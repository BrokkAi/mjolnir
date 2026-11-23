// Port of mj-store/src/database/tests.rs, continued (target runtime,
// sub-agent reports, profile preferences, migrations and the rest).

package store

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/review"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

func TestTargetAccessSurvivesLifecycleUpdatesAndChangesWithTheTarget(t *testing.T) {
	database := dtDatabase(t, "target-runtime.sqlite3")
	record := session("runtime-session", "project")
	template := dtMust(core.UnmarshalTargetTemplateJSON([]byte(
		`{"kind":"ssh-podman","host":"original.test","user":"builder","identity_file":"/keys/id","extra_args":["-p","2222"],"image":"test"}`,
	)))
	runtime := core.TargetRuntimeSettingsFromTemplate(template)
	runtime.ToolCache = &core.ToolCachePlacement{
		Host:      "ssh:builder@original.test",
		Directory: "/fast/shared native caches",
		NxHome:    "/home/hel/.nx",
	}
	record.TargetRuntime = &runtime
	record.Target = core.TargetLocatorSshPodman{
		Host:             "original.test",
		ContainerID:      "original-container",
		WorkspaceStorage: core.PodmanWorkspaceLocatorContainerLayer{},
	}
	record.Project = &core.ProjectBundleSnapshot{
		Bundle: core.ProjectBundle{
			PrimaryRepo: "source",
			Repositories: []core.ProjectRepository{{
				ID:          "source",
				Github:      dtPtr("acme/source"),
				Destination: "source",
			}},
		},
		Identities: map[string]core.RepositoryIdentity{
			"source": core.RepositoryIdentityGithub{Owner: "acme", Repository: "source"},
		},
		NetworkSources: map[string]core.NetworkGitSource{},
	}
	projectJSON := string(dtJSON(t, record.Project))
	runtimeJSON := string(dtJSON(t, record.TargetRuntime))
	dtOK(t, SaveSessionTo(database, &record))
	var storedProject, storedRuntime string
	dtOK(t, dtOpenReader(t, database).QueryRow(
		"SELECT project_json, target_runtime_json FROM sessions WHERE session_id=?1", record.ID,
	).Scan(&storedProject, &storedRuntime))
	dtEqual(t, storedProject, projectJSON)
	dtEqual(t, storedRuntime, runtimeJSON)
	loaded := dtMust(LoadStateFrom(database))
	loadedRecord := loaded.Sessions.Index(record.ID)
	dtEqual(t, string(dtJSON(t, loadedRecord.Project)), projectJSON)
	dtEqual(t, string(dtJSON(t, loadedRecord.TargetRuntime)), runtimeJSON)
	interns := dtMust(sessionInternsFromState(&loaded))
	refreshed := dtMust(loadSessionInternedWith(dtOpenReader(t, database), record.ID, interns))
	dtTrue(t, refreshed != nil)
	dtTrue(t, loadedRecord.Project == refreshed.Project, "the project snapshot is shared, not copied")
	dtTrue(t, loadedRecord.TargetRuntime == refreshed.TargetRuntime, "the target runtime is shared, not copied")

	record.State = core.SessionStateError
	dtOK(t, saveLifecycleSessionTo(database, &record))
	// The database's failure trigger advances the generation on the state change.
	record.FailureGeneration++
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), record.ID), record)
	bare := core.TargetRuntimeSettingsFromTemplate(core.TargetTemplateLocalBare{})
	record.TargetRuntime = &bare
	record.Target = core.TargetLocatorLocalBare{WorkerRoot: filepath.Join("/new", record.ID)}
	dtOK(t, saveLifecycleSessionTo(database, &record))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), record.ID), record)
}

// A child's report is recorded once per turn and replaced by the next turn's,
// and it goes away with the child's relation.
func TestASubagentReportIsKeptOncePerTurnAndLeavesWithItsChild(t *testing.T) {
	path := dtDatabase(t, "state.sqlite3")
	_, child := subagentPair(t, path)
	handback := func(commandID, message string) *core.SubagentHandback {
		return &core.SubagentHandback{CommandID: commandID, Message: message, RecordedAtMs: 1}
	}
	dtEqual(t, dtMust(loadSubagentReportFrom(path, child.ID)), core.SubagentReport{})
	dtTrue(t, dtMust(recordSubagentHandbackTo(path, child.ID, handback("turn-1", "first"))))
	dtTrue(t, !dtMust(recordSubagentHandbackTo(path, child.ID, handback("turn-1", "again"))),
		"a second report for the same turn is refused")
	// Hard-won: 540e61a0d: a restart continuation could overwrite its turn's report.
	recovered := core.RestartContinuationID("turn-1")
	dtTrue(t, !dtMust(recordSubagentHandbackTo(path, child.ID, handback(recovered, "again"))),
		"a restart continuation is the same turn")
	dtTrue(t, dtMust(recordSubagentHandbackTo(path, child.ID, handback("turn-2", "next"))))
	dtEqual(t, dtMust(loadSubagentReportFrom(path, child.ID)).Handback, handback("turn-2", "next"))

	state := dtMust(LoadStateFrom(path))
	state.Subagents.Remove(child.ID)
	state.Sessions.Remove(child.ID)
	dtOK(t, deleteSessionFrom(path, child.ID))
	dtOK(t, SaveStateTo(path, &state))
	dtEqual(t, dtMust(loadSubagentReportFrom(path, child.ID)), core.SubagentReport{}, "the report leaves with its child")
}

// A parent's suspend lists the children it stopped on the parent, in the
// order it stopped them. A suspend that runs again after a restart updates a
// child already listed instead of listing it twice, a full state save from
// any controller copy leaves the list alone, and the list goes with the
// parent's record.
func TestAParentsStoppedSubAgentsAreListedOnceAndLeaveWithTheParent(t *testing.T) {
	path := dtDatabase(t, "state.sqlite3")
	parent, child := subagentPair(t, path)
	stopped := func(id string, handedBack bool) core.StoppedSubagent {
		return core.StoppedSubagent{
			ChildSessionID: id,
			Title:          fmt.Sprintf("title %s", id),
			Task:           dtPtr("say ready"),
			HandedBack:     handedBack,
		}
	}
	dtEqual(t, len(dtMust(loadStoppedSubagentsFrom(path, parent.ID))), 0)
	dtOK(t, recordStoppedSubagentsTo(path, parent.ID, []core.StoppedSubagent{stopped(child.ID, false), stopped("other-child", true)}))
	dtOK(t, recordStoppedSubagentsTo(path, parent.ID, []core.StoppedSubagent{stopped(child.ID, true)}))
	dtEqual(t, dtMust(loadStoppedSubagentsFrom(path, parent.ID)),
		[]core.StoppedSubagent{stopped(child.ID, true), stopped("other-child", true)})

	// The child's own record is gone; a full save of that state keeps the list.
	state := dtMust(LoadStateFrom(path))
	state.Subagents.Remove(child.ID)
	state.Sessions.Remove(child.ID)
	dtOK(t, deleteSessionFrom(path, child.ID))
	dtOK(t, SaveStateTo(path, &state))
	dtEqual(t, len(dtMust(loadStoppedSubagentsFrom(path, parent.ID))), 2)

	dtOK(t, deleteSessionFrom(path, parent.ID))
	dtEqual(t, len(dtMust(loadStoppedSubagentsFrom(path, parent.ID))), 0)
}

// Once the parent's relay has taken the note, only the children it named
// are forgotten: one listed after the note was built waits for the next.
func TestDeliveringTheNoteClearsOnlyTheSubAgentsItNamed(t *testing.T) {
	path := dtDatabase(t, "state.sqlite3")
	parent, _ := subagentPair(t, path)
	stopped := func(id string) core.StoppedSubagent {
		return core.StoppedSubagent{ChildSessionID: id, Title: id}
	}
	dtOK(t, recordStoppedSubagentsTo(path, parent.ID, []core.StoppedSubagent{stopped("a"), stopped("b")}))
	dtOK(t, clearStoppedSubagentsFrom(path, parent.ID, []string{"a"}))
	dtEqual(t, dtMust(loadStoppedSubagentsFrom(path, parent.ID)), []core.StoppedSubagent{stopped("b")})
	dtOK(t, clearStoppedSubagentsFrom(path, parent.ID, []string{"b", "c"}))
	dtEqual(t, len(dtMust(loadStoppedSubagentsFrom(path, parent.ID))), 0)
}

// The ordinal of the parent's newest prompt is recorded only for a sub-agent
// child, and a late write for an older prompt never moves it back.
// Hard-won: a91cfd04: a parent reported a just-started child complete before its turn was stored
func TestASubagentPromptOrdinalIsKeptForChildrenAndOnlyMovesForward(t *testing.T) {
	path := dtDatabase(t, "state.sqlite3")
	parent, child := subagentPair(t, path)
	dtOK(t, recordSubagentPromptTo(path, child.ID, 20))
	dtOK(t, recordSubagentPromptTo(path, child.ID, 45))
	dtOK(t, recordSubagentPromptTo(path, child.ID, 30))
	dtEqual(t, dtMust(loadSubagentReportFrom(path, child.ID)).AwaitedOrdinal, dtPtr(uint64(45)))
	dtOK(t, recordSubagentPromptTo(path, parent.ID, 7))
	dtEqual(t, dtMust(loadSubagentReportFrom(path, parent.ID)), core.SubagentReport{},
		"a session that is not a child records nothing")
}

// Migration 53 rebuilds `sessions` so its state constraint admits a parked
// sub-agent. The rebuild keeps every row, column and trigger of the table,
// and raises the compatibility floor: an older build reads a session state it
// does not know as a broken store, so it must refuse this one.
func TestTheParkedStateMigrationKeepsEverySessionAndRefusesOlderBuilds(t *testing.T) {
	// Other tests install subscribers that make tracing cache "no interest"
	// in the migration event, so the log is checked alone, in a child with a
	// global subscriber (the same shape as the worker-source pinning test).
	const child = "MJ_PARKED_MIGRATION_LOG_TEST_CHILD"
	if _, ok := os.LookupEnv(child); !ok {
		root := t.TempDir()
		targets.NewIsolatedTest(targets.TestName(
			"mj_store::database::tests",
			"TestTheParkedStateMigrationKeepsEverySessionAndRefusesOlderBuilds",
		)).
			Env(child, "1").
			IsolatedStore(root).
			Run()
		return
	}
	database := dtDatabase(t, "parked-migration.sqlite3")
	old := session("old-session", "project-1")
	old.LaunchBranch = dtPtr("main")
	dtOK(t, SaveSessionTo(database, &old))

	// Put the store back at revision 52, whose constraint has no `parked`.
	raw := dtMust(OpenConnection(database))
	restoreGithubV84Schema(t, raw)
	restorePre101SubagentPreference(t, raw)
	restoreReviewStateSchema(t, raw, 52)
	restorePre153TranscriptLayout(t, raw)
	dtExec(t, raw, `PRAGMA writable_schema = ON;
         UPDATE sqlite_schema
            SET sql = replace(replace(sql, '''startup-cleanup'',', ''), '''stopped'',''parked'',''lost'',', '''stopped'',''lost'',')
          WHERE type = 'table' AND name = 'sessions';
         PRAGMA writable_schema = OFF;
         DELETE FROM schema_migrations WHERE version >= 53;
         UPDATE schema_compatibility SET minimum_compatible_version = 49;
         DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away; DROP TABLE IF EXISTS subagent_accounting; DROP TABLE IF EXISTS session_turn_selections; ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic; ALTER TABLE mailbox_outbox DROP COLUMN failure; PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '') WHERE type='table' AND name='sessions'; PRAGMA writable_schema=RESET; PRAGMA user_version = 52;`)
	dtOK(t, raw.Close())
	raw = dtMust(OpenConnection(database))
	_, err := raw.Exec("UPDATE sessions SET state = 'parked'")
	dtTrue(t, err != nil, "the rolled-back store refuses the new state")
	dtOK(t, raw.Close())
	forgetVerifiedSchema(database)

	log := core.NewCapturedLog()
	slog.SetDefault(slog.New(log))
	connection := dtOpen(t, database)
	var logged []string
	for _, event := range log.AtOrAbove(slog.LevelInfo) {
		if strings.Contains(event, "database migrations applied") {
			logged = append(logged, event)
		}
	}
	dtEqual(t, len(logged), 1, logged)
	dtTrue(t, strings.Contains(logged[0], "database migrations applied") &&
		strings.Contains(logged[0], "from_revision=52") &&
		strings.Contains(logged[0], fmt.Sprintf("to_revision=%d", schemaVersion)), logged)
	state := dtMust(readSchemaState(connection))
	dtEqual(t, state.revision, schemaVersion)
	floor := dtQuery[int64](t, connection, "SELECT minimum_compatible_version FROM schema_compatibility")
	dtTrue(t, floor >= 53, "builds predating parked sessions are refused")
	triggers := dtQuery[int64](t, connection,
		"SELECT count(*) FROM sqlite_schema WHERE type = 'trigger' AND name IN ('api_session_error_updated','api_session_error_inserted')")
	dtEqual(t, triggers, int64(0), "generic error triggers are retired by migration 57")
	dtTrue(t, !dtHasRow(t, connection, "PRAGMA foreign_key_check"))
	dtOK(t, connection.Close())

	loaded := dtMust(LoadStateFrom(database))
	dtEqual(t, dtSessionOf(loaded, "old-session").LaunchBranch, dtPtr("main"))
	parked := dtSessionOf(loaded, "old-session")
	parked.State = core.SessionStateParked
	dtOK(t, saveLifecycleSessionTo(database, &parked))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), "old-session").State, core.SessionStateParked)
}

func TestOracleChoiceAndProfilePreferenceSurviveReopenAndRename(t *testing.T) {
	path := dtDatabase(t, "oracle-choice.sqlite3")
	record := session("advisor-parent", "project")
	record.Oracle = false
	record.OracleChoice = &core.SubagentChoice{Model: "advisor", Effort: dtPtr("high")}
	dtOK(t, saveNewSessionTo(path, &record, nil, true, false))
	state := dtMust(LoadStateFrom(path))
	choice, ok := state.LastOracleChoices[record.LastProfile]
	dtTrue(t, ok && !choice)
	dtTrue(t, !dtSessionOf(state, record.ID).Oracle)
	dtEqual(t, dtSessionOf(state, record.ID).OracleChoice, record.OracleChoice)
	dtMust(renameProfileReferencesAt(path, record.LastProfile, "renamed"))
	renamed := dtMust(LoadStateFrom(path))
	choice, ok = renamed.LastOracleChoices["renamed"]
	dtTrue(t, ok && !choice)
	_, present := renamed.LastOracleChoices[record.LastProfile]
	dtTrue(t, !present)
}

func dtMjolnirPolicy(orchestrators bool) core.SubagentPolicy {
	return core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir, Orchestrators: orchestrators}
}

func dtPolicy(mode core.SubagentPolicyMode) core.SubagentPolicy {
	return core.SubagentPolicy{Mode: mode}
}

// dtRememberedPolicy is state.last_subagent_policies.get(profile).
func dtRememberedPolicy(state core.StateState, profile string) *core.SubagentPolicy {
	policy, ok := state.LastSubagentPolicies[profile]
	if !ok {
		return nil
	}
	return &policy
}

func TestSchemaOrchestratorPolicySurvivesReopenAndProfileRename(t *testing.T) {
	path := dtDatabase(t, "orchestrators.sqlite3")
	record := session("parent", "project")
	record.Subagents = dtPtr(dtMjolnirPolicy(true))
	dtOK(t, saveNewSessionTo(path, &record, nil, true, false))
	state := dtMust(LoadStateFrom(path))
	dtTrue(t, dtSessionOf(state, record.ID).Subagents.OrchestratorsEnabled())
	dtTrue(t, dtRememberedPolicy(state, record.LastProfile).OrchestratorsEnabled())

	dtMust(renameProfileReferencesAt(path, record.LastProfile, "renamed"))
	renamed := dtMust(LoadStateFrom(path))
	dtTrue(t, dtRememberedPolicy(renamed, "renamed").OrchestratorsEnabled())
	dtTrue(t, dtRememberedPolicy(renamed, record.LastProfile) == nil)

	record = dtSessionOf(renamed, record.ID)
	record.Subagents = dtPtr(dtMjolnirPolicy(false))
	dtOK(t, saveNewSessionTo(path, &record, nil, true, false))
	disabled := dtMust(LoadStateFrom(path))
	dtTrue(t, !dtSessionOf(disabled, record.ID).Subagents.OrchestratorsEnabled())
	dtTrue(t, !dtRememberedPolicy(disabled, "renamed").OrchestratorsEnabled())
}

func TestAcceptedSessionPolicySurvivesReopenAndOnlyCreationChangesTheDefault(t *testing.T) {
	path := dtDatabase(t, "mj.sqlite3")
	native := dtPolicy(core.SubagentPolicyNative)
	none := dtPolicy(core.SubagentPolicyNone)
	connection := dtOpen(t, path)
	dtExec(t, connection, "INSERT INTO subagent_preference(profile_id, policy) VALUES('', ?1)", string(dtJSON(t, native)))
	dtOK(t, connection.Close())
	parent := session("fixed-parent", "project")
	fixed := core.SubagentPolicy{
		Mode:            core.SubagentPolicyMjolnir,
		Orchestrators:   false,
		Straightforward: &core.SubagentChoice{Model: "gpt-6-luna", Effort: dtPtr("max")},
	}
	parent.Subagents = dtPtr(fixed)
	dtOK(t, saveNewSessionTo(path, &parent, nil, true, true))
	state := dtMust(LoadStateFrom(path))
	dtEqual(t, dtRememberedPolicy(state, "codex"), &fixed)
	dtEqual(t, dtRememberedPolicy(state, ""), &native)
	dtEqual(t, state.LastSubagentPolicyFor("unseen-profile"), native)
	dtEqual(t, dtSessionOf(state, parent.ID).Subagents, &fixed)
	otherProfile := session("other-profile", "project")
	otherProfile.LastProfile = "codex-alt"
	otherProfile.Subagents = dtPtr(native)
	dtOK(t, saveNewSessionTo(path, &otherProfile, nil, true, true))
	state = dtMust(LoadStateFrom(path))
	dtEqual(t, dtRememberedPolicy(state, "codex"), &fixed)
	dtEqual(t, dtRememberedPolicy(state, "codex-alt"), &native)
	child := session("child", "project")
	child.Subagents = dtPtr(none)
	dtOK(t, SaveSessionTo(path, &child))
	parent.Title = "resumed parent"
	dtOK(t, SaveSessionTo(path, &parent))
	dtEqual(t, dtRememberedPolicy(dtMust(LoadStateFrom(path)), "codex"), &fixed)
	// An explicit unattended create keeps the accepted session policy but
	// cannot replace the interactive choice or host-size preselection.
	builder := &HostContainerSizeChoice{Host: "builder", Size: core.HostContainerSize{Cpus: 16, MemoryBytes: 64}}
	unattended := session("unattended", "project")
	unattended.Subagents = dtPtr(native)
	dtOK(t, saveNewSessionTo(path, &unattended, builder, false, false))
	state = dtMust(LoadStateFrom(path))
	dtEqual(t, dtRememberedPolicy(state, "codex"), &fixed)
	dtEqual(t, dtSessionOf(state, "unattended").Subagents, &native)
	dtTrue(t, !state.ContainerSizes.ContainsKey("builder"))
	// Interactive defaults and the container-size default are independent.
	// Checking the broader launch-choice flag does not opt into saving size.
	interactiveWithoutSize := session("interactive-without-size", "project")
	interactiveWithoutSize.HarnessKind = core.HarnessKindDsh
	interactiveWithoutSize.LastProfile = "dsh"
	interactiveWithoutSize.Subagents = dtPtr(native)
	dtOK(t, saveNewSessionTo(path, &interactiveWithoutSize, builder, true, false))
	dtEqual(t, dtRememberedPolicy(dtMust(LoadStateFrom(path)), "dsh"), &native)
	dtTrue(t, !dtMust(LoadStateFrom(path)).ContainerSizes.ContainsKey("builder"))
	interactiveWithSize := session("interactive-with-size", "project")
	interactiveWithSize.HarnessKind = core.HarnessKindDsh
	interactiveWithSize.LastProfile = "dsh"
	interactiveWithSize.Subagents = dtPtr(native)
	dtOK(t, saveNewSessionTo(path, &interactiveWithSize, builder, true, true))
	dtEqual(t, *dtMust(LoadStateFrom(path)).ContainerSizes.Index("builder"), core.HostContainerSize{Cpus: 16, MemoryBytes: 64})
	// Each profile remembers its own accepted policy, including DSH choices.
	dsh := session("dsh-none", "project")
	dsh.HarnessKind = core.HarnessKindDsh
	dsh.LastProfile = "dsh"
	dsh.Subagents = dtPtr(none)
	dtOK(t, saveNewSessionTo(path, &dsh, nil, true, true))
	dtEqual(t, dtRememberedPolicy(dtMust(LoadStateFrom(path)), "dsh"), &none)
	dtEqual(t, dtRememberedPolicy(dtMust(LoadStateFrom(path)), "codex"), &fixed)
	plain := dtMjolnirPolicy(false)
	dsh = session("dsh-mjolnir", "project")
	dsh.HarnessKind = core.HarnessKindDsh
	dsh.LastProfile = "dsh"
	dsh.Subagents = dtPtr(plain)
	dtOK(t, saveNewSessionTo(path, &dsh, nil, true, true))
	dtEqual(t, dtRememberedPolicy(dtMust(LoadStateFrom(path)), "dsh"), &plain)
	next := session("next", "project")
	next.Subagents = dtPtr(none)
	dtOK(t, saveNewSessionTo(path, &next, nil, true, true))
	dtEqual(t, dtRememberedPolicy(dtMust(LoadStateFrom(path)), "codex"), &none)
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(path)), parent.ID).Subagents, &fixed)
	connection = dtOpen(t, path)
	for _, entry := range []struct {
		stored   string
		expected core.SubagentPolicy
	}{
		{`{"mode":"all_models"}`, dtMjolnirPolicy(false)},
		{`{"mode":"single_model","model":"old","effort":"high"}`, dtMjolnirPolicy(false)},
		{`{"mode":"single_model","model":"old","effort":"adaptive"}`, dtMjolnirPolicy(false)},
		{"true", dtMjolnirPolicy(false)},
		{"false", native},
	} {
		dtExec(t, connection, "UPDATE sessions SET subagents=?1 WHERE session_id=?2", entry.stored, parent.ID)
		dtExec(t, connection, "UPDATE subagent_preference SET policy=?1 WHERE profile_id='codex'", entry.stored)
		state := dtMust(LoadStateFrom(path))
		dtEqual(t, dtSessionOf(state, parent.ID).Subagents, &entry.expected)
		dtEqual(t, dtRememberedPolicy(state, "codex"), &entry.expected)
	}
}

func TestProfileRenameMovesRememberedChoiceAtomicallyAndIdempotently(t *testing.T) {
	path := dtDatabase(t, "rename-profile.sqlite3")
	old := session("old-profile-session", "project")
	old.LastProfile = "old-profile"
	old.Subagents = dtPtr(dtMjolnirPolicy(false))
	dtOK(t, saveNewSessionTo(path, &old, nil, true, true))
	destination := session("new-profile-session", "project")
	destination.LastProfile = "new-profile"
	destination.Subagents = dtPtr(dtPolicy(core.SubagentPolicyNone))
	dtOK(t, saveNewSessionTo(path, &destination, nil, true, true))

	dtEqual(t, dtMust(renameProfileReferencesAt(path, "old-profile", "new-profile")), 1)
	state := dtMust(LoadStateFrom(path))
	dtEqual(t, dtSessionOf(state, old.ID).LastProfile, "new-profile")
	dtEqual(t, dtRememberedPolicy(state, "new-profile"), old.Subagents)
	dtTrue(t, dtRememberedPolicy(state, "old-profile") == nil)
	dtEqual(t, dtMust(renameProfileReferencesAt(path, "old-profile", "new-profile")), 0)
	dtEqual(t, dtRememberedPolicy(dtMust(LoadStateFrom(path)), "new-profile"), old.Subagents)
	dtEqual(t, dtMust(renameProfileReferencesAt(path, "new-profile", "new-profile")), 0)
	dtEqual(t, dtRememberedPolicy(dtMust(LoadStateFrom(path)), "new-profile"), old.Subagents)
}

func TestCommittedProfilePreferencesTrackInsertOverwriteRenameRollbackAndDelete(t *testing.T) {
	const child = "MJ_COMMITTED_PROFILE_PREFERENCE_CHILD"
	if _, ok := os.LookupEnv(child); !ok {
		directory := t.TempDir()
		targets.NewIsolatedTest(targets.TestName(
			"mj_store::database::tests",
			"TestCommittedProfilePreferencesTrackInsertOverwriteRenameRollbackAndDelete",
		)).
			Env(child, "1").
			IsolatedStore(directory).
			Run()
		return
	}

	native := dtPolicy(core.SubagentPolicyNative)
	none := dtPolicy(core.SubagentPolicyNone)
	writer := InstallIsolatedTestWriter()
	defer writer.Shutdown()
	committed := func() *CommittedState {
		state := dtMust(WriterCommittedState())
		if state == nil {
			panic("called unwrap on a None value")
		}
		return state
	}
	dtMust(submitDatabaseWrite("seed_legacy_profile_preference", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("INSERT INTO subagent_preference(profile_id, policy) VALUES('', ?1)", string(dtJSON(t, native)))
		return struct{}{}, err
	}))
	dtEqual(t, dtRememberedPolicy(committed().State, ""), &native)

	first := session("profile-choice-one", "project")
	first.Subagents = dtPtr(none)
	dtOK(t, SaveNewSession(&first, nil, true, false))
	dtEqual(t, dtRememberedPolicy(committed().State, "codex"), &none)
	second := session("profile-choice-two", "project")
	second.Subagents = dtPtr(native)
	dtOK(t, SaveNewSession(&second, nil, true, false))
	dtEqual(t, dtRememberedPolicy(committed().State, "codex"), &native)
	dtEqual(t, dtRememberedPolicy(committed().State, ""), &native)

	dtMust(RenameProfileReferences("codex", "codex-renamed"))
	dtEqual(t, dtRememberedPolicy(committed().State, "codex-renamed"), &native)
	dtTrue(t, dtRememberedPolicy(committed().State, "codex") == nil)
	dtEqual(t, dtRememberedPolicy(committed().State, ""), &native)

	dtMust(submitDatabaseWrite("rollback_profile_preference", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE subagent_preference SET policy='true' WHERE profile_id='codex-renamed';
             ROLLBACK;`)
		return struct{}{}, err
	}))
	dtEqual(t, dtRememberedPolicy(committed().State, "codex-renamed"), &native)

	dtMust(submitDatabaseWrite("delete_profile_preference", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("DELETE FROM subagent_preference WHERE profile_id='codex-renamed'")
		return struct{}{}, err
	}))
	dtTrue(t, dtRememberedPolicy(committed().State, "codex-renamed") == nil)
	state := committed().State
	dtEqual(t, state.LastSubagentPolicyFor("codex-renamed"), native)
}

// A converted session's bundle may already be an alias of another project in
// the catalog. The context then holds the canonical project, and rebinding
// reports it so the resumed record can follow (resume publication rejects a
// record whose bundle differs from its context).
// Hard-won: cb051da9: canonical bundle aliases made imported session resume publication fail
func TestRebindingToAnAliasedBundleReportsTheCanonicalProject(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	dtOK(t, recordPromptTo(database, "session-1", "project-1", 1, dtPtr("2026-08-12T00:00:00Z"), "fix parser"))
	dtEqual(t, dtMust(rebindSessionBundleTo(database, "session-1", "project-2")), "project-2",
		"a bundle the catalog does not alias is its own project")
	connection := dtOpen(t, database)
	dtExec(t, connection, `INSERT INTO project_catalog(bundle_id, project_key, snapshot_json, hidden)
             VALUES ('project-2', 'key-project-2', '{}', 0);
             INSERT INTO project_aliases(bundle_id, canonical_id, snapshot_json, config_pending)
             VALUES ('import-project-3', 'project-2', '{}', 0);`)
	dtOK(t, connection.Close())
	dtEqual(t, dtMust(rebindSessionBundleTo(database, "session-1", "import-project-3")), "project-2")
	context := dtQuery[string](t, dtOpen(t, database), "SELECT bundle_id FROM session_contexts WHERE session_id = 'session-1'")
	dtEqual(t, context, "project-2")
}

func TestObsoleteTurnReviewRowsAreRetiredBeforeRecovery(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	for _, id := range []string{"turn", "forward", "push"} {
		record := session(id, "project-1")
		dtOK(t, SaveSessionTo(database, &record))
	}
	connection := dtMust(open(database))
	restorePre145ReviewColumns(t, connection)
	for _, row := range []struct {
		id      string
		group   string
		forward *string
	}{
		{"turn", `{"scope":"turn_delta","driver":{"obsolete":"shape"}}`, nil},
		{"forward", `{"scope":"pushable_commits"}`, dtPtr("obsolete handoff")},
		{"push", `{"scope":"pushable_commits"}`, nil},
	} {
		dtExec(t, connection,
			"INSERT INTO turn_review_state(session_id, baselines, reviewed_through_ordinal, active_group, pending_forward, pending_followup) VALUES (?1, '{}', 0, ?2, ?3, 1)",
			row.id, row.group, row.forward)
	}
	dropEnvironmentSchema(t, connection)
	restorePre153TranscriptLayout(t, connection)
	dtExec(t, connection, `ALTER TABLE sessions DROP COLUMN no_github_credentials;
             DELETE FROM schema_migrations WHERE version >= 115;
             UPDATE schema_compatibility SET minimum_compatible_version = 114;
             PRAGMA user_version = 114;`)
	dtOK(t, connection.Close())
	forgetVerifiedSchema(database)
	connection = dtOpen(t, database)
	type reviewRow struct {
		sessionID string
		group     bool
		forward   bool
		followup  bool
	}
	rows, err := connection.Query(`SELECT session_id,
                EXISTS(SELECT 1 FROM json_each(open_reviews) WHERE json_extract(value, '$.group') IS NOT NULL),
                pending_forward IS NOT NULL, pending_followup
         FROM turn_review_state ORDER BY session_id`)
	dtOK(t, err)
	var groups []reviewRow
	for rows.Next() {
		var row reviewRow
		dtOK(t, rows.Scan(&row.sessionID, &row.group, &row.forward, &row.followup))
		groups = append(groups, row)
	}
	dtOK(t, rows.Err())
	dtOK(t, rows.Close())
	dtEqual(t, groups, []reviewRow{
		{"forward", false, false, false},
		{"push", true, false, false},
		{"turn", false, false, false},
	})
	dtEqual(t, len(dtMust(turnReviewStateIn(database, "turn")).OpenReviews), 0)
}

// Hard-won: e13ed5c7c: page joins duplicated parked output across page boundaries
func TestHistoryPagesJoinParkedTerminalOutputWithoutChangingPageBoundaries(t *testing.T) {
	for _, outputFirst := range []bool{false, true} {
		path := dtDatabase(t, "history.sqlite3")
		full := MaterializedSession("joined-history")
		owner := *full.Transcript[2]
		record := core.TerminalOutputRecord{
			TerminalID: "parked",
			Output:     core.NewPackedText("authoritative terminal output"),
			ExitCode:   dtPtr(uint32(0)),
		}
		if tool, ok := owner.Body.(core.TranscriptBodyTool); ok {
			tool.TerminalRefs = append(slices.Clone(tool.TerminalRefs), record.TerminalID)
			older := record
			older.Output = core.NewPackedText("older streamed output")
			embedded := record
			embedded.TerminalID = "embedded-only"
			embedded.Output = core.NewPackedText("embedded terminal output")
			tool.TerminalOutputs = append(slices.Clone(tool.TerminalOutputs), older, embedded)
			owner.Body = tool
		}
		position := uint64(4)
		if outputFirst {
			position = 1
		}
		parked := &core.TranscriptItem{
			StableID:        "terminal:parked",
			Position:        position,
			CreatedAtMs:     1,
			LastChangedAtMs: 1,
			Body:            core.TranscriptBodyTerminalOutputOf(record),
		}
		if outputFirst {
			full.Transcript = []*core.TranscriptItem{parked, &owner}
		} else {
			full.Transcript = []*core.TranscriptItem{&owner, parked}
		}
		sessionRecord := session(full.SessionID, "project-1")
		dtOK(t, SaveSessionTo(path, &sessionRecord))
		dtOK(t, SaveMaterializedSessionTo(path, &full))
		filtered := dtFilteredPage(t, path, full.SessionID, 0, 1, []core.TranscriptRole{core.TranscriptRoleTool}, false)
		dtEqual(t, len(filtered.Items), 1)
		dtEqual(t, filtered.Items[0].StableID, owner.StableID)
		text := transcript.TranscriptItemText(filtered.Items[0])
		dtTrue(t, strings.Contains(text, "authoritative terminal output"), text)
		dtTrue(t, strings.Contains(text, "embedded terminal output"), text)
		dtTrue(t, !strings.Contains(text, "older streamed output"), text)
		var before *core.TranscriptCursor
		if !outputFirst {
			before = dtPtr(core.TranscriptCursorOf(full.Transcript[1]))
		}
		page := dtMust(loadTranscriptHistoryFrom(path, full.SessionID, before, 1))
		dtTrue(t, page != nil)
		dtEqual(t, page.Items[0].StableID, owner.StableID)
		dtTrue(t, !strings.Contains(client.HistoryEntries(page)[0].Text, "authoritative terminal output"))
		// Convert single-item pages independently, as dispatch_earlier_history does.
		newest := dtMust(loadTranscriptHistoryFrom(path, full.SessionID, nil, 1))
		dtTrue(t, newest != nil)
		older := dtMust(loadTranscriptHistoryFrom(path, full.SessionID, newest.Before, 1))
		dtTrue(t, older != nil)
		var texts []string
		for _, historyPage := range []*core.TranscriptHistoryPage{newest, older} {
			for _, entry := range client.HistoryEntries(historyPage) {
				texts = append(texts, entry.Text)
			}
		}
		dtEqual(t, strings.Count(strings.Join(texts, "\n"), "authoritative terminal output"), 1)
		unfiltered := dtFilteredPage(t, path, full.SessionID, 0, 10, nil, false)
		texts = nil
		for _, item := range unfiltered.Items {
			texts = append(texts, transcript.TranscriptItemText(item))
		}
		dtEqual(t, strings.Count(strings.Join(texts, "\n"), "authoritative terminal output"), 1)
		stored := dtMust(LoadMaterializedSessionFrom(path, full.SessionID))
		dtEqual(t, stored.Transcript, full.Transcript, "presentation joins never write to storage")
	}
}

// Hard-won: e13ed5c7c: history hydration duplicated output for evicted owners
func TestTerminalOutputForEvictedOwnerParksAndJoinsHistoryPages(t *testing.T) {
	path := dtDatabase(t, "history.sqlite3")
	full := longConversation(t)
	owner := dtFixtureTool(full.SessionID)
	if tool, ok := owner.Body.(core.TranscriptBodyTool); ok {
		tool.TerminalRefs = append(slices.Clone(tool.TerminalRefs), "evicted-terminal")
		owner.Body = tool
	}
	full.Transcript[2] = owner
	record := session(full.SessionID, "project-1")
	dtOK(t, SaveSessionTo(path, &record))
	dtOK(t, SaveMaterializedSessionTo(path, &full))
	live, _ := dtActorProjection(t, path, full.SessionID)
	dtTrue(t, !dtHasStableID(live.Transcript, owner.StableID))
	output := "output from an evicted terminal owner"
	event := dtRelayEvent(t, 2501, full.AppliedEventDigest, core.RelayObservationTerminalOutput{
		TerminalID: "evicted-terminal",
		Output:     output,
		Truncated:  false,
		ExitCode:   dtPtr(uint32(0)),
	})
	references := dtMust(loadProjectionReferencesFrom(path, &live, []core.RelayEvent{event}))
	dtEqual(t, len(references), 0, "terminal output must not scan history for its owner")
	dtCommitEvent(t, path, full.SessionID, &live, &event)
	stored := dtMust(LoadMaterializedSessionFrom(path, full.SessionID))
	index := slices.IndexFunc(stored.Transcript, func(item *core.TranscriptItem) bool { return item.StableID == owner.StableID })
	dtTrue(t, index >= 0)
	dtEqual(t, stored.Transcript[index], owner)
	dtTrue(t, dtHasStableID(stored.Transcript, "terminal:evicted-terminal"))
	for _, roles := range [][]core.TranscriptRole{nil, {core.TranscriptRoleTool}} {
		page := dtFilteredPage(t, path, full.SessionID, 2, 1, roles, false)
		dtEqual(t, len(page.Items), 1)
		dtEqual(t, page.Items[0].StableID, owner.StableID)
		items := make([]core.TranscriptItem, len(page.Items))
		for index, item := range page.Items {
			items[index] = *item
		}
		outputs := core.TerminalOutputsOf(items)
		text := transcript.TranscriptItemTextWithOutputs(page.Items[0], outputs)
		expected := 0
		if len(roles) != 0 {
			expected = 1
		}
		dtEqual(t, strings.Count(text, output), expected)
	}
	before := core.TranscriptCursorOf(full.Transcript[3])
	page := dtMust(loadTranscriptHistoryFrom(path, full.SessionID, &before, 1))
	dtTrue(t, page != nil)
	dtEqual(t, strings.Count(client.HistoryEntries(page)[0].Text, output), 0)
	all := core.TranscriptHistoryPage{
		Items:    stored.Transcript,
		Frontier: stored.AppliedEventOrdinal,
	}
	total := 0
	for _, entry := range client.HistoryEntries(&all) {
		total += strings.Count(entry.Text, output)
	}
	dtEqual(t, total, 1)
	summary := transcript.TranscriptSummaryFromMaterializedKeeping(stored, int(^uint(0)>>1))
	dtEqual(t, strings.Count(summary.Render(int(^uint(0)>>1)), output), 1)
	total = 0
	for _, entry := range client.MaterializedChatEntries(stored) {
		total += strings.Count(entry.Text, output)
	}
	dtEqual(t, total, 1)
}

// A ReadThrough card names the agent message complete at its ordinal; a newer
// reply must never answer for it.
func TestAgentMessageThroughAnOrdinalIgnoresNewerReplies(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	materialized := MaterializedSession("session-1")
	materialized.Transcript = append(materialized.Transcript, &core.TranscriptItem{
		StableID:                  "agent:6",
		Position:                  6,
		LatestContentEventOrdinal: dtPtr(uint64(7)),
		CreatedAtMs:               1_700,
		LastChangedAtMs:           1_700,
		Body: core.TranscriptBodyAgent{
			Chunks:    []json.RawMessage{dtAgentText(t, "Newer reply")},
			Streaming: false,
		},
	})
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))
	connection := dtOpen(t, database)
	through := func(ordinal *uint64) *string {
		_, text, ok, err := lastMaterializedAgentMessageIn(connection, "session-1", 0, nil, ordinal)
		dtOK(t, err)
		if !ok {
			return nil
		}
		return &text
	}
	dtEqual(t, through(dtPtr(uint64(2))), dtPtr("Working on it"))
	dtEqual(t, through(dtPtr(uint64(6))), dtPtr("Working on it"))
	dtEqual(t, through(dtPtr(uint64(7))), dtPtr("Newer reply"))
	dtEqual(t, through(nil), dtPtr("Newer reply"))
	dtEqual(t, through(dtPtr(uint64(1))), (*string)(nil))
}

// Hard-won: c4688b4: an interrupted or slower adoption must not replace a committed cut.
func TestLiveBaselineInstallRollsBackAndRetriesWithoutOverwritingAWinner(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	snapshot := core.NewRelaySnapshot("session-1")
	state := snapshot.OperationalState()
	state.LatestOrdinal = 7
	state.LatestDigest = EventDigest(7)
	child := core.NativeAgent{
		OwnerSessionID: "session-1",
		SessionID:      "child",
		Name:           "child",
		Task:           "work",
		Capabilities:   core.NativeAgentCapabilities{},
		State:          core.NativeAgentStateRunning,
	}
	state.NativeAgents = append(state.NativeAgents, child)
	wrongOwner := child
	wrongOwner.OwnerSessionID = "wrong-owner"
	question := dtMust(core.ElicitationRequestFromACPParams("recovered-question", dtValue(t, map[string]any{
		"mode":            "form",
		"sessionId":       "session-1",
		"message":         "Approve?",
		"requestedSchema": map[string]any{"type": "object", "properties": map[string]any{}},
	})))
	baseline := core.LiveSessionBaseline{
		DeliveredInitialPrompts: []core.LiveQueuedCommand{},
		State:                   state,
		Queue:                   []core.LiveQueuedCommand{},
		Questions:               []core.ElicitationRequest{question},
		UnavailableQuestions:    0,
		NativeAgentReplay:       &[]core.NativeAgent{wrongOwner},
	}
	projection := dtMust(transcript.MaterializedSessionFromLiveBaseline(&baseline))
	dtEqual(t, projection.Execution, core.MaterializedExecutionState(core.MaterializedExecutionStateIdle{}))
	// Fail after published identities were written but before the cut commits.
	_, err := installLiveSessionBaselineTo(database, &projection, &baseline)
	dtTrue(t, err != nil)
	saved := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	dtTrue(t, saved == nil || saved.AppliedEventOrdinal == 0)
	connection := dtOpen(t, database)
	dtEqual(t, dtQuery[int64](t, connection, "SELECT count(*) FROM native_agents WHERE owner='session-1'"), int64(0))
	filter := core.ApiEventFilter{SessionID: dtPtr("session-1")}
	for _, event := range dtMust(loadAPIEventsFrom(database, &filter, dtPtr(uint64(0)), 100)).Events {
		_, required := event.Event.(core.ApiEventDataInputRequired)
		dtTrue(t, !required)
	}
	baseline.NativeAgentReplay = &[]core.NativeAgent{child}
	dtTrue(t, dtMust(installLiveSessionBaselineTo(database, &projection, &baseline)))
	dtTrue(t, !dtMust(installLiveSessionBaselineTo(database, &projection, &baseline)))
	requiredEvents := dtMust(loadAPIEventsFrom(database, &filter, dtPtr(uint64(0)), 100))
	matching := 0
	for _, event := range requiredEvents.Events {
		if required, ok := event.Event.(core.ApiEventDataInputRequired); ok && required.Request != nil && required.Request.ID == question.ID {
			matching++
		}
	}
	dtEqual(t, matching, 1)
	dtEqual(t, dtQuery[int64](t, connection,
		"SELECT count(*) FROM operator_away_requests WHERE session_id='session-1' AND elicitation_id='recovered-question'"), int64(1))
	dtEqual(t, dtMust(LoadMaterializedSessionFrom(database, "session-1")), &projection)
}

// Dropping an unaccepted enqueue must not leave a blocked sender behind.
func TestCancelledWriterEnqueueCannotCommitAfterQueueSpaceReturns(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	owner := dtMust(startDatabaseWriterAt(database, false))
	writer := owner.writer
	entered := make(chan struct{}, 1)
	release := make(chan struct{}, 1)
	writer.sender <- databaseWriterMessageRun{
		label: "block writer",
		job: func(*Connection, error) {
			entered <- struct{}{}
			<-release
		},
	}
	<-entered
	for range databaseWriteQueueCapacity {
		select {
		case writer.sender <- databaseWriterMessageRun{label: "fill writer", job: func(*Connection, error) {}}:
		default:
			t.Fatal("queue unexpectedly full")
		}
	}
	var ran atomic.Bool
	{
		ctx, cancel := context.WithCancel(context.Background())
		type enqueued struct {
			accepted *AcceptedDatabaseWrite[struct{}, *struct{}]
			err      error
		}
		done := make(chan enqueued, 1)
		guard := &struct{}{}
		go func() {
			accepted, err := DatabaseWriterEnqueue(ctx, writer, "cancelled adoption", guard, func(*Connection) (struct{}, error) {
				ran.Store(true)
				return struct{}{}, nil
			})
			done <- enqueued{accepted, err}
		}()
		// Rust polls the future once and sees it pending; here the enqueue must
		// still be waiting for queue space.
		select {
		case result := <-done:
			t.Fatalf("an enqueue into a full queue returned at once: %+v", result)
		case <-time.After(50 * time.Millisecond):
		}
		// Cancel without leaving a blocked sender which might later accept it.
		cancel()
		result := <-done
		dtTrue(t, result.accepted == nil && result.err != nil, "a cancelled enqueue accepts nothing")
	}
	release <- struct{}{}
	finish := dtMust(DatabaseWriterEnqueue(context.Background(), writer, "barrier", struct{}{}, func(*Connection) (struct{}, error) {
		return struct{}{}, nil
	}))
	reply := dtMust(finish.Finish())
	dtOK(t, reply.Err)
	dtTrue(t, !ran.Load())
	dtOK(t, owner.Shutdown())
}

// Hard-won: 282a657: a slower repair replaced transcript already committed and acknowledged by another attachment.
func TestProjectionRepairYieldsToAConcurrentCommittedFrontier(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	observed := MaterializedSession("session-1")
	dtOK(t, SaveMaterializedSessionTo(database, &observed))
	replacement := dtCloneSession(observed)
	// Both repairs observed the same broken cursor. The first repairs and
	// catches up before the second reaches the serialized replacement write.
	dtTrue(t, dtMust(repairMaterializedSessionIfCurrentTo(database, &replacement, observed.AppliedEventOrdinal, observed.AppliedEventDigest)))
	winner := dtCloneSession(observed)
	winner.AppliedEventOrdinal++
	winner.AppliedEventDigest = EventDigest(winner.AppliedEventOrdinal)
	winner.SessionTitle = dtPtr("concurrent recovered transcript")
	dtOK(t, SaveMaterializedSessionTo(database, &winner))
	dtTrue(t, !dtMust(repairMaterializedSessionIfCurrentTo(database, &replacement, observed.AppliedEventOrdinal, observed.AppliedEventDigest)))
	saved := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	dtEqual(t, dtJSON(t, saved), dtJSON(t, &winner))
	// A changed digest at the same ordinal is also a different owner state.
	dtTrue(t, !dtMust(repairMaterializedSessionIfCurrentTo(database, &replacement, winner.AppliedEventOrdinal, observed.AppliedEventDigest)))
}

// Hard-won: c1da39f: a pending ACK could prune the good prefix after repair rewound its durable projection.
func TestProjectionRepairNeverRewindsAFrontierAnAttachmentCanAcknowledge(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	durable := MaterializedSession("session-1")
	dtTrue(t, durable.AppliedEventOrdinal > 0)
	dtOK(t, SaveMaterializedSessionTo(database, &durable))
	// Both a genesis probe and a checkpoint probe may succeed on their first
	// replay page while the later journal tail is corrupt. Neither permits
	// deleting a prefix another connection has committed and can now ACK.
	for _, ordinal := range []uint64{0, durable.AppliedEventOrdinal - 1} {
		candidate := core.MaterializedSessionEmpty("session-1")
		candidate.AppliedEventOrdinal = ordinal
		if ordinal > 0 {
			candidate.AppliedEventDigest = EventDigest(ordinal)
		}
		_, err := repairMaterializedSessionIfCurrentTo(database, &candidate, durable.AppliedEventOrdinal, durable.AppliedEventDigest)
		err = dtErr(t, err)
		dtTrue(t, strings.Contains(err.Error(), fmt.Sprintf("durable frontier %d:", durable.AppliedEventOrdinal)), err)
		saved := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
		dtEqual(t, dtJSON(t, saved), dtJSON(t, &durable))
	}
}

// Two reviews of one session can have a role of the same name. A report
// belongs to the review it names, and closing one review finishes only it.
func TestClosingOneOfTwoOpenReviewsFinishesOnlyThatReview(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	owner := "two-reviews-owner"
	record := session(owner, "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	openReview := func(reviewID, root string) core.OpenReview {
		driver, _ := review.TurnReviewDriverStart(core.TurnReviewSeed{
			Task:         "inspect",
			UserMessages: []core.UserMessage{},
			Declined:     []core.ReviewOverride{},
		}, reviewID, 1)
		driver.DeltaCaptured([]core.RepoDelta{{
			Root:        root,
			BaseCommit:  dtPtr("base"),
			HeadCommit:  dtPtr("head"),
			CurrentTree: fmt.Sprintf("tree-%s", reviewID),
			Patch:       "+one",
		}})
		return core.OpenReview{
			Active: dtPtr(reviewID),
			ActiveGroup: &core.ActiveReviewGroup{
				Driver:    driver.Snapshot(),
				Reviewers: make([]core.ResolvedReviewSettings, 1),
				Registrations: []core.ReviewerRegistration{{
					Role:       "reviewer-1",
					RequestKey: fmt.Sprintf("key-%s", reviewID),
				}},
			},
		}
	}
	first, second := openReview("review-a", "/workspace/a"), openReview("review-c", "/workspace/c")
	dtEqual(t, first.Roots(), []string{"/workspace/a"})
	both := core.TurnReviewState{OpenReviews: []core.OpenReview{first, second}}
	// The report names review-a; review-c has a role of the same name.
	dtOK(t, saveReviewerOutcomeIn(database, owner, &both, []AcceptedReport{{
		ReviewID: "review-a",
		Role:     "reviewer-1",
		Text:     "A's report",
	}}))
	dtEqual(t, dtMust(turnReviewStateIn(database, owner)), both)

	dtOK(t, closeTurnReviewStateIn(database, owner, "review-a", &core.TurnReviewState{
		OpenReviews: []core.OpenReview{second},
	}, core.FinishedReviewResolutionCompleted))
	finished := dtMust(finishedReviewIn(database, owner, nil))
	if finished == nil {
		t.Fatal("review-a finished")
	}
	dtEqual(t, finished.ReviewID, "review-a")
	dtEqual(t, finished.Resolution, core.FinishedReviewResolutionCompleted)
	dtEqual(t, finished.Roles[0].Handback, dtPtr("A's report"))
	dtTrue(t, dtMust(finishedReviewIn(database, owner, dtPtr("review-c"))) == nil, "the other review is still open")
	connection := dtOpen(t, database)
	otherHandback := dtQuery[*string](t, connection, "SELECT handback FROM review_reports WHERE review_id = 'review-c'")
	dtEqual(t, otherHandback, (*string)(nil), "A's report did not reach review-c")
}

func TestInterruptedSpillLeavesInlineBodyAndRetryCommitsDurableDetail(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	sessionID := "session-1"
	record := session(sessionID, "project-1")
	dtOK(t, SaveSessionTo(path, &record))
	projection := core.MaterializedSessionEmpty(sessionID)
	projection.AppliedEventOrdinal = 10
	projection.AppliedEventDigest = EventDigest(10)
	item := toolItem(t, 4, "source.rs", "old\n", "new\n")
	projection.Transcript = []*core.TranscriptItem{item}
	dtOK(t, SaveMaterializedSessionTo(path, &projection))
	connection := dtOpen(t, path)
	dtExec(t, connection, `CREATE TRIGGER stop_spill BEFORE UPDATE OF detail_offset ON materialized_transcript_items
         WHEN NEW.detail_offset IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'interrupted after append'); END;`)

	_, failure := SpillAndTrimSettledToolsThroughFrom(path, sessionID, 10)
	failure = dtErr(t, failure)
	dtTrue(t, strings.Contains(failure.Error(), "interrupted after append"), failure)
	detailPath := filepath.Join(directory, "tool-details", "session-1.mjtd")
	info := dtMust(os.Stat(detailPath))
	orphanLen := uint64(info.Size())
	dtTrue(t, orphanLen > 0)
	dtTrue(t, dtMust(ReadStoredToolDetailFrom(path, sessionID, item)) == nil)
	dtEqual(t, dtMust(LoadMaterializedSessionFrom(path, sessionID)).Transcript, []*core.TranscriptItem{item})

	dtExec(t, connection, "DROP TRIGGER stop_spill")
	dtEqual(t, len(dtMust(SpillAndTrimSettledToolsThroughFrom(path, sessionID, 10))), 1)
	dtEqual(t, dtMust(ReadStoredToolDetailFrom(path, sessionID, item)), item.Body)
	offset := dtQuery[int64](t, connection,
		"SELECT detail_offset FROM materialized_transcript_items WHERE session_id = ?1 AND stable_id = ?2",
		sessionID, item.StableID)
	dtEqual(t, uint64(offset), orphanLen)

	compact := dtMust(LoadMaterializedSessionFrom(path, sessionID))
	dtOK(t, SaveMaterializedSessionTo(path, compact))
	dtEqual(t, dtMust(ReadStoredToolDetailFrom(path, sessionID, item)), item.Body)
	dtExec(t, connection, `CREATE TRIGGER stop_delete BEFORE DELETE ON sessions
         BEGIN SELECT RAISE(ABORT, 'interrupted delete'); END;`)
	dtTrue(t, deleteSessionFrom(path, sessionID) != nil)
	stat, err := os.Stat(detailPath)
	dtTrue(t, err == nil && stat.Mode().IsRegular())
	dtExec(t, connection, "DROP TRIGGER stop_delete")
	dtOK(t, deleteSessionFrom(path, sessionID))
	_, err = os.Stat(detailPath)
	dtTrue(t, os.IsNotExist(err))
}

func TestUnsettledFallbackTerminalDefersSpillingSettledTools(t *testing.T) {
	path := dtDatabase(t, "mj.sqlite3")
	sessionID := "session-fallback-defer"
	record := session(sessionID, "project-1")
	dtOK(t, SaveSessionTo(path, &record))
	projection := core.MaterializedSessionEmpty(sessionID)
	projection.AppliedEventOrdinal = 10
	projection.AppliedEventDigest = EventDigest(10)

	fallback := &core.TranscriptItem{
		StableID:        "tool:hel-terminal:term-1",
		Position:        2,
		CreatedAtMs:     1_000,
		LastChangedAtMs: 1_000,
		Body: core.TranscriptBodyTool{
			Call:            core.OpaqueToolCallFromValue(dtJSON(t, core.FallbackTerminalToolCall("term-1", "cargo test"))),
			TerminalOutputs: []core.TerminalOutputRecord{},
			TerminalRefs:    []string{"term-1"},
		},
	}
	providerTool := toolItem(t, 4, "source.rs", "old\n", "new\n")
	projection.Transcript = []*core.TranscriptItem{fallback, providerTool}
	dtOK(t, SaveMaterializedSessionTo(path, &projection))

	// While the fallback terminal tool is unsettled, settled tools stay inline so
	// raw-result matching can claim the fallback when the terminal closes.
	dtEqual(t, len(dtMust(SpillAndTrimSettledToolsThroughFrom(path, sessionID, 10))), 0)
	loaded := dtMust(LoadMaterializedSessionFrom(path, sessionID))
	dtTrue(t, !core.TranscriptBodyDetailTrimmed(loaded.Transcript[1].Body))

	// Once the fallback terminal is consumed and removed, the settled tool spills cleanly.
	projection.Transcript = []*core.TranscriptItem{providerTool}
	dtOK(t, SaveMaterializedSessionTo(path, &projection))
	spilled := dtMust(SpillAndTrimSettledToolsThroughFrom(path, sessionID, 10))
	dtEqual(t, len(spilled), 1)
	dtEqual(t, spilled[0].StableID, providerTool.StableID)
	dtTrue(t, core.TranscriptBodyDetailTrimmed(spilled[0].Body))
}

func TestLoadToolCallDetailJoinsSpilledReferencedStandaloneTerminal(t *testing.T) {
	path := dtDatabase(t, "mj.sqlite3")
	sessionID := "session-referenced-terminal-detail"
	record := session(sessionID, "project-1")
	dtOK(t, SaveSessionTo(path, &record))

	projection := core.MaterializedSessionEmpty(sessionID)
	projection.AppliedEventOrdinal = 10
	projection.AppliedEventDigest = EventDigest(10)

	fullTerminalOutput := "authoritative stdout\n" + strings.Repeat("line\n", 4_000)
	tool := &core.TranscriptItem{
		StableID:        "tool:cmd-1",
		Position:        2,
		CreatedAtMs:     1_000,
		LastChangedAtMs: 1_000,
		Body: core.TranscriptBodyTool{
			Call: core.OpaqueToolCallFromValue(dtValue(t, map[string]any{
				"toolCallId": "cmd-1",
				"title":      "Run command",
				"kind":       "execute",
				"status":     "completed",
				"rawInput":   map[string]any{"command": "cargo test", "padding": strings.Repeat("x", 8_000)},
			})),
			TerminalOutputs: []core.TerminalOutputRecord{{
				TerminalID: "term-1",
				Output:     core.NewPackedText("stale embedded output\n"),
			}},
			TerminalRefs: []string{"term-1"},
		},
	}
	standalone := &core.TranscriptItem{
		StableID:        "terminal:term-1",
		Position:        3,
		CreatedAtMs:     1_001,
		LastChangedAtMs: 1_001,
		Body: core.TranscriptBodyTerminalOutputOf(core.TerminalOutputRecord{
			TerminalID: "term-1",
			Output:     core.NewPackedText(fullTerminalOutput),
			ExitCode:   dtPtr(uint32(0)),
		}),
	}
	projection.Transcript = []*core.TranscriptItem{tool, standalone}
	dtOK(t, SaveMaterializedSessionTo(path, &projection))

	spilled := dtMust(SpillAndTrimSettledToolsThroughFrom(path, sessionID, 10))
	dtEqual(t, len(spilled), 2)

	detail := dtMust(LoadToolCallDetailFrom(path, sessionID, "tool:cmd-1"))
	full, ok := detail.(core.ToolCallDetailResultFull)
	var body core.TranscriptBodyTool
	if ok {
		body, ok = full.Body.(core.TranscriptBodyTool)
	}
	if !ok {
		t.Fatalf("expected hydrated tool detail with joined terminal output, got %#v", detail)
	}
	dtEqual(t, len(body.TerminalOutputs), 1)
	dtEqual(t, body.TerminalOutputs[0].TerminalID, "term-1")
	dtEqual(t, body.TerminalOutputs[0].Output.Text(), fullTerminalOutput)
	dtEqual(t, body.TerminalOutputs[0].ExitCode, dtPtr(uint32(0)))
}
