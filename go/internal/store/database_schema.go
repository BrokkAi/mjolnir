// Port of mj-store/src/database/schema.rs.

package store

import (
	"context"
	"database/sql"
	_ "embed"
	"errors"
	"fmt"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/core"

	// The pure-Go SQLite driver, registered as "sqlite".
	_ "modernc.org/sqlite"
)

// --- rusqlite's Connection and Transaction on database/sql ---
//
// Rust's store runs on rusqlite, whose Connection is one SQLite connection:
// pragmas, the busy timeout and the transaction behaviour belong to it, and
// a statement run on it while a transaction is open runs inside that
// transaction. A *sql.DB is a pool, so the Go store wraps one pinned
// *sql.Conn instead. Every function of the Rust store that takes
// `&Connection` and begins a transaction takes *Connection; one that only
// runs statements takes a Queryer, which *Connection, *Transaction and the
// readers openReader returns all satisfy, as Rust's Transaction and Reader
// deref to Connection.

// Connection is one SQLite connection, rusqlite's Connection. Its methods
// have the shape of *sql.DB's, without contexts, and all run on the same
// underlying connection.
type Connection struct {
	db   *sql.DB
	conn *sql.Conn
	// immediate is rusqlite's transaction behaviour for Begin: true for
	// TransactionBehavior::Immediate, false for the Deferred default.
	immediate bool
}

// Transaction is rusqlite's Transaction. Dropping a Rust transaction rolls
// it back; Go callers defer Rollback, which does nothing after Commit.
type Transaction = sql.Tx

// Queryer is what a function that takes Rust's `&Connection` needs when it
// only runs statements: *Connection, *Transaction and the store's readers
// all provide it.
type Queryer interface {
	Exec(query string, args ...any) (sql.Result, error)
	Query(query string, args ...any) (*sql.Rows, error)
	QueryRow(query string, args ...any) *sql.Row
	Prepare(query string) (*sql.Stmt, error)
}

// TransactionBehavior is rusqlite's TransactionBehavior: how a transaction
// begins.
type TransactionBehavior int

const (
	// TransactionDeferred begins with a plain BEGIN, SQLite's default.
	TransactionDeferred TransactionBehavior = iota
	// TransactionImmediate begins with BEGIN IMMEDIATE, taking the write
	// lock at once, where the busy handler applies.
	TransactionImmediate
)

// OpenConnection is rusqlite's Connection::open: path read-write, created
// when missing, with SQLite's defaults (deferred transactions, no busy
// timeout).
func OpenConnection(path string) (*Connection, error) {
	return openConnectionWith(path, "mode=rwc")
}

// openConnectionWith opens path on one pinned connection. query holds
// SQLite URI parameters (mode=ro, mode=rwc). Every connection is opened with
// modernc.org/sqlite's `_txlock=immediate`, under which a transaction begun
// with sql.TxOptions{ReadOnly: true} still issues a plain BEGIN, so Begin
// and BeginWith choose the behaviour per transaction as rusqlite does.
func openConnectionWith(path string, query string) (*Connection, error) {
	db, err := sql.Open("sqlite", sqliteURI(path, query+"&_txlock=immediate"))
	if err != nil {
		return nil, err
	}
	db.SetMaxOpenConns(1)
	conn, err := db.Conn(context.Background())
	if err != nil {
		db.Close()
		return nil, err
	}
	return &Connection{db: db, conn: conn}, nil
}

// sqliteURI is a `file:` URI naming path, with query appended, that SQLite
// reads back as path whatever bytes it holds.
func sqliteURI(path string, query string) string {
	slashed := filepath.ToSlash(path)
	if filepath.VolumeName(path) != "" && !strings.HasPrefix(slashed, "/") {
		slashed = "/" + slashed
	}
	var uri strings.Builder
	uri.WriteString("file:")
	for i := 0; i < len(slashed); i++ {
		b := slashed[i]
		switch {
		case 'A' <= b && b <= 'Z', 'a' <= b && b <= 'z', '0' <= b && b <= '9',
			b == '-', b == '_', b == '.', b == '~', b == '/', b == ':':
			uri.WriteByte(b)
		default:
			fmt.Fprintf(&uri, "%%%02X", b)
		}
	}
	uri.WriteByte('?')
	uri.WriteString(query)
	return uri.String()
}

// Exec runs one or more statements without results: rusqlite's execute and
// execute_batch.
func (c *Connection) Exec(query string, args ...any) (sql.Result, error) {
	return c.conn.ExecContext(context.Background(), query, args...)
}

