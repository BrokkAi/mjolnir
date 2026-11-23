// Port of mj-accounts/src/opencode_logins.rs.

// OpenCode 2's logins, read from its own database.
//
// OpenCode 2 keeps every login in the `credential` table of `opencode.db` in
// its data directory, the same file that holds every session it ran there.
// Staging carries only the logins into a session home, as the JSON array
// `opencode auth export` prints and `opencode auth import` reads; each
// launch imports it (core.OpencodeACPScript).
//
// The database is read here rather than through `opencode auth export`
// because the daemon cannot rely on finding the person's own `opencode`, and
// Mjolnir's pinned one would migrate a database the person's OpenCode owns.
// A read-only query changes nothing, and a schema it does not expect fails
// the read with SQLite's error rather than staging a session without
// logins.
//
// OpenCode 1 kept logins in `auth.json` instead. OpenCode 2 imports that
// file when it upgrades the database beside it, and ignores it once it has;
// a login Mjolnir finds in a directory OpenCode 2 has not taken over yet is
// carried the way that upgrade would carry it.

package accounts

import (
	"cmp"
	"database/sql"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// opencodeDatabase is the database file name of every released OpenCode
// channel.
const opencodeDatabase = "opencode.db"

// opencodeLegacyLogins is OpenCode 1's login file, in the data directory.
const opencodeLegacyLogins = "auth.json"

// opencodeLegacyImportMigration is the migration through which OpenCode 2
// takes over an OpenCode 1 data directory, importing its
// opencodeLegacyLogins. OpenCode 2 also records it in every database it
// creates, where it imports nothing.
const opencodeLegacyImportMigration = "20260805200742_import_legacy_credentials"

// opencodeLogin is one login, keyed by the fields of an `auth export`
// entry.
type opencodeLogin struct {
	id          string
	integration string
	label       string
	value       any
	// rank is how OpenCode orders the logins of one integration to select
	// one: the active flag (set, clear, then never set), then the newest.
	rank opencodeLoginRank
}

// opencodeLoginRank is Rust's `(u8, i64, String)`, compared in order.
type opencodeLoginRank struct {
	flag    uint8
	created int64
	id      string
}

func (r opencodeLoginRank) compare(other opencodeLoginRank) int {
	return cmp.Or(cmp.Compare(r.flag, other.flag), cmp.Compare(r.created, other.created), cmp.Compare(r.id, other.id))
}

// ProfileLogins is the logins OpenCode keeps for profile, in `auth export`
// form: those in its database, then OpenCode 1 logins for integrations the
// database lacks, from the data directory and from the place Mjolnir kept
// them for an OpenCode 1 profile. Nil when there are none.
func ProfileLogins(profile *core.HarnessProfile) ([]byte, error) {
	return HomeLogins(profile.Home)
}

// HomeLogins is ProfileLogins for the profile whose home is home.
func HomeLogins(home string) ([]byte, error) {
	panic("unported: mj-accounts/src/opencode_logins.rs home_logins")
}

// opencodeStoredLogins is the `credential` rows of the database in data,
// oldest first.
func opencodeStoredLogins(data string) ([]opencodeLogin, error) {
	panic("unported: mj-accounts/src/opencode_logins.rs stored_logins")
}

// opencodeLegacyLoginsIn adds the OpenCode 1 logins of directory for
// integrations logins lacks, converted as OpenCode 2's own upgrade converts
// them. Nothing when OpenCode 2 has taken the directory over: it decided
// about that file itself, and a login removed since must not come back.
func opencodeLegacyLoginsIn(directory string, logins *[]opencodeLogin) error {
	panic("unported: mj-accounts/src/opencode_logins.rs legacy_logins")
}

// opencodeLegacyValue is one OpenCode 1 login as OpenCode 2's upgrade
// stores it, with its label; ok false for an entry that upgrade skips
// because it is not a login it knows.
func opencodeLegacyValue(integration string, entry any) (label string, value any, ok bool) {
	panic("unported: mj-accounts/src/opencode_logins.rs legacy_value")
}

// takenOverByOpencode2 is whether OpenCode 2 has opened the database in
// directory.
func takenOverByOpencode2(directory string) (bool, error) {
	panic("unported: mj-accounts/src/opencode_logins.rs taken_over_by_opencode_2")
}

// opencodeOpen opens the database read-only. The person's own OpenCode may
// be writing; a reader waits out its brief write locks instead of failing
// the launch.
func opencodeOpen(path string) (*sql.DB, error) {
	panic("unported: mj-accounts/src/opencode_logins.rs open")
}

// opencodeExport is the `auth export` entries for logins. Each
// integration's selected login, the one OpenCode uses, is the active one, so
// a fresh session selects it too; `auth import` activates the first login of
// an integration it lacks whatever its flag says.
func opencodeExport(logins []opencodeLogin) []any {
	panic("unported: mj-accounts/src/opencode_logins.rs export")
}
