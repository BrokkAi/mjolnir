// Port of mj-worker/src/acp/antigravity_tasks.rs.

// Read-only follower for Antigravity's native task state.

package worker

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/binary"
	"errors"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"strings"
	"unicode/utf8"

	// The pure-Go SQLite driver, registered as "sqlite".
	_ "modernc.org/sqlite"
)

// AntigravityRunStatus is a conversation's run status.
type AntigravityRunStatus int

const (
	AntigravityRunStatusUnspecified AntigravityRunStatus = iota
	AntigravityRunStatusIdle
	AntigravityRunStatusRunning
	AntigravityRunStatusCanceling
	AntigravityRunStatusBusy
)

// IsActive reports whether the conversation is doing work.
func (s AntigravityRunStatus) IsActive() bool {
	return s == AntigravityRunStatusRunning || s == AntigravityRunStatusCanceling || s == AntigravityRunStatusBusy
}

func (s AntigravityRunStatus) String() string {
	switch s {
	case AntigravityRunStatusIdle:
		return "Idle"
	case AntigravityRunStatusRunning:
		return "Running"
	case AntigravityRunStatusCanceling:
		return "Canceling"
	case AntigravityRunStatusBusy:
		return "Busy"
	}
	return "Unspecified"
}

// AntigravitySummarySnapshot is a conversation's summary row.
type AntigravitySummarySnapshot struct {
	Status              AntigravityRunStatus
	NotFullyIdle        bool
	Killed              bool
	StepCount           int64
	BackgroundTaskCount uint64
	HasRunningSubagents bool
}

// AntigravityBackgroundTask is a running background task of the
// conversation's trajectory.
type AntigravityBackgroundTask struct {
	StepIndex int64
	ID        string
	Command   string
	// StartedAtMs is nil when the task recorded no start.
	StartedAtMs *int64
}

// AntigravitySnapshot is one read of the conversation's native state.
type AntigravitySnapshot struct {
	// Summary is nil when the summary database holds no row for the
	// conversation.
	Summary               *AntigravitySummarySnapshot
	ConversationDBPresent bool
	SummaryCommitChanged  bool
	// SummaryRowDigest is nil without a summary row.
	SummaryRowDigest *[32]byte
	RunningTasks     []AntigravityBackgroundTask
}

// AntigravityRefreshCursor is how far a follower has read the summary
// database. Copies share its immutable summary.
type AntigravityRefreshCursor struct {
	dataVersion *int64
	summary     *antigravityCursorSummary
}

// antigravityCursorSummary is the cursor's summary row and its digest.
type antigravityCursorSummary struct {
	snapshot AntigravitySummarySnapshot
	digest   [32]byte
}

// AntigravityStateFollower follows one conversation's native state in an
// Antigravity home. Close releases its summary connection (Rust's Drop).
type AntigravityStateFollower struct {
	home           string
	conversationID string
	summary        *antigravityDatabase
	// summaryIdentity is the open summary database's file identity; nil
	// when none is open.
	summaryIdentity *[2]uint64
	cursor          AntigravityRefreshCursor
}

// antigravitySummaryRow is a raw `conversation_summaries` row. The untyped
// columns hold any SQLite value: int64, float64, string, []byte or nil
// (Rust's rusqlite Value).
type antigravitySummaryRow struct {
	status           string
	notFullyIdle     any
	killed           any
	stepCount        int64
	lastModifiedTime any
	// rawSummary is nil for SQL NULL.
	rawSummary *[]byte
}

func (r *antigravitySummaryRow) digest() [32]byte {
	hash := sha256.New()
	var number [8]byte
	writeU64 := func(value uint64) {
		binary.LittleEndian.PutUint64(number[:], value)
		hash.Write(number[:])
	}
	writeBytes := func(bytes []byte) {
		writeU64(uint64(len(bytes)))
		hash.Write(bytes)
	}
	writeValue := func(value any) {
		switch value := value.(type) {
		case nil:
			hash.Write([]byte{0})
		case int64:
			hash.Write([]byte{1})
			writeU64(uint64(value))
		case float64:
			hash.Write([]byte{2})
			writeU64(math.Float64bits(value))
		case string:
			hash.Write([]byte{3})
			writeBytes([]byte(value))
		case []byte:
			hash.Write([]byte{4})
			writeBytes(value)
		default:
			panic(fmt.Sprintf("unexpected SQLite value %T", value))
		}
	}
	writeBytes([]byte(r.status))
	writeValue(r.notFullyIdle)
	writeValue(r.killed)
	writeU64(uint64(r.stepCount))
	writeValue(r.lastModifiedTime)
	if r.rawSummary != nil {
		hash.Write([]byte{1})
		writeBytes(*r.rawSummary)
	} else {
		hash.Write([]byte{0})
	}
	var digest [32]byte
	copy(digest[:], hash.Sum(nil))
	return digest
}