// Query runs a statement returning rows.
func (c *Connection) Query(query string, args ...any) (*sql.Rows, error) {
	return c.conn.QueryContext(context.Background(), query, args...)
}

// QueryRow runs a statement returning at most one row: rusqlite's
// query_row, whose missing row is sql.ErrNoRows from Scan.
func (c *Connection) QueryRow(query string, args ...any) *sql.Row {
	return c.conn.QueryRowContext(context.Background(), query, args...)
}

// Prepare compiles a statement on this connection.
func (c *Connection) Prepare(query string) (*sql.Stmt, error) {
	return c.conn.PrepareContext(context.Background(), query)
}

// Begin starts a transaction with the connection's behaviour: rusqlite's
// transaction and unchecked_transaction.
func (c *Connection) Begin() (*Transaction, error) {
	behavior := TransactionDeferred
	if c.immediate {
		behavior = TransactionImmediate
	}
	return c.BeginWith(behavior)
}

// BeginWith starts a transaction with behavior: rusqlite's
// transaction_with_behavior.
func (c *Connection) BeginWith(behavior TransactionBehavior) (*Transaction, error) {
	return c.conn.BeginTx(context.Background(), &sql.TxOptions{ReadOnly: behavior == TransactionDeferred})
}

// SetTransactionBehavior is rusqlite's set_transaction_behavior: the
// behaviour of later Begin calls.
func (c *Connection) SetTransactionBehavior(behavior TransactionBehavior) {
	c.immediate = behavior == TransactionImmediate
}

// Raw runs f with the driver's connection, for modernc.org/sqlite features
// database/sql does not expose.
func (c *Connection) Raw(f func(driverConn any) error) error {
	return c.conn.Raw(f)
}

// Close closes the connection. Every transaction and result set begun on it
// must be finished first.
func (c *Connection) Close() error {
	return errors.Join(c.conn.Close(), c.db.Close())
}

// isAutocommit is rusqlite's is_autocommit: whether the connection is
// outside any transaction. database/sql does not expose
// sqlite3_get_autocommit, so it asks SQLite: BEGIN fails inside a
// transaction and is rolled back at once outside one.
func (c *Connection) isAutocommit() bool {
	if _, err := c.Exec("BEGIN"); err != nil {
		return false
	}
	_, err := c.Exec("ROLLBACK")
	return err == nil
}

// --- schema.rs ---

const compatibilityMetadataVersion int64 = 30

func migrateOrchestratorPolicies(transaction *Transaction) error {
	panic("unported: mj-store/src/database/schema.rs migrate_orchestrator_policies")
}

type schemaState struct {
	revision          int64
	minimumCompatible *int64
}

func (s schemaState) ensureSupported() error {
	return s.ensureSupportedBy(schemaVersion)
}

func (s schemaState) ensureSupportedBy(supported int64) error {
	var reason core.StoreSchemaMismatchReason
	switch {
	case s.revision < supported:
		reason = core.StoreSchemaMismatchReasonNeedsMigration{}
	case s.minimumCompatible != nil:
		if *s.minimumCompatible <= supported {
			return nil
		}
		reason = core.StoreSchemaMismatchReasonIncompatible{MinimumCompatible: *s.minimumCompatible}
	default:
		reason = core.StoreSchemaMismatchReasonInvalidCompatibilityMetadata{}
	}
	return core.StoreSchemaMismatch{Found: s.revision, Supported: supported, Reason: reason}
}

// readSchemaState reads the revision, ledger, and compatibility floor, which
// must describe one snapshot. A missing floor is only legitimate before
// compatibility was introduced.
func readSchemaState(connection *Connection) (schemaState, error) {
	panic("unported: mj-store/src/database/schema.rs read_schema_state")
}

func DatabasePath() string {
	return filepath.Join(core.DataDir(), "mj.sqlite3")
}

// CheckReadCompatibility verifies that this client can read the daemon's
// store without creating or migrating it. Startup must pass this gate before
// handing out a connection, even when an older daemon happens to speak the
// same wire protocol.
func CheckReadCompatibility() error {
	reader, err := openReaderStrict(DatabasePath())
	if err != nil {
		return err
	}
	reader.Close()
	return nil
}

// openWriter is a writer-capable connection whose transactions take the WAL
// write lock at `BEGIN`, where the busy handler applies. A DEFERRED
// transaction that has already read cannot wait: SQLite only calls the busy
// handler when the connection holds no transaction, so the upgrade to a
// write returns `SQLITE_BUSY` at once (issue 1117).
func openWriter(path string) (*Connection, error) {
	connection, err := openWritable(path)
	if err != nil {
		return nil, err
	}
	connection.SetTransactionBehavior(TransactionImmediate)
	return connection, nil
}

