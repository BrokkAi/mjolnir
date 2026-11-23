// Port of mj-store/src/database/tests.rs.
//
// The Rust module is one file of about 9,700 lines; the Go port splits it
// into database_tests_test.go (helpers and the first tests) and
// database_tests_part2_test.go through database_tests_part11_test.go, in the
// Rust order. Rust's test-only read hooks (AFTER_STATE_SESSIONS_READ,
// AFTER_MATERIALIZED_FRONTIER_READ) live in database_tests.go, because the
// store's own files call them.

package store

import (
	"bytes"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"path/filepath"
	"reflect"
	"strings"
	"sync/atomic"
	"testing"
	"unsafe"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// --- Rust's unwrap, assert_eq! and rusqlite one-liners ---

// dtDatabase is a database path in a fresh directory of the test's own
// (Rust: tempfile::tempdir().unwrap().path().join(name)).
func dtDatabase(t testing.TB, name string) string {
	t.Helper()
	return filepath.Join(t.TempDir(), name)
}

// dtMust is Rust's unwrap() on a Result: it panics on an error, which fails
// the test (Go passes a multi-value call only as the whole argument list).
func dtMust[T any](value T, err error) T {
	if err != nil {
		panic(fmt.Sprintf("called unwrap on an error: %v", err))
	}
	return value
}

// dtOK is Rust's unwrap() on a Result<()>.
func dtOK(t testing.TB, err error) {
	t.Helper()
	if err != nil {
		t.Fatalf("called unwrap on an error: %v", err)
	}
}

// dtErr is Rust's unwrap_err().
func dtErr(t testing.TB, err error) error {
	t.Helper()
	if err == nil {
		t.Fatal("called unwrap_err on an Ok value")
	}
	return err
}

func dtPtr[T any](value T) *T { return &value }

// dtTrue is assert!.
func dtTrue(t testing.TB, condition bool, message ...any) {
	t.Helper()
	if !condition {
		t.Fatalf("assertion failed: %s", fmt.Sprint(message...))
	}
}

// dtEqual is assert_eq! on values Rust compares structurally. Go has nil and
// empty slices and maps where Rust has one empty Vec or map, so those compare
// equal; JSON text compares as the value it holds; a type with an Equal
// method (Rust's hand-written PartialEq) compares with it.
func dtEqual(t testing.TB, got, want any, message ...any) {
	t.Helper()
	if !dtLooseEqual(reflect.ValueOf(got), reflect.ValueOf(want)) {
		t.Fatalf("assertion failed: left == right %s\n  left: %s\n right: %s", fmt.Sprint(message...), dtShow(got), dtShow(want))
	}
}

// dtNotEqual is assert_ne!.
func dtNotEqual(t testing.TB, got, want any, message ...any) {
	t.Helper()
	if dtLooseEqual(reflect.ValueOf(got), reflect.ValueOf(want)) {
		t.Fatalf("assertion failed: left != right %s\n  both: %s", fmt.Sprint(message...), dtShow(got))
	}
}

func dtShow(value any) string {
	if encoded, err := json.Marshal(value); err == nil {
		return fmt.Sprintf("%#v %s", value, encoded)
	}
	return fmt.Sprintf("%#v", value)
}

var dtRawMessageType = reflect.TypeOf(json.RawMessage(nil))

var dtQueuedCommandKindType = reflect.TypeFor[core.QueuedCommandKind]()

func dtDefaultQueuedKind(kind reflect.Value) reflect.Value {
	if !kind.IsNil() {
		return kind
	}
	prompt := reflect.New(dtQueuedCommandKindType).Elem()
	prompt.Set(reflect.ValueOf(core.QueuedCommandKindPrompt{}))
	return prompt
}

// dtExported is v readable through Interface and callable: values reached
// through unexported fields are re-addressed or copied, as go-cmp does.
func dtExported(v reflect.Value) reflect.Value {
	if v.CanAddr() {
		return reflect.NewAt(v.Type(), unsafe.Pointer(v.UnsafeAddr())).Elem()
	}
	if v.CanInterface() {
		copied := reflect.New(v.Type()).Elem()
		copied.Set(v)
		return copied
	}
	return v
}

func dtNumeric(v reflect.Value) bool {
	switch v.Kind() {
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64,
		reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64,
		reflect.Float32, reflect.Float64:
		return true
	}
	return false
}

func dtNumber(v reflect.Value) float64 {
	switch v.Kind() {
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		return float64(v.Int())
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64:
		return float64(v.Uint())
	default:
		return v.Float()
	}
}

// dtEqualMethod compares with the type's Equal(T) bool, when it has one.
func dtEqualMethod(left, right reflect.Value) (bool, bool) {
	if !left.CanInterface() || !right.CanInterface() {
		return false, false
	}
	method := left.MethodByName("Equal")
	if !method.IsValid() && left.CanAddr() {
		method = left.Addr().MethodByName("Equal")
	}
	if !method.IsValid() {
		return false, false
	}
	signature := method.Type()
	if signature.NumIn() != 1 || signature.NumOut() != 1 || signature.Out(0).Kind() != reflect.Bool {
		return false, false
	}
	argument := right
	switch {
	case signature.In(0) == right.Type():
	case signature.In(0) == reflect.PointerTo(right.Type()) && right.CanAddr():
		argument = right.Addr()
	default:
		return false, false
	}
	return method.Call([]reflect.Value{argument})[0].Bool(), true
}

func dtJSONEqual(left, right []byte) bool {
	if bytes.Equal(left, right) {
		return true
	}
	var leftValue, rightValue any
	if json.Unmarshal(left, &leftValue) != nil || json.Unmarshal(right, &rightValue) != nil {
		return false
	}
	return reflect.DeepEqual(leftValue, rightValue)
}

func dtLooseEqual(left, right reflect.Value) bool {
	if !left.IsValid() || !right.IsValid() {
		return left.IsValid() == right.IsValid()
	}
	left, right = dtExported(left), dtExported(right)
	if left.Type() != right.Type() {
		if dtNumeric(left) && dtNumeric(right) {
			return dtNumber(left) == dtNumber(right)
		}
		return false
	}
	if left.Type() == dtRawMessageType {
		return dtJSONEqual(left.Bytes(), right.Bytes())
	}
	if left.Kind() != reflect.Pointer && left.Kind() != reflect.Interface {
		if equal, ok := dtEqualMethod(left, right); ok {
			return equal
		}
	}
	switch left.Kind() {
	case reflect.Slice:
		if left.Len() != right.Len() {
			return false
		}
		for index := range left.Len() {
			if !dtLooseEqual(left.Index(index), right.Index(index)) {
				return false
			}
		}
		return true
	case reflect.Array:
		for index := range left.Len() {
			if !dtLooseEqual(left.Index(index), right.Index(index)) {
				return false
			}
		}
		return true
	case reflect.Map:
		if left.Len() != right.Len() {
			return false
		}
		for _, key := range left.MapKeys() {
			other := right.MapIndex(key)
			if !other.IsValid() || !dtLooseEqual(left.MapIndex(key), other) {
				return false
			}
		}
		return true
	case reflect.Pointer, reflect.Interface:
		if left.Kind() == reflect.Interface && left.Type() == dtQueuedCommandKindType {
			// core.MaterializedQueuedPrompt documents a nil Kind as Prompt,
			// Rust's serde default.
			left, right = dtDefaultQueuedKind(left), dtDefaultQueuedKind(right)
		}
		if left.IsNil() || right.IsNil() {
			return left.IsNil() == right.IsNil()
		}
		return dtLooseEqual(left.Elem(), right.Elem())
	case reflect.Struct:
		for index := range left.NumField() {
			if !dtLooseEqual(left.Field(index), right.Field(index)) {
				return false
			}
		}
		return true
	case reflect.Bool:
		return left.Bool() == right.Bool()
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		return left.Int() == right.Int()
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		return left.Uint() == right.Uint()
	case reflect.Float32, reflect.Float64:
		return left.Float() == right.Float()
	case reflect.Complex64, reflect.Complex128:
		return left.Complex() == right.Complex()
	case reflect.String:
		return left.String() == right.String()
	default:
		return left.Pointer() == right.Pointer()
	}
}

// dtRaw is rusqlite's Connection::open, closed when the test ends.
func dtRaw(t testing.TB, path string) *Connection {
	t.Helper()
	connection := dtMust(OpenConnection(path))
	t.Cleanup(func() { connection.Close() })
	return connection
}

// dtOpen is the store's open, closed when the test ends.
func dtOpen(t testing.TB, path string) *Connection {
	t.Helper()
	connection := dtMust(open(path))
	t.Cleanup(func() { connection.Close() })
	return connection
}

// dtOpenReader is the store's open_reader, closed when the test ends.
func dtOpenReader(t testing.TB, path string) *reader {
	t.Helper()
	connection := dtMust(openReader(path))
	t.Cleanup(connection.Close)
	return connection
}

// dtExec is rusqlite's execute and execute_batch, unwrapped.
func dtExec(t testing.TB, connection Queryer, query string, args ...any) int64 {
	t.Helper()
	result, err := connection.Exec(query, args...)
	if err != nil {
		t.Fatalf("%s: %v", query, err)
	}
	changed, _ := result.RowsAffected()
	return changed
}

// dtQuery is rusqlite's query_row of one column, unwrapped.
func dtQuery[T any](t testing.TB, connection Queryer, query string, args ...any) T {
	t.Helper()
	var value T
	if err := connection.QueryRow(query, args...).Scan(&value); err != nil {
		t.Fatalf("%s: %v", query, err)
	}
	return value
}

// dtQueryOptional is query_row(..).optional() of one column, unwrapped: nil
// when there is no row.
func dtQueryOptional[T any](t testing.TB, connection Queryer, query string, args ...any) *T {
	t.Helper()
	var value T
	err := connection.QueryRow(query, args...).Scan(&value)
	if errors.Is(err, sql.ErrNoRows) {
		return nil
	}
	if err != nil {
		t.Fatalf("%s: %v", query, err)
	}
	return &value
}

// dtQueryColumn is query_map of one column collected into a Vec, unwrapped.
func dtQueryColumn[T any](t testing.TB, connection Queryer, query string, args ...any) []T {
	t.Helper()
	rows, err := connection.Query(query, args...)
	if err != nil {
		t.Fatalf("%s: %v", query, err)
	}
	defer rows.Close()
	values := []T{}
	for rows.Next() {
		var value T
		if err := rows.Scan(&value); err != nil {
			t.Fatalf("%s: %v", query, err)
		}
		values = append(values, value)
	}
	if err := rows.Err(); err != nil {
		t.Fatalf("%s: %v", query, err)
	}
	return values
}

// dtHasRow is whether query returns any row (query_row(..).optional() with a
// unit mapping).
func dtHasRow(t testing.TB, connection Queryer, query string, args ...any) bool {
	t.Helper()
	rows, err := connection.Query(query, args...)
	if err != nil {
		t.Fatalf("%s: %v", query, err)
	}
	defer rows.Close()
	found := rows.Next()
	if err := rows.Err(); err != nil {
		t.Fatalf("%s: %v", query, err)
	}
	return found
}

// dtBegin is connection.transaction(), unwrapped; the test commits it.
func dtBegin(t testing.TB, connection *Connection) *Transaction {
	t.Helper()
	transaction := dtMust(connection.Begin())
	t.Cleanup(func() { transaction.Rollback() })
	return transaction
}

// dtWrite is DatabaseWriter::execute of a job with no result.
func dtWrite(writer *DatabaseWriter, label string, job func(connection *Connection) error) error {
	_, err := DatabaseWriterExecute(writer, label, func(connection *Connection) (struct{}, error) {
		return struct{}{}, job(connection)
	})
	return err
}

// dtSessionOf is state.sessions[id]: the record, panicking when absent.
func dtSessionOf(state core.StateState, id string) core.SessionRecord {
	return *state.Sessions.Index(id)
}

// dtJSON is serde_json::to_value: the value's JSON, unwrapped.
func dtJSON(t testing.TB, value any) json.RawMessage {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatalf("serialize %T: %v", value, err)
	}
	return encoded
}