func (r *antigravitySummaryRow) snapshot() (AntigravitySummarySnapshot, error) {
	notFullyIdle, err := antigravityValueBool(r.notFullyIdle)
	if err != nil {
		return AntigravitySummarySnapshot{}, err
	}
	killed, err := antigravityValueBool(r.killed)
	if err != nil {
		return AntigravitySummarySnapshot{}, err
	}
	summary := AntigravitySummarySnapshot{
		Status:       antigravityStatusFromText(r.status),
		NotFullyIdle: notFullyIdle,
		Killed:       killed,
		StepCount:    r.stepCount,
	}
	if r.rawSummary != nil {
		if err := antigravityDecodeSummary(*r.rawSummary, &summary); err != nil {
			return AntigravitySummarySnapshot{}, err
		}
	}
	return summary, nil
}

// NewAntigravityStateFollower follows conversationID in the Antigravity
// home.
func NewAntigravityStateFollower(home string, conversationID string) *AntigravityStateFollower {
	return &AntigravityStateFollower{home: home, conversationID: conversationID}
}

func (f *AntigravityStateFollower) Home() string {
	return f.home
}

func (f *AntigravityStateFollower) ConversationID() string {
	return f.conversationID
}

// Close releases the summary connection the follower holds.
func (f *AntigravityStateFollower) Close() {
	f.closeSummary()
}

func (f *AntigravityStateFollower) closeSummary() {
	if f.summary != nil {
		f.summary.close()
		f.summary = nil
	}
}

func (f *AntigravityStateFollower) Refresh() (AntigravitySnapshot, error) {
	snapshot, cursor, err := f.RefreshPending()
	if err != nil {
		return AntigravitySnapshot{}, err
	}
	f.CommitRefresh(cursor)
	return snapshot, nil
}

// RefreshPending reads the current database state without advancing the
// committed summary cursor. Call CommitRefresh only after the relay accepts
// the snapshot so a rejected concurrent scan cannot drop a real commit.
func (f *AntigravityStateFollower) RefreshPending() (AntigravitySnapshot, AntigravityRefreshCursor, error) {
	if err := antigravityValidateConversationID(f.conversationID); err != nil {
		return AntigravitySnapshot{}, AntigravityRefreshCursor{}, err
	}
	dir := filepath.Join(f.home, "antigravity-cli")
	// Do not consume a summary commit if the steps read fails and the relay
	// cannot reconcile the resulting snapshot.
	taskPath := filepath.Join(dir, "conversations", f.conversationID+".db")
	taskExists, err := pathTryExists(taskPath)
	if err != nil {
		return AntigravitySnapshot{}, AntigravityRefreshCursor{}, fmt.Errorf("locate Antigravity steps database: %w", err)
	}
	conversationDBPresent := false
	var runningTasks []AntigravityBackgroundTask
	if taskExists {
		conversationDBPresent = true
		if runningTasks, err = antigravityReadTasks(taskPath); err != nil {
			return AntigravitySnapshot{}, AntigravityRefreshCursor{}, err
		}
	}
	summaryPath := antigravitySummaryDBPath(f.home)
	summaryExists, err := pathTryExists(summaryPath)
	if err != nil {
		return AntigravitySnapshot{}, AntigravityRefreshCursor{}, fmt.Errorf("locate Antigravity summary database: %w", err)
	}
	if summaryExists {
		meta, err := os.Stat(summaryPath)
		if err != nil {
			return AntigravitySnapshot{}, AntigravityRefreshCursor{}, fmt.Errorf("stat Antigravity summary database: %w", err)
		}
		identity := antigravitySummaryIdentity(meta)
		if f.summaryIdentity == nil || *f.summaryIdentity != identity || f.summary == nil {
			database, err := antigravityOpenReadonly(summaryPath)
			if err != nil {
				return AntigravitySnapshot{}, AntigravityRefreshCursor{}, err
			}
			f.closeSummary()
			f.summary = database
			f.summaryIdentity = &identity
			f.cursor.dataVersion = nil
		}
	} else {
		f.closeSummary()
		f.summaryIdentity = nil
	}
	var cursor AntigravityRefreshCursor
	if f.summary != nil {
		var version int64
		if err := f.summary.conn.QueryRowContext(context.Background(), "PRAGMA data_version").Scan(&version); err != nil {
			return AntigravitySnapshot{}, AntigravityRefreshCursor{}, fmt.Errorf("read Antigravity summary data_version: %w", err)
		}
		if f.cursor.dataVersion != nil && *f.cursor.dataVersion == version {
			cursor = f.cursor
		} else {
			row, err := antigravityReadSummaryRow(f.summary.conn, f.conversationID)
			if err != nil {
				return AntigravitySnapshot{}, AntigravityRefreshCursor{}, err
			}
			cursor = AntigravityRefreshCursor{dataVersion: &version}
			if row != nil {
				snapshot, err := row.snapshot()
				if err != nil {
					return AntigravitySnapshot{}, AntigravityRefreshCursor{}, err
				}
				cursor.summary = &antigravityCursorSummary{snapshot: snapshot, digest: row.digest()}
			}
		}
	}
	// data_version covers the entire database. A child conversation can
	// commit without changing any field in the parent's row.
	var summaryRowDigest *[32]byte
	var summary *AntigravitySummarySnapshot
	if cursor.summary != nil {
		digest := cursor.summary.digest
		summaryRowDigest = &digest
		snapshot := cursor.summary.snapshot
		summary = &snapshot
	}
	summaryCommitChanged := summaryRowDigest != nil &&
		(f.cursor.summary == nil || f.cursor.summary.digest != *summaryRowDigest)
	return AntigravitySnapshot{
		Summary:               summary,
		ConversationDBPresent: conversationDBPresent,
		SummaryCommitChanged:  summaryCommitChanged,
		SummaryRowDigest:      summaryRowDigest,
		RunningTasks:          runningTasks,
	}, cursor, nil
}