func openWritable(path string) (*Connection, error) {
	panic("unported: mj-store/src/database/schema.rs open_writable")
}

func open(path string) (*Connection, error) {
	return openWriter(path)
}

// openReader opens an existing database without permitting schema or data
// mutation. Client processes use this path so an accidental write fails
// locally instead of competing with the daemon's writer. Callers Close the
// reader when done, as Rust drops it.
func openReader(path string) (*reader, error) {
	// Rust's `cfg!(test)`: the store's own tests enable the fixtures.
	if core.TestFixturesEnabled() {
		// Migration fixtures intentionally read older or uninitialized
		// schemas. The unpooled connection keeps SQLite's DEFERRED default.
		connection, err := openWritable(path)
		if err != nil {
			return nil, err
		}
		return &reader{Connection: connection}, nil
	}
	return openReaderStrict(path)
}

// idleReadersPerStore is the idle read-only connections kept per database
// file. Each new connection parses the whole schema on its first query,
// which cost about 8 ms of CPU; a daemon serving many readers spent most of
// its time doing that.
const idleReadersPerStore = 8

// storeIdentity is which file a connection was opened on. A pooled
// connection is handed out again only while the path still names that file,
// so a store that was removed and created again under the same path is never
// read through a connection to the old one. The idle connection keeps the
// old file open, so a new file cannot be given its inode. Elsewhere than
// Unix, SQLite's own sharing mode keeps an open store from being replaced, so
// the path alone identifies it and both fields stay zero.
type storeIdentity struct {
	device uint64
	inode  uint64
}

type idleReader struct {
	connection *Connection
	identity   storeIdentity
}

// idleReaders is the one owner of this process's read-only store
// connections.
var idleReaders struct {
	sync.Mutex
	byPath map[string][]idleReader
}

// openedReaders counts the read-only connections this process opened rather
// than reused. Rust keeps it per test thread; Go tests that read it must not
// run in parallel.
var openedReaders atomic.Uint64

// reader is a read-only store connection. Close returns the connection to
// the idle set for its store, unless it is still inside a transaction.
type reader struct {
	*Connection
	home *readerHome
}

type readerHome struct {
	path     string
	identity storeIdentity
}

func (r *reader) String() string {
	if r.home == nil {
		return "Reader { home: None, .. }"
	}
	return fmt.Sprintf("Reader { home: Some((%q, %+v)), .. }", r.home.path, r.home.identity)
}

// Close is Rust's Drop for Reader: it returns the connection to its store's
// idle set, or closes it when it has no home, is inside a transaction, or
// the set is full. The reader is unusable afterwards.
func (r *reader) Close() {
	connection, home := r.Connection, r.home
	r.Connection, r.home = nil, nil
	if connection == nil {
		return
	}
	if home == nil || !connection.isAutocommit() {
		_ = connection.Close()
		return
	}
	idleReaders.Lock()
	defer idleReaders.Unlock()
	readers := idleReaders.byPath[home.path]
	if len(readers) >= idleReadersPerStore {
		_ = connection.Close()
		return
	}
	if idleReaders.byPath == nil {
		idleReaders.byPath = map[string][]idleReader{}
	}
	idleReaders.byPath[home.path] = append(readers, idleReader{connection: connection, identity: home.identity})
}

// openReaderStrict is a read-only connection to path, reused when one is
// idle. Every checkout checks the store's schema compatibility again, as a
// new connection did, so a store that another build migrated is refused just
// the same. On a connection that has already parsed the schema that check is
// a few small queries; SQLite parses the schema again itself if it changed.
func openReaderStrict(path string) (*reader, error) {
	panic("unported: mj-store/src/database/schema.rs open_reader_strict")
}

// verifiedSchemas is the databases this process has already migrated. A
// controller owns its store exclusively (`ControllerStoreGuard`), so a
// schema verified once stays verified and later connections skip the
// migration probes entirely.
var verifiedSchemas struct {
	sync.Mutex
	paths map[string]struct{}
}

// schemaCacheKey is the stable cache identity for a database. The file
// itself may not exist yet, so the canonicalized parent directory carries
// the identity.
func schemaCacheKey(path string) string {
	parent, name := filepath.Split(path)
	if parent == "" || name == "" || name == ".." {
		return path
	}
	canonical, err := filepath.Abs(parent)
	if err == nil {
		canonical, err = filepath.EvalSymlinks(canonical)
	}
	if err != nil {
		return path
	}
	return filepath.Join(canonical, name)
}