// dtSchemaMismatch is the StoreSchemaMismatch anywhere in err's chain.
func dtSchemaMismatch(err error) (core.StoreSchemaMismatch, bool) {
	var mismatch core.StoreSchemaMismatch
	if errors.As(err, &mismatch) {
		return mismatch, true
	}
	var pointer *core.StoreSchemaMismatch
	if errors.As(err, &pointer) && pointer != nil {
		return *pointer, true
	}
	return core.StoreSchemaMismatch{}, false
}

// --- tests.rs ---

func TestInvalidProjectLocationsAreRefusedBeforeDatabaseAdmission(t *testing.T) {
	valid := core.ProjectLocation{
		Host:           "remote",
		Directory:      "/projects/app",
		CheckoutRoot:   "/projects/app",
		RepositoryRoot: "/projects/app",
		Identity:       core.RepositoryIdentityGithub{Owner: "acme", Repository: "app"},
		SeenAt:         "now",
	}
	for field := range 4 {
		location := valid
		switch field {
		case 0:
			location.Directory = "relative"
		case 1:
			location.CheckoutRoot = "--path-format=absolute"
		case 2:
			location.RepositoryRoot = "relative"
		default:
			location.Host = ""
		}
		message := dtErr(t, StoreProjectLocation(&location)).Error()
		dtTrue(t, strings.Contains(message, "must contain absolute"), message)
	}
}

