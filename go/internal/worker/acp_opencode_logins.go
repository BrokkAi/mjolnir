// Port of mj-worker/src/acp/opencode_logins.rs.

// The logins an OpenCode bridge imported when it started.
//
// Staging and the credential sync keep the profile's logins in the session
// home as the export `opencode auth import` reads
// (core.HarnessKind.CredentialFileName), and every launch imports it
// (core's opencode ACP script). The session's own database copies those
// logins, and OpenCode refreshes an OAuth login in its copy in place. Before
// each bridge starts, the copy is brought to the staged export without
// losing such a refresh (reconcileDatabaseLogins), and an idle session whose
// staged export has changed reloads on a fresh bridge, which imports it. A
// running OpenCode reloads its providers the same way when a login changes
// through its own server, which Mjolnir cannot reach.

package worker

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
	_ "modernc.org/sqlite"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// checkInterval is how often an idle OpenCode session looks for changed
// logins (20 ms in tests).
var checkInterval = func() time.Duration {
	if testing.Testing() {
		return 20 * time.Millisecond
	}
	return 5 * time.Second
}()

// ImportedLogins is the staged export a bridge's launch imported, as it read
// it.
type ImportedLogins struct {
	path    string
	entries []any
}

// importedLoginsPrepare runs before an OpenCode bridge starts in
// environment: it brings the session database's logins to the staged export,
// and records the export the launch's import reads. Recorded before the
// bridge reads it, so a change made in between is seen as a change.
func importedLoginsPrepare(environment map[string]string) (ImportedLogins, error) {
	kind := core.HarnessKindOpenCode
	variable := func(name string) (string, error) {
		value, ok := environment[name]
		if !ok {
			return "", fmt.Errorf("the OpenCode launch sets no %s", name)
		}
		return value, nil
	}
	homeValue, err := variable(kind.HomeEnv())
	if err != nil {
		return ImportedLogins{}, err
	}
	home := kind.HomeFromEnvironment(homeValue)
	path := filepath.Join(home, kind.CredentialFileName())
	// OpenCode reads its database from `XDG_DATA_HOME`, which a staged home
	// places inside itself (HarnessKind.ConfigureHomeEnvironment); only a
	// database there is the session's own.
	data, err := variable("XDG_DATA_HOME")
	if err != nil {
		return ImportedLogins{}, err
	}
	if !samePath(data, filepath.Join(home, ".data")) {
		return ImportedLogins{}, fmt.Errorf("the OpenCode launch keeps its data in %s, outside its staged home %s", data, home)
	}
	entries, err := readEntries(path)
	if err != nil {
		return ImportedLogins{}, err
	}
	if err := reconcileDatabaseLogins(home, entries); err != nil {
		return ImportedLogins{}, err
	}
	return ImportedLogins{path: path, entries: entries}, nil
}

// changed says whether the staged export now differs from the one this
// bridge imported. A missing export is the same as one without logins.
func (l *ImportedLogins) changed(ctx context.Context) (bool, error) {
	entries, err := fileReadOffRuntime(ctx, "opencode-logins-read", func() ([]any, error) {
		return readEntries(l.path)
	})
	if err != nil {
		return false, fmt.Errorf("read the staged OpenCode logins: %w", err)
	}
	return !acpValueEqual(entries, l.entries), nil
}

func readEntries(path string) ([]any, error) {
	bytes, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return []any{}, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read the staged OpenCode logins %s: %w", path, err)
	}
	value, err := jsonvalue.Parse(bytes)
	if err != nil {
		return nil, fmt.Errorf("decode the staged OpenCode logins %s: %w", path, err)
	}
	entries, ok := value.([]any)
	if !ok {
		return nil, fmt.Errorf("decode the staged OpenCode logins %s: invalid type, expected a sequence", path)
	}
	return entries, nil
}

// Staged is one staged login, as `auth export` writes it.
type Staged struct {
	integration string
	active      bool
	value       any
}

// opencodeDatabaseURI is the SQLite URI of path, with the busy timeout the
// reconciliation waits for a lock (rusqlite's busy_timeout of five seconds).
func opencodeDatabaseURI(path string) string {
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
	uri.WriteString("?_pragma=busy_timeout(5000)")
	return uri.String()
}

