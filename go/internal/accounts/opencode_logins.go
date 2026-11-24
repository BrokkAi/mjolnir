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
	"bytes"
	"cmp"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
	_ "modernc.org/sqlite"

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
	data, ok := core.HarnessKindOpenCode.NestedDataDir(home)
	if !ok {
		panic("OpenCode has an XDG home")
	}
	logins, err := opencodeStoredLogins(data)
	if err != nil {
		return nil, err
	}
	legacy := []string{data}
	// Mjolnir pointed OpenCode 1 at `<home>/.data` even for the person's own
	// home, so a login made through `mj login` may be there instead.
	mjolnir := filepath.Join(home, ".data", core.HarnessKindOpenCode.ID())
	if filepath.Clean(mjolnir) != filepath.Clean(data) {
		legacy = append(legacy, mjolnir)
	}
	for _, directory := range legacy {
		if err := opencodeLegacyLoginsIn(directory, &logins); err != nil {
			return nil, err
		}
	}
	if len(logins) == 0 {
		return nil, nil
	}
	compact, err := jsonvalue.Marshal(opencodeSortedValue(opencodeExport(logins)))
	if err != nil {
		return nil, err
	}
	// serde_json::to_vec_pretty: two-space indentation, `": "` separators.
	var pretty bytes.Buffer
	if err := json.Indent(&pretty, compact, "", "  "); err != nil {
		return nil, err
	}
	return pretty.Bytes(), nil
}

// opencodeSortedValue orders every object's members by key, as
// serde_json's Map (a BTreeMap without `preserve_order`, which Mjolnir does
// not enable) holds and writes them.
func opencodeSortedValue(value any) any {
	switch value := value.(type) {
	case jsonvalue.Object:
		keys := value.Keys()
		sort.Strings(keys)
		sorted := make(jsonvalue.Object, 0, len(keys))
		for _, key := range keys {
			member, _ := value.Get(key)
			sorted = append(sorted, jsonvalue.Member{Key: key, Value: opencodeSortedValue(member)})
		}
		return sorted
	case []any:
		items := make([]any, len(value))
		for index, item := range value {
			items[index] = opencodeSortedValue(item)
		}
		return items
	}
	return value
}

// opencodeStoredLogins is the `credential` rows of the database in data,
// oldest first.
func opencodeStoredLogins(data string) ([]opencodeLogin, error) {
	path := filepath.Join(data, opencodeDatabase)
	if !harnessAuthIsFile(path) {
		return []opencodeLogin{}, nil
	}
	read := func() ([]opencodeLogin, error) {
		connection, err := opencodeOpen(path)
		if err != nil {
			return nil, err
		}
		defer connection.Close()
		rows, err := connection.Query("SELECT id, integration_id, label, active, value, time_created FROM credential " +
			"WHERE integration_id IS NOT NULL ORDER BY time_created, id")
		if err != nil {
			return nil, err
		}
		defer rows.Close()
		logins := []opencodeLogin{}
		for rows.Next() {
			var (
				id, integration, label, value string
				active                        sql.NullInt64
				created                       int64
			)
			if err := rows.Scan(&id, &integration, &label, &active, &value, &created); err != nil {
				return nil, err
			}
			decoded, err := jsonvalue.Parse([]byte(value))
			if err != nil {
				return nil, fmt.Errorf("decode stored login %s: %w", id, err)
			}
			var flag uint8
			switch {
			case active.Valid && active.Int64 != 0:
				flag = 2
			case active.Valid:
				flag = 1
			}
			logins = append(logins, opencodeLogin{
				id:          id,
				integration: integration,
				label:       label,
				value:       decoded,
				rank:        opencodeLoginRank{flag: flag, created: created, id: id},
			})
		}
		if err := rows.Err(); err != nil {
			return nil, err
		}
		return logins, nil
	}
	logins, err := read()
	if err != nil {
		return nil, fmt.Errorf("read OpenCode logins from %s: %w", path, err)
	}
	return logins, nil
}