func TestABoundedPromptSearchReportsThatItStoppedEarly(t *testing.T) {
	// Without the flag a caller cannot tell ten matches from the first ten of
	// many, and will present a partial answer as a whole one.
	database := dtDatabase(t, "hel.sqlite3")
	liveSessions(t, database, [][2]string{{"session-1", "bundle-1"}})
	for index := range uint64(50) {
		dtOK(t, recordPromptTo(database, "session-1", "bundle-1", index, dtPtr("2026-01-01T00:00:00Z"), fmt.Sprintf("ship it %d", index)))
	}

	found := dtMust(searchPromptPageFrom(database, "session-1", "bundle-1", core.HistoryScopeSession, "ship", nil, 10))
	dtEqual(t, len(found.Entries), 10)
	dtTrue(t, found.Next != nil, "a search that stopped early reported a complete answer")

	all := dtMust(searchPromptPageFrom(database, "session-1", "bundle-1", core.HistoryScopeSession, "ship", nil, 100))
	dtEqual(t, len(all.Entries), 50)
	dtTrue(t, all.Next == nil, "a complete answer reported itself partial")

	// Ten distinct prompts, each sent twice: the older copies add no match.
	for round := range uint64(2) {
		for index := range uint64(10) {
			dtOK(t, recordPromptTo(database, "session-1", "bundle-1", 100+round*10+index, dtPtr("2026-01-01T00:00:00Z"), fmt.Sprintf("repeat %d", index)))
		}
	}
	// Hard-won: a58ad7eb8: a full page followed only by repeats claimed more matches existed
	repeats := dtMust(searchPromptPageFrom(database, "session-1", "bundle-1", core.HistoryScopeSession, "repeat", nil, 10))
	dtEqual(t, len(repeats.Entries), 10)
	dtTrue(t, repeats.Next == nil, "repeated text is not another match")

	// However often each is repeated, the ten texts fill one page exactly.
	for round := uint64(2); round < 5; round++ {
		for index := range uint64(10) {
			dtOK(t, recordPromptTo(database, "session-1", "bundle-1", 100+round*10+index, dtPtr("2026-01-01T00:00:00Z"), fmt.Sprintf("repeat %d", index)))
		}
	}
	repeated := dtMust(searchPromptPageFrom(database, "session-1", "bundle-1", core.HistoryScopeSession, "repeat", nil, 10))
	dtEqual(t, len(repeated.Entries), 10)
	dtTrue(t, repeated.Next == nil)
}

// The eviction the daemon performs depends on the typed cause reaching the
// refresher through `Controller::load`, which is three `anyhow` hops away.
// Nothing plumbs it; this pins that nothing has to.
func TestStoreSchemaMismatchSurvivesTheControllerLoadErrorChain(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	dtOK(t, dtMust(openWriter(database)).Close())
	migrateStoreUnderneath(t, database)

	_, err := LoadStateFrom(database)
	mismatch, ok := dtSchemaMismatch(dtErr(t, err))
	dtTrue(t, ok, "the mismatch survives every hop to the caller")
	dtEqual(t, mismatch.Found, schemaVersion+1)
	dtEqual(t, mismatch.Supported, schemaVersion)
}

