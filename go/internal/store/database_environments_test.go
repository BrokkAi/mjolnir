// Port of the tests in mj-store/src/database/environments.rs.

package store

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

func TestRegistrationAndPlacementKeepOwnedEnvironmentsSeparate(t *testing.T) {
	const child = "MJ_ISOLATED_OWNED_ENVIRONMENT_REGISTRATION"
	if os.Getenv(child) == "" {
		targets.NewIsolatedTest(targets.TestName("database::environments::tests",
			"TestRegistrationAndPlacementKeepOwnedEnvironmentsSeparate")).
			Env(child, "1").
			IsolatedStore(t.TempDir()).
			Run()
		return
	}
	writer := InstallIsolatedTestWriter()
	defer writer.Shutdown()
	must := func(err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}
	parent := session("parent", "project")
	owned := session("owned", "other-project")
	borrowed := session("borrowed", "project")
	target := func(placement, id string) core.TargetLocator {
		return core.TargetLocatorLocalBare{WorkerRoot: filepath.Join("/workers", placement, id)}
	}
	parent.State = core.SessionStateRunning
	parent.Target = target("initial", parent.ID)
	must(SaveSession(&parent))
	owned.Target = nil
	relation := testSubagent(parent.ID, owned.ID)
	must(RegisterSubagentWithStartup(&owned, &relation, nil, nil, nil, true))
	borrowedRelation := testSubagent(parent.ID, borrowed.ID)
	must(SaveSubagentSession(&borrowed, &borrowedRelation))
	if owns, err := SessionOwnsEnvironment(owned.ID); err != nil || !owns {
		t.Fatalf("owned child owns its environment: %v, %v", owns, err)
	}
	if owns, err := SessionOwnsEnvironment(borrowed.ID); err != nil || owns {
		t.Fatalf("borrowed child owns no environment: %v, %v", owns, err)
	}
	if owner, err := SessionEnvironmentOwner(borrowed.ID); err != nil || owner == nil || *owner != parent.ID {
		t.Fatalf("borrowed owner = %v, %v", owner, err)
	}
	environmentID := func(id string) string {
		t.Helper()
		environment, err := SessionEnvironment(id)
		if err != nil || environment == nil {
			t.Fatalf("environment of %s: %v, %v", id, environment, err)
		}
		return environment.ID
	}
	ownedEnvironment := environmentID(owned.ID)
	borrowedEnvironment := environmentID(borrowed.ID)

	parent.Target = target("moved", parent.ID)
	must(SaveSession(&parent))
	newParentEnvironment := environmentID(parent.ID)
	if borrowedEnvironment == newParentEnvironment {
		t.Fatal("a moved parent must get a new environment")
	}
	owned.Target = target("initial", owned.ID)
	must(SaveSession(&owned))
	must(SaveSession(&borrowed))
	if got := environmentID(owned.ID); got != ownedEnvironment {
		t.Fatalf("owned environment changed to %s", got)
	}
	if got := environmentID(borrowed.ID); got != newParentEnvironment {
		t.Fatalf("borrowed environment = %s, want the moved parent's", got)
	}

	loaded, err := LoadSubagent(owned.ID)
	if err != nil || loaded == nil {
		t.Fatalf("load owned relation: %v, %v", loaded, err)
	}
	if loaded.ResumeWithParent {
		t.Fatal("a new relation does not resume with its parent")
	}
	must(SetSubagentResumeWithParent(owned.ID, true))
	state, err := LoadState()
	must(err)
	must(SaveState(&state))
	expected := relation
	expected.ResumeWithParent = true
	if loaded, err := LoadSubagent(owned.ID); err != nil || loaded == nil || !reflect.DeepEqual(*loaded, expected) {
		t.Fatalf("relation after resume = %+v, %v", loaded, err)
	}
	must(SetSubagentResumeWithParent(owned.ID, false))
	expected.ResumeWithParent = false
	if loaded, err := LoadSubagent(owned.ID); err != nil || loaded == nil || !reflect.DeepEqual(*loaded, expected) {
		t.Fatalf("relation after clearing resume = %+v, %v", loaded, err)
	}

	for _, closing := range []core.SessionState{core.SessionStateClosing, core.SessionStateStopped} {
		parent.State = closing
		must(SaveSession(&parent))
		refused := session("refused", "other-project")
		relation := testSubagent(parent.ID, refused.ID)
		err := RegisterSubagentWithStartup(&refused, &relation, nil, nil, nil, true)
		if err == nil || !strings.Contains(err.Error(), "parent is no longer accepting child work") {
			t.Fatalf("registration under a %s parent = %v", closing, err)
		}
		if record, err := LoadSessionRecord(refused.ID); err != nil || record != nil {
			t.Fatalf("refused child record = %v, %v", record, err)
		}
		if owner, err := SessionEnvironmentOwner(refused.ID); err != nil || owner != nil {
			t.Fatalf("refused child owner = %v, %v", owner, err)
		}
	}
}