// reconcileDatabaseLogins brings the logins of the session database under
// home to the staged entries, keeping what the session refreshed since. The
// launch's `auth import` then adds every staged login the database lacks.
//
// `auth import` adds only logins whose id is new, activates a new login only
// for an integration the database lacks, and pins an integration's newest
// remaining login active when every one of its logins is inactive. So an
// integration is kept only while its selected login is: the session's copy
// matches the staged one, or is a later refresh of the same OAuth login.
// Then its other logins stay where they still match and the staged selection
// is set; otherwise the integration is emptied and the import rebuilds it,
// selecting as staged. A login the profile no longer has is removed.
//
// The previous bridge has been stopped before this one is prepared, and its
// server child ends with it. The database is the regular file at
// `.data/opencode/opencode.db` in the staged home, reached without a link
// anywhere from the home down, so a session home cannot point this at
// another database.
func reconcileDatabaseLogins(home string, entries []any) error {
	relative := filepath.Join(".data", core.HarnessKindOpenCode.ID(), "opencode.db")
	if err := core.RejectSymlinks(home, relative, true); err != nil {
		return fmt.Errorf("refusing to change OpenCode logins: %w", err)
	}
	path := filepath.Join(home, relative)
	info, err := os.Lstat(path)
	switch {
	case errors.Is(err, fs.ErrNotExist):
		return nil
	case err != nil:
		return fmt.Errorf("inspect %s: %w", path, err)
	case !info.Mode().IsRegular():
		return fmt.Errorf("refusing to change OpenCode logins: %s is not a regular file", path)
	}
	staged := map[string]Staged{}
	for _, entry := range entries {
		object, ok := entry.(jsonvalue.Object)
		if !ok {
			continue
		}
		id, ok := object.Get("id")
		idText, isText := id.(string)
		if !ok || !isText {
			continue
		}
		integration, ok := object.Get("integrationID")
		integrationText, isText := integration.(string)
		if !ok || !isText {
			continue
		}
		active, ok := object.Get("active")
		activeFlag, isBool := active.(bool)
		if !ok || !isBool {
			continue
		}
		value, ok := object.Get("value")
		if !ok {
			continue
		}
		// A later entry with the same id replaces the earlier one, as a
		// BTreeMap collected from the entries keeps the last.
		staged[idText] = Staged{integration: integrationText, active: activeFlag, value: value}
	}
	reconcile := func() error {
		database, err := sql.Open("sqlite", opencodeDatabaseURI(path))
		if err != nil {
			return err
		}
		defer database.Close()
		database.SetMaxOpenConns(1)
		var exists bool
		if err := database.QueryRow(
			"SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'credential')",
		).Scan(&exists); err != nil {
			return err
		}
		if !exists {
			return nil
		}
		transaction, err := database.Begin()
		if err != nil {
			return err
		}
		defer transaction.Rollback()
		type row struct {
			id          string
			integration *string
			value       string
		}
		var rows []row
		err = func() error {
			query, err := transaction.Query("SELECT id, integration_id, value FROM credential")
			if err != nil {
				return err
			}
			defer query.Close()
			for query.Next() {
				var current row
				var integration sql.NullString
				if err := query.Scan(&current.id, &integration, &current.value); err != nil {
					return err
				}
				if integration.Valid {
					text := integration.String
					current.integration = &text
				}
				rows = append(rows, current)
			}
			return query.Err()
		}()
		if err != nil {
			return err
		}
		holdsStaged := func(id, value string) bool {
			login, ok := staged[id]
			if !ok {
				return false
			}
			session, err := jsonvalue.Parse([]byte(value))
			return err == nil && holds(session, login.value)
		}
		keptIntegrations := map[string]bool{}
		for id, login := range staged {
			if !login.active {
				continue
			}
			for _, current := range rows {
				if current.id == id && holdsStaged(current.id, current.value) {
					keptIntegrations[login.integration] = true
					break
				}
			}
		}
		for _, current := range rows {
			keep := current.integration != nil && keptIntegrations[*current.integration] &&
				holdsStaged(current.id, current.value)
			if keep {
				if _, err := transaction.Exec("UPDATE credential SET active = ?2 WHERE id = ?1",
					current.id, staged[current.id].active); err != nil {
					return err
				}
			} else if _, err := transaction.Exec("DELETE FROM credential WHERE id = ?1", current.id); err != nil {
				return err
			}
		}
		return transaction.Commit()
	}
	if err := reconcile(); err != nil {
		return fmt.Errorf("reconcile the OpenCode session's logins in %s: %w", path, err)
	}
	return nil
}

// holds says whether the session's copy of a login still holds the staged
// one: the same login, or a later refresh of the same OAuth login.
func holds(session, staged any) bool {
	expiry := func(value any) (uint64, bool) {
		object, _ := value.(jsonvalue.Object)
		if kind, _ := object.Get("type"); kind != "oauth" {
			return 0, false
		}
		switch expires, _ := object.Get("expires"); expires := expires.(type) {
		case int64:
			return uint64(expires), expires >= 0
		case uint64:
			return expires, true
		}
		return 0, false
	}
	sessionExpiry, sessionOAuth := expiry(session)
	stagedExpiry, stagedOAuth := expiry(staged)
	return acpValueEqual(session, staged) || (sessionOAuth && stagedOAuth && sessionExpiry > stagedExpiry)
}

// acpValueEqual compares two decoded JSON values as serde_json's Value
// equality does: object members compare as maps, whatever their order.
func acpValueEqual(a, b any) bool {
	switch x := a.(type) {
	case jsonvalue.Object:
		y, ok := b.(jsonvalue.Object)
		if !ok || len(x) != len(y) {
			return false
		}
		for _, member := range x {
			other, ok := y.Get(member.Key)
			if !ok || !acpValueEqual(member.Value, other) {
				return false
			}
		}
		return true
	case []any:
		y, ok := b.([]any)
		if !ok || len(x) != len(y) {
			return false
		}
		for i := range x {
			if !acpValueEqual(x[i], y[i]) {
				return false
			}
		}
		return true
	}
	return jsonvalue.Equal(a, b)
}