// The writer verifies the schema once, when it opens. Issue #24 is what
// happens next: another process migrated the store, and this lane kept
// writing rows the store's new ladder does not expect. Every queued write is
// now refused with the reason.
// Hard-won: b19f7b1c: a queued projection write after another process advances the store
func TestWriterRefusesAProjectionWriteAfterTheStoreMoves(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	owner := dtMust(startDatabaseWriterAt(database, false))
	writer := owner.writer
	migrateStoreUnderneath(t, database)

	mutation := core.MaterializedSessionMutation{LastActivityAtMs: dtPtr(int64(105))}
	digest := EventDigest(1)
	_, err := DatabaseWriterExecute(writer, "apply_projection_event", func(connection *Connection) (core.ProjectionApplyOutcome, error) {
		return applyProjectionPageWith(connection, "session-1", func(page *ProjectionPage) (core.ProjectionApplyOutcome, error) {
			return page.Apply(1, core.RelayEventGenesisDigest, digest, &mutation)
		})
	})

	assertMismatch(t, dtErr(t, err))
	// Read back on a raw connection: the store is ahead of this build now, so
	// every reader this crate offers correctly refuses to open it.
	applied := dtQueryOptional[int64](t, dtRaw(t, database),
		"SELECT applied_event_ordinal FROM materialized_sessions WHERE session_id = 'session-1'")
	dtEqual(t, applied, dtPtr(int64(0)), "the refused write left the projection where it was")
	dtOK(t, owner.Shutdown())
}

// Hard-won: b19f7b1c: a queued receipt write after another process advances the store
func TestWriterRefusesAReadReceiptAfterTheStoreMoves(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	owner := dtMust(startDatabaseWriterAt(database, false))
	writer := owner.writer
	migrateStoreUnderneath(t, database)

	_, err := DatabaseWriterExecute(writer, "persist_read_receipt", func(connection *Connection) (uint64, error) {
		return persistReadReceiptWith(connection, "client-1", core.DefaultWorkspaceID, "session-1", 0)
	})

	assertMismatch(t, dtErr(t, err))
	receipts := dtQuery[int64](t, dtRaw(t, database), "SELECT count(*) FROM client_read_frontiers")
	dtEqual(t, receipts, int64(0), "the refused receipt wrote no row")
	dtOK(t, owner.Shutdown())
}

// migrateStoreUnderneath moves the store's recorded schema forward the way
// another build's ladder would, under a writer that has already opened it.
func migrateStoreUnderneath(t testing.TB, path string) {
	t.Helper()
	advanceTestSchema(t, path, schemaVersion+1, schemaVersion+1)
}

func assertMismatch(t testing.TB, failure error) {
	t.Helper()
	mismatch, ok := dtSchemaMismatch(failure)
	if !ok {
		t.Fatalf("the refusal names the divergence, got %v", failure)
	}
	dtEqual(t, mismatch.Found, schemaVersion+1)
	dtEqual(t, mismatch.Supported, schemaVersion)
	dtEqual(t, mismatch.Reason, core.StoreSchemaMismatchReason(core.StoreSchemaMismatchReasonIncompatible{
		MinimumCompatible: schemaVersion + 1,
	}))
}

func TestCompatibleMigrationPreservesNewDataThroughExistingSessionWrites(t *testing.T) {
	database := dtDatabase(t, "mj.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	owner := dtMust(startDatabaseWriterAt(database, false))
	raw := dtRaw(t, database)
	dtExec(t, raw, `ALTER TABLE sessions ADD COLUMN future_note TEXT;
         UPDATE sessions SET future_note = 'new feature data';
         CREATE TABLE future_feature(value TEXT NOT NULL);
         INSERT INTO future_feature VALUES ('keep');`)
	advanceTestSchema(t, database, schemaVersion+1, schemaVersion)

	record.Title = "updated by older build"
	updated := record
	dtOK(t, dtWrite(owner.writer, "save session", func(connection *Connection) error {
		tx, err := connection.Begin()
		if err != nil {
			return err
		}
		defer tx.Rollback()
		if err := insertSession(tx, &updated); err != nil {
			return err
		}
		return tx.Commit()
	}))
	dtOK(t, owner.Shutdown())
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), record.ID).Title, record.Title)
	// The ordinary path-taking writer reopens the future store too.
	record.Title = "updated after reopening"
	dtOK(t, SaveSessionTo(database, &record))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), record.ID).Title, record.Title)
	dtEqual(t, dtQuery[string](t, raw, "SELECT future_note FROM sessions WHERE session_id = 'session-1'"), "new feature data")
	dtEqual(t, dtQuery[string](t, raw, "SELECT value FROM future_feature"), "keep")
	dtEqual(t, dtQuery[int64](t, raw, "PRAGMA user_version"), schemaVersion+1)
	dtEqual(t, dtQuery[int64](t, raw, "SELECT max(version) FROM schema_migrations"), schemaVersion+1)
}