// opencodeLegacyLoginsIn adds the OpenCode 1 logins of directory for
// integrations logins lacks, converted as OpenCode 2's own upgrade converts
// them. Nothing when OpenCode 2 has taken the directory over: it decided
// about that file itself, and a login removed since must not come back.
func opencodeLegacyLoginsIn(directory string, logins *[]opencodeLogin) error {
	path := filepath.Join(directory, opencodeLegacyLogins)
	text, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil
	}
	if err != nil {
		return fmt.Errorf("read %s: %w", path, err)
	}
	takenOver, err := takenOverByOpencode2(directory)
	if err != nil {
		return err
	}
	if takenOver {
		return nil
	}
	parsed, err := jsonvalue.Parse(text)
	if err == nil {
		if _, isObject := parsed.(jsonvalue.Object); !isObject {
			err = fmt.Errorf("invalid type: %s, expected a map", opencodeJSONKind(parsed))
		}
	}
	if err != nil {
		return fmt.Errorf("%s must hold a JSON object of logins: %w", path, err)
	}
	// Rust reads the file into a BTreeMap: one entry per key, in key order.
	entries := parsed.(jsonvalue.Object)
	ids := entries.Keys()
	sort.Strings(ids)
	for _, id := range ids {
		entry, _ := entries.Get(id)
		integration := strings.TrimRight(id, "/")
		if integration == "" || opencodeHasIntegration(*logins, integration) {
			continue
		}
		label, value, ok := opencodeLegacyValue(integration, entry)
		if !ok {
			continue
		}
		*logins = append(*logins, opencodeLogin{
			// Stable, so a relaunch that imports it again adds nothing.
			id:          "cred_mjolnir_" + core.LowerHex([]byte(integration)),
			integration: integration,
			label:       label,
			value:       value,
			rank:        opencodeLoginRank{},
		})
	}
	return nil
}

func opencodeHasIntegration(logins []opencodeLogin, integration string) bool {
	for _, login := range logins {
		if login.integration == integration {
			return true
		}
	}
	return false
}

// opencodeJSONKind names a decoded value's JSON type as serde's
// "invalid type" errors do.
func opencodeJSONKind(value any) string {
	switch value := value.(type) {
	case nil:
		return "null"
	case bool:
		return fmt.Sprintf("boolean `%t`", value)
	case string:
		return fmt.Sprintf("string %q", value)
	case []any:
		return "sequence"
	case int64, uint64:
		return fmt.Sprintf("integer `%d`", value)
	case float64:
		return fmt.Sprintf("floating point `%v`", value)
	}
	return "map"
}

// opencodeLegacyValue is one OpenCode 1 login as OpenCode 2's upgrade
// stores it, with its label; ok false for an entry that upgrade skips
// because it is not a login it knows.
func opencodeLegacyValue(integration string, entry any) (label string, value any, ok bool) {
	object, isObject := entry.(jsonvalue.Object)
	if !isObject {
		return "", nil, false
	}
	text := func(name string) (string, bool) {
		field, present := object.Get(name)
		if !present {
			return "", false
		}
		text, isText := field.(string)
		return text, isText
	}
	kind, present := text("type")
	if !present {
		return "", nil, false
	}
	switch kind {
	case "api":
		key, present := text("key")
		if !present {
			return "", nil, false
		}
		result := jsonvalue.Object{{Key: "type", Value: "key"}, {Key: "key", Value: key}}
		if metadata, present := object.Get("metadata"); present {
			metadata, isObject := metadata.(jsonvalue.Object)
			if !isObject {
				return "", nil, false
			}
			for _, key := range metadata.Keys() {
				member, _ := metadata.Get(key)
				if _, isText := member.(string); !isText {
					return "", nil, false
				}
			}
			result.Set("metadata", metadata.Clone())
		}
		return "API key", result, true
	case "wellknown":
		if _, present := text("key"); !present {
			return "", nil, false
		}
		token, present := text("token")
		if !present {
			return "", nil, false
		}
		return "API key", jsonvalue.Object{{Key: "type", Value: "key"}, {Key: "key", Value: token}}, true
	case "oauth":
		method := "oauth"
		switch integration {
		case "openai":
			method = "chatgpt-browser"
		case "github-copilot", "opencode", "xai":
			method = "device"
		}
		refresh, present := text("refresh")
		if !present {
			return "", nil, false
		}
		access, present := text("access")
		if !present {
			return "", nil, false
		}
		expiresValue, present := object.Get("expires")
		if !present {
			return "", nil, false
		}
		var expires any
		switch number := expiresValue.(type) {
		case int64:
			if number < 0 {
				return "", nil, false
			}
			expires = number
		case uint64:
			expires = number
		default:
			return "", nil, false
		}
		result := jsonvalue.Object{
			{Key: "type", Value: "oauth"},
			{Key: "methodID", Value: method},
			{Key: "refresh", Value: refresh},
			{Key: "access", Value: access},
			{Key: "expires", Value: expires},
		}
		metadata := jsonvalue.Object{}
		if account, present := text("accountId"); present && account != "" {
			metadata.Set("accountID", account)
		}
		if url, present := text("enterpriseUrl"); present && url != "" {
			metadata.Set("enterpriseUrl", url)
		}
		if len(metadata) > 0 {
			result.Set("metadata", metadata)
		}
		return "OAuth", result, true
	}
	return "", nil, false
}

