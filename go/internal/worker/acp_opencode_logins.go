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
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
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
	panic("unported: mj-worker/src/acp/opencode_logins.rs ImportedLogins::prepare")
}

// changed says whether the staged export now differs from the one this
// bridge imported. A missing export is the same as one without logins.
func (l *ImportedLogins) changed(ctx context.Context) (bool, error) {
	panic("unported: mj-worker/src/acp/opencode_logins.rs ImportedLogins::changed")
}

func readEntries(path string) ([]any, error) {
	panic("unported: mj-worker/src/acp/opencode_logins.rs read_entries")
}

// Staged is one staged login, as `auth export` writes it.
type Staged struct {
	integration string
	active      bool
	value       any
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
	panic("unported: mj-worker/src/acp/opencode_logins.rs reconcile_database_logins")
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
	panic("unported: mj-worker/src/acp/opencode_logins.rs serde_json::Value PartialEq")
}