func TestWriterRefusesRollbackAfterObservingACompatibleMigration(t *testing.T) {
	for _, reopen := range []bool{false, true} {
		database := dtDatabase(t, "mj.sqlite3")
		owner := dtMust(startDatabaseWriterAt(database, false))
		advanceTestSchema(t, database, schemaVersion+1, schemaVersion)
		if reopen {
			dtOK(t, owner.Shutdown())
			owner = dtMust(startDatabaseWriterAt(database, false))
		}
		dtOK(t, dtWrite(owner.writer, "observe compatible revision", func(connection *Connection) error {
			_, err := connection.Exec(
				"INSERT INTO mount_history(host, source, ordinal) VALUES ('test', ?1, 0)",
				pathToBlob("/first"),
			)
			return err
		}))
		raw := dtRaw(t, database)
		dtExec(t, raw, "DELETE FROM schema_migrations WHERE version > ?1", schemaVersion)
		dtExec(t, raw, fmt.Sprintf("PRAGMA user_version = %d", schemaVersion))
		err := dtErr(t, dtWrite(owner.writer, "write after rollback", func(connection *Connection) error {
			_, err := connection.Exec("DELETE FROM mount_history")
			return err
		}))
		var mismatch core.StoreSchemaMismatch
		dtTrue(t, errors.As(err, &mismatch), err)
		dtEqual(t, mismatch.Reason, core.StoreSchemaMismatchReason(core.StoreSchemaMismatchReasonRollback{
			Previous: schemaVersion + 1,
		}))
		dtEqual(t, dtQuery[int64](t, raw, "SELECT count(*) FROM mount_history"), int64(1))
		dtOK(t, owner.Shutdown())
	}
}

