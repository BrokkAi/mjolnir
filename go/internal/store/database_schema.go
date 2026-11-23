// Port of mj-store/src/database/schema.rs.

package store

import (
	"context"
	"database/sql"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"unicode"

	// The pure-Go SQLite driver, registered as "sqlite".
	"modernc.org/sqlite"
	sqlite3 "modernc.org/sqlite/lib"

	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	for _, step := range [][2]string{
		{
			`SELECT session_id, subagents FROM sessions WHERE supervisors = 1 AND subagents IS NOT NULL`,
			`UPDATE sessions SET subagents = ?2 WHERE session_id = ?1`,
		},
		{
			`SELECT p.profile_id, p.policy FROM subagent_preference p
             JOIN supervisor_preference s ON s.profile_id = p.profile_id WHERE s.enabled = 1`,
			`UPDATE subagent_preference SET policy = ?2 WHERE profile_id = ?1`,
		},
	} {
		selectSQL, updateSQL := step[0], step[1]
		rows, err := transaction.Query(selectSQL)
		if err != nil {
			return err
		}
		type stored struct{ id, json string }
		var policies []stored
		for rows.Next() {
			var row stored
			if err := rows.Scan(&row.id, &row.json); err != nil {
				rows.Close()
				return err
			}
			policies = append(policies, row)
		}
		if err := errors.Join(rows.Err(), rows.Close()); err != nil {
			return err
		}
		for _, row := range policies {
			policy, err := core.ParseStoredPolicy(row.json)
			if err != nil {
				return fmt.Errorf("read sub-agent policy for %s: %w", row.id, err)
			}
			if policy.Mode == core.SubagentPolicyMjolnir {
				policy.Orchestrators = true
				encoded, err := eventsSerdeText(json.Marshal(policy))
				if err != nil {
					return err
				}
				if _, err := transaction.Exec(updateSQL, row.id, encoded); err != nil {
					return err
				}
			}
		}
	}
	return nil
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

// schemaMismatchCause is anyhow's `error.context(mismatch)`: the typed
// mismatch is what the error says and what errors.As finds, and the SQLite
// failure that revealed it stays in the chain.
type schemaMismatchCause struct {
	mismatch core.StoreSchemaMismatch
	cause    error
}

func (e schemaMismatchCause) Error() string {
	return e.mismatch.Error() + ": " + e.cause.Error()
}

func (e schemaMismatchCause) Unwrap() []error {
	return []error{e.mismatch, e.cause}
}

// schemaFailureIsStructural is whether reading the compatibility metadata
// failed because of the schema itself. Missing tables/columns and invalid
// field types are structural: rusqlite reports the first as SQLite's
// generic error code (its ErrorCode::Unknown) and the second without any
// SQLite code. Busy, I/O, and interruption errors are not evidence of an
// incompatible migration.
func schemaFailureIsStructural(err error) bool {
	var failure *sqlite.Error
	if !errors.As(err, &failure) {
		return true
	}
	switch failure.Code() & 0xff {
	case sqlite3.SQLITE_INTERNAL, sqlite3.SQLITE_PERM, sqlite3.SQLITE_ABORT,
		sqlite3.SQLITE_BUSY, sqlite3.SQLITE_LOCKED, sqlite3.SQLITE_NOMEM,
		sqlite3.SQLITE_READONLY, sqlite3.SQLITE_INTERRUPT, sqlite3.SQLITE_IOERR,
		sqlite3.SQLITE_CORRUPT, sqlite3.SQLITE_NOTFOUND, sqlite3.SQLITE_FULL,
		sqlite3.SQLITE_CANTOPEN, sqlite3.SQLITE_PROTOCOL, sqlite3.SQLITE_SCHEMA,
		sqlite3.SQLITE_TOOBIG, sqlite3.SQLITE_CONSTRAINT, sqlite3.SQLITE_MISMATCH,
		sqlite3.SQLITE_MISUSE, sqlite3.SQLITE_NOLFS, sqlite3.SQLITE_AUTH,
		sqlite3.SQLITE_RANGE, sqlite3.SQLITE_NOTADB:
		return false
	}
	return true
}

// schemaOptionalInteger is rusqlite's Option<i64> read: NULL or an INTEGER,
// and an invalid column type for anything else.
func schemaOptionalInteger(value any) (*int64, error) {
	switch v := value.(type) {
	case nil:
		return nil, nil
	case int64:
		return &v, nil
	}
	return nil, fmt.Errorf("invalid column type %T for an integer", value)
}

// readSchemaState reads the revision, ledger, and compatibility floor, which
// must describe one snapshot. A missing floor is only legitimate before
// compatibility was introduced.
func readSchemaState(connection *Connection) (schemaState, error) {
	snapshot, err := connection.Begin()
	if err != nil {
		return schemaState{}, fmt.Errorf("start database compatibility snapshot: %w", err)
	}
	defer snapshot.Rollback()
	var revision int64
	if err := snapshot.QueryRow("PRAGMA user_version").Scan(&revision); err != nil {
		return schemaState{}, fmt.Errorf("read database migration revision: %w", err)
	}
	var minimumCompatible *int64
	if revision >= compatibilityMetadataVersion {
		invalid := core.StoreSchemaMismatch{
			Found:     revision,
			Supported: schemaVersion,
			Reason:    core.StoreSchemaMismatchReasonInvalidCompatibilityMetadata{},
		}
		var count int64
		var singleton, floor, recorded *int64
		err := func() error {
			var rawSingleton, rawFloor, rawRecorded any
			if err := snapshot.QueryRow(`SELECT count(*), min(singleton), min(minimum_compatible_version),
                    (SELECT max(version) FROM schema_migrations)
             FROM schema_compatibility`).Scan(&count, &rawSingleton, &rawFloor, &rawRecorded); err != nil {
				return err
			}
			var err error
			if singleton, err = schemaOptionalInteger(rawSingleton); err != nil {
				return err
			}
			if floor, err = schemaOptionalInteger(rawFloor); err != nil {
				return err
			}
			recorded, err = schemaOptionalInteger(rawRecorded)
			return err
		}()
		if err != nil {
			// Missing tables/columns and invalid field types are
			// structural. Busy, I/O, and interruption errors are not
			// evidence of an incompatible migration.
			if schemaFailureIsStructural(err) {
				return schemaState{}, schemaMismatchCause{mismatch: invalid, cause: err}
			}
			return schemaState{}, fmt.Errorf("read database compatibility metadata: %w", err)
		}
		if count != 1 ||
			singleton == nil || *singleton != 1 ||
			recorded == nil || *recorded != revision ||
			floor == nil || *floor < compatibilityMetadataVersion || *floor > revision {
			return schemaState{}, invalid
		}
		minimumCompatible = floor
	}
	if err := snapshot.Commit(); err != nil {
		return schemaState{}, fmt.Errorf("finish database compatibility snapshot: %w", err)
	}
	return schemaState{revision: revision, minimumCompatible: minimumCompatible}, nil
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

// schemaPathParent is Rust's Path::parent: the path without its last
// component, "" for a bare file name, and false for a root or empty path.
func schemaPathParent(path string) (string, bool) {
	trimmed := strings.TrimRight(path, string(filepath.Separator)+"/")
	volume := filepath.VolumeName(path)
	if trimmed == "" || trimmed == volume {
		return "", false
	}
	directory, _ := filepath.Split(trimmed)
	if directory == "" {
		return "", true
	}
	return filepath.Clean(directory), true
}

// storeBusyTimeout is rusqlite's busy_timeout(Duration::from_secs(5)).
const storeBusyTimeout = "PRAGMA busy_timeout = 5000;"

func openWritable(path string) (*Connection, error) {
	// A writable open migrates the store, so this is where the decision
	// belongs: test binaries reach it without the controller lock.
	if store, ok := schemaPathParent(path); ok {
		if err := core.EnsureMayControlStore(store, "open this database for writing"); err != nil {
			return nil, err
		}
		if store != "" {
			if err := os.MkdirAll(store, 0o777); err != nil {
				return nil, fmt.Errorf("create Mjolnir data directory %s: %w", store, err)
			}
		}
	}
	connection, err := OpenConnection(path)
	if err != nil {
		return nil, fmt.Errorf("open Mjolnir database %s: %w", path, err)
	}
	if err := func() error {
		if _, err := connection.Exec(storeBusyTimeout); err != nil {
			return err
		}
		if _, err := connection.Exec(`PRAGMA foreign_keys = ON;
         PRAGMA synchronous = FULL;`); err != nil {
			return err
		}
		if err := verifySchemaOnce(path, connection); err != nil {
			return err
		}
		return observeConnection(connection, path)
	}(); err != nil {
		_ = connection.Close()
		return nil, err
	}
	return connection, nil
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

// reuseIdleReader takes an idle connection to the file path names now, and
// closes the ones to a file it no longer names.
func reuseIdleReader(path string, identity storeIdentity) *Connection {
	idleReaders.Lock()
	defer idleReaders.Unlock()
	readers, ok := idleReaders.byPath[path]
	if !ok {
		return nil
	}
	// A connection to a file the path no longer names is dropped here.
	kept := readers[:0]
	for _, idle := range readers {
		if idle.identity == identity {
			kept = append(kept, idle)
		} else {
			_ = idle.connection.Close()
		}
	}
	if len(kept) == 0 {
		idleReaders.byPath[path] = kept
		return nil
	}
	reused := kept[len(kept)-1].connection
	idleReaders.byPath[path] = kept[:len(kept)-1]
	return reused
}

// openReaderStrict is a read-only connection to path, reused when one is
// idle. Every checkout checks the store's schema compatibility again, as a
// new connection did, so a store that another build migrated is refused just
// the same. On a connection that has already parsed the schema that check is
// a few small queries; SQLite parses the schema again itself if it changed.
func openReaderStrict(path string) (*reader, error) {
	identity, known := storeIdentityOf(path)
	var connection *Connection
	if known {
		connection = reuseIdleReader(path, identity)
	}
	if connection == nil {
		opened, err := openConnectionWith(path, "mode=ro")
		if err != nil {
			return nil, fmt.Errorf("open Mjolnir database read-only %s: %w", path, err)
		}
		openedReaders.Add(1)
		if _, err := opened.Exec(storeBusyTimeout); err != nil {
			_ = opened.Close()
			return nil, err
		}
		if _, err := opened.Exec(`PRAGMA foreign_keys = ON;
                 PRAGMA query_only = ON;`); err != nil {
			_ = opened.Close()
			return nil, err
		}
		connection = opened
	}
	state, err := readSchemaState(connection)
	if err == nil {
		err = state.ensureSupported()
	}
	if err != nil {
		_ = connection.Close()
		return nil, err
	}
	reader := &reader{Connection: connection}
	if known {
		reader.home = &readerHome{path: path, identity: identity}
	}
	return reader, nil
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
	parent, ok := schemaPathParent(path)
	name := filepath.Base(path)
	if !ok || parent == "" || name == ".." || name == "." || name == string(filepath.Separator) {
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
	key := schemaCacheKey(path)
	verifiedSchemas.Lock()
	defer verifiedSchemas.Unlock()
	// Converting a fresh file to WAL fails at once with SQLITE_BUSY, without
	// the busy handler, while another connection is migrating it. This lock
	// is what keeps first opens from overlapping, so the conversion belongs
	// under it. Once the file is WAL the pragma only reads the mode.
	if _, err := connection.Exec("PRAGMA journal_mode = WAL;"); err != nil {
		return err
	}
	state, err := readSchemaState(connection)
	if err != nil {
		return err
	}
	_, verified := verifiedSchemas.paths[key]
	if state.revision > schemaVersion || (state.revision == schemaVersion && verified) {
		// An older build must never run its repairs against a newer schema.
		return state.ensureSupported()
	}
	// Holding the lock across the ladder keeps two first opens of the same
	// database from running the additive migration steps against each other.
	if err := migrateSchema(connection); err != nil {
		return err
	}
	state, err = readSchemaState(connection)
	if err != nil {
		return err
	}
	if err := state.ensureSupported(); err != nil {
		return err
	}
	if verifiedSchemas.paths == nil {
		verifiedSchemas.paths = map[string]struct{}{}
	}
	verifiedSchemas.paths[key] = struct{}{}
	return nil
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

// schemaRowExists is rusqlite's Statement::exists: whether query returns a
// row.
func schemaRowExists(connection Queryer, query string, args ...any) (bool, error) {
	rows, err := connection.Query(query, args...)
	if err != nil {
		return false, err
	}
	found := rows.Next()
	return found, errors.Join(rows.Err(), rows.Close())
}

// schemaObjects is the stored SQL of the indexes and triggers query selects.
func schemaObjects(transaction *Transaction, query string, args ...any) ([]string, error) {
	rows, err := transaction.Query(query, args...)
	if err != nil {
		return nil, err
	}
	var objects []string
	for rows.Next() {
		var object string
		if err := rows.Scan(&object); err != nil {
			rows.Close()
			return nil, err
		}
		objects = append(objects, object)
	}
	return objects, errors.Join(rows.Err(), rows.Close())
}

func migrateSchema(connection *Connection) error {
	state, err := readSchemaState(connection)
	if err != nil {
		return err
	}
	version := state.revision
	if version > schemaVersion {
		return state.ensureSupported()
	}
	if version == 0 {
		if err := createBaselineSchema(connection); err != nil {
			return err
		}
	} else if version < baselineSchemaVersion {
		if err := migrateToBaseline(connection); err != nil {
			return fmt.Errorf("upgrade historical database schema: %w", err)
		}
	}
	// Compatible: adds one table. Older readers ignore it and treat read-write
	// mounts as copy-on-write, a behaviour difference rather than lost data.
	// Older writers rewrite `session_mounts` but never touch this table, so its
	// rows survive their updates; a row only applies while a mount with the same
	// source and destination is still not read-only, so an older build that
	// makes the mount read-only or removes it keeps that choice. The
	// compatibility floor stays where it is.
	if version < 34 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS session_mount_access (
                 session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                 source BLOB NOT NULL,
                 destination BLOB NOT NULL,
                 access TEXT NOT NULL CHECK(access IN ('rw')),
                 PRIMARY KEY(session_id, destination)
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (34, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 34;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Compatible: adds one nullable column. Older readers ignore it, and the
	// older writer's session upsert lists columns explicitly, so it preserves
	// the value. An older executable launching such a session uses the shared
	// `/workspace` instead of the recorded per-session path, which is a
	// behaviour difference, not data loss. The compatibility floor stays where
	// it is.
	if version < 35 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN container_workspace TEXT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (35, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 35;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Compatible: adds one nullable column. Older readers ignore it, and the
	// older writer's session upsert lists columns explicitly, so it preserves
	// the value. An older executable launching such a session runs it without
	// the mbx build cache, which is a behaviour difference, not data loss. The
	// compatibility floor stays where it is.
	if version < 36 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN build_cache_json TEXT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (36, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 36;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Compatible: adds one nullable column to `session_targets`. Only a
	// container sub-agent child row ever carries a value, and older builds
	// could never start such a child, so an older update that rewrites the row
	// without the column loses nothing usable. Older readers ignore it. The
	// compatibility floor stays where it is.
	if version < 37 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE session_targets ADD COLUMN borrowed_from TEXT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (37, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 37;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Compatible: adds one table holding the dashboard's conversation pane
	// arrangement per workspace. Older readers never select from it and older
	// writers never touch it, so their updates leave its rows intact; a
	// workspace deleted by an older build still removes them through the
	// foreign key. Losing the table only means the conversation area opens as
	// a single pane. The compatibility floor stays where it is.
	if version < 38 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS workspace_layouts (
                 workspace_id TEXT PRIMARY KEY REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
                 layout TEXT NOT NULL
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (38, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 38;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: persisted input_required API events can now omit the structured
	// request. Older readers require it and fail to deserialize the event log;
	// the shared read/write compatibility floor must advance with the revision.
	if version < 39 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 39 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (39, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 39;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: native child projections and relay observations must be preserved
	// by every reader/writer; older builds cannot interpret their lifecycle.
	if version < 40 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE native_agents (
                 owner TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                 child TEXT NOT NULL,
                 staging INTEGER NOT NULL CHECK(staging IN (0,1)),
                 body TEXT NOT NULL CHECK(json_valid(body)),
                 PRIMARY KEY(owner, child, staging)
             ) STRICT;
             CREATE TABLE native_agent_transcript (
                 owner TEXT NOT NULL,
                 child TEXT NOT NULL,
                 staging INTEGER NOT NULL,
                 stable_id TEXT NOT NULL,
                 position INTEGER NOT NULL,
                 body TEXT NOT NULL CHECK(json_valid(body)),
                 PRIMARY KEY(owner, child, staging, stable_id),
                 FOREIGN KEY(owner, child, staging) REFERENCES native_agents(owner, child, staging)
                     ON DELETE CASCADE ON UPDATE CASCADE
             ) STRICT;
             CREATE INDEX native_agent_transcript_position ON native_agent_transcript(owner, child, staging, position);
             CREATE TABLE native_agent_replay (
                 owner TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE
             ) STRICT;
             UPDATE schema_compatibility SET minimum_compatible_version = 40 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (40, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 40;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: clear-context relay commands/outcomes are persisted in event
	// JSON. Older readers cannot decode them or honor the context boundary.
	if version < 41 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 41 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (41, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 41;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older layout writers discard Browse identity and pin badges.
	if version < 42 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 42 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (42, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 42;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: durable steering commands/observations and native availability
	// cannot be interpreted or preserved by older readers and writers.
	if version < 43 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
            UPDATE schema_compatibility SET minimum_compatible_version = 43 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (43, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 43;
            COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: new quota recovery commands in stored relay JSON cannot be
	// read or preserved by older binaries, even though the cache is additive.
	if version < 44 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
            CREATE TABLE quota_reset_cache (identity TEXT PRIMARY KEY, body TEXT NOT NULL);
            UPDATE schema_compatibility SET minimum_compatible_version = 44 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (44, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 44;
            COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: adds one nullable column. Older readers ignore it, and the
	// older writer's session upsert lists columns explicitly, so it preserves
	// the value. An older executable relaunching such a session starts it at
	// HEAD or the remote default branch instead of the recorded revision,
	// which is a behaviour difference, not data loss. The compatibility floor
	// stays where it is.
	if version < 45 {
		// The column is added only when it is absent, the way migration 34
		// creates its table only when absent: a store rolled back to an older
		// revision still carries the column, and a second ALTER would refuse.
		addColumn := ""
		if has, err := tableHasColumn(connection, "sessions", "launch_base"); err != nil {
			return err
		} else if !has {
			addColumn = `ALTER TABLE sessions ADD COLUMN launch_base TEXT;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumn + `
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (45, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 45;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older writers can replace a target without updating its saved
	// connection, leaving access metadata attached to the wrong resource.
	// Refuse both older readers and writers before they operate that target.
	if version < 46 {
		addColumn := ""
		if has, err := tableHasColumn(connection, "sessions", "target_runtime_json"); err != nil {
			return err
		} else if !has {
			addColumn = `ALTER TABLE sessions ADD COLUMN target_runtime_json TEXT;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumn + `
             UPDATE schema_compatibility SET minimum_compatible_version = 46 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (46, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 46;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: the managed checkout JSON can now describe an independent
	// clone. Older readers reject its `kind`, and older writers cannot safely
	// retain that clone's unpublished commits during lifecycle cleanup.
	if version < 47 {
		addBranch := ""
		if has, err := tableHasColumn(connection, "sessions", "launch_branch"); err != nil {
			return err
		} else if !has {
			addBranch = `ALTER TABLE sessions ADD COLUMN launch_branch TEXT;`
		}
		addPublication := ""
		if has, err := tableHasColumn(connection, "sessions", "publication_json"); err != nil {
			return err
		} else if !has {
			addPublication = `ALTER TABLE sessions ADD COLUMN publication_json TEXT;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addBranch + `
             ` + addPublication + `
             UPDATE schema_compatibility SET minimum_compatible_version = 47 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (47, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 47;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: stored relay JSON can now hold the steering-returned outcome,
	// and stored active-turn JSON the prompt a steer continued, which older
	// readers reject and older writers would drop.
	if version < 48 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 48 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (48, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 48;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: a sub-agent record can now say its child has the `handback`
	// tool, and older readers refuse the unknown field. The reports themselves
	// live in their own table, because a full state save rewrites every
	// sub-agent record from whatever copy the saving controller holds.
	if version < 49 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS subagent_handbacks (
                 child_session_id TEXT PRIMARY KEY,
                 handback_command_id TEXT,
                 handback_message TEXT,
                 handback_recorded_at_ms INTEGER,
                 reminder_command_id TEXT,
                 reminder_for_command_id TEXT,
                 reminder_sent_at_ms INTEGER,
                 reminder_failed_for_command_id TEXT
             );
             UPDATE schema_compatibility SET minimum_compatible_version = 49 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (49, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 49;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: a new nullable column that only this build reads. Older
	// readers ignore it, and older writers name their columns, so an update
	// from one keeps it.
	if version < 50 {
		addColumn := ""
		if has, err := tableHasColumn(connection, "subagent_handbacks", "awaited_ordinal"); err != nil {
			return err
		} else if !has {
			addColumn = `ALTER TABLE subagent_handbacks ADD COLUMN awaited_ordinal INTEGER;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumn + `
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (50, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 50;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: the directory on the parent's target where a child writes
	// the details its report points to. A nullable column only this build
	// reads, like `awaited_ordinal`.
	if version < 51 {
		addColumn := ""
		if has, err := tableHasColumn(connection, "subagent_handbacks", "report_dir"); err != nil {
			return err
		} else if !has {
			addColumn = `ALTER TABLE subagent_handbacks ADD COLUMN report_dir TEXT;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumn + `
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (51, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 51;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: adds one table that only this build reads. It lists the
	// sub-agents a parent's suspend stopped until the parent's model has been
	// told. It is not a column of `sessions` because a full state save
	// rewrites every session row from whatever copy the saving controller
	// holds, which could drop the list or bring back one already delivered.
	// Rows go with their parent's session row.
	if version < 52 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS stopped_subagents (
                 parent_session_id TEXT NOT NULL
                     REFERENCES sessions(session_id) ON DELETE CASCADE,
                 child_session_id TEXT NOT NULL,
                 record_json TEXT NOT NULL CHECK(json_valid(record_json)),
                 PRIMARY KEY(parent_session_id, child_session_id)
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (52, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 52;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: a sub-agent whose turn ended can be stored as `parked`. The
	// `sessions.state` constraint has to be rebuilt to admit it, and an older
	// reader panics on a state it does not know, so the floor rises with the
	// revision.
	if version < 53 {
		if err := migrateParkedSessionState(connection); err != nil {
			return err
		}
	}

	// Breaking: older daemons cannot enforce exact checkout preparation on
	// admitted sessions, so they must not recover or provision these records.
	if version < 54 {
		addColumn := ""
		if has, err := tableHasColumn(connection, "sessions", "checkout_json"); err != nil {
			return err
		} else if !has {
			addColumn = `ALTER TABLE sessions ADD COLUMN checkout_json TEXT;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumn + `
             UPDATE schema_compatibility SET minimum_compatible_version = 54 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (54, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 54;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons cannot enforce the runtime constraint or read
	// the new runtime receipt stored in relay/API events.
	if version < 55 {
		addColumn := ""
		if has, err := tableHasColumn(connection, "sessions", "expected_runtime_identity"); err != nil {
			return err
		} else if !has {
			addColumn = `ALTER TABLE sessions ADD COLUMN expected_runtime_identity TEXT;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumn + `
             UPDATE schema_compatibility SET minimum_compatible_version = 55 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (55, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 55;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older readers/writers cannot honor None or fixed model/effort.
	if version < 56 {
		addColumn := ""
		if has, err := tableHasColumn(connection, "sessions", "subagents"); err != nil {
			return err
		} else if !has {
			addColumn = `ALTER TABLE sessions ADD COLUMN subagents TEXT CHECK(subagents IS NULL OR json_valid(subagents));`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumn + `
             UPDATE sessions SET subagents = CASE WHEN mjolnir_subagents = 1
                 THEN '{"mode":"all_models"}' ELSE '{"mode":"native"}' END WHERE subagents IS NULL AND mjolnir_subagents IS NOT NULL;
             CREATE TABLE IF NOT EXISTS subagent_preference (singleton INTEGER PRIMARY KEY CHECK(singleton = 1), policy TEXT NOT NULL CHECK(json_valid(policy)));
             UPDATE schema_compatibility SET minimum_compatible_version = 56 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at) VALUES (56, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 56;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: typed event bodies cannot be decoded by older readers, and
	// pending checkpoint identities must survive every writer's transitions.
	if version < 57 {
		if err := func() error {
			tx, err := connection.Begin()
			if err != nil {
				return err
			}
			defer tx.Rollback()
			if _, err := tx.Exec(`DROP TRIGGER IF EXISTS api_session_error_updated;
            DROP TRIGGER IF EXISTS api_session_error_inserted;
            CREATE TABLE IF NOT EXISTS checkpoint_operations (
                session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                command_id TEXT NOT NULL UNIQUE,
                related_command_ids TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(related_command_ids))
            ) STRICT;`); err != nil {
				return err
			}
			if err := migrateEventOutcomes(tx); err != nil {
				return err
			}
			if _, err := tx.Exec(`UPDATE schema_compatibility SET minimum_compatible_version = 57 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (57, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 57;`); err != nil {
				return err
			}
			return tx.Commit()
		}(); err != nil {
			return err
		}
	}

	// Breaking: relay assessment observations and seeded-context commands are
	// persisted as JSON that older readers cannot decode or preserve.
	if version < 58 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
            UPDATE schema_compatibility SET minimum_compatible_version = 58 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (58, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 58;
            COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: an older daemon cannot resume acknowledged startup delivery
	// and may instead restore or resend its prompt with a different identity.
	if version < 59 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS startup_steps (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                group_id TEXT,
                command_id TEXT NOT NULL UNIQUE,
                step_json TEXT NOT NULL,
                phase TEXT NOT NULL DEFAULT 'pending'
                    CHECK (phase IN ('pending', 'delivering', 'accepted', 'cancelling', 'rejecting', 'done', 'failed', 'dismissed')),
                error TEXT,
                accepted_ordinal INTEGER
            ) STRICT;
            CREATE INDEX IF NOT EXISTS startup_steps_session_sequence
                ON startup_steps(session_id, sequence);
            CREATE INDEX IF NOT EXISTS startup_steps_group_sequence
                ON startup_steps(group_id, sequence) WHERE group_id IS NOT NULL;
            CREATE INDEX IF NOT EXISTS startup_steps_pending
                ON startup_steps(session_id, sequence)
                WHERE phase IN ('pending', 'delivering', 'accepted', 'cancelling', 'rejecting');
            UPDATE schema_compatibility SET minimum_compatible_version = 59 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (59, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 59;
            COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: older dispatchers would resolve a prepared request's target
	// again instead of honoring its persisted effect identity and outcome.
	if version < 60 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS delegation_effects (
                parent_session_id TEXT NOT NULL,
                request_id TEXT NOT NULL,
                phase TEXT NOT NULL,
                prepared_json TEXT,
                result_json TEXT,
                receipt_pending INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(parent_session_id, request_id)
            ) STRICT;
            CREATE INDEX IF NOT EXISTS delegation_effects_receipt_cleanup
                ON delegation_effects(parent_session_id, request_id) WHERE receipt_pending = 1;
            UPDATE schema_compatibility SET minimum_compatible_version = 60 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (60, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 60;
            COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: older review hosts clear active reviews on startup and cannot
	// reconstruct or preserve the durable orchestration and effect outbox.
	if version < 61 {
		addColumn := ""
		if has, err := tableHasColumn(connection, "turn_review_state", "orchestration"); err != nil {
			return err
		} else if !has {
			addColumn = `ALTER TABLE turn_review_state ADD COLUMN orchestration TEXT;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
            ` + addColumn + `
            UPDATE schema_compatibility SET minimum_compatible_version = 61 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (61, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 61;
            COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: older recovery cannot distinguish an accepted worker swap
	// awaiting readiness from a worker that should be recreated or abandoned.
	if version < 62 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS worker_restart_intents (
                session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                operation_id TEXT NOT NULL,
                target_json TEXT NOT NULL,
                desired_build TEXT NOT NULL,
                phase TEXT NOT NULL CHECK(phase IN ('prepared', 'swapping', 'awaiting_readiness'))
            ) STRICT;
            UPDATE schema_compatibility SET minimum_compatible_version = 62 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (62, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 62;
            COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older delegation executors do not fence delayed lifecycle
	// effects against the durable incarnation they originally selected.
	if version < 63 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS session_incarnations (
                session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                identity TEXT NOT NULL
            ) STRICT;
            INSERT OR IGNORE INTO session_incarnations(session_id, identity)
                SELECT session_id, lower(hex(randomblob(16))) FROM sessions;
            CREATE TRIGGER IF NOT EXISTS session_incarnation_insert
                AFTER INSERT ON sessions BEGIN
                    INSERT INTO session_incarnations(session_id, identity)
                    VALUES(NEW.session_id, lower(hex(randomblob(16))));
                END;
            CREATE TRIGGER IF NOT EXISTS session_incarnation_resume
                AFTER UPDATE OF state ON sessions
                WHEN (NEW.state = 'provisioning' AND OLD.state <> 'provisioning')
                  OR (NEW.state = 'running' AND OLD.state IN
                      ('stopped', 'parked', 'error', 'lost', 'destroyed-with-data-loss'))
                BEGIN
                    UPDATE session_incarnations SET identity = lower(hex(randomblob(16)))
                    WHERE session_id = NEW.session_id;
                END;
            UPDATE schema_compatibility SET minimum_compatible_version = 63 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (63, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 63;
            COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: Move-only handoffs do not contain workspace backups. Older
	// daemons can mistake a retained source for a target safe to destroy.
	if version < 64 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS retained_move_sources (
                operation_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                source_json TEXT NOT NULL,
                exclusions_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            ) STRICT;
            UPDATE schema_compatibility SET minimum_compatible_version = 64 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (64, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 64;
            COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: removes the durable review orchestration and the execution
	// incarnation fence that migrations 61 and 63 introduced. Reviews running
	// when the daemon stops are cancelled and re-covered by the next review,
	// and a sub-agent close is admitted once per request instead of being
	// fenced against a resumed child. An older daemon would still write both.
	if version < 65 {
		dropColumn := ""
		if has, err := tableHasColumn(connection, "turn_review_state", "orchestration"); err != nil {
			return err
		} else if has {
			dropColumn = `ALTER TABLE turn_review_state DROP COLUMN orchestration;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
            DROP TRIGGER IF EXISTS session_incarnation_insert;
            DROP TRIGGER IF EXISTS session_incarnation_resume;
            DROP TABLE IF EXISTS session_incarnations;
            ` + dropColumn + `
            UPDATE schema_compatibility SET minimum_compatible_version = 65 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (65, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 65;
            COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: removes the saved runtime constraint; older daemons still
	// read and write it. Historical receipts remain readable in event history.
	if version < 66 {
		dropColumn := ""
		if has, err := tableHasColumn(connection, "sessions", "expected_runtime_identity"); err != nil {
			return err
		} else if has {
			dropColumn = `ALTER TABLE sessions DROP COLUMN expected_runtime_identity;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
            ` + dropColumn + `
            UPDATE schema_compatibility SET minimum_compatible_version = 66 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (66, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 66;
            COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: accounting is owned by durable session identities. Older
	// projection writers cannot capture selections or preserve tree identity.
	if version < 67 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
            ` + accountingMigrationSQL + `
            UPDATE schema_compatibility SET minimum_compatible_version = 67 WHERE singleton = 1;
            INSERT INTO schema_migrations(version, applied_at) VALUES (67, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            PRAGMA user_version = 67;
            COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons cannot decode or resume startup teardown, and
	// would release capacity or provision over a surviving failed worker.
	if version < 68 {
		if err := migrateStartupCleanupState(connection); err != nil {
			return err
		}
	}

	// Breaking: revision 67 existed in two branches (accounting or projects).
	// Reconcile both histories; neither older writer can preserve the union.
	if version < 69 {
		hasAccounting, err := schemaRowExists(connection, `SELECT 1 FROM sqlite_schema WHERE type='table' AND name='subagent_accounting'`)
		if err != nil {
			return err
		}
		accounting := accountingMigrationSQL
		if hasAccounting {
			accounting = ""
		}
		addSnapshot := ""
		if has, err := tableHasColumn(connection, "sessions", "project_json"); err != nil {
			return err
		} else if !has {
			addSnapshot = `ALTER TABLE sessions ADD COLUMN project_json TEXT CHECK(project_json IS NULL OR json_valid(project_json));`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
            ` + accounting + `
            ` + addSnapshot + `
            CREATE TABLE IF NOT EXISTS project_catalog (
                bundle_id TEXT PRIMARY KEY,
                project_key TEXT NOT NULL UNIQUE,
                snapshot_json TEXT NOT NULL CHECK(json_valid(snapshot_json)),
                hidden INTEGER NOT NULL DEFAULT 0 CHECK(hidden IN (0,1))
            ) STRICT;
            CREATE TABLE IF NOT EXISTS project_aliases (
                bundle_id TEXT PRIMARY KEY,
                canonical_id TEXT NOT NULL REFERENCES project_catalog(bundle_id),
                snapshot_json TEXT NOT NULL CHECK(json_valid(snapshot_json)),
                config_pending INTEGER NOT NULL DEFAULT 0 CHECK(config_pending IN (0,1))
            ) STRICT;
            CREATE TABLE IF NOT EXISTS project_session_aliases (
                session_id TEXT NOT NULL REFERENCES session_contexts(session_id) ON DELETE CASCADE,
                bundle_id TEXT NOT NULL, PRIMARY KEY(session_id,bundle_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS project_locations (
                host TEXT NOT NULL,
                directory BLOB NOT NULL,
                checkout_root BLOB NOT NULL,
                repository_root BLOB NOT NULL,
                identity_json TEXT NOT NULL CHECK(json_valid(identity_json)),
                seen_at TEXT NOT NULL,
                PRIMARY KEY(host, directory)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS project_seed_homes (
                harness TEXT NOT NULL,
                home BLOB NOT NULL,
                PRIMARY KEY(harness, home)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS project_seed_failures (
             harness TEXT NOT NULL, home BLOB NOT NULL, directory BLOB NOT NULL, error TEXT NOT NULL, source_file INTEGER NOT NULL CHECK(source_file IN (0,1)),
             PRIMARY KEY(harness,home,directory)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS project_discovery_changes (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                directory BLOB,
                managed_worktree TEXT,
                target_template_id TEXT NOT NULL
            ) STRICT;
            CREATE TABLE IF NOT EXISTS project_discovery_progress (
                singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                sequence INTEGER NOT NULL DEFAULT 0
            ) STRICT;
            CREATE TABLE IF NOT EXISTS project_discovery_failures (
                sequence INTEGER PRIMARY KEY REFERENCES project_discovery_changes(sequence) ON DELETE CASCADE,
                error TEXT NOT NULL
            ) STRICT;
            INSERT OR IGNORE INTO project_discovery_progress(singleton) VALUES(1);
            INSERT INTO project_discovery_changes(session_id, directory, managed_worktree, target_template_id)
                SELECT session_id, project_directory, managed_worktree, target_template_id FROM sessions
                WHERE project_directory IS NOT NULL AND NOT EXISTS (
                    SELECT 1 FROM project_discovery_changes d WHERE d.session_id=sessions.session_id
                );
            CREATE TRIGGER IF NOT EXISTS project_discovery_insert AFTER INSERT ON sessions
                WHEN NEW.project_directory IS NOT NULL BEGIN
                    INSERT INTO project_discovery_changes(session_id,directory,managed_worktree,target_template_id)
                    VALUES(NEW.session_id,NEW.project_directory,NEW.managed_worktree,NEW.target_template_id);
                END;
            CREATE TRIGGER IF NOT EXISTS project_discovery_update AFTER UPDATE OF project_directory,managed_worktree,target_template_id ON sessions
                WHEN NEW.project_directory IS NOT NULL AND
                    (NEW.project_directory IS NOT OLD.project_directory
                    OR NEW.managed_worktree IS NOT OLD.managed_worktree
                    OR NEW.target_template_id IS NOT OLD.target_template_id) BEGIN
                    INSERT INTO project_discovery_changes(session_id,directory,managed_worktree,target_template_id)
                    VALUES(NEW.session_id,NEW.project_directory,NEW.managed_worktree,NEW.target_template_id);
                END;
            UPDATE schema_compatibility SET minimum_compatible_version=69 WHERE singleton=1;
            INSERT INTO schema_migrations(version,applied_at) VALUES(69,strftime('%Y-%m-%dT%H:%M:%fZ','now'));
            PRAGMA user_version=69;
            COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: only shortens existing title text. Older readers and writers
	// accept the same column and values; no schema or JSON shape changes.
	if version < 70 {
		if err := func() error {
			transaction, err := connection.Begin()
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			rows, err := transaction.Query(`SELECT session_id, acp_session_title FROM sessions WHERE length(acp_session_title) > ?1`, core.MaxSessionTitleChars)
			if err != nil {
				return err
			}
			type title struct{ sessionID, title string }
			var titles []title
			for rows.Next() {
				var row title
				if err := rows.Scan(&row.sessionID, &row.title); err != nil {
					rows.Close()
					return err
				}
				titles = append(titles, row)
			}
			if err := errors.Join(rows.Err(), rows.Close()); err != nil {
				return err
			}
			for _, row := range titles {
				var normalized any
				if text, ok := core.NormalizeSessionTitle(row.title); ok {
					normalized = text
				}
				if _, err := transaction.Exec(`UPDATE sessions SET acp_session_title = ?2 WHERE session_id = ?1`, row.sessionID, normalized); err != nil {
					return err
				}
			}
			if _, err := transaction.Exec(`INSERT INTO schema_migrations(version, applied_at)
                 VALUES (70, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 70;`); err != nil {
				return err
			}
			return transaction.Commit()
		}(); err != nil {
			return err
		}
	}

	// Breaking: older strict Move JSON readers discard prepared EC2 ownership.
	if version < 71 {
		if err := func() error {
			transaction, err := connection.Begin()
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			if _, err := transaction.Exec(`UPDATE schema_compatibility SET minimum_compatible_version=71 WHERE singleton=1;
             INSERT INTO schema_migrations(version,applied_at)
                 VALUES(71,strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version=71;`); err != nil {
				return err
			}
			return transaction.Commit()
		}(); err != nil {
			return err
		}
	}

	// Compatible: adds one nullable column. Older readers ignore it, and the
	// older writer's session upsert lists columns explicitly, so it preserves
	// the value. An older executable reviews such a session by `[review]`
	// alone, which is a behaviour difference, not data loss. The
	// compatibility floor stays where it is.
	if version < 72 {
		addColumn := ""
		if has, err := tableHasColumn(connection, "sessions", "review_json"); err != nil {
			return err
		} else if !has {
			addColumn = `ALTER TABLE sessions ADD COLUMN review_json TEXT;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumn + `
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (72, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 72;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: adds a table for accepted session restarts. Older builds do
	// not query or rewrite it, and session updates leave its rows intact. A
	// restart row is removed only by the newer daemon after completion or an
	// explicit cancellation, so the compatibility floor stays where it is.
	if version < 73 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS session_restart_intents (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                 operation_id TEXT NOT NULL,
                 phase TEXT NOT NULL CHECK(phase IN ('stopping', 'restoring_in_place', 'fallback')),
                 updated_at TEXT NOT NULL,
                 checkpoint_sha256 TEXT,
                 checkpoint_started INTEGER NOT NULL DEFAULT 0 CHECK(checkpoint_started IN (0, 1))
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (73, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 73;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons do not know the `opencode` harness. They skip
	// such a session when loading state, and the next full state save deletes
	// it, so the compatibility floor rises with the new harness kind.
	if version < 74 {
		if err := migrateOpencodeHarnessKind(connection); err != nil {
			return err
		}
	}

	// Compatible: adds only mailbox/GitHub tables. Older daemons do not read
	// or rewrite them, and these tables deliberately have no session foreign
	// keys so older session-table maintenance cannot cascade away queued data.
	// The current daemon prunes rows after it observes a destroyed session.
	if version < 75 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS github_watch_cursors (
                 owner TEXT NOT NULL,
                 repo TEXT NOT NULL,
                 items_watermark_at TEXT,
                 items_watermark_id INTEGER,
                 items_etag TEXT,
                 comments_cursor TEXT,
                 PRIMARY KEY(owner, repo)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS github_watch_classifications (
                 owner TEXT NOT NULL,
                 repo TEXT NOT NULL,
                 number INTEGER NOT NULL,
                 session_id TEXT NOT NULL,
                 kind TEXT NOT NULL CHECK(kind IN ('issue', 'pull_request')),
                 title TEXT NOT NULL,
                 url TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 interested INTEGER NOT NULL CHECK(interested IN (0, 1)),
                 created INTEGER NOT NULL CHECK(created IN (0, 1)),
                 PRIMARY KEY(owner, repo, number, session_id)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS github_watch_items (
                 owner TEXT NOT NULL,
                 repo TEXT NOT NULL,
                 number INTEGER NOT NULL,
                 creator_session_id TEXT NOT NULL,
                 kind TEXT NOT NULL CHECK(kind IN ('issue', 'pull_request')),
                 title TEXT NOT NULL,
                 url TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 pull_request_etag TEXT,
                 pull_request_state TEXT CHECK(pull_request_state IS NULL OR pull_request_state IN ('open', 'closed')),
                 PRIMARY KEY(owner, repo, number)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS mailbox_outbox (
                 event_key TEXT PRIMARY KEY,
                 target_session_id TEXT NOT NULL,
                 event_json TEXT NOT NULL,
                 wake INTEGER NOT NULL CHECK(wake IN (0, 1)),
                 unpark INTEGER NOT NULL DEFAULT 0 CHECK(unpark IN (0, 1)),
                 created_at TEXT NOT NULL,
                 accepted_at TEXT,
                 accepted_command_id TEXT,
                 accepted_ordinal INTEGER
             ) STRICT;
             CREATE INDEX IF NOT EXISTS mailbox_outbox_pending
                 ON mailbox_outbox(accepted_at, created_at);
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (75, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 75;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: adds a nullable mailbox failure column. Protocol-75 readers
	// ignore it, and the accepted_at marker keeps them from retrying a row
	// already failed by a newer daemon; their explicit-column updates preserve
	// the failure value. The compatibility floor stays at 74.
	if version < 76 {
		addColumn := ""
		if has, err := tableHasColumn(connection, "mailbox_outbox", "failure"); err != nil {
			return err
		} else if !has {
			addColumn = `ALTER TABLE mailbox_outbox ADD COLUMN failure TEXT;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumn + `
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (76, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 76;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: abandoned composer prompts have no relay event ordinal.
	// Older readers select the history id, session, and text without decoding
	// this column; older writers continue inserting non-null ordinals. The
	// uniqueness constraint and existing rows are preserved, and NULL ordinals
	// remain distinct under SQLite's UNIQUE semantics.
	if version < 77 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE prompt_history_nullable_v77 (
                 history_id INTEGER PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES session_contexts(session_id),
                 event_ordinal INTEGER CHECK(event_ordinal IS NULL OR event_ordinal >= 0),
                 submitted_at TEXT NOT NULL,
                 text TEXT NOT NULL CHECK(length(trim(text)) > 0),
                 UNIQUE(session_id, event_ordinal)
             ) STRICT;
             INSERT INTO prompt_history_nullable_v77
                 SELECT history_id, session_id, event_ordinal, submitted_at, text
                 FROM prompt_history;
             DROP TABLE prompt_history;
             ALTER TABLE prompt_history_nullable_v77 RENAME TO prompt_history;
             CREATE INDEX prompt_history_session_recent
                 ON prompt_history(session_id, history_id DESC);
             CREATE INDEX prompt_history_recent
                 ON prompt_history(history_id DESC);
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (77, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 77;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older readers cannot decode the typed `message` transcript
	// body. Preserve all history and advance both read/write compatibility.
	if version < 78 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 78 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (78, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 78;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: runtime JSON now records native cache placement. Older writers
	// discard that field, losing the authoritative path of fixed container
	// mounts during lifecycle transitions. No existing placement is changed.
	if version < 79 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 79 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (79, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 79;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: adds two columns with defaults, the per-session opt-outs
	// from project memory and from the agent mailbox. Older readers ignore
	// them, and the older writer's session upsert lists columns explicitly, so
	// it preserves the values. An older executable would start such a session
	// with the workspace's project memory and the mailbox, which is a
	// behaviour difference after a manual downgrade, not data loss. The
	// compatibility floor stays where it is.
	if version < 80 {
		addColumn := func(column string) (string, error) {
			has, err := tableHasColumn(connection, "sessions", column)
			if err != nil || has {
				return "", err
			}
			return `ALTER TABLE sessions ADD COLUMN ` + column + ` INTEGER NOT NULL DEFAULT 0;`, nil
		}
		projectMemory, err := addColumn("no_project_memory")
		if err != nil {
			return err
		}
		mailbox, err := addColumn("no_mailbox")
		if err != nil {
			return err
		}
		addColumns := projectMemory + mailbox
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumns + `
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (80, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 80;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older readers cannot decode the new mjolnir policy or task_type
	// spawn actions; older writers can discard those durable routing choices.
	if version < 81 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 81 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (81, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 81;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older readers reject semantic turn kinds, and older writers
	// discard the exact raw outcome retained beside the semantic projection.
	if version < 82 {
		if err := migrateTurnOutcomeSchema(connection); err != nil {
			return err
		}
	}

	// Breaking: older daemons reject the recreation phase and discard applied
	// container settings from target_runtime_json when saving a session.
	if version < 83 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE session_restart_intents RENAME TO session_restart_intents_v82;
             CREATE TABLE session_restart_intents (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                 operation_id TEXT NOT NULL,
                 phase TEXT NOT NULL CHECK(phase IN ('stopping', 'restoring_in_place', 'recreating_container', 'cancel_requested', 'fallback')),
                 updated_at TEXT NOT NULL,
                 checkpoint_sha256 TEXT,
                 checkpoint_started INTEGER NOT NULL DEFAULT 0 CHECK(checkpoint_started IN (0, 1)),
                 container_recreation_acknowledged INTEGER NOT NULL DEFAULT 0 CHECK(container_recreation_acknowledged IN (0, 1))
             ) STRICT;
             INSERT INTO session_restart_intents(session_id, operation_id, phase, updated_at, checkpoint_sha256, checkpoint_started)
                 SELECT session_id, operation_id, phase, updated_at, checkpoint_sha256, checkpoint_started
                 FROM session_restart_intents_v82;
             DROP TABLE session_restart_intents_v82;
             UPDATE schema_compatibility SET minimum_compatible_version = 83 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (83, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 83;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Keep notification_epoch for schema-84 compatibility; new notifications do not use it.
	// Breaking: older daemons cannot decode persisted AwaitChildren requests
	// and discard the worker-owned child-wait intent from continuation state.
	if version < 84 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE subagent_sessions ADD COLUMN notification_epoch INTEGER NOT NULL DEFAULT 0 CHECK(notification_epoch >= 0);
             UPDATE schema_compatibility SET minimum_compatible_version = 84 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (84, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 84;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons address the removed feed and PR-state columns.
	// Reset progress atomically so existing watches deliver only new activity.
	if version < 85 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE github_watch_cursors RENAME TO github_watch_cursors_v84;
             CREATE TABLE github_watch_cursors (
                 owner TEXT NOT NULL, repo TEXT NOT NULL,
                 updated_at TEXT NOT NULL, watch_started_at TEXT NOT NULL,
                 reconciled_through TEXT NOT NULL,
                 last_reconciled_at_ms INTEGER,
                 PRIMARY KEY(owner, repo)
             ) STRICT;
             INSERT INTO github_watch_cursors(owner, repo, updated_at, watch_started_at, reconciled_through)
                 SELECT owner, repo, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 FROM github_watch_cursors_v84;
             INSERT OR IGNORE INTO github_watch_cursors(owner, repo, updated_at, watch_started_at, reconciled_through)
                 SELECT DISTINCT owner, repo, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 FROM github_watch_items;
             DROP TABLE github_watch_cursors_v84;
             ALTER TABLE github_watch_items DROP COLUMN pull_request_etag;
             ALTER TABLE github_watch_items DROP COLUMN pull_request_state;
             ALTER TABLE github_watch_items ADD COLUMN timeline_at TEXT NOT NULL DEFAULT '';
             ALTER TABLE github_watch_items ADD COLUMN timeline_id INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE github_watch_items ADD COLUMN review_comments_at TEXT NOT NULL DEFAULT '';
             ALTER TABLE github_watch_items ADD COLUMN review_comments_id INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE github_watch_items ADD COLUMN processed_at TEXT NOT NULL DEFAULT '';
             UPDATE github_watch_items SET timeline_at=(SELECT watch_started_at FROM github_watch_cursors c WHERE c.owner=github_watch_items.owner AND c.repo=github_watch_items.repo),
                 review_comments_at=(SELECT watch_started_at FROM github_watch_cursors c WHERE c.owner=github_watch_items.owner AND c.repo=github_watch_items.repo),
                 processed_at=(SELECT watch_started_at FROM github_watch_cursors c WHERE c.owner=github_watch_items.owner AND c.repo=github_watch_items.repo);
             CREATE TABLE github_seen_items (
                 owner TEXT NOT NULL, repo TEXT NOT NULL, number INTEGER NOT NULL,
                 kind TEXT NOT NULL, title TEXT NOT NULL, url TEXT NOT NULL,
                 author TEXT NOT NULL, body TEXT NOT NULL,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 PRIMARY KEY(owner, repo, number)
             ) STRICT;
             CREATE TABLE github_watch_classification_failures (
                 owner TEXT NOT NULL, repo TEXT NOT NULL, number INTEGER NOT NULL,
                 session_id TEXT NOT NULL, attempts INTEGER NOT NULL CHECK(attempts > 0),
                 last_attempt_at_ms INTEGER NOT NULL, next_attempt_at_ms INTEGER NOT NULL,
                 PRIMARY KEY(owner, repo, number, session_id)
             ) STRICT;
             UPDATE schema_compatibility SET minimum_compatible_version=85 WHERE singleton=1;
             INSERT INTO schema_migrations(version, applied_at) VALUES (85, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version=85;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: older readers and writers leave this independent receipt
	// table intact; no existing JSON, enum, or update contract changes.
	if version < 86 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS mailbox_reset_frontiers (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                 through_ordinal INTEGER NOT NULL CHECK(through_ordinal >= 0)
             ) STRICT;
             INSERT OR IGNORE INTO mailbox_reset_frontiers(session_id, through_ordinal)
                 SELECT session_id, applied_event_ordinal FROM materialized_sessions
                 WHERE session_id IN (SELECT session_id FROM sessions);
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (86, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 86;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: stored elicitation requests can say they are tool permission
	// forms (`tool_permission`), and older readers refuse the unknown field.
	if version < 87 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 87 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (87, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 87;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: a container's applied settings in target_runtime_json now
	// record the default image it was created from, which decides whether an
	// upgrade recreates it. Older writers discard that field, and the
	// container would then never move to the upgraded image's worker.
	if version < 88 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 88 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (88, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 88;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: a Restart intent can now be automatic, the container
	// recreation an upgrade starts on its own. Such a Restart defers rather
	// than interrupting work; an older daemon would recover it as one a person
	// asked for and interrupt a running turn to finish it.
	if version < 89 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE session_restart_intents ADD COLUMN automatic INTEGER NOT NULL DEFAULT 0 CHECK(automatic IN (0, 1));
             UPDATE schema_compatibility SET minimum_compatible_version = 89 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (89, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 89;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons do not know the `mimo` harness. They skip such
	// sessions when loading state, and a full state save would delete them, so
	// the compatibility floor rises with the new harness kind. MiMo Code has
	// since been removed; the constraint keeps admitting `mimo` so rows an
	// earlier release wrote stay readable, and loading skips them.
	if version < 90 {
		if err := migrateMimoHarnessKind(connection); err != nil {
			return err
		}
	}

	// Breaking: older daemons interpret every restart as checkpoint-based
	// recreation and can delete storage retained by an explicit upgrade.
	if version < 91 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE session_restart_intents ADD COLUMN container_upgrade_json TEXT;
             UPDATE schema_compatibility SET minimum_compatible_version = 91 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (91, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 91;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons do not know the `dsh` harness. They skip such
	// sessions when loading state, and a full state save would delete them, so
	// the compatibility floor rises with the new harness kind.
	if version < 92 {
		if err := admitHarnessKind(connection, "mimo", "dsh", 92); err != nil {
			return fmt.Errorf("migrate DSH harness constraints: %w", err)
		}
	}

	// Compatible: each turn's selection now records the profile that ran it,
	// so usage reports can filter by profile across sub-agent trees and after
	// a child is destroyed. Older daemons name their columns when recording a
	// selection and never read this one; turns they record leave it NULL,
	// which the report treats as an unknown profile. Existing rows take their
	// session's current profile, the only record that survives for them.
	if version < 93 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE session_turn_selections ADD COLUMN profile TEXT;
             UPDATE session_turn_selections SET profile = (
                 SELECT last_profile FROM sessions s
                 WHERE s.session_id = session_turn_selections.session_id);
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (93, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 93;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: the new frontier is projection bookkeeping. Older readers
	// ignore it and older named-column upserts preserve it; the read/write
	// floor stays at 92. Rebuilds must not publish old questions as new mail.
	if version < 94 {
		addColumn := ""
		if has, err := tableHasColumn(connection, "materialized_sessions", "input_required_event_frontier"); err != nil {
			return err
		} else if !has {
			addColumn = `ALTER TABLE materialized_sessions ADD COLUMN input_required_event_frontier INTEGER NOT NULL DEFAULT 0 CHECK(input_required_event_frontier >= 0);`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumn + `
             UPDATE materialized_sessions SET input_required_event_frontier = max(input_required_event_frontier, applied_event_ordinal);
             INSERT INTO schema_migrations(version, applied_at) VALUES (94, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 94;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons still read and write the removed plan-review tables.
	if version < 95 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             DROP TABLE IF EXISTS second_opinion_reviews;
             DROP TABLE IF EXISTS second_opinion_defaults;
             UPDATE schema_compatibility SET minimum_compatible_version = 95 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (95, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 95;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older readers reject reviewer child records, and must never
	// deliver a reviewer's private verdict as an ordinary child report.
	if version < 96 {
		childState := ""
		if has, err := tableHasColumn(connection, "turn_review_state", "child_session_id"); err != nil {
			return err
		} else if !has {
			childState = `ALTER TABLE turn_review_state ADD COLUMN child_session_id TEXT;
             UPDATE subagent_sessions SET request_key = 'worker:' || request_key;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + childState + `
             UPDATE schema_compatibility SET minimum_compatible_version = 96 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (96, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 96;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons cannot decode durable RequestReview delegation
	// actions or ReviewVerdict mailbox bodies. Refuse their reads and writes
	// before either new enum value can be stored.
	if version < 97 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 97 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (97, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 97;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: a session on a remote Apple container host records the new
	// `ssh-apple-container` target kind, and older readers treat an unknown
	// kind as impossible. Refuse their reads and writes before such a row can
	// be stored. The table is recreated because SQLite cannot widen a column
	// CHECK in place.
	if version < 98 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE session_targets RENAME TO session_targets_v97;
             CREATE TABLE session_targets (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                 kind TEXT NOT NULL CHECK(kind IN ('local-bare','local-podman','local-docker','apple-container','aws-ec2','ssh-bare','ssh-podman','ssh-docker','ssh-apple-container')),
                 host TEXT,
                 resource_id TEXT,
                 address TEXT,
                 workspace BLOB,
                 worker_id TEXT,
                 workspace_storage TEXT,
                 borrowed_from TEXT,
                 CHECK(
                     (kind = 'local-bare' AND workspace IS NOT NULL
                      AND host IS NULL AND resource_id IS NULL AND address IS NULL AND worker_id IS NULL)
                  OR (kind IN ('local-podman','local-docker','apple-container') AND resource_id IS NOT NULL
                      AND host IS NULL AND address IS NULL AND workspace IS NULL AND worker_id IS NULL)
                  OR (kind = 'aws-ec2' AND resource_id IS NOT NULL
                      AND host IS NULL AND workspace IS NULL AND worker_id IS NULL)
                  OR (kind = 'ssh-bare' AND host IS NOT NULL AND workspace IS NOT NULL
                      AND resource_id IS NULL AND address IS NULL)
                  OR (kind IN ('ssh-podman','ssh-docker','ssh-apple-container') AND host IS NOT NULL AND resource_id IS NOT NULL
                      AND address IS NULL AND workspace IS NULL AND worker_id IS NULL)
                 )
             ) STRICT;
             INSERT INTO session_targets(session_id, kind, host, resource_id, address, workspace, worker_id, workspace_storage, borrowed_from)
                 SELECT session_id, kind, host, resource_id, address, workspace, worker_id, workspace_storage, borrowed_from
                 FROM session_targets_v97;
             DROP TABLE session_targets_v97;
             UPDATE schema_compatibility SET minimum_compatible_version = 98 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (98, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 98;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons cannot decode OperatorAway/OperatorBack mailbox bodies.
	if version < 99 {
		addFrontier := ""
		if has, err := tableHasColumn(connection, "materialized_sessions", "operator_away_event_frontier"); err != nil {
			return err
		} else if !has {
			addFrontier = `ALTER TABLE materialized_sessions ADD COLUMN operator_away_event_frontier INTEGER NOT NULL DEFAULT 0 CHECK(operator_away_event_frontier >= 0);`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS operator_away (singleton INTEGER PRIMARY KEY CHECK(singleton=1), away INTEGER NOT NULL CHECK(away IN (0,1)), revision INTEGER NOT NULL DEFAULT 0 CHECK(revision >= 0));
             INSERT OR IGNORE INTO operator_away(singleton,away,revision) VALUES(1,0,0);
             CREATE TABLE IF NOT EXISTS operator_away_workers (session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE, protocol INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS operator_away_requests (session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE, requested_seq INTEGER NOT NULL, elicitation_id TEXT NOT NULL, request TEXT NOT NULL, period INTEGER NOT NULL, resolved_seq INTEGER, action TEXT, PRIMARY KEY(session_id, requested_seq));
             ` + addFrontier + `
             UPDATE schema_compatibility SET minimum_compatible_version = 99 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at) VALUES(99, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 99;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: old daemons ignore structured Prompt content and encoded drafts,
	// sending marker text and losing images. Preserve legacy text rows unchanged.
	if version < 100 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TRIGGER IF NOT EXISTS startup_steps_session_deleted AFTER DELETE ON sessions
                 BEGIN DELETE FROM startup_steps WHERE session_id=OLD.session_id
                     AND group_id IS NULL AND json_type(step_json, '$.Prompt.prompt')='array'; END;
             UPDATE schema_compatibility SET minimum_compatible_version = 100 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at) VALUES(100, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 100;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: old writers address the singleton preference row and would
	// silently lose profile-specific choices. Preserve its value as the
	// empty-profile fallback while replacing the key with profile_id.
	if version < 101 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE subagent_preference RENAME TO subagent_preference_legacy;
             CREATE TABLE subagent_preference (
                 profile_id TEXT PRIMARY KEY NOT NULL,
                 policy TEXT NOT NULL CHECK(json_valid(policy))
             ) STRICT;
             INSERT INTO subagent_preference(profile_id, policy)
                 SELECT '', policy FROM subagent_preference_legacy WHERE singleton=1;
             DROP TABLE subagent_preference_legacy;
             UPDATE schema_compatibility SET minimum_compatible_version = 101 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at) VALUES(101, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 101;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons cannot recover a Restart-owned session handoff
	// and would sweep its archive or restore a stale full checkpoint instead.
	if version < 102 {
		addColumn := ""
		if has, err := tableHasColumn(connection, "session_restart_intents", "handoff_json"); err != nil {
			return err
		} else if !has {
			addColumn = `ALTER TABLE session_restart_intents ADD COLUMN handoff_json TEXT CHECK(handoff_json IS NULL OR json_valid(handoff_json));`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumn + `
             UPDATE schema_compatibility SET minimum_compatible_version = 102 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at) VALUES(102, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 102;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: independent observations have no session/profile foreign
	// keys. Older readers ignore them and older writers leave them intact,
	// including when updating the reset cache or deleting sessions.
	if version < 103 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS quota_history (
                 id INTEGER PRIMARY KEY,
                 identity TEXT NOT NULL,
                 profile_id TEXT NOT NULL,
                 harness TEXT NOT NULL,
                 sampled_at_epoch_seconds INTEGER NOT NULL,
                 body TEXT NOT NULL CHECK(json_valid(body))
             ) STRICT;
             CREATE INDEX IF NOT EXISTS quota_history_profile_time
                 ON quota_history(profile_id, sampled_at_epoch_seconds, id);
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (103, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 103;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons do not maintain SQL-backed pending push
	// findings when resolving reviews. Refuse their reads and writes.
	if version < 104 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE turn_review_state ADD COLUMN push_findings TEXT;
             UPDATE schema_compatibility SET minimum_compatible_version = 104 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (104, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 104;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons cannot decode durable OverrideReview actions.
	// Refuse their reads and writes before the new action can be queued.
	if version < 105 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 105 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (105, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 105;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons discard active reviewer groups, per-session
	// reviewer count, and aggregate failure metadata in stored JSON updates.
	if version < 106 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE turn_review_state ADD COLUMN active_group TEXT;
             UPDATE schema_compatibility SET minimum_compatible_version = 106 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (106, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 106;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons cannot decode durable SessionReview::Inherit
	// values in stored JSON. Refuse their reads and writes before one is saved.
	if version < 107 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 107 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (107, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 107;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older state validators reject reviewer sessions owned by a
	// managed child. Refuse their reads and writes before saving that shape.
	if version < 108 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 108 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (108, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 108;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older writers cannot preserve pending follow-up and bounded
	// retry state, so a handoff could silently lose coverage or retry forever.
	if version < 109 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE turn_review_state ADD COLUMN pending_followup INTEGER NOT NULL DEFAULT 0 CHECK(pending_followup IN (0, 1));
             ALTER TABLE turn_review_state ADD COLUMN forward_attempts INTEGER NOT NULL DEFAULT 0 CHECK(forward_attempts BETWEEN 0 AND 255);
             UPDATE schema_compatibility SET minimum_compatible_version = 109 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (109, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 109;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older readers require ActiveReviewGroup `reviewer` and reject
	// SessionReview `reviewers` and retained legacy override fields.
	if version < 110 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 110 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (110, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 110;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older writers would drop a session's Oracle choice on update,
	// and older daemons cannot decode AskOracle actions or Oracle-kind children.
	if version < 111 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;`); err != nil {
			return err
		}
		if has, err := tableHasColumn(connection, "sessions", "oracle"); err != nil {
			return err
		} else if !has {
			if _, err := connection.Exec(`ALTER TABLE sessions ADD COLUMN oracle INTEGER NOT NULL DEFAULT 1 CHECK(oracle IN (0, 1));`); err != nil {
				return err
			}
		}
		if has, err := tableHasColumn(connection, "sessions", "oracle_choice"); err != nil {
			return err
		} else if !has {
			if _, err := connection.Exec(`ALTER TABLE sessions ADD COLUMN oracle_choice TEXT CHECK(oracle_choice IS NULL OR json_valid(oracle_choice));`); err != nil {
				return err
			}
		}
		if _, err := connection.Exec(`CREATE TABLE IF NOT EXISTS oracle_preference (
                 profile_id TEXT PRIMARY KEY,
                 enabled INTEGER NOT NULL CHECK(enabled IN (0, 1))
             );
             UPDATE schema_compatibility SET minimum_compatible_version = 111 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (111, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 111;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: clearing a nullable override changes no stored shape; older
	// readers and writers already support absent manual titles (as in the compatible
	// turn-profile backfill). Keep the existing compatibility floor.
	if version < 112 {
		if err := func() error {
			transaction, err := connection.BeginWith(TransactionImmediate)
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			rows, err := transaction.Query(`SELECT s.session_id, s.title FROM sessions s
                 WHERE s.session_title_override = s.title
                   AND NOT EXISTS (SELECT 1 FROM subagent_sessions a
                                   WHERE a.child_session_id = s.session_id)`)
			if err != nil {
				return err
			}
			type candidate struct{ id, title string }
			var candidates []candidate
			for rows.Next() {
				var row candidate
				if err := rows.Scan(&row.id, &row.title); err != nil {
					rows.Close()
					return err
				}
				candidates = append(candidates, row)
			}
			if err := errors.Join(rows.Err(), rows.Close()); err != nil {
				return err
			}
			for _, row := range candidates {
				// Creation inputs change during provisioning, moves and bundle
				// renames. Match the approved placeholder shape instead.
				project, profile, found := strings.Cut(row.title, " via ")
				if found &&
					project != "" &&
					profile != "" &&
					strings.IndexFunc(project, unicode.IsSpace) < 0 &&
					strings.IndexFunc(profile, unicode.IsSpace) < 0 {
					// Advance updated_at as a rename does, so SessionWiki's
					// change token moves and an indexed archive is refreshed.
					if _, err := transaction.Exec(`UPDATE sessions SET session_title_override = NULL,
                             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
                         WHERE session_id = ?1`, row.id); err != nil {
						return err
					}
				}
			}
			if _, err := transaction.Exec(`INSERT INTO schema_migrations(version, applied_at)
                 VALUES (112, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 112;`); err != nil {
				return err
			}
			return transaction.Commit()
		}(); err != nil {
			return err
		}
	}

	// Breaking: older daemons reject answered_through in persisted outcome JSON.
	if version < 113 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 113 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (113, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 113;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons cannot decode durable per-finding
	// `override_review` or `set_review_baseline` requests, and never clear the
	// declined findings when a baseline moves, so a later daemon would tell
	// reviewers to skip findings of a scope that no longer exists.
	if version < 114 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE turn_review_state ADD COLUMN declined_findings TEXT;
             UPDATE schema_compatibility SET minimum_compatible_version = 114 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (114, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 114;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: retire obsolete turn orchestration without changing columns
	// or the push-group format. Older readers can still read and write rows.
	if version < 115 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE turn_review_state
             SET active_group = NULL, active = NULL, pending_forward = NULL,
                 child_session_id = NULL, pending_followup = 0, forward_attempts = 0
             WHERE pending_forward IS NOT NULL
                OR (active_group IS NOT NULL
                    AND coalesce(json_extract(active_group, '$.scope'), 'turn_delta') <> 'pushable_commits');
             UPDATE turn_review_state SET pending_followup = 0;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (115, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 115;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: limits `prompt_history` to prompts of top-level sessions
	// that still exist, and adds a full-text index over it. The migration
	// deletes the spawn instructions recorded for sub-agent children and the
	// prompts of destroyed sessions. Triggers then keep both rules for every
	// writer: a prompt is stored only while its session exists and is not a
	// child, registering a child removes what was stored for it, and
	// destroying a session removes its prompts. Writers are serialized, so a
	// prompt racing a destroy lands either before it, and is removed, or
	// after it, and is ignored.
	// It also adds a nullable `command_id` with a unique index per session;
	// older writers list their columns and leave it empty, which the index
	// allows. Older readers ignore the index. Older writers' inserts, deletes and
	// destroys fire the same triggers, so the table and the index follow their
	// writes; every shipped build links a bundled SQLite compiled with FTS5, so
	// they can run them. Fewer rows for older builds is a behaviour difference,
	// not lost data they rely on. The compatibility floor stays where it is.
	// A later migration that recreates `prompt_history`, `sessions` or
	// `subagent_sessions`, as revision 77 recreated `prompt_history`, drops
	// the triggers on that table and must create them again.
	if version < 116 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;`); err != nil {
			return err
		}
		// A prompt sent as a command is stored once per command, so a retried
		// command does not record its prompt again.
		if has, err := tableHasColumn(connection, "prompt_history", "command_id"); err != nil {
			return err
		} else if !has {
			if _, err := connection.Exec(`ALTER TABLE prompt_history ADD COLUMN command_id TEXT;`); err != nil {
				return err
			}
		}
		if _, err := connection.Exec(`CREATE UNIQUE INDEX IF NOT EXISTS prompt_history_command
                 ON prompt_history(session_id, command_id) WHERE command_id IS NOT NULL;
             DELETE FROM prompt_history
                 WHERE session_id IN (SELECT child_session_id FROM subagent_sessions)
                    OR session_id NOT IN (SELECT session_id FROM sessions);
             CREATE TRIGGER IF NOT EXISTS prompt_history_top_level_only
                 BEFORE INSERT ON prompt_history
                 WHEN NOT EXISTS (SELECT 1 FROM sessions WHERE session_id = new.session_id)
                   OR EXISTS (SELECT 1 FROM subagent_sessions
                              WHERE child_session_id = new.session_id)
                 BEGIN SELECT RAISE(IGNORE); END;
             CREATE TRIGGER IF NOT EXISTS prompt_history_child_registered
                 AFTER INSERT ON subagent_sessions BEGIN
                     DELETE FROM prompt_history WHERE session_id = new.child_session_id;
                 END;
             CREATE TRIGGER IF NOT EXISTS prompt_history_session_destroyed
                 AFTER DELETE ON sessions BEGIN
                     DELETE FROM prompt_history WHERE session_id = old.session_id;
                 END;
             CREATE VIRTUAL TABLE IF NOT EXISTS prompt_history_text USING fts5(
                 text, content='prompt_history', content_rowid='history_id',
                 tokenize='trigram');
             CREATE TRIGGER IF NOT EXISTS prompt_history_text_insert
                 AFTER INSERT ON prompt_history BEGIN
                     INSERT INTO prompt_history_text(rowid, text)
                         VALUES (new.history_id, new.text);
                 END;
             CREATE TRIGGER IF NOT EXISTS prompt_history_text_delete
                 AFTER DELETE ON prompt_history BEGIN
                     INSERT INTO prompt_history_text(prompt_history_text, rowid, text)
                         VALUES ('delete', old.history_id, old.text);
                 END;
             CREATE TRIGGER IF NOT EXISTS prompt_history_text_update
                 AFTER UPDATE OF text ON prompt_history BEGIN
                     INSERT INTO prompt_history_text(prompt_history_text, rowid, text)
                         VALUES ('delete', old.history_id, old.text);
                     INSERT INTO prompt_history_text(rowid, text)
                         VALUES (new.history_id, new.text);
                 END;
             INSERT INTO prompt_history_text(prompt_history_text) VALUES ('rebuild');
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (116, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 116;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: deletes only the `worker_restart_intents` rows of sessions
	// that are `stopped` and hold no target, so no worker can exist to
	// restart. A sub-agent's close left them behind. Older builds read and
	// write these rows per session, only while restarting or observing that
	// session's worker; a stopped session without a target has neither, and
	// a restart that finds no row begins a new one. No column, constraint or
	// stored value changes, so the compatibility floor stays where it is.
	if version < 117 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             DELETE FROM worker_restart_intents
                 WHERE session_id IN (
                     SELECT session_id FROM sessions
                     WHERE state = 'stopped'
                       AND NOT EXISTS (SELECT 1 FROM session_targets
                                       WHERE session_targets.session_id = sessions.session_id));
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (117, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 117;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: no table changes, but a sub-agent's prompt outcome now carries
	// what its worker observed (`work_facts`), stored in the materialized
	// session's `last_turn_outcome_json`, which a revision-117 daemon decodes
	// with `deny_unknown_fields` and would refuse; and a finished child's report
	// carries the facts in `mailbox_outbox.event_json`. A revision-117 daemon must
	// not write or read this store once either exists.
	if version < 118 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 118 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (118, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 118;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: older daemons cannot honor the accepted credential prohibition
	// and could restore GitHub authentication when resuming or syncing a session.
	if version < 119 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ALTER TABLE sessions ADD COLUMN no_github_credentials INTEGER NOT NULL DEFAULT 0;
             UPDATE schema_compatibility SET minimum_compatible_version = 119 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (119, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 119;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Compatible: adds an index only. Reading a parent's child messages joined
	// the outbox to each child by `target_session_id` with no index, scanning
	// every outbox row per child (27 s for a parent with 2,209 children).
	// Older builds read and write the same rows and SQLite maintains the index
	// for them, so the compatibility floor stays where it is.
	if version < 120 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE INDEX IF NOT EXISTS mailbox_outbox_target
                 ON mailbox_outbox(target_session_id);
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (120, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 120;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: rows that belong to a session which is gone (deleted, or
	// destroyed with data loss) are removed by triggers in the transaction
	// that removes the session, and an insert for a gone session is ignored,
	// so no ordering of writers can leave one behind. A revision-120 build
	// reads an enqueued event back after inserting it; when a trigger ignores
	// that insert the read fails and rolls back the build's whole write, so
	// older builds must not write this store.
	if version < 121 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TRIGGER IF NOT EXISTS session_rows_deleted AFTER DELETE ON sessions BEGIN
                     DELETE FROM mailbox_outbox WHERE target_session_id = OLD.session_id;
                     DELETE FROM github_watch_items WHERE creator_session_id = OLD.session_id;
                     DELETE FROM github_watch_classifications WHERE session_id = OLD.session_id;
                     DELETE FROM github_watch_classification_failures WHERE session_id = OLD.session_id;
                 END;
             CREATE TRIGGER IF NOT EXISTS session_rows_destroyed
                 AFTER UPDATE OF state ON sessions
                 WHEN NEW.state = 'destroyed-with-data-loss' BEGIN
                     DELETE FROM mailbox_outbox WHERE target_session_id = NEW.session_id;
                     DELETE FROM github_watch_items WHERE creator_session_id = NEW.session_id;
                     DELETE FROM github_watch_classifications WHERE session_id = NEW.session_id;
                     DELETE FROM github_watch_classification_failures WHERE session_id = NEW.session_id;
                 END;
             CREATE TRIGGER IF NOT EXISTS mailbox_outbox_live_target BEFORE INSERT ON mailbox_outbox
                 WHEN NOT EXISTS (SELECT 1 FROM sessions WHERE session_id = NEW.target_session_id AND state <> 'destroyed-with-data-loss')
                 BEGIN SELECT RAISE(IGNORE); END;
             CREATE TRIGGER IF NOT EXISTS github_watch_items_live_creator
                 BEFORE INSERT ON github_watch_items
                 WHEN NOT EXISTS (SELECT 1 FROM sessions WHERE session_id = NEW.creator_session_id AND state <> 'destroyed-with-data-loss')
                 BEGIN SELECT RAISE(IGNORE); END;
             CREATE TRIGGER IF NOT EXISTS github_watch_classifications_live_session
                 BEFORE INSERT ON github_watch_classifications
                 WHEN NOT EXISTS (SELECT 1 FROM sessions WHERE session_id = NEW.session_id AND state <> 'destroyed-with-data-loss')
                 BEGIN SELECT RAISE(IGNORE); END;
             CREATE TRIGGER IF NOT EXISTS github_watch_classification_failures_live_session
                 BEFORE INSERT ON github_watch_classification_failures
                 WHEN NOT EXISTS (SELECT 1 FROM sessions WHERE session_id = NEW.session_id AND state <> 'destroyed-with-data-loss')
                 BEGIN SELECT RAISE(IGNORE); END;
             DELETE FROM mailbox_outbox WHERE target_session_id NOT IN (
                 SELECT session_id FROM sessions WHERE state <> 'destroyed-with-data-loss');
             DELETE FROM github_watch_items WHERE creator_session_id NOT IN (
                 SELECT session_id FROM sessions WHERE state <> 'destroyed-with-data-loss');
             DELETE FROM github_watch_classifications WHERE session_id NOT IN (
                 SELECT session_id FROM sessions WHERE state <> 'destroyed-with-data-loss');
             DELETE FROM github_watch_classification_failures WHERE session_id NOT IN (
                 SELECT session_id FROM sessions WHERE state <> 'destroyed-with-data-loss');
             UPDATE schema_compatibility SET minimum_compatible_version = 121 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (121, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 121;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking: physical worker ownership survives logical session deletion.
	// Older writers can erase that evidence and remove a live worker's files.
	if version < 122 {
		if err := func() error {
			transaction, err := connection.Begin()
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			if _, err := transaction.Exec(environmentsSQL); err != nil {
				return err
			}
			if err := backfillEnvironments(transaction); err != nil {
				return err
			}
			if err := backfillMoveEnvironments(transaction); err != nil {
				return err
			}
			if _, err := transaction.Exec(`UPDATE schema_compatibility SET minimum_compatible_version=122 WHERE singleton=1`); err != nil {
				return err
			}
			if _, err := transaction.Exec(`INSERT INTO schema_migrations(version,applied_at) VALUES (122,strftime('%Y-%m-%dT%H:%M:%fZ','now'))`); err != nil {
				return err
			}
			if _, err := transaction.Exec(`PRAGMA user_version = 122`); err != nil {
				return err
			}
			return transaction.Commit()
		}(); err != nil {
			return err
		}
	}

	// Breaking: old readers show cleared drafts and old writers bypass revision ordering.
	if version < 123 {
		if err := func() error {
			transaction, err := connection.Begin()
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			if _, err := transaction.Exec(`CREATE TABLE detached_drafts_v123 (
                 draft_id TEXT PRIMARY KEY CHECK(length(trim(draft_id)) > 0),
                 workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id),
                 session_id TEXT REFERENCES session_contexts(session_id),
                 source TEXT NOT NULL CHECK(length(trim(source)) > 0),
                 owner_pid INTEGER CHECK(owner_pid IS NULL OR owner_pid > 0),
                 saved_at TEXT NOT NULL,
                 revision INTEGER CHECK(revision IS NULL OR revision > 0),
                 text TEXT NOT NULL CHECK(length(text) > 0 OR revision IS NOT NULL),
                 recovered_at TEXT
             ) STRICT;
             INSERT INTO detached_drafts_v123(
                 draft_id, workspace_id, session_id, source, owner_pid,
                 saved_at, text, recovered_at, revision
             ) SELECT draft_id, workspace_id, session_id, source, owner_pid,
                      saved_at, text, recovered_at, NULL
                 FROM detached_drafts;
             DROP TABLE detached_drafts;
             ALTER TABLE detached_drafts_v123 RENAME TO detached_drafts;
             CREATE INDEX detached_drafts_workspace_recent
                 ON detached_drafts(workspace_id, saved_at DESC);
             CREATE UNIQUE INDEX detached_drafts_terminal_revision
                 ON detached_drafts(source, session_id)
                 WHERE revision IS NOT NULL;`); err != nil {
				return err
			}
			if _, err := transaction.Exec(`UPDATE schema_compatibility SET minimum_compatible_version=123 WHERE singleton=1`); err != nil {
				return err
			}
			if _, err := transaction.Exec(`INSERT INTO schema_migrations(version,applied_at) VALUES (123,strftime('%Y-%m-%dT%H:%M:%fZ','now'))`); err != nil {
				return err
			}
			if _, err := transaction.Exec(`PRAGMA user_version = 123`); err != nil {
				return err
			}
			return transaction.Commit()
		}(); err != nil {
			return err
		}
	}

	// Breaking: a session on a Nitro Sandbox host records the new `ssh-nitro`
	// target kind, and older readers treat an unknown kind as impossible
	// (their decoder is `unreachable!` on a kind the CHECK admitted). Refuse
	// their reads and writes before such a row can be stored. Two tables
	// constrain the kind: `session_targets` (revision 98) and the physical
	// ownership ledger `environment_workers` (revision 122). Their rows copy
	// over unchanged; no column is added.
	if version < 124 {
		if err := migrateNitroTargetKind(connection); err != nil {
			return err
		}
	}

	// Compatible: only expression indexes; previous readers and writers
	// keep the same turn bodies, and SQLite maintains them for their writes.
	if version < 125 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE INDEX IF NOT EXISTS session_turn_usage_completed_time
                 ON session_turn_usage(json_extract(body, '$.completed_at_ms'), session_id);
             CREATE INDEX IF NOT EXISTS materialized_sessions_active_usage
                 ON materialized_sessions(json_extract(active_turn_json, '$.started_at_ms'),
                     session_id, json_extract(active_turn_json, '$.command_id'))
                 WHERE active_turn_json IS NOT NULL;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (125, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 125;
             COMMIT;`); err != nil {
			return err
		}
	}

	// Breaking once: older snapshot writers delete unsupported sessions they
	// did not load. Revision 126 writers preserve them, so future harness kinds
	// require neither a schema change nor a new compatibility floor.
	if version < 126 {
		if err := removeHarnessConstraints(connection); err != nil {
			return err
		}
	}

	// Compatible: standalone push tables are neither read nor rewritten by older
	// controllers. No stored session JSON or existing constraints change.
	// Provisional revision: assign the final number when merging this lane.
	if version < 127 {
		if _, err := connection.Exec("BEGIN IMMEDIATE;" + webPushSQL +
			`INSERT INTO schema_migrations(version,applied_at) VALUES (127,strftime('%Y-%m-%dT%H:%M:%fZ','now')); PRAGMA user_version=127; COMMIT;`); err != nil {
			return err
		}
	}
	// Compatible: adds the operator-away transition time. Older daemons read
	// and update the table with explicit column lists, so they ignore the
	// column; a store written by an older daemon simply has no timestamp until
	// the next transition. The compatibility floor is unchanged.
	if version < 128 {
		addColumn := ""
		if has, err := tableHasColumn(connection, "operator_away", "changed_at_ms"); err != nil {
			return err
		} else if !has {
			addColumn = `ALTER TABLE operator_away ADD COLUMN changed_at_ms INTEGER;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumn + `
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (128, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 128;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Compatible: adds a per-session notification mute. Older daemons read and
	// write sessions with explicit column lists, so the default applies to
	// anything they save; the flag is person-facing only and never gates work.
	if version < 129 {
		addColumn := ""
		if has, err := tableHasColumn(connection, "sessions", "notifications_ignored"); err != nil {
			return err
		} else if !has {
			addColumn = `ALTER TABLE sessions ADD COLUMN notifications_ignored INTEGER NOT NULL DEFAULT 0 CHECK(notifications_ignored IN (0,1));`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumn + `
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (129, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 129;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: `github_watch_items.creator_session_id` may be NULL, which an
	// older daemon decodes as a string and fails on; and interested-session
	// lifecycle rows in `mailbox_outbox.event_json` carry a `recipient` field
	// that an older daemon refuses with `deny_unknown_fields`.
	if version < 130 {
		if err := migrateGithubWatchFollowers(connection); err != nil {
			return err
		}
	}
	// Compatible: older writers preserve this nullable receipt through explicit
	// column lists. The trigger maintains the generation and invalidates receipts
	// when state/error changes, including writes by older daemons.
	if version < 131 {
		addColumns := ""
		for _, added := range [][2]string{
			{"dismissed_failure_json", "TEXT"},
			{"failure_generation", "INTEGER NOT NULL DEFAULT 0"},
		} {
			column, definition := added[0], added[1]
			has, err := tableHasColumn(connection, "sessions", column)
			if err != nil {
				return err
			}
			if !has {
				addColumns += `ALTER TABLE sessions ADD COLUMN ` + column + ` ` + definition + `;`
			}
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addColumns + `
             CREATE TRIGGER IF NOT EXISTS clear_dismissed_failure
             AFTER UPDATE OF state, last_error ON sessions
             WHEN NEW.state IS NOT OLD.state OR NEW.last_error IS NOT OLD.last_error
             BEGIN
                 UPDATE sessions SET dismissed_failure_json = NULL, failure_generation = failure_generation + 1 WHERE session_id = NEW.session_id;
             END;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (131, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 131;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: removes legacy size columns; older writers cannot preserve edits.
	if version < 132 {
		changes, err := migrate(connection)
		if err != nil {
			return err
		}
		Report(changes)
	}
	// Compatible: revision-132 Move writers name columns explicitly, preserving
	// this nullable intent (new rows default NULL). Exact operation-ID matching
	// makes stale IDs inert; retries that reuse an ID invalidate it via trigger.
	// NULL is conservative: cancellation alone never authorizes suspension.
	// Older daemons retain their pre-existing reaper behaviour, including a
	// destructive fallback to an older checkpoint if the Move row is lost.
	// Compatibility means no worse than before; only upgraded daemons fix it.
	if version < 133 {
		if err := func() error {
			transaction, err := connection.BeginWith(TransactionImmediate)
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			addColumn := ""
			if has, err := tableHasColumn(transaction, "session_moves", "close_requested_operation_id"); err != nil {
				return err
			} else if !has {
				addColumn = `ALTER TABLE session_moves ADD COLUMN close_requested_operation_id TEXT NULL;`
			}
			if _, err := transaction.Exec(addColumn + `
             CREATE TRIGGER IF NOT EXISTS invalidate_move_close_intent
             AFTER UPDATE OF operation_id, operation_json ON session_moves
             WHEN NEW.close_requested_operation_id IS NOT NULL
               AND (NEW.operation_id IS NOT OLD.operation_id
                    OR json_extract(NEW.operation_json, '$.cancellation_requested') = 0)
             BEGIN
                 UPDATE session_moves SET close_requested_operation_id=NULL WHERE session_id=NEW.session_id;
             END;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (133, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version=133;`); err != nil {
				return err
			}
			return transaction.Commit()
		}(); err != nil {
			return err
		}
	}
	// Compatible: adds a table that records unfinished session creations.
	// Older daemons never read or write it and leave its rows intact when they
	// rewrite session rows; an older daemon that meets a provisioning session
	// fails it exactly as before, and the new daemon discards a leftover row
	// whose session is no longer creating. The compatibility floor stays.
	if version < 134 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS session_create_intents (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                 operation_id TEXT NOT NULL,
                 remember_launch_choices INTEGER NOT NULL CHECK(remember_launch_choices IN (0, 1)),
                 created_at TEXT NOT NULL
             ) STRICT;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (134, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 134;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Compatible: old writers name columns; triggers retain their launch kinds too.
	// Leave kinds unknown when the authoritative launch record is gone.
	if version < 135 {
		if err := func() error {
			transaction, err := connection.BeginWith(TransactionImmediate)
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			if has, err := tableHasColumn(transaction, "subagent_accounting", "kind"); err != nil {
				return err
			} else if !has {
				if _, err := transaction.Exec(`ALTER TABLE subagent_accounting ADD COLUMN kind TEXT;`); err != nil {
					return err
				}
			}
			if _, err := transaction.Exec(`CREATE TRIGGER IF NOT EXISTS subagent_accounting_kind_insert
             AFTER INSERT ON subagent_sessions BEGIN
                 UPDATE subagent_accounting
                 SET kind=COALESCE(json_extract(NEW.record_json, '$.kind'), 'worker')
                 WHERE child_session_id=NEW.child_session_id;
             END;
             CREATE TRIGGER IF NOT EXISTS subagent_accounting_kind_update
             AFTER UPDATE OF record_json ON subagent_sessions BEGIN
                 UPDATE subagent_accounting
                 SET kind=COALESCE(json_extract(NEW.record_json, '$.kind'), 'worker')
                 WHERE child_session_id=NEW.child_session_id;
             END;
             UPDATE subagent_accounting SET kind=(
                 SELECT COALESCE(json_extract(s.record_json, '$.kind'), 'worker')
                 FROM subagent_sessions s
                 WHERE s.child_session_id=subagent_accounting.child_session_id)
             WHERE kind IS NULL;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (135, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version=135;`); err != nil {
				return err
			}
			return transaction.Commit()
		}(); err != nil {
			return err
		}
	}
	// Breaking: older daemons cannot decode the writing_brief review phase, a seed's
	// brief, or a brief-writer registration in a stored active review group.
	// Refuse their reads and writes before any can be stored.
	if version < 136 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 136 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (136, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 136;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: finished review output is durable data that an older daemon
	// cannot preserve. It settles reviewers, clears the active group and
	// prunes their handbacks without writing `review_reports`, which would
	// lose a review's output irrecoverably. Refuse its reads and writes before
	// such a review can be lost; a store written by a newer daemon raises this
	// floor further still. The table itself is standalone; the floor is raised
	// for this preservation reason only. The trigger removes a session's
	// reports with the session itself, so a destroyed session cannot leave
	// unreachable handbacks behind. A later migration that recreates `sessions`
	// drops this trigger and must create it again.
	if version < 137 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS review_reports (
                 parent_session_id TEXT NOT NULL,
                 review_id TEXT NOT NULL,
                 role TEXT NOT NULL,
                 ordinal INTEGER NOT NULL,
                 command_id TEXT NOT NULL,
                 child_session_id TEXT,
                 profile TEXT NOT NULL DEFAULT '',
                 model TEXT,
                 effort TEXT,
                 handback TEXT,
                 verdict TEXT,
                 finished_at_ms INTEGER,
                 resolution TEXT,
                 updated_at_ms INTEGER NOT NULL,
                 PRIMARY KEY(parent_session_id, review_id, role)
             );
             CREATE INDEX IF NOT EXISTS review_reports_finished
                 ON review_reports(parent_session_id, finished_at_ms DESC);
             CREATE TRIGGER IF NOT EXISTS review_reports_session_destroyed
                 AFTER DELETE ON sessions BEGIN
                     DELETE FROM review_reports WHERE parent_session_id = OLD.session_id;
                 END;
             UPDATE schema_compatibility SET minimum_compatible_version = 137 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (137, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 137;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Compatible: older daemons neither read nor write the new push ledger.
	// Their episode tables remain available and the compatibility floor stays put.
	if version < 138 {
		if _, err := connection.Exec("BEGIN IMMEDIATE;" + webPushEntriesSQL +
			`INSERT INTO schema_migrations(version, applied_at)
                 VALUES (138, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 138;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Compatible: older readers and writers leave the independent alert ledger intact.
	if version < 139 {
		if _, err := connection.Exec("BEGIN IMMEDIATE;" + alertEventsSQL +
			`INSERT INTO schema_migrations(version, applied_at)
                 VALUES (139, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version = 139;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Compatible: older session deletion also runs this trigger; the indexes only speed
	// its lookups, and no existing data shape changes.
	if version < 140 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
            CREATE INDEX IF NOT EXISTS push_entries_session ON push_entries(session);
            CREATE TRIGGER IF NOT EXISTS alert_events_session_destroyed AFTER DELETE ON sessions BEGIN
                DELETE FROM alert_events WHERE destination=OLD.session_id OR identity IN
                    (SELECT identity FROM push_entries WHERE session=OLD.session_id);
            END;
            INSERT INTO schema_migrations(version,applied_at) VALUES(140,strftime('%Y-%m-%dT%H:%M:%fZ','now'));
            PRAGMA user_version=140;
            COMMIT;`); err != nil {
			return err
		}
	}
	// Compatible: these non-unique indexes change only access paths. Older
	// daemons do not name the replaced index, and SQLite maintains the new
	// indexes on their writes. Constraints and stored data are unchanged.
	if version < 141 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE INDEX IF NOT EXISTS environment_workers_environment
                 ON environment_workers(environment_id, worker_session_id, lease_id);

             -- Session deletion also runs environment and watch cleanup.
             CREATE INDEX IF NOT EXISTS api_idempotency_session
                 ON api_idempotency(session_id);
             CREATE INDEX IF NOT EXISTS environments_owner
                 ON environments(owner_session_id, phase);
             CREATE INDEX IF NOT EXISTS environments_target_lease
                 ON environments(target_lease_id) WHERE target_lease_id IS NOT NULL;
             CREATE INDEX IF NOT EXISTS environment_bindings_environment
                 ON environment_bindings(environment_id);
             CREATE INDEX IF NOT EXISTS retained_move_sources_environment
                 ON retained_move_sources(environment_id) WHERE environment_id IS NOT NULL;
             CREATE INDEX IF NOT EXISTS session_moves_source_environment
                 ON session_moves(source_environment_id) WHERE source_environment_id IS NOT NULL;
             CREATE INDEX IF NOT EXISTS github_watch_items_creator
                 ON github_watch_items(creator_session_id);
             CREATE INDEX IF NOT EXISTS github_watch_classifications_session
                 ON github_watch_classifications(session_id);
             CREATE INDEX IF NOT EXISTS github_watch_classification_failures_session
                 ON github_watch_classification_failures(session_id);

             CREATE INDEX IF NOT EXISTS mailbox_outbox_subagent_messages
                 ON mailbox_outbox(target_session_id) WHERE event_key LIKE 'subagent-message-%';
             -- Only unaccepted rows are polled, in per-target delivery order.
             DROP INDEX IF EXISTS mailbox_outbox_pending;
             CREATE INDEX IF NOT EXISTS mailbox_outbox_pending_target
                 ON mailbox_outbox(target_session_id, created_at, event_key)
                 WHERE accepted_at IS NULL;
             CREATE INDEX IF NOT EXISTS materialized_sessions_pending_elicitations
                 ON materialized_sessions(session_id) WHERE pending_elicitations_json != '[]';

             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (141, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 141;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: checkpoint archives now carry the worker's accepted,
	// undelivered mail in their canonical session (archive schema 13). Older
	// daemons reject that field, so they could not resume a session this
	// daemon suspended, and the mail the daemon already marked accepted would
	// be lost with it. Refuse their reads and writes. No table changes.
	if version < 142 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 142 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (142, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 142;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: saved permission forms can name their tool call. Older
	// readers reject the new field and cannot replay these forms.
	if version < 143 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 143 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (143, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 143;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: older archive readers cannot restore an origin-delta archive
	// after suspend removes its independent clone.
	if version < 144 {
		if err := migrateOriginDeltaArchives(connection); err != nil {
			return err
		}
	}
	// Breaking: a session's review state holds a list of open reviews
	// (`open_reviews`) and a list of findings records (`push_findings`), so
	// reviews of disjoint worktrees can run at the same time. The single
	// `active` and `active_group` columns move into `open_reviews`, and
	// `push_findings` becomes a JSON array. Older daemons read the removed
	// columns and a single findings object, so they would see no open review
	// and could start a second review over a restored one.
	if version < 145 {
		// A store that already has the list shape only needs the revision.
		convert := ""
		if has, err := tableHasColumn(connection, "turn_review_state", "active"); err != nil {
			return err
		} else if has {
			convert = `ALTER TABLE turn_review_state ADD COLUMN open_reviews TEXT NOT NULL DEFAULT '[]';
             UPDATE turn_review_state
                 SET open_reviews = json_array(json_object(
                     'active', active,
                     'group', CASE WHEN active_group IS NULL THEN NULL ELSE json(active_group) END
                 ))
                 WHERE active IS NOT NULL OR active_group IS NOT NULL;
             UPDATE turn_review_state
                 SET push_findings = json_array(json(push_findings))
                 WHERE push_findings IS NOT NULL;
             ALTER TABLE turn_review_state DROP COLUMN active;
             ALTER TABLE turn_review_state DROP COLUMN active_group;`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + convert + `
             UPDATE schema_compatibility SET minimum_compatible_version = 145 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (145, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 145;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: checkpoint archives now carry worker-owned native-session-use
	// evidence in their canonical session (archive schema 15). Older daemons
	// reject that field (`deny_unknown_fields`), so they cannot read or resume
	// sessions suspended by this daemon. Refuse their reads and writes. No
	// table changes.
	if version < 146 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             UPDATE schema_compatibility SET minimum_compatible_version = 146 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (146, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 146;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: persists typed suspend consent (`closing_consent`) on `sessions`
	// when entering `Closing`. Older daemons do not read this column, so crash
	// recovery on a pre-147 daemon would checkpoint a `without_checkpoint` close
	// or discard without the user's `Shown` consent list.
	if version < 147 {
		addCol := ""
		if has, err := tableHasColumn(connection, "sessions", "closing_consent"); err != nil {
			return err
		} else if !has {
			addCol = `ALTER TABLE sessions ADD COLUMN closing_consent TEXT CHECK(closing_consent IS NULL OR json_valid(closing_consent));`
		}
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             ` + addCol + `
             UPDATE schema_compatibility SET minimum_compatible_version = 147 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (147, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 147;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Compatible: adds the `host_snapshot_cache` table to track host-side
	// snapshot archive copies for resume reuse, Destroy cleanup, and 48-hour
	// expiry. Older revision-147 daemons ignore the new table and no existing
	// schema or data changes.
	if version < 148 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS host_snapshot_cache (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL,
                 host TEXT NOT NULL,
                 sha256 TEXT NOT NULL,
                 path TEXT NOT NULL,
                 written_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS host_snapshot_cache_host_sha256
             ON host_snapshot_cache(host, sha256);
             CREATE INDEX IF NOT EXISTS host_snapshot_cache_session
             ON host_snapshot_cache(session_id);
             CREATE INDEX IF NOT EXISTS host_snapshot_cache_written_at
             ON host_snapshot_cache(written_at);
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (148, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 148;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Breaking: replaces `MoveOperation` shape (cross-target/in-place phases,
	// transport grants, in-place state) and adds `ec2_move_grants` and
	// `daemon_instance_metadata`. Older daemons decode `MoveOperation` with
	// `deny_unknown_fields` and treat decode failure as no intent, risking
	// overlooking active moves and their grants.
	if version < 149 {
		if _, err := connection.Exec(`BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS ec2_move_grants (
                 security_group_id TEXT NOT NULL,
                 cidr_ip TEXT NOT NULL,
                 port INTEGER NOT NULL,
                 state TEXT NOT NULL,
                 users_json TEXT NOT NULL CHECK(json_valid(users_json)),
                 absent_checks INTEGER NOT NULL DEFAULT 0,
                 attempt_id INTEGER NOT NULL DEFAULT 0,
                 updated_at TEXT NOT NULL,
                 PRIMARY KEY (security_group_id, cidr_ip, port)
             );
             CREATE TABLE IF NOT EXISTS daemon_instance_metadata (
                 singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                 instance_id TEXT NOT NULL,
                 created_at TEXT NOT NULL
             );
             UPDATE schema_compatibility SET minimum_compatible_version = 149 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (149, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 149;
             COMMIT;`); err != nil {
			return err
		}
	}
	// Compatible: older readers ignore the new nullable columns. Older writers
	// name columns explicitly, so a category this build records survives their
	// inserts and updates. The compatibility floor stays at 149.
	if version < 150 {
		if err := func() error {
			transaction, err := connection.BeginWith(TransactionImmediate)
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			if has, err := tableHasColumn(transaction, "session_turn_selections", "category"); err != nil {
				return err
			} else if !has {
				if _, err := transaction.Exec(`ALTER TABLE session_turn_selections ADD COLUMN category TEXT;`); err != nil {
					return err
				}
			}
			if has, err := tableHasColumn(transaction, "subagent_accounting", "category"); err != nil {
				return err
			} else if !has {
				if _, err := transaction.Exec(`ALTER TABLE subagent_accounting ADD COLUMN category TEXT;`); err != nil {
					return err
				}
			}
			if _, err := transaction.Exec(`INSERT INTO schema_migrations(version, applied_at)
                 VALUES (150, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version=150;`); err != nil {
				return err
			}
			return transaction.Commit()
		}(); err != nil {
			return err
		}
	}
	// Breaking: older writers drop `sessions.supervisors` on update and older
	// readers reject `supervisor: true` in `subagent_sessions.record_json` due
	// to `deny_unknown_fields`. Raise `minimum_compatible_version` to 151 in
	// the same transaction.
	if version < 151 {
		if err := func() error {
			transaction, err := connection.BeginWith(TransactionImmediate)
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			if has, err := tableHasColumn(transaction, "sessions", "supervisors"); err != nil {
				return err
			} else if !has {
				if _, err := transaction.Exec(`ALTER TABLE sessions ADD COLUMN supervisors INTEGER NOT NULL DEFAULT 0;`); err != nil {
					return err
				}
			}
			if _, err := transaction.Exec(`CREATE TABLE IF NOT EXISTS supervisor_preference (
                 profile_id TEXT PRIMARY KEY,
                 enabled INTEGER NOT NULL
             );
             UPDATE schema_compatibility
                 SET minimum_compatible_version = 151
                 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (151, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version=151;`); err != nil {
				return err
			}
			if has, err := tableHasColumn(transaction, "session_contexts", "profile_id"); err != nil {
				return err
			} else if !has {
				if _, err := transaction.Exec(`ALTER TABLE session_contexts ADD COLUMN profile_id TEXT;`); err != nil {
					return err
				}
			}
			if _, err := transaction.Exec(`UPDATE session_contexts
                 SET profile_id = (SELECT last_profile FROM sessions
                                   WHERE sessions.session_id = session_contexts.session_id)
                 WHERE profile_id IS NULL;`); err != nil {
				return err
			}
			return transaction.Commit()
		}(); err != nil {
			return err
		}
	}
	// Breaking: revision-151 placement writes would rebind a target-bearing
	// owned child to its parent's environment. Older strict record_json readers
	// also reject resume_with_parent. Fold the legacy supervisor switches into
	// the Mjolnir policy without removing the old schema or runtime switches.
	if version < 152 {
		if err := func() error {
			transaction, err := connection.BeginWith(TransactionImmediate)
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			if err := migrateOrchestratorPolicies(transaction); err != nil {
				return err
			}
			if _, err := transaction.Exec(`UPDATE schema_compatibility SET minimum_compatible_version = ?1 WHERE singleton = 1`, 152); err != nil {
				return err
			}
			if _, err := transaction.Exec(`INSERT INTO schema_migrations(version, applied_at)
                 VALUES (152, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version=152;`); err != nil {
				return err
			}
			return transaction.Commit()
		}(); err != nil {
			return err
		}
	}
	// Revision 153 is breaking: older readers cannot resolve spilled bodies,
	// and older writers can replace the body without clearing its detail pointer.
	if version < 153 {
		if err := func() error {
			transaction, err := connection.BeginWith(TransactionImmediate)
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			if _, err := transaction.Exec(`ALTER TABLE materialized_transcript_items ADD COLUMN detail_offset INTEGER;
             ALTER TABLE materialized_transcript_items ADD COLUMN detail_len INTEGER;
             ALTER TABLE materialized_transcript_items ADD COLUMN item_kind TEXT NOT NULL DEFAULT '';
             ALTER TABLE materialized_transcript_items ADD COLUMN settled INTEGER NOT NULL DEFAULT 1;
             ALTER TABLE materialized_transcript_items ADD COLUMN update_ordinal INTEGER NOT NULL DEFAULT 0;
             UPDATE materialized_transcript_items
             SET update_ordinal = COALESCE(latest_content_event_ordinal, position),
                 item_kind = CASE WHEN json_valid(body_json)
                     THEN COALESCE(json_extract(body_json, '$.kind'), '') ELSE '' END,
                 settled = CASE
                     WHEN NOT json_valid(body_json) THEN 1
                     WHEN json_extract(body_json, '$.kind') IN ('agent', 'thought')
                          AND COALESCE(json_extract(body_json, '$.streaming'), 0) != 0 THEN 0
                     WHEN json_extract(body_json, '$.kind') = 'tool'
                          AND COALESCE(json_extract(body_json, '$.call.status'), 'pending')
                              IN ('pending', 'in_progress') THEN 0
                     ELSE 1
                 END;
             CREATE INDEX materialized_transcript_update_ordinal
                 ON materialized_transcript_items (session_id, update_ordinal, position, stable_id);
             CREATE INDEX materialized_transcript_unsettled
                 ON materialized_transcript_items (session_id, position, stable_id) WHERE settled = 0;
             UPDATE schema_compatibility
                 SET minimum_compatible_version = 153 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (153, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
             PRAGMA user_version=153;`); err != nil {
				return err
			}
			return transaction.Commit()
		}(); err != nil {
			return err
		}
	}

	/// Trigger body that removes a gone session's rows. `gone` is `OLD` or `NEW`.
	///
	/// A watch the session created keeps going with no creator while another live
	/// session is interested in the item, and a creator-less watch the session was
	/// following is deleted with its last follower. The classification rows are
	/// deleted last because the follower tests read them.
	var recorded sql.NullInt64
	if err := connection.QueryRow("SELECT max(version) FROM schema_migrations").Scan(&recorded); err != nil {
		return err
	}
	if recorded.Valid && recorded.Int64 == schemaVersion && version < schemaVersion {
		slog.Info("database migrations applied",
			"from_revision", version,
			"to_revision", schemaVersion)
	}
	if !recorded.Valid || recorded.Int64 != schemaVersion {
		ledger := "None"
		if recorded.Valid {
			ledger = fmt.Sprintf("Some(%d)", recorded.Int64)
		}
		return fmt.Errorf("Mjolnir database migration ledger %s does not match schema %d", ledger, schemaVersion)
	}
	return nil
}

// goneSessionRowsSQL is the trigger body that removes a gone session's rows.
// gone is `OLD` or `NEW`.
//
// A watch the session created keeps going with no creator while another
// live session is interested in the item, and a creator-less watch the
// session was following is deleted with its last follower. The
// classification rows are deleted last because the follower tests read them.
func goneSessionRowsSQL(gone string) string {
	id := gone + ".session_id"
	otherLiveFollower := `EXISTS (SELECT 1 FROM github_watch_classifications c
                     JOIN sessions s ON s.session_id = c.session_id
                     WHERE c.owner = github_watch_items.owner
                       AND c.repo = github_watch_items.repo
                       AND c.number = github_watch_items.number
                       AND c.interested = 1 AND c.created = 0
                       AND c.session_id <> ` + id + `
                       AND s.state <> 'destroyed-with-data-loss')`
	return `DELETE FROM mailbox_outbox WHERE target_session_id = ` + id + `;
             DELETE FROM github_watch_items
                 WHERE (creator_session_id = ` + id + `
                        OR (creator_session_id IS NULL
                            AND EXISTS (SELECT 1 FROM github_watch_classifications f
                                        WHERE f.owner = github_watch_items.owner
                                          AND f.repo = github_watch_items.repo
                                          AND f.number = github_watch_items.number
                                          AND f.session_id = ` + id + `)))
                   AND NOT ` + otherLiveFollower + `;
             UPDATE github_watch_items SET creator_session_id = NULL WHERE creator_session_id = ` + id + `;
             DELETE FROM github_watch_classifications WHERE session_id = ` + id + `;
             DELETE FROM github_watch_classification_failures WHERE session_id = ` + id + `;`
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
	transaction, err := connection.Begin()
	if err != nil {
		return err
	}
	defer transaction.Rollback()
	deleted := goneSessionRowsSQL("OLD")
	destroyed := goneSessionRowsSQL("NEW")
	if _, err := transaction.Exec(`DROP TRIGGER IF EXISTS session_rows_deleted;
         DROP TRIGGER IF EXISTS session_rows_destroyed;
         DROP TRIGGER IF EXISTS github_watch_items_live_creator;
         CREATE TABLE github_watch_items_v130 (
             owner TEXT NOT NULL,
             repo TEXT NOT NULL,
             number INTEGER NOT NULL,
             creator_session_id TEXT,
             kind TEXT NOT NULL CHECK(kind IN ('issue', 'pull_request')),
             title TEXT NOT NULL,
             url TEXT NOT NULL,
             created_at TEXT NOT NULL,
             timeline_at TEXT NOT NULL DEFAULT '',
             timeline_id INTEGER NOT NULL DEFAULT 0,
             review_comments_at TEXT NOT NULL DEFAULT '',
             review_comments_id INTEGER NOT NULL DEFAULT 0,
             processed_at TEXT NOT NULL DEFAULT '',
             lifecycle_event_key TEXT,
             lifecycle_event_json TEXT CHECK(lifecycle_event_json IS NULL OR json_valid(lifecycle_event_json)),
             lifecycle_at TEXT,
             PRIMARY KEY(owner, repo, number)
         ) STRICT;
         INSERT INTO github_watch_items_v130(owner, repo, number, creator_session_id, kind,
                 title, url, created_at, timeline_at, timeline_id, review_comments_at,
                 review_comments_id, processed_at)
             SELECT owner, repo, number, creator_session_id, kind, title, url, created_at,
                    timeline_at, timeline_id, review_comments_at, review_comments_id, processed_at
             FROM github_watch_items;
         DROP TABLE github_watch_items;
         ALTER TABLE github_watch_items_v130 RENAME TO github_watch_items;
         CREATE TRIGGER session_rows_deleted AFTER DELETE ON sessions BEGIN
             ` + deleted + `
         END;
         CREATE TRIGGER session_rows_destroyed
             AFTER UPDATE OF state ON sessions
             WHEN NEW.state = 'destroyed-with-data-loss' BEGIN
             ` + destroyed + `
         END;
         CREATE TRIGGER github_watch_items_live_creator
             BEFORE INSERT ON github_watch_items
             WHEN NEW.creator_session_id IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM sessions WHERE session_id = NEW.creator_session_id AND state <> 'destroyed-with-data-loss')
             BEGIN SELECT RAISE(IGNORE); END;
         INSERT INTO github_watch_items(owner, repo, number, creator_session_id, kind, title, url,
                 created_at, timeline_at, timeline_id, review_comments_at, review_comments_id, processed_at)
             SELECT owner, repo, number, NULL, kind, title, url, MIN(created_at),
                    strftime('%Y-%m-%dT%H:%M:%SZ','now'), 0,
                    strftime('%Y-%m-%dT%H:%M:%SZ','now'), 0,
                    strftime('%Y-%m-%dT%H:%M:%SZ','now')
             FROM github_watch_classifications c
             WHERE kind = 'pull_request' AND interested = 1 AND created = 0
               AND NOT EXISTS (SELECT 1 FROM github_watch_items w
                               WHERE w.owner = c.owner AND w.repo = c.repo AND w.number = c.number)
             GROUP BY owner, repo, number;
         UPDATE schema_compatibility SET minimum_compatible_version = 130 WHERE singleton = 1;
         INSERT INTO schema_migrations(version, applied_at)
             VALUES (130, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
         PRAGMA user_version = 130;`); err != nil {
		return err
	}
	return transaction.Commit()
}

// schemaWithoutForeignKeys runs migration with foreign key enforcement off,
// as the table rebuilds need, and turns it back on whatever happened.
// migrationContext and restoreContext are the messages Rust's anyhow
// context gives each failure; an empty migrationContext adds none.
func schemaWithoutForeignKeys(connection *Connection, migrationContext string, restoreContext string, migration func() error) error {
	if _, err := connection.Exec("PRAGMA foreign_keys = OFF;"); err != nil {
		return err
	}
	migrated := migration()
	_, restored := connection.Exec("PRAGMA foreign_keys = ON;")
	if migrated != nil {
		if migrationContext == "" {
			return migrated
		}
		return fmt.Errorf("%s: %w", migrationContext, migrated)
	}
	if restored != nil {
		return fmt.Errorf("%s: %w", restoreContext, restored)
	}
	return nil
}

// schemaForeignKeysHold is `!PRAGMA foreign_key_check.exists()`.
func schemaForeignKeysHold(transaction *Transaction) (bool, error) {
	violated, err := schemaRowExists(transaction, "PRAGMA foreign_key_check")
	return !violated, err
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
	const kinds = `'local-bare','local-podman','local-docker','apple-container','aws-ec2','ssh-bare','ssh-podman','ssh-docker','ssh-apple-container','ssh-nitro'`
	return schemaWithoutForeignKeys(connection,
		"migrate the target-kind constraints for Nitro sandboxes",
		"restore foreign key enforcement after the Nitro target-kind migration",
		func() error {
			transaction, err := connection.Begin()
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			if _, err := transaction.Exec(`CREATE TABLE session_targets_v124 (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                 kind TEXT NOT NULL CHECK(kind IN (` + kinds + `)),
                 host TEXT,
                 resource_id TEXT,
                 address TEXT,
                 workspace BLOB,
                 worker_id TEXT,
                 workspace_storage TEXT,
                 borrowed_from TEXT,
                 CHECK(
                     (kind = 'local-bare' AND workspace IS NOT NULL
                      AND host IS NULL AND resource_id IS NULL AND address IS NULL AND worker_id IS NULL)
                  OR (kind IN ('local-podman','local-docker','apple-container') AND resource_id IS NOT NULL
                      AND host IS NULL AND address IS NULL AND workspace IS NULL AND worker_id IS NULL)
                  OR (kind = 'aws-ec2' AND resource_id IS NOT NULL
                      AND host IS NULL AND workspace IS NULL AND worker_id IS NULL)
                  OR (kind = 'ssh-bare' AND host IS NOT NULL AND workspace IS NOT NULL
                      AND resource_id IS NULL AND address IS NULL)
                  OR (kind IN ('ssh-podman','ssh-docker','ssh-apple-container','ssh-nitro') AND host IS NOT NULL AND resource_id IS NOT NULL
                      AND address IS NULL AND workspace IS NULL AND worker_id IS NULL)
                 )
             ) STRICT;
             INSERT INTO session_targets_v124(session_id, kind, host, resource_id, address, workspace, worker_id, workspace_storage, borrowed_from)
                 SELECT session_id, kind, host, resource_id, address, workspace, worker_id, workspace_storage, borrowed_from
                 FROM session_targets;
             DROP TABLE session_targets;
             ALTER TABLE session_targets_v124 RENAME TO session_targets;
             CREATE TABLE environment_workers_v124 (
                 lease_id TEXT PRIMARY KEY,
                 environment_id TEXT NOT NULL REFERENCES environments(environment_id) ON DELETE CASCADE,
                 worker_session_id TEXT NOT NULL,
                 kind TEXT NOT NULL CHECK(kind IN (` + kinds + `)),
                 host TEXT,
                 resource_id TEXT,
                 address TEXT,
                 workspace BLOB,
                 worker_id TEXT,
                 workspace_storage TEXT,
                 borrowed_from TEXT,
                 target_template_id TEXT NOT NULL,
                 runtime_json TEXT CHECK(runtime_json IS NULL OR json_valid(runtime_json)),
                 CHECK((kind='local-bare' AND workspace IS NOT NULL)
                     OR (kind='ssh-bare' AND host IS NOT NULL AND workspace IS NOT NULL)
                     OR (kind IN ('local-podman','local-docker','apple-container','aws-ec2') AND resource_id IS NOT NULL)
                     OR (kind IN ('ssh-podman','ssh-docker','ssh-apple-container','ssh-nitro') AND host IS NOT NULL AND resource_id IS NOT NULL))
             ) STRICT;
             INSERT INTO environment_workers_v124(lease_id, environment_id, worker_session_id, kind, host, resource_id, address, workspace, worker_id, workspace_storage, borrowed_from, target_template_id, runtime_json)
                 SELECT lease_id, environment_id, worker_session_id, kind, host, resource_id, address, workspace, worker_id, workspace_storage, borrowed_from, target_template_id, runtime_json
                 FROM environment_workers;
             DROP TABLE environment_workers;
             ALTER TABLE environment_workers_v124 RENAME TO environment_workers;
             CREATE INDEX environment_worker_identity ON environment_workers(worker_session_id);`); err != nil {
				return err
			}
			if hold, err := schemaForeignKeysHold(transaction); err != nil {
				return err
			} else if !hold {
				return errors.New("foreign key violation in the Nitro target-kind migration")
			}
			if _, err := transaction.Exec(`UPDATE schema_compatibility SET minimum_compatible_version = 124
                 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (124, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 124;`); err != nil {
				return err
			}
			return transaction.Commit()
		})
}

// migrateParkedSessionState is migration 53: rebuild `sessions` so its
// `state` constraint admits `'parked'`. SQLite cannot alter a CHECK
// constraint, so the table is copied into one declared with the new
// constraint, the way migration 32 (`migrate_zcode_harness_kind`) did. The
// table's own stored definition is edited, so every column later migrations
// added is kept as it is. A store whose constraint already admits the value
// is only stamped.
func migrateParkedSessionState(connection *Connection) error {
	const before = "'stopped','lost',"
	const after = "'stopped','parked','lost',"
	return schemaWithoutForeignKeys(connection,
		"migrate the sessions state constraint for parked sub-agents",
		"restore foreign key enforcement after the parked-state migration",
		func() error {
			transaction, err := connection.Begin()
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			var sql string
			if err := transaction.QueryRow(`SELECT sql FROM sqlite_schema WHERE type='table' AND name='sessions'`).Scan(&sql); err != nil {
				return err
			}
			_, definition, found := strings.Cut(sql, "(")
			if !found {
				return errors.New("missing sessions table definition")
			}
			if !strings.Contains(definition, after) {
				if strings.Count(definition, before) != 1 {
					return errors.New("unexpected sessions state constraint")
				}
				definition = strings.ReplaceAll(definition, before, after)
				objects, err := schemaObjects(transaction, `SELECT sql FROM sqlite_schema WHERE tbl_name='sessions'
                     AND type IN ('index','trigger') AND sql IS NOT NULL`)
				if err != nil {
					return err
				}
				if _, err := transaction.Exec(`CREATE TABLE sessions_parked_v53 (` + definition + `;
                 INSERT INTO sessions_parked_v53 SELECT * FROM sessions;
                 DROP TABLE sessions;
                 ALTER TABLE sessions_parked_v53 RENAME TO sessions;`); err != nil {
					return err
				}
				for _, object := range objects {
					if _, err := transaction.Exec(object); err != nil {
						return err
					}
				}
				if hold, err := schemaForeignKeysHold(transaction); err != nil {
					return err
				} else if !hold {
					return errors.New("foreign key violation in the parked-state migration")
				}
			}
			if _, err := transaction.Exec(`UPDATE schema_compatibility SET minimum_compatible_version = 53
                 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (53, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 53;`); err != nil {
				return err
			}
			return transaction.Commit()
		})
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
	return schemaWithoutForeignKeys(connection,
		"migrate OpenCode harness constraints",
		"restore foreign key enforcement after OpenCode migration",
		func() error {
			transaction, err := connection.Begin()
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			for _, table := range []string{"sessions", "hidden_native_sessions"} {
				var sql string
				if err := transaction.QueryRow(`SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1`, table).Scan(&sql); err != nil {
					return err
				}
				_, definition, found := strings.Cut(sql, "(")
				if !found {
					return errors.New("missing harness table definition")
				}
				// Later revisions append values after OpenCode, so recognize the
				// value rather than requiring it to remain the final CHECK member.
				if strings.Contains(definition, "'opencode'") {
					continue
				}
				if !strings.Contains(definition, "'zcode')") {
					return fmt.Errorf("unexpected %s harness constraint", table)
				}
				definition = strings.ReplaceAll(definition, "'zcode')", "'zcode','opencode')")
				objects, err := schemaObjects(transaction, `SELECT sql FROM sqlite_schema WHERE tbl_name=?1
                     AND type IN ('index','trigger') AND sql IS NOT NULL`, table)
				if err != nil {
					return err
				}
				if _, err := transaction.Exec(`CREATE TABLE ` + table + `_opencode_v74 (` + definition + `;
                 INSERT INTO ` + table + `_opencode_v74 SELECT * FROM ` + table + `;
                 DROP TABLE ` + table + `;
                 ALTER TABLE ` + table + `_opencode_v74 RENAME TO ` + table + `;`); err != nil {
					return err
				}
				for _, object := range objects {
					if _, err := transaction.Exec(object); err != nil {
						return err
					}
				}
			}
			if hold, err := schemaForeignKeysHold(transaction); err != nil {
				return err
			} else if !hold {
				return errors.New("foreign key violation in OpenCode migration")
			}
			if _, err := transaction.Exec(`UPDATE schema_compatibility SET minimum_compatible_version = 74
                 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (74, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 74;`); err != nil {
				return err
			}
			return transaction.Commit()
		})
}

// migrateMimoHarnessKind is migration 90: admit `'mimo'` in the
// harness-kind constraints of `sessions` and `hidden_native_sessions`.
//
// Breaking: older daemons do not understand the new harness value, and the
// current load/save model can discard a session they cannot represent. The
// compatibility floor therefore rises to this revision.
func migrateMimoHarnessKind(connection *Connection) error {
	return schemaWithoutForeignKeys(connection,
		"migrate MiMo Code harness constraints",
		"restore foreign key enforcement after MiMo Code migration",
		func() error {
			transaction, err := connection.Begin()
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			for _, table := range []string{"sessions", "hidden_native_sessions"} {
				var sql string
				if err := transaction.QueryRow(`SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1`, table).Scan(&sql); err != nil {
					return err
				}
				_, definition, found := strings.Cut(sql, "(")
				if !found {
					return errors.New("missing harness table definition")
				}
				// Later revisions append values after MiMo Code, so recognize the
				// value rather than requiring it to remain the final CHECK member.
				if strings.Contains(definition, "'mimo'") {
					continue
				}
				if !strings.Contains(definition, "'opencode')") {
					return fmt.Errorf("unexpected %s harness constraint", table)
				}
				definition = strings.ReplaceAll(definition, "'opencode')", "'opencode','mimo')")
				objects, err := schemaObjects(transaction, `SELECT sql FROM sqlite_schema WHERE tbl_name=?1
                     AND type IN ('index','trigger') AND sql IS NOT NULL`, table)
				if err != nil {
					return err
				}
				if _, err := transaction.Exec(`CREATE TABLE ` + table + `_mimo_v90 (` + definition + `;
                 INSERT INTO ` + table + `_mimo_v90 SELECT * FROM ` + table + `;
                 DROP TABLE ` + table + `;
                 ALTER TABLE ` + table + `_mimo_v90 RENAME TO ` + table + `;`); err != nil {
					return err
				}
				for _, object := range objects {
					if _, err := transaction.Exec(object); err != nil {
						return err
					}
				}
			}
			if hold, err := schemaForeignKeysHold(transaction); err != nil {
				return err
			} else if !hold {
				return errors.New("foreign key violation in MiMo Code migration")
			}
			if _, err := transaction.Exec(`UPDATE schema_compatibility SET minimum_compatible_version = 90
                 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (90, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 90;`); err != nil {
				return err
			}
			return transaction.Commit()
		})
}

// admitHarnessKind admits added in the harness-kind constraints of
// `sessions` and `hidden_native_sessions`, after previous, the value the
// constraint currently ends with, and records revision as a breaking
// migration.
//
// SQLite cannot alter a CHECK constraint, so each table is rebuilt from its
// stored definition, keeping every shipped column, index and trigger. A
// table that already admits added is left alone.
func admitHarnessKind(connection *Connection, previous string, added string, revisionNumber int64) error {
	revision := strconv.FormatInt(revisionNumber, 10)
	return schemaWithoutForeignKeys(connection,
		"",
		"restore foreign key enforcement after a harness-kind migration",
		func() error {
			transaction, err := connection.Begin()
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			for _, table := range []string{"sessions", "hidden_native_sessions"} {
				var sql string
				if err := transaction.QueryRow(`SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1`, table).Scan(&sql); err != nil {
					return err
				}
				_, definition, found := strings.Cut(sql, "(")
				if !found {
					return errors.New("missing harness table definition")
				}
				if strings.Contains(definition, "'"+added+"'") {
					continue
				}
				last := "'" + previous + "')"
				if strings.Count(definition, last) != 1 {
					return fmt.Errorf("unexpected %s harness constraint", table)
				}
				definition = strings.ReplaceAll(definition, last, "'"+previous+"','"+added+"')")
				objects, err := schemaObjects(transaction, `SELECT sql FROM sqlite_schema WHERE tbl_name=?1
                     AND type IN ('index','trigger') AND sql IS NOT NULL`, table)
				if err != nil {
					return err
				}
				if _, err := transaction.Exec(`CREATE TABLE ` + table + `_` + added + `_v` + revision + ` (` + definition + `;
                 INSERT INTO ` + table + `_` + added + `_v` + revision + ` SELECT * FROM ` + table + `;
                 DROP TABLE ` + table + `;
                 ALTER TABLE ` + table + `_` + added + `_v` + revision + ` RENAME TO ` + table + `;`); err != nil {
					return err
				}
				for _, object := range objects {
					if _, err := transaction.Exec(object); err != nil {
						return err
					}
				}
			}
			if hold, err := schemaForeignKeysHold(transaction); err != nil {
				return err
			} else if !hold {
				return fmt.Errorf("foreign key violation while admitting harness %s", added)
			}
			if _, err := transaction.Exec(`UPDATE schema_compatibility SET minimum_compatible_version = ` + revision + `
                 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (` + revision + `, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = ` + revision + `;`); err != nil {
				return err
			}
			return transaction.Commit()
		})
}

func removeHarnessConstraints(connection *Connection) error {
	var legacyAlterTable bool
	if err := connection.QueryRow("PRAGMA legacy_alter_table").Scan(&legacyAlterTable); err != nil {
		return err
	}
	if _, err := connection.Exec("PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON;"); err != nil {
		return err
	}
	migrated := func() error {
		transaction, err := connection.Begin()
		if err != nil {
			return err
		}
		defer transaction.Rollback()
		for _, table := range []string{"sessions", "hidden_native_sessions"} {
			var sql string
			if err := transaction.QueryRow(`SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1`, table).Scan(&sql); err != nil {
				return err
			}
			_, definition, found := strings.Cut(sql, "(")
			if !found {
				return errors.New("missing harness table definition")
			}
			start := strings.Index(definition, " CHECK(harness_kind IN (")
			if start < 0 {
				continue
			}
			length := strings.Index(definition[start:], "))")
			if length < 0 {
				return errors.New("unterminated harness constraint")
			}
			end := start + length + 2
			definition = definition[:start] + definition[end:]
			objects, err := schemaObjects(transaction, `SELECT sql FROM sqlite_schema WHERE tbl_name=?1
                     AND type IN ('index','trigger') AND sql IS NOT NULL`, table)
			if err != nil {
				return err
			}
			if _, err := transaction.Exec(`CREATE TABLE ` + table + `_unconstrained_v126 (` + definition + `;
                 INSERT INTO ` + table + `_unconstrained_v126 SELECT * FROM ` + table + `;
                 DROP TABLE ` + table + `;
                 ALTER TABLE ` + table + `_unconstrained_v126 RENAME TO ` + table + `;`); err != nil {
				return err
			}
			for _, object := range objects {
				if _, err := transaction.Exec(object); err != nil {
					return err
				}
			}
		}
		if hold, err := schemaForeignKeysHold(transaction); err != nil {
			return err
		} else if !hold {
			return errors.New("foreign key violation while removing harness constraints")
		}
		if _, err := transaction.Exec(`UPDATE schema_compatibility SET minimum_compatible_version = 126
                 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (126, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 126;`); err != nil {
			return err
		}
		return transaction.Commit()
	}()
	_, restored := connection.Exec("PRAGMA foreign_keys = ON;")
	restoredAlter := "PRAGMA legacy_alter_table = 0"
	if legacyAlterTable {
		restoredAlter = "PRAGMA legacy_alter_table = 1"
	}
	_, restoredAlterErr := connection.Exec(restoredAlter)
	if migrated != nil {
		return migrated
	}
	if restoredAlterErr != nil {
		return fmt.Errorf("restore table rename behavior after harness migration: %w", restoredAlterErr)
	}
	if restored != nil {
		return fmt.Errorf("restore foreign key enforcement after a harness-kind migration: %w", restored)
	}
	return nil
}

// migrateStartupCleanupState rebuilds from the stored definition so every
// shipped column and trigger survives.
func migrateStartupCleanupState(connection *Connection) error {
	const before = "'stopped','parked','lost',"
	const after = "'stopped','parked','startup-cleanup','lost',"
	return schemaWithoutForeignKeys(connection,
		"migrate the sessions state constraint for failed startup cleanup",
		"restore foreign key enforcement after the startup-cleanup migration",
		func() error {
			transaction, err := connection.Begin()
			if err != nil {
				return err
			}
			defer transaction.Rollback()
			var sql string
			if err := transaction.QueryRow(`SELECT sql FROM sqlite_schema WHERE type='table' AND name='sessions'`).Scan(&sql); err != nil {
				return err
			}
			_, definition, found := strings.Cut(sql, "(")
			if !found {
				return errors.New("missing sessions table definition")
			}
			if !strings.Contains(definition, after) {
				if strings.Count(definition, before) != 1 {
					return errors.New("unexpected sessions state constraint")
				}
				definition = strings.ReplaceAll(definition, before, after)
				objects, err := schemaObjects(transaction, `SELECT sql FROM sqlite_schema WHERE tbl_name='sessions'
                     AND type IN ('index','trigger') AND sql IS NOT NULL`)
				if err != nil {
					return err
				}
				if _, err := transaction.Exec(`CREATE TABLE sessions_startup_cleanup_v68 (` + definition + `;
                 INSERT INTO sessions_startup_cleanup_v68 SELECT * FROM sessions;
                 DROP TABLE sessions;
                 ALTER TABLE sessions_startup_cleanup_v68 RENAME TO sessions;`); err != nil {
					return err
				}
				for _, object := range objects {
					if _, err := transaction.Exec(object); err != nil {
						return err
					}
				}
				if hold, err := schemaForeignKeysHold(transaction); err != nil {
					return err
				} else if !hold {
					return errors.New("foreign key violation in the startup-cleanup migration")
				}
			}
			if _, err := transaction.Exec(`UPDATE schema_compatibility SET minimum_compatible_version = 68
                 WHERE singleton = 1;
             INSERT INTO schema_migrations(version, applied_at)
                 VALUES (68, strftime('%Y-%m-%dT%H:%M:%fZ','now'));
             PRAGMA user_version = 68;`); err != nil {
				return err
			}
			return transaction.Commit()
		})
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
	if _, err := connection.Exec("BEGIN IMMEDIATE;"); err != nil {
		return err
	}
	created := func() error {
		var version int64
		if err := connection.QueryRow("PRAGMA user_version").Scan(&version); err != nil {
			return err
		}
		if version != 0 {
			return nil
		}
		if _, err := connection.Exec(baselineSQL); err != nil {
			return err
		}
		// Reconstruct the historical revision before traversing its migrations.
		if _, err := connection.Exec(`ALTER TABLE sessions ADD COLUMN container_cpus TEXT;
            ALTER TABLE sessions ADD COLUMN container_memory TEXT;`); err != nil {
			return err
		}
		if _, err := connection.Exec(`INSERT INTO schema_compatibility(singleton, minimum_compatible_version) VALUES (1, ?1)`, baselineMinimumCompatibleVersion); err != nil {
			return err
		}
		if _, err := connection.Exec(`INSERT INTO schema_migrations(version, applied_at)
             VALUES (?1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))`, baselineSchemaVersion); err != nil {
			return err
		}
		_, err := connection.Exec(fmt.Sprintf("PRAGMA user_version = %d", baselineSchemaVersion))
		return err
	}()
	if created == nil {
		if _, err := connection.Exec("COMMIT;"); err != nil {
			return fmt.Errorf("commit baseline database schema: %w", err)
		}
		return nil
	}
	if _, rollback := connection.Exec("ROLLBACK;"); rollback != nil {
		slog.Warn("could not roll back a failed baseline schema", "rollback", rollback)
	}
	return fmt.Errorf("create baseline database schema: %w", created)
}