func (f *AntigravityStateFollower) CommitRefresh(cursor AntigravityRefreshCursor) {
	f.cursor = cursor
}

func antigravityValidateConversationID(conversationID string) error {
	if conversationID == "" || strings.Contains(conversationID, "/") ||
		strings.Contains(conversationID, `\`) || strings.Contains(conversationID, "..") {
		return errors.New("invalid Antigravity conversation ID")
	}
	return nil
}

func antigravitySummaryDBPath(home string) string {
	return filepath.Join(home, "antigravity-cli", "conversation_summaries.db")
}

// antigravityQueryer is the query surface shared by a pinned *sql.Conn and
// a one-off database.
type antigravityQueryer interface {
	QueryRowContext(ctx context.Context, query string, args ...any) *sql.Row
}

// The untyped columns are read through unary `+`, which returns the stored
// value unchanged but gives the result no declared type, so the driver never
// turns text in a DATE, DATETIME or TIMESTAMP column into a time: rusqlite
// reads every value exactly as stored.
const antigravitySummaryRowQuery = `SELECT status, +not_fully_idle, +killed, step_count, +last_modified_time, raw_summary
         FROM conversation_summaries WHERE conversation_id = ?1`

func antigravityReadSummaryRow(conn antigravityQueryer, conversationID string) (*antigravitySummaryRow, error) {
	var status, stepCount, rawSummary any
	row := antigravitySummaryRow{}
	err := conn.QueryRowContext(context.Background(), antigravitySummaryRowQuery, conversationID).
		Scan(&status, &row.notFullyIdle, &row.killed, &stepCount, &row.lastModifiedTime, &rawSummary)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err == nil {
		if text, ok := status.(string); ok {
			row.status = text
		} else {
			err = antigravityInvalidColumnType(0, "status", status)
		}
	}
	if err == nil {
		if count, ok := stepCount.(int64); ok {
			row.stepCount = count
		} else {
			err = antigravityInvalidColumnType(3, "step_count", stepCount)
		}
	}
	if err == nil {
		switch raw := rawSummary.(type) {
		case nil:
		case []byte:
			row.rawSummary = &raw
		default:
			err = antigravityInvalidColumnType(5, "raw_summary", rawSummary)
		}
	}
	if err != nil {
		return nil, fmt.Errorf("query Antigravity conversation_summaries: %w", err)
	}
	return &row, nil
}

// antigravityInvalidColumnType is rusqlite's InvalidColumnType error for a
// value that does not convert to the column's Rust type.
func antigravityInvalidColumnType(index int, name string, value any) error {
	kind := "Null"
	switch value.(type) {
	case int64:
		kind = "Integer"
	case float64:
		kind = "Real"
	case string:
		kind = "Text"
	case []byte:
		kind = "Blob"
	}
	return fmt.Errorf("Invalid column type %s at index: %d, name: %s", kind, index, name)
}

// antigravityDatabase is a read-only connection to an Antigravity database,
// pinned so `PRAGMA data_version` compares commits on one connection.
type antigravityDatabase struct {
	db   *sql.DB
	conn *sql.Conn
}

func (d *antigravityDatabase) close() {
	d.conn.Close()
	d.db.Close()
}

func antigravityOpenReadonly(path string) (*antigravityDatabase, error) {
	db, err := sql.Open("sqlite", codexGoalURI(path, "mode=ro&_pragma=busy_timeout(250)"))
	if err != nil {
		return nil, fmt.Errorf("open Antigravity database %s: %w", path, err)
	}
	conn, err := db.Conn(context.Background())
	if err != nil {
		db.Close()
		return nil, fmt.Errorf("open Antigravity database %s: %w", path, err)
	}
	return &antigravityDatabase{db: db, conn: conn}, nil
}

func antigravityStatusFromText(text string) AntigravityRunStatus {
	switch text {
	case "CASCADE_RUN_STATUS_IDLE":
		return AntigravityRunStatusIdle
	case "CASCADE_RUN_STATUS_RUNNING":
		return AntigravityRunStatusRunning
	case "CASCADE_RUN_STATUS_CANCELING":
		return AntigravityRunStatusCanceling
	case "CASCADE_RUN_STATUS_BUSY":
		return AntigravityRunStatusBusy
	}
	return AntigravityRunStatusUnspecified
}

func antigravityValueBool(value any) (bool, error) {
	switch value := value.(type) {
	case int64:
		return value != 0, nil
	case float64:
		return value != 0, nil
	case string:
		switch asciiLower(value) {
		case "true", "1":
			return true, nil
		case "false", "0", "":
			return false, nil
		}
		return false, fmt.Errorf("invalid Antigravity boolean %s", value)
	case nil:
		return false, nil
	}
	return false, errors.New("invalid blob Antigravity boolean")
}

func antigravityReadVarint(b []byte, p *int) (uint64, error) {
	n := uint64(0)
	for shift := 0; shift < 70; shift += 7 {
		if *p >= len(b) {
			return 0, errors.New("truncated protobuf varint")
		}
		octet := b[*p]
		*p++
		if shift >= 63 && octet > 1 {
			return 0, errors.New("protobuf varint overflow")
		}
		n |= uint64(octet&127) << shift
		if octet&128 == 0 {
			return n, nil
		}
	}
	return 0, errors.New("invalid protobuf varint")
}

// antigravityFields calls f with each protobuf field of b: its number, wire
// type, varint or fixed value, and the bytes of a length-delimited field
// (nil for the other wire types).
func antigravityFields(b []byte, f func(field uint32, wire uint8, value uint64, bytes []byte) error) error {
	p := 0
	for p < len(b) {
		key, err := antigravityReadVarint(b, &p)
		if err != nil {
			return err
		}
		field := uint32(key >> 3)
		wire := uint8(key & 7)
		if field == 0 {
			return errors.New("protobuf field zero")
		}
		var value uint64
		var bytes []byte
		switch wire {
		case 0:
			if value, err = antigravityReadVarint(b, &p); err != nil {
				return err
			}
		case 1:
			if len(b)-p < 8 {
				return errors.New("truncated fixed64")
			}
			value = binary.LittleEndian.Uint64(b[p : p+8])
			p += 8
		case 2:
			length, err := antigravityReadVarint(b, &p)
			if err != nil {
				return err
			}
			if length > uint64(^uint(0)) {
				return errors.New("protobuf length overflow")
			}
			if length > uint64(len(b)-p) {
				return errors.New("truncated protobuf field")
			}
			bytes = b[p : p+int(length)]
			p += int(length)
		case 5:
			if len(b)-p < 4 {
				return errors.New("truncated fixed32")
			}
			value = uint64(binary.LittleEndian.Uint32(b[p : p+4]))
			p += 4
		default:
			return fmt.Errorf("unsupported protobuf wire type %d", wire)
		}
		if err := f(field, wire, value, bytes); err != nil {
			return err
		}
	}
	return nil
}

func antigravityDecodeSummary(bytes []byte, s *AntigravitySummarySnapshot) error {
	return antigravityFields(bytes, func(n uint32, w uint8, v uint64, _ []byte) error {
		switch n {
		case 5:
			if w != 0 {
				return errors.New("invalid summary status wire type")
			}
			switch v {
			case 1:
				s.Status = AntigravityRunStatusIdle
			case 2:
				s.Status = AntigravityRunStatusRunning
			case 3:
				s.Status = AntigravityRunStatusCanceling
			case 4:
				s.Status = AntigravityRunStatusBusy
			default:
				s.Status = AntigravityRunStatusUnspecified
			}
		case 21:
			if w != 0 {
				return errors.New("invalid summary idle wire type")
			}
			s.NotFullyIdle = v != 0
		case 29:
			if w != 0 {
				return errors.New("invalid summary task count wire type")
			}
			s.BackgroundTaskCount = v
		case 30:
			if w != 0 {
				return errors.New("invalid summary subagent wire type")
			}
			s.HasRunningSubagents = v != 0
		}
		return nil
	})
}

func antigravityReadTasks(path string) ([]AntigravityBackgroundTask, error) {
	database, err := antigravityOpenReadonly(path)
	if err != nil {
		return nil, err
	}
	defer database.close()
	ctx := context.Background()
	statement, err := database.conn.PrepareContext(ctx,
		"SELECT idx, task_details FROM steps WHERE status = 2 AND task_details IS NOT NULL ORDER BY idx ASC")
	if err != nil {
		return nil, fmt.Errorf("prepare Antigravity steps query: %w", err)
	}
	defer statement.Close()
	rows, err := statement.QueryContext(ctx)
	if err != nil {
		return nil, fmt.Errorf("query Antigravity steps: %w", err)
	}
	defer rows.Close()
	var tasks []AntigravityBackgroundTask
	for rows.Next() {
		var idxValue, details any
		if err := rows.Scan(&idxValue, &details); err != nil {
			return nil, err
		}
		idx, ok := idxValue.(int64)
		if !ok {
			return nil, antigravityInvalidColumnType(0, "idx", idxValue)
		}
		b, ok := details.([]byte)
		if !ok {
			return nil, antigravityInvalidColumnType(1, "task_details", details)
		}
		task, err := antigravityDecodeTask(idx, b)
		if err != nil {
			return nil, err
		}
		if task != nil {
			tasks = append(tasks, *task)
		}
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	return tasks, nil
}

func antigravityDecodeTask(idx int64, b []byte) (*AntigravityBackgroundTask, error) {
	var id, title, description string
	daemon := false
	var timestamp *int64
	err := antigravityFields(b, func(n uint32, w uint8, v uint64, bytes []byte) error {
		switch n {
		case 1, 4, 7:
			if w != 2 {
				return errors.New("invalid TaskDetails string wire type")
			}
			if !utf8.Valid(bytes) {
				return errors.New("invalid TaskDetails UTF-8")
			}
			switch n {
			case 1:
				id = string(bytes)
			case 4:
				description = string(bytes)
			case 7:
				title = string(bytes)
			}
		case 2:
			if w != 2 {
				return errors.New("invalid TaskDetails log URI wire type")
			}
		case 6:
			if w != 0 {
				return errors.New("invalid daemon wire type")
			}
			daemon = v != 0
		case 8:
			if w != 2 {
				return errors.New("invalid timestamp wire type")
			}
			stamp, err := antigravityDecodeTimestamp(bytes)
			if err != nil {
				return err
			}
			timestamp = &stamp
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	if daemon {
		return nil, nil
	}
	command := fmt.Sprintf("step-%d", idx)
	for _, candidate := range []string{title, description, id} {
		if candidate != "" {
			command = candidate
			break
		}
	}
	if id == "" {
		id = fmt.Sprintf("step-%d", idx)
	}
	return &AntigravityBackgroundTask{StepIndex: idx, ID: id, Command: command, StartedAtMs: timestamp}, nil
}

func antigravityDecodeTimestamp(b []byte) (int64, error) {
	var sec, nanos int64
	err := antigravityFields(b, func(n uint32, w uint8, v uint64, _ []byte) error {
		switch n {
		case 1:
			if w != 0 {
				return errors.New("invalid timestamp seconds wire type")
			}
			sec = int64(v)
		case 2:
			if w != 0 {
				return errors.New("invalid timestamp nanos wire type")
			}
			nanos = int64(v)
		}
		return nil
	})
	if err != nil {
		return 0, err
	}
	if sec > math.MaxInt64/1000 || sec < math.MinInt64/1000 {
		return 0, errors.New("timestamp overflow")
	}
	millis := sec * 1000
	add := nanos / 1_000_000
	if (add > 0 && millis > math.MaxInt64-add) || (add < 0 && millis < math.MinInt64-add) {
		return 0, errors.New("timestamp overflow")
	}
	return millis + add, nil
}