func TestWriterRefusesMissingCompatibilityMetadataInsteadOfRunningTheJob(t *testing.T) {
	database := dtDatabase(t, "mj.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	owner := dtMust(startDatabaseWriterAt(database, false))
	raw := dtRaw(t, database)
	dtExec(t, raw, "DROP TABLE schema_compatibility")
	err := dtErr(t, dtWrite(owner.writer, "delete sessions", func(connection *Connection) error {
		_, err := connection.Exec("DELETE FROM sessions")
		return err
	}))
	var mismatch core.StoreSchemaMismatch
	dtTrue(t, errors.As(err, &mismatch), err)
	dtEqual(t, mismatch.Reason, core.StoreSchemaMismatchReason(core.StoreSchemaMismatchReasonInvalidCompatibilityMetadata{}))
	dtEqual(t, dtQuery[int64](t, raw, "SELECT count(*) FROM sessions"), int64(1))
	dtOK(t, owner.Shutdown())
}

func TestDatabaseWriterOrdersJobsAndSurvivesAnOperationError(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	owner := dtMust(startDatabaseWriterAt(database, false))
	writer := owner.writer

	dtOK(t, dtWrite(writer, "create_test_log", func(connection *Connection) error {
		_, err := connection.Exec(`CREATE TABLE writer_test_log (
                    sequence INTEGER PRIMARY KEY,
                    value TEXT NOT NULL
                 ) STRICT;`)
		return err
	}))
	for _, entry := range []struct {
		sequence int64
		value    string
	}{{1, "first"}, {2, "second"}, {3, "third"}} {
		dtOK(t, dtWrite(writer, "append_test_log", func(connection *Connection) error {
			_, err := connection.Exec(
				"INSERT INTO writer_test_log(sequence, value) VALUES (?1, ?2)",
				entry.sequence, entry.value,
			)
			return err
		}))
	}
	failure := dtErr(t, dtWrite(writer, "expected_failure", func(connection *Connection) error {
		_, err := connection.Exec("INSERT INTO missing_table VALUES (1)")
		return err
	}))
	dtTrue(t, strings.Contains(failure.Error(), "missing_table"), failure)
	values := dtMust(DatabaseWriterExecute(writer, "read_test_log", func(connection *Connection) ([]string, error) {
		rows, err := connection.Query("SELECT value FROM writer_test_log ORDER BY sequence")
		if err != nil {
			return nil, err
		}
		defer rows.Close()
		var values []string
		for rows.Next() {
			var value string
			if err := rows.Scan(&value); err != nil {
				return nil, err
			}
			values = append(values, value)
		}
		return values, rows.Err()
	}))
	dtEqual(t, values, []string{"first", "second", "third"})
	dtOK(t, owner.Shutdown())
}

func TestDatabaseWriterAppliesBoundedBackpressureAndDrainsAcceptedJobs(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	owner := dtMust(startDatabaseWriterAt(database, false))
	writer := owner.writer
	started := make(chan struct{}, 1)
	release := make(chan struct{}, 1)
	writer.sender <- databaseWriterMessageRun{
		label: "block_test_writer",
		job: func(*Connection, error) {
			started <- struct{}{}
			<-release
		},
	}
	<-started

	var completed atomic.Int64
	for range databaseWriteQueueCapacity {
		select {
		case writer.sender <- databaseWriterMessageRun{
			label: "queued_test_write",
			job:   func(*Connection, error) { completed.Add(1) },
		}:
		default:
			t.Fatal("the queue refused a write within its capacity")
		}
	}
	select {
	case writer.sender <- databaseWriterMessageRun{label: "queue_overflow_probe", job: func(*Connection, error) {}}:
		t.Fatal("the queue accepted a write beyond its capacity")
	default:
	}

	release <- struct{}{}
	dtOK(t, owner.Shutdown())
	dtEqual(t, completed.Load(), int64(databaseWriteQueueCapacity))
}

func TestDatabaseWriterReportsAFatalJobPanicWithoutHangingShutdown(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	owner := dtMust(startDatabaseWriterAt(database, false))
	writer := owner.writer

	operation := dtErr(t, dtWrite(writer, "panic_probe", func(*Connection) error {
		panic("intentional writer panic")
	}))
	dtTrue(t, strings.Contains(operation.Error(), "writer stopped"), operation)
	shutdown := dtErr(t, owner.Shutdown())
	dtTrue(t, strings.Contains(shutdown.Error(), "intentional writer panic"), shutdown)
}

// liveSessions saves sessions first: prompt history keeps only sessions that
// exist, so a test that records prompts saves its sessions first.
func liveSessions(t testing.TB, database string, sessions [][2]string) {
	t.Helper()
	for _, entry := range sessions {
		record := session(entry[0], entry[1])
		dtOK(t, SaveSessionTo(database, &record))
	}
}

// session is the store tests' session record (database.rs test_session).
func session(id, bundle string) core.SessionRecord {
	return TestSession(id, bundle)
}

func TestNormalizedStateRoundTripPreservesChildrenAndOrder(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	state := core.NewStateState()
	record := session("session-1", "project-1")
	record.ProjectDirectory = dtPtr("/srv/project-1/.mj/worktrees/session-1")
	record.ManagedWorktree = &core.ManagedWorktree{
		SourceProjectDirectory: "/srv/project-1",
		SourceRepository:       "/srv/project-1",
		WorktreeRoot:           "/srv/project-1/.mj/worktrees/session-1",
		Branch:                 "mj/session-1",
		Target: core.ManagedWorktreeTargetSsh{
			Destination: "builder",
			SSHArgs:     []string{"-o", "BatchMode=yes"},
		},
	}
	record.ResourceAllocation = nil
	record.LaunchBase = dtPtr("origin/main")
	record.Checkout = &core.ExactCheckout{
		RepositoryID: "project",
		Commit:       strings.Repeat("a", 40),
		Branch:       dtPtr("town/run-123"),
	}
	record.Target = core.TargetLocatorLocalBare{WorkerRoot: "/var/lib/hel/workers/session-1"}
	state.EnvironmentOwners.Insert(record.ID, record.ID)
	state.Sessions.Insert(record.ID, record)
	state.MountHistory.Insert("local", []string{"/recent", "/older"})
	state.ContainerSizes.Insert("local", core.HostContainerSize{Cpus: 12, MemoryBytes: 48 * 1024 * 1024 * 1024})

	dtOK(t, SaveStateTo(database, &state))

	dtEqual(t, dtMust(LoadStateFrom(database)), state)
	connection := dtOpen(t, database)
	dtEqual(t, dtQuery[int64](t, connection, "PRAGMA user_version"), schemaVersion)
	dtTrue(t, !dtHasRow(t, connection, "PRAGMA foreign_key_check"))
}

func TestRemovingRuntimeIdentityUpgradesExistingSessionsAndPreservesReceiptHistory(t *testing.T) {
	path := dtDatabase(t, "runtime-removal.sqlite3")
	record := session("old-session", "project-1")
	dtOK(t, SaveSessionTo(path, &record))
	receipt := json.RawMessage(`{"data":{"receipt":{"components":[{"name":"acp_bridge","sha256":null,"version":"old"}],` +
		`"event_ordinal":7,"harness":"codex","id":"mj-runtime-v1:saved","observed_at_ms":123,"platform":"linux-x86_64",` +
		`"provenance":"managed_installation","unavailable_reason":null}},"type":"runtime_resolved"}`)
	connection := dtMust(OpenConnection(path))
	restoreGithubV84Schema(t, connection)
	restorePre101SubagentPreference(t, connection)
	restoreReviewStateSchema(t, connection, 65)
	restorePre153TranscriptLayout(t, connection)
	dtExec(t, connection, `ALTER TABLE sessions ADD COLUMN expected_runtime_identity TEXT;
        UPDATE sessions SET expected_runtime_identity = 'mj-runtime-v1:saved';
        ALTER TABLE sessions DROP COLUMN project_json;
        DROP TRIGGER project_discovery_insert;
        DROP TRIGGER project_discovery_update;
        DROP TABLE project_discovery_failures;
        DROP TABLE project_discovery_changes;
        DROP TABLE project_discovery_progress;
        DROP TABLE project_session_aliases;
        DROP TABLE project_aliases;
        DROP TABLE project_catalog;
        DROP TABLE project_locations;
        DROP TABLE project_seed_homes;
        DROP TABLE project_seed_failures;
        DELETE FROM schema_migrations WHERE version >= 66;
        UPDATE schema_compatibility SET minimum_compatible_version = 65;
        DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away; DROP TABLE IF EXISTS subagent_accounting; DROP TABLE IF EXISTS session_turn_selections; ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic; ALTER TABLE mailbox_outbox DROP COLUMN failure; PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '') WHERE type='table' AND name='sessions'; PRAGMA writable_schema=RESET; PRAGMA user_version = 65;`)
	dtExec(t, connection, "INSERT INTO api_events(session_id, recorded_at_ms, body) VALUES (?1, 123, ?2)", record.ID, string(receipt))
	dtOK(t, connection.Close())
	forgetVerifiedSchema(path)

	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(path)), "old-session"), record)
	opened := dtOpen(t, path)
	dtTrue(t, !dtMust(tableHasColumn(opened, "sessions", "expected_runtime_identity")))
	dtEqual(t, dtQuery[int64](t, opened, "SELECT minimum_compatible_version FROM schema_compatibility"), minimumCompatibleVersion)
	events := dtMust(loadAPIEventsFrom(path, &core.ApiEventFilter{}, dtPtr(uint64(0)), 100)).Events
	dtEqual(t, len(events), 1)
	// serde_json::to_value writes the adjacently tagged envelope.
	dtEqual(t, json.RawMessage(dtMust(core.MarshalApiEventData(events[0].Event))), receipt)
	// Saving/resuming the old session no longer requires the removed column.
	dtOK(t, SaveSessionTo(path, &record))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(path)), "old-session"), record)
}