// verifySchemaOnce runs the migration ladder the first time this process
// opens a database. Later opens confirm compatibility without repeating
// schema repairs. A database behind this build is migrated again, so a
// recreated file under a reused path still converges. Compatible future
// stores are never repaired.
func verifySchemaOnce(path string, connection *Connection) error {
	panic("unported: mj-store/src/database/schema.rs verify_schema_once")
}

// migrateOriginDeltaArchives: new stores start at the revision Mjolnir 2.7.2
// shipped. Existing stores retain the complete migration path, even when
// users skip many releases.
func migrateOriginDeltaArchives(connection *Connection) error {
	_, err := connection.Exec(`BEGIN IMMEDIATE;
        UPDATE schema_compatibility SET minimum_compatible_version = 144 WHERE singleton = 1;
        INSERT INTO schema_migrations(version, applied_at) VALUES (144, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
        PRAGMA user_version = 144;
        COMMIT;`)
	return err
}

const baselineSchemaVersion int64 = 33

// baselineMinimumCompatibleVersion is the compatibility floor a baseline
// store records. Migration 32 (ZCode) was the last breaking change before the
// baseline.
const baselineMinimumCompatibleVersion int64 = 32

// accountingMigrationSQL is the data changes from the original accounting
// revision 67, also needed when upgrading the independently published
// project-catalog revision 67.
const accountingMigrationSQL = `
            ALTER TABLE session_turn_usage RENAME TO old_session_turn_usage;
            CREATE TABLE session_turn_usage (
                session_id TEXT NOT NULL REFERENCES session_contexts(session_id),
                command_id TEXT NOT NULL,
                completed_ordinal INTEGER NOT NULL,
                turn_start_position INTEGER,
                body TEXT NOT NULL,
                PRIMARY KEY(session_id, command_id)
            );
            INSERT INTO session_turn_usage SELECT * FROM old_session_turn_usage;
            DROP TABLE old_session_turn_usage;
            CREATE INDEX session_turn_usage_order ON session_turn_usage(session_id, completed_ordinal);
            ALTER TABLE session_provider_cost RENAME TO old_session_provider_cost;
            CREATE TABLE session_provider_cost (
                session_id TEXT PRIMARY KEY REFERENCES session_contexts(session_id),
                body TEXT NOT NULL
            );
            INSERT INTO session_provider_cost SELECT * FROM old_session_provider_cost;
            DROP TABLE old_session_provider_cost;
            CREATE TABLE subagent_accounting (
                child_session_id TEXT PRIMARY KEY REFERENCES session_contexts(session_id),
                parent_session_id TEXT NOT NULL REFERENCES session_contexts(session_id),
                task_name TEXT NOT NULL,
                CHECK(child_session_id <> parent_session_id)
            ) STRICT;
            CREATE INDEX subagent_accounting_parent ON subagent_accounting(parent_session_id);
            INSERT INTO subagent_accounting SELECT child_session_id, parent_session_id,
                json_extract(record_json, '$.task_name') FROM subagent_sessions;
            CREATE TABLE session_turn_selections (
                session_id TEXT NOT NULL REFERENCES session_contexts(session_id),
                command_id TEXT NOT NULL,
                model TEXT,
                effort TEXT,
                PRIMARY KEY(session_id, command_id)
            ) STRICT;
`

// The SQL files schema.rs includes with include_str!, copied from
// mj-store/src/database/.
var (
	//go:embed data/database/baseline.sql
	baselineSQL string
	//go:embed data/database/environments.sql
	environmentsSQL string
	//go:embed data/database/web_push.sql
	webPushSQL string
	//go:embed data/database/web_push_entries.sql
	webPushEntriesSQL string
	//go:embed data/database/alert_events.sql
	alertEventsSQL string
)

func migrateSchema(connection *Connection) error {
	panic("unported: mj-store/src/database/schema.rs migrate_schema")
}

func goneSessionRowsSQL(gone string) string {
	panic("unported: mj-store/src/database/schema.rs gone_session_rows_sql")
}

// migrateGithubWatchFollowers is migration 130: a PR watch may have no
// creator, and records its latest lifecycle event.
//
// A watch row now exists for every PR with at least one interested session,
// including PRs another account opened (merge batches). SQLite cannot drop
// `NOT NULL`, so the table is rebuilt with a nullable `creator_session_id`
// and four `lifecycle_*` columns: the most recent lifecycle event's base
// key, serialized `MailboxEventBody` (recipient `Creator`), causing GitHub
// login and timestamp. A session that is classified after the event was
// committed reads them to be told. The session-row triggers name this table,
// and SQLite refuses to rename a table while a trigger names a missing one,
// so they are dropped first and recreated afterwards. Every PR that already
// has an interested session and no watch gets a creator-less one, whose
// cursors start at the second of the migration (GitHub's timestamps have no
// finer precision, and an event in that second must still compare as not
// earlier), so a PR a session was told about before the upgrade reports what
// happens next without replaying its history. Revision 121's rule changes in
// one way: a watch loses its creator, and is deleted only when no live
// interested session remains.
func migrateGithubWatchFollowers(connection *Connection) error {
	panic("unported: mj-store/src/database/schema.rs migrate_github_watch_followers")
}