// takenOverByOpencode2 is whether OpenCode 2 has opened the database in
// directory.
func takenOverByOpencode2(directory string) (bool, error) {
	path := filepath.Join(directory, opencodeDatabase)
	if !harnessAuthIsFile(path) {
		return false, nil
	}
	read := func() (bool, error) {
		connection, err := opencodeOpen(path)
		if err != nil {
			return false, err
		}
		defer connection.Close()
		var journal string
		err = connection.QueryRow("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'migration'").Scan(&journal)
		if errors.Is(err, sql.ErrNoRows) {
			return false, nil
		}
		if err != nil {
			return false, err
		}
		var one int64
		err = connection.QueryRow("SELECT 1 FROM migration WHERE id = ?1", opencodeLegacyImportMigration).Scan(&one)
		if errors.Is(err, sql.ErrNoRows) {
			return false, nil
		}
		if err != nil {
			return false, err
		}
		return true, nil
	}
	takenOver, err := read()
	if err != nil {
		return false, fmt.Errorf("read OpenCode migrations from %s: %w", path, err)
	}
	return takenOver, nil
}

// opencodeOpen opens the database read-only. The person's own OpenCode may
// be writing; a reader waits out its brief write locks instead of failing
// the launch.
func opencodeOpen(path string) (*sql.DB, error) {
	connection, err := sql.Open("sqlite", opencodeReadOnlyURI(path))
	if err != nil {
		return nil, err
	}
	// One connection, as rusqlite's Connection is one.
	connection.SetMaxOpenConns(1)
	if err := connection.Ping(); err != nil {
		connection.Close()
		return nil, err
	}
	return connection, nil
}

// opencodeReadOnlyURI is the read-only SQLite URI of path, with rusqlite's
// busy_timeout of five seconds.
func opencodeReadOnlyURI(path string) string {
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
	uri.WriteString("?mode=ro&_pragma=busy_timeout(5000)")
	return uri.String()
}

// opencodeExport is the `auth export` entries for logins. Each
// integration's selected login, the one OpenCode uses, is the active one, so
// a fresh session selects it too; `auth import` activates the first login of
// an integration it lacks whatever its flag says.
func opencodeExport(logins []opencodeLogin) []any {
	selected := map[string]int{}
	for index, login := range logins {
		best, ok := selected[login.integration]
		if !ok {
			selected[login.integration] = index
			continue
		}
		if login.rank.compare(logins[best].rank) > 0 {
			selected[login.integration] = index
		}
	}
	active := map[int]bool{}
	for _, index := range selected {
		active[index] = true
	}
	entries := make([]any, 0, len(logins))
	for index, login := range logins {
		entries = append(entries, jsonvalue.Object{
			{Key: "id", Value: login.id},
			{Key: "integrationID", Value: login.integration},
			{Key: "label", Value: login.label},
			{Key: "active", Value: active[index]},
			{Key: "value", Value: login.value},
		})
	}
	return entries
}