func TestExactCheckoutMigrationPreservesHistoryAndLifecycleUpdatesPreserveSelection(t *testing.T) {
	path := dtDatabase(t, "exact-checkout.sqlite3")
	record := session("old-session", "project-1")
	dtOK(t, SaveSessionTo(path, &record))
	connection := dtMust(OpenConnection(path))
	restoreGithubV84Schema(t, connection)
	restorePre101SubagentPreference(t, connection)
	restoreReviewStateSchema(t, connection, 53)
	restorePre153TranscriptLayout(t, connection)
	dtExec(t, connection, `ALTER TABLE sessions DROP COLUMN checkout_json;
        ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic;
        DELETE FROM schema_migrations WHERE version >= 54;
        UPDATE schema_compatibility SET minimum_compatible_version = 53;
        DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away; DROP TABLE IF EXISTS subagent_accounting;
        DROP TABLE IF EXISTS session_turn_selections;
        DROP TABLE IF EXISTS github_seen_items;
        DROP TABLE IF EXISTS github_watch_cursors;
        DROP TABLE IF EXISTS github_watch_classifications;
        DROP TABLE IF EXISTS github_watch_items;
        DROP TABLE IF EXISTS mailbox_outbox;
        PRAGMA writable_schema=ON;
        UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '') WHERE type='table' AND name='sessions';
        PRAGMA writable_schema=RESET;
        PRAGMA user_version = 53;`)
	dtOK(t, connection.Close())
	forgetVerifiedSchema(path)
	opened := dtOpen(t, path)
	dtEqual(t, dtQuery[int64](t, opened, "PRAGMA user_version"), schemaVersion)
	for _, table := range []string{
		"github_watch_cursors",
		"github_watch_classifications",
		"github_watch_items",
		"mailbox_outbox",
	} {
		dtTrue(t, dtQuery[bool](t, opened, "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", table),
			"migration 75 creates ", table)
	}
	watchColumns := dtQueryColumn[string](t, opened, "SELECT name FROM pragma_table_info('github_watch_items')")
	dtTrue(t, dtContains(watchColumns, "timeline_at"))
	dtTrue(t, dtContains(watchColumns, "processed_at"))
	dtEqual(t, dtQuery[int64](t, opened, "SELECT minimum_compatible_version FROM schema_compatibility"), minimumCompatibleVersion)
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(path)), "old-session"), record)
	record.Checkout = &core.ExactCheckout{
		RepositoryID: "project",
		Commit:       strings.Repeat("a", 40),
		Branch:       dtPtr("town/run-123"),
	}
	dtOK(t, SaveSessionTo(path, &record))
	writer := dtOpen(t, path)
	tx := dtBegin(t, writer)
	stale := record
	stale.Checkout = nil
	stale.State = core.SessionStateError
	dtOK(t, updateLifecycleFields(tx, &stale))
	dtOK(t, tx.Commit())
	loaded := dtMust(LoadStateFrom(path))
	dtEqual(t, dtSessionOf(loaded, "old-session").Checkout, record.Checkout)
	dtEqual(t, dtSessionOf(loaded, "old-session").State, core.SessionStateError)
}

func dtContains[T comparable](values []T, wanted T) bool {
	for _, value := range values {
		if value == wanted {
			return true
		}
	}
	return false
}

func TestClonePublicationEvidenceRoundTripsAndMigrationPreservesOldRows(t *testing.T) {
	database := dtDatabase(t, "publication.sqlite3")
	old := session("old-session", "project-1")
	old.LaunchBranch = dtPtr("main")
	dtOK(t, SaveSessionTo(database, &old))

	connection := dtMust(OpenConnection(database))
	restoreGithubV84Schema(t, connection)
	restorePre101SubagentPreference(t, connection)
	restoreReviewStateSchema(t, connection, 46)
	restorePre153TranscriptLayout(t, connection)
	dtExec(t, connection, `ALTER TABLE sessions DROP COLUMN publication_json;
         ALTER TABLE sessions DROP COLUMN launch_branch;
         DELETE FROM schema_migrations WHERE version >= 47;
         UPDATE schema_compatibility SET minimum_compatible_version = 46;
         DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away; DROP TABLE IF EXISTS subagent_accounting; DROP TABLE IF EXISTS session_turn_selections; ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic; ALTER TABLE mailbox_outbox DROP COLUMN failure; PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '') WHERE type='table' AND name='sessions'; PRAGMA writable_schema=RESET; PRAGMA user_version = 46;`)
	dtOK(t, connection.Close())
	forgetVerifiedSchema(database)
	dtOK(t, dtMust(openWriter(database)).Close())
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), "old-session").Title, old.Title)

	clone := session("clone-session", "project-1")
	root := "/srv/project/.mj/clones/clone-session"
	clone.ProjectDirectory = dtPtr(root)
	clone.Target = nil
	clone.ManagedWorktree = &core.ManagedWorktree{
		Kind:                   core.ManagedCheckoutKindClone,
		SourceProjectDirectory: "/srv/project",
		SourceRepository:       "/srv/project",
		WorktreeRoot:           root,
		Branch:                 "main",
		Target:                 core.ManagedWorktreeTargetLocal{},
		BaseCommit:             dtPtr(strings.Repeat("1", 40)),
	}
	clone.Publication = &core.PublicationAssessment{
		CheckpointSha256: strings.Repeat("a", 64),
		State:            core.PublicationStatePublished,
		SavedCommits:     []string{strings.Repeat("1", 40)},
		Destinations:     []string{"https://example.test/repository.git"},
		CheckedAt:        "2026-08-12T01:00:00Z",
	}
	dtOK(t, SaveSessionTo(database, &clone))
	loaded := dtMust(LoadStateFrom(database))
	loadedClone := dtSessionOf(loaded, "clone-session")
	dtEqual(t, loadedClone.Publication, clone.Publication)
	publication, ok := loadedClone.PublicationState()
	dtTrue(t, ok)
	dtEqual(t, publication, core.PublicationStatePublished)
}