// migrateNitroTargetKind is migration 124: admit `'ssh-nitro'` in the
// target-kind constraints of `session_targets` and `environment_workers`.
//
// SQLite cannot widen a CHECK in place, so each table is rebuilt. Foreign
// keys are off while it happens: `environments.target_lease_id` references
// `environment_workers` with `ON DELETE SET NULL`, and dropping the old table
// with enforcement on would clear every environment's lease. The revision-98
// and revision-122 definitions are not edited; this rebuild is the new
// revision that supersedes them.
func migrateNitroTargetKind(connection *Connection) error {
	panic("unported: mj-store/src/database/schema.rs migrate_nitro_target_kind")
}

// migrateParkedSessionState is migration 53: rebuild `sessions` so its
// `state` constraint admits `'parked'`. SQLite cannot alter a CHECK
// constraint, so the table is copied into one declared with the new
// constraint, the way migration 32 (`migrate_zcode_harness_kind`) did. The
// table's own stored definition is edited, so every column later migrations
// added is kept as it is. A store whose constraint already admits the value
// is only stamped.
func migrateParkedSessionState(connection *Connection) error {
	panic("unported: mj-store/src/database/schema.rs migrate_parked_session_state")
}

// migrateOpencodeHarnessKind is migration 74: admit `'opencode'` in the
// harness-kind constraints of `sessions` and `hidden_native_sessions`, the
// way migrations 27 (Muse) and 32 (ZCode) admitted their kinds. SQLite
// cannot widen a CHECK constraint in place, so each table is rebuilt from
// its stored definition, which keeps every column and trigger later
// migrations added.
//
// Breaking: older daemons skip a session whose harness they do not know when
// loading state, and their next full state save deletes it.
func migrateOpencodeHarnessKind(connection *Connection) error {
	panic("unported: mj-store/src/database/schema.rs migrate_opencode_harness_kind")
}

// migrateMimoHarnessKind is migration 90: admit `'mimo'` in the
// harness-kind constraints of `sessions` and `hidden_native_sessions`.
//
// Breaking: older daemons do not understand the new harness value, and the
// current load/save model can discard a session they cannot represent. The
// compatibility floor therefore rises to this revision.
func migrateMimoHarnessKind(connection *Connection) error {
	panic("unported: mj-store/src/database/schema.rs migrate_mimo_harness_kind")
}

// admitHarnessKind admits added in the harness-kind constraints of
// `sessions` and `hidden_native_sessions`, after previous, the value the
// constraint currently ends with, and records revision as a breaking
// migration.
//
// SQLite cannot alter a CHECK constraint, so each table is rebuilt from its
// stored definition, keeping every shipped column, index and trigger. A
// table that already admits added is left alone.
func admitHarnessKind(connection *Connection, previous string, added string, revision int64) error {
	panic("unported: mj-store/src/database/schema.rs admit_harness_kind")
}

func removeHarnessConstraints(connection *Connection) error {
	panic("unported: mj-store/src/database/schema.rs remove_harness_constraints")
}

// migrateStartupCleanupState rebuilds from the stored definition so every
// shipped column and trigger survives.
func migrateStartupCleanupState(connection *Connection) error {
	panic("unported: mj-store/src/database/schema.rs migrate_startup_cleanup_state")
}

func migrateTurnOutcomeSchema(connection *Connection) error {
	tx, err := connection.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if err := migrateTurnOutcomes(tx); err != nil {
		return err
	}
	if _, err := tx.Exec(`UPDATE schema_compatibility SET minimum_compatible_version = 82 WHERE singleton = 1;
         INSERT INTO schema_migrations(version, applied_at)
             VALUES (82, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
         PRAGMA user_version = 82;`); err != nil {
		return err
	}
	return tx.Commit()
}

// createBaselineSchema creates an empty store at the baseline revision in
// one immediate transaction. The revision is read again under the write
// lock, so a second process that raced to create the same store finds it
// already created.
func createBaselineSchema(connection *Connection) error {
	panic("unported: mj-store/src/database/schema.rs create_baseline_schema")
}