// Hard-won: deleting an unreachable child erased the last evidence of a live shared-workspace writer.
func TestMigrationAndLogicalDeletionPreservePhysicalWorkersIncludingUnknownHarnesses(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "ownership.sqlite3")
	connection, err := openWriter(path)
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if connection != nil {
			connection.Close()
		}
	}()
	must := func(err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}
	parent := CheckpointTestSession("22222222222222222222222222222222")
	parent.Target = core.TargetLocatorLocalBare{WorkerRoot: filepath.Join(directory, parent.ID)}
	if runtime.GOOS != "windows" {
		physical := filepath.Join(directory, "physical-\xff")
		parent.ProjectDirectory = &physical
		parent.Target = core.TargetLocatorLocalBare{WorkerRoot: filepath.Join(physical, parent.ID)}
	}
	child := parent
	child.ID = "33333333333333333333333333333333"
	child.Target = core.TargetLocatorLocalBare{WorkerRoot: filepath.Join(directory, child.ID)}
	transaction, err := connection.Begin()
	must(err)
	must(insertSession(transaction, &parent))
	must(insertSessionWithParent(transaction, &child, &parent.ID))
	_, err = transaction.Exec("INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json) VALUES (?1,?2,'fixture','{}')", child.ID, parent.ID)
	must(err)
	must(transaction.Commit())
	source := child
	source.ID = parent.ID
	source.ProjectDirectory = nil
	source.Target = core.TargetLocatorLocalBare{WorkerRoot: filepath.Join(directory, "old-source")}
	encodedSource, err := json.Marshal(&source)
	must(err)
	sourceValue, err := jsonvalue.Parse(encodedSource)
	must(err)
	sourceJSON := sourceValue.(jsonvalue.Object)
	sourceJSON.Set("harness_kind", "removed-harness")
	runtimeSettings := core.TargetRuntimeSettingsFromTemplate(core.TargetTemplateLocalBare{})
	runtimeSettings.Kind = "aws-ec2"
	runtimeSettings.Connection = core.TargetConnectionAws{
		Profile: "fixture",
		Region:  "us-east-1",
		SSHUser: "ubuntu",
		SSHArgs: []string{},
	}
	prepared := core.PreparedMoveDestination{
		LaunchArgs: []string{"--client-token", "fixture-token"},
		Runtime:    runtimeSettings,
		State:      core.PreparedDestinationStateCreated{InstanceID: "i-fixture"},
	}
	encodedPrepared, err := json.Marshal(&prepared)
	must(err)
	preparedValue, err := jsonvalue.Parse(encodedPrepared)
	must(err)
	operation := jsonvalue.Object{}
	operation.Set("operation_id", "old-move")
	operation.Set("workspace_transfer", jsonvalue.ObjectOf(map[string]any{"source": sourceJSON}))
	operation.Set("prepared_destination", preparedValue)
	operationText, err := jsonvalue.Marshal(operation)
	must(err)
	sourceText, err := jsonvalue.Marshal(sourceJSON)
	must(err)
	_, err = connection.Exec("INSERT INTO session_moves(session_id,operation_id,operation_json) VALUES (?1,'old-move',?2)", parent.ID, string(operationText))
	must(err)
	_, err = connection.Exec("INSERT INTO retained_move_sources(operation_id,session_id,source_json,exclusions_json,created_at) VALUES ('old-move',?1,?2,'[]','now')", parent.ID, string(sourceText))
	must(err)
	_, err = connection.Exec(`PRAGMA ignore_check_constraints=ON; UPDATE sessions SET harness_kind='removed-harness' WHERE session_id='33333333333333333333333333333333';
            PRAGMA ignore_check_constraints=OFF;`)
	must(err)
	dropEnvironmentSchema(t, connection)
	restorePre153TranscriptLayout(t, connection)
	_, err = connection.Exec(`DELETE FROM schema_migrations WHERE version>=122;
            UPDATE schema_compatibility SET minimum_compatible_version=121;
            PRAGMA user_version=121;
            CREATE TRIGGER interrupt_ownership_migration BEFORE INSERT ON schema_migrations WHEN NEW.version=122 BEGIN SELECT RAISE(ABORT,'fixture interruption'); END;`)
	must(err)
	if migrateSchema(connection) == nil {
		t.Fatal("the interrupted ownership migration must fail")
	}
	queryInt := func(query string) int64 {
		t.Helper()
		var value int64
		must(connection.QueryRow(query).Scan(&value))
		return value
	}
	if got := queryInt("PRAGMA user_version"); got != 121 {
		t.Fatalf("user_version = %d", got)
	}
	if got := queryInt("SELECT count(*) FROM sqlite_schema WHERE name='environments'"); got != 0 {
		t.Fatalf("environments tables = %d", got)
	}
	if got := queryInt("SELECT minimum_compatible_version FROM schema_compatibility"); got != 121 {
		t.Fatalf("minimum compatible = %d", got)
	}
	_, err = connection.Exec("DROP TRIGGER interrupt_ownership_migration")
	must(err)
	must(migrateSchema(connection))
	queryText := func(query string, args ...any) string {
		t.Helper()
		var value string
		must(connection.QueryRow(query, args...).Scan(&value))
		return value
	}
	environmentID := queryText("SELECT environment_id FROM environment_bindings WHERE session_id=?1", parent.ID)
	sourceEnvironment := queryText("SELECT source_environment_id FROM session_moves WHERE operation_id='old-move'")
	retainedEnvironment := queryText("SELECT environment_id FROM retained_move_sources WHERE operation_id='old-move'")
	if environmentID == sourceEnvironment {
		t.Fatal("the Move source keeps its own environment")
	}
	if sourceEnvironment != retainedEnvironment {
		t.Fatal("the retained source and the Move share one environment")
	}
	sourceRecord, err := readEnvironment(connection, sourceEnvironment)
	must(err)
	if sourceRecord == nil || !reflect.DeepEqual(sourceRecord.Placement.Target, source.Target) {
		t.Fatalf("source placement = %+v", sourceRecord)
	}
	childEnvironment := queryText("SELECT environment_id FROM environment_bindings WHERE session_id=?1", child.ID)
	if environmentID != childEnvironment {
		t.Fatal("the child shares its parent's environment")
	}
	physical, err := readEnvironment(connection, environmentID)
	must(err)
	if physical == nil {
		t.Fatal("the parent environment is missing")
	}
	if !committedOptionalEqual(physical.Placement.ProjectDirectory, parent.ProjectDirectory) {
		t.Fatalf("project directory = %v", physical.Placement.ProjectDirectory)
	}
	if !reflect.DeepEqual(physical.Placement.Target, parent.Target) {
		t.Fatalf("parent target = %+v", physical.Placement.Target)
	}
	workers, err := readEnvironmentWorkers(connection, environmentID)
	must(err)
	if len(workers) != 2 {
		t.Fatalf("workers = %d", len(workers))
	}
	_, err = connection.Exec("DELETE FROM sessions WHERE session_id=?1", child.ID)
	must(err)
	_, err = connection.Exec("DELETE FROM sessions WHERE session_id=?1", parent.ID)
	must(err)
	must(connection.Close())
	connection, err = openWriter(path)
	must(err)
	workers, err = readEnvironmentWorkers(connection, environmentID)
	must(err)
	if len(workers) != 2 {
		t.Fatalf("workers after logical deletion = %d", len(workers))
	}
	retained, err := readEnvironment(connection, environmentID)
	must(err)
	if retained == nil || retained.OwnerSessionID != parent.ID {
		t.Fatalf("retained environment = %+v", retained)
	}
	preparedJSON := queryText("SELECT placement_json FROM environments WHERE json_extract(placement_json,'$.prepared_destination.operation_id')='old-move'")
	var preparedOwner EnvironmentPlacement
	must(json.Unmarshal([]byte(preparedJSON), &preparedOwner))
	if preparedOwner.PreparedDestination == nil {
		t.Fatal("the prepared destination lost its cleanup reservation")
	}
	if instance, ok := preparedOwner.PreparedDestination.Destination.InstanceID(); !ok || instance != "i-fixture" {
		t.Fatalf("prepared instance = %q, %v", instance, ok)
	}
	workers, err = readEnvironmentWorkers(connection, sourceEnvironment)
	must(err)
	if len(workers) != 1 {
		t.Fatalf("source workers = %d", len(workers))
	}
	state, err := readSchemaState(connection)
	must(err)
	if state.ensureSupportedBy(121) == nil {
		t.Fatal("revision 121 must not read the migrated store")
	}
}