func TestLocalDockerLocatorRoundTripsThroughTheNormalizedTargetTable(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	record.TargetTemplateID = "docker"
	record.Target = core.TargetLocatorLocalDocker{ContainerID: "hel-session-1"}

	dtOK(t, SaveSessionTo(database, &record))

	loaded := dtMust(LoadStateFrom(database))
	dtEqual(t, dtSessionOf(loaded, "session-1"), record)
	connection := dtOpen(t, database)
	dtEqual(t, dtQuery[string](t, connection, "SELECT kind FROM session_targets WHERE session_id = 'session-1'"), "local-docker")

	// A sandbox keeps its host and resource in the same normalized columns,
	// and its borrowing child keeps its owner.
	for _, entry := range []struct {
		id           string
		borrowedFrom *string
	}{{"sandbox", nil}, {"sandbox-child", dtPtr("sandbox")}} {
		record := session(entry.id, "project-1")
		record.TargetTemplateID = "nitro"
		record.Target = core.TargetLocatorSshNitro{
			BorrowedFrom: entry.borrowedFrom,
			Host:         "ec2.example.test",
			ContainerID:  "mj-sandbox",
		}
		dtOK(t, SaveSessionTo(database, &record))
		dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), entry.id), record)
	}
	dtEqual(t, dtQuery[string](t, connection, "SELECT kind FROM session_targets WHERE session_id = 'sandbox'"), "ssh-nitro")
}

func TestABorrowedContainerTargetRoundTripsAndANullColumnMeansOwned(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	record.TargetTemplateID = "podman"
	record.Target = core.TargetLocatorLocalPodman{
		BorrowedFrom:     dtPtr("parent-session"),
		ContainerID:      "hel-parent-session",
		WorkspaceStorage: core.PodmanWorkspaceLocatorContainerLayer{},
	}

	dtOK(t, SaveSessionTo(database, &record))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), "session-1"), record)

	// A row an older build wrote, or rewrote without the column, is an
	// ordinary session-owned target.
	connection := dtOpen(t, database)
	dtExec(t, connection, "UPDATE session_targets SET borrowed_from = NULL WHERE session_id = 'session-1'")
	dtOK(t, connection.Close())
	locator, ok := dtSessionOf(dtMust(LoadStateFrom(database)), "session-1").Target.(core.TargetLocatorLocalPodman)
	if !ok {
		t.Fatal("Podman locator changed kind")
	}
	dtEqual(t, locator.BorrowedFrom, (*string)(nil))
}

func TestPodmanWorkspaceLocatorRoundTripsAndLegacyNullDefaultsToContainerLayer(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	record.Target = core.TargetLocatorLocalPodman{
		ContainerID:      "hel-session-1",
		WorkspaceStorage: core.PodmanWorkspaceLocatorVolume{Name: "hel-session-1-workspace"},
	}

	dtOK(t, SaveSessionTo(database, &record))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), "session-1"), record)

	connection := dtOpen(t, database)
	dtExec(t, connection, "UPDATE session_targets SET workspace_storage = NULL WHERE session_id = 'session-1'")
	dtOK(t, connection.Close())
	loaded := dtMust(LoadStateFrom(database))
	locator, ok := dtSessionOf(loaded, "session-1").Target.(core.TargetLocatorLocalPodman)
	if !ok {
		t.Fatal("Podman locator changed kind")
	}
	dtEqual(t, locator.WorkspaceStorage, core.PodmanWorkspaceLocator(core.PodmanWorkspaceLocatorContainerLayer{}))
}

func TestSessionAndHostContainerSizeCommitTogetherAndLatestWins(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, saveSessionWithContainerSizeTo(database, &record, &HostContainerSizeChoice{
		Host: "builder",
		Size: core.HostContainerSize{Cpus: 8, MemoryBytes: 32},
	}))
	dtOK(t, saveSessionWithContainerSizeTo(database, &record, &HostContainerSizeChoice{
		Host: "builder",
		Size: core.HostContainerSize{Cpus: 16, MemoryBytes: 64},
	}))

	loaded := dtMust(LoadStateFrom(database))
	dtEqual(t, *loaded.ContainerSizes.Index("builder"), core.HostContainerSize{Cpus: 16, MemoryBytes: 64})
	connection := dtOpen(t, database)
	dtEqual(t, dtQuery[int64](t, connection, "SELECT count(*) FROM host_container_sizes"), int64(1))
}

func TestLoadingStateDoesNotRestoreAHiddenContextSessionName(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	state := core.NewStateState()
	record := session("session-1", "project-1")
	record.ACPSessionTitle = dtPtr("<mj-project-memory>private and truncated")
	state.Sessions.Insert(record.ID, record)
	dtOK(t, SaveStateTo(database, &state))

	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), "session-1").ACPSessionTitle, (*string)(nil))
}
