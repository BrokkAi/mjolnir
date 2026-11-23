// Port of the tests in mj-worker/src/acp/opencode_logins.rs.

package worker

import (
	"database/sql"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"runtime"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// acpDriveSkipUnlessIgnoredTestsRequested is Rust's #[ignore]: the test runs
// only when MJ_RUN_IGNORED_TESTS=1 asks for it, as `cargo test -- --ignored`
// does.
func acpDriveSkipUnlessIgnoredTestsRequested(t *testing.T, reason string) {
	t.Helper()
	if os.Getenv("MJ_RUN_IGNORED_TESTS") != "1" {
		t.Skip(reason)
	}
}

// opencodeSessionHome is a session home for OpenCode under root, with export
// staged, and the launch environment that points OpenCode at it.
func opencodeSessionHome(t *testing.T, root string, export any) map[string]string {
	t.Helper()
	home := filepath.Join(root, "profile", "opencode")
	staged := filepath.Join(home, core.HarnessKindOpenCode.CredentialFileName())
	if err := os.MkdirAll(filepath.Dir(staged), 0o755); err != nil {
		t.Fatal(err)
	}
	encoded, err := json.Marshal(export)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(staged, encoded, 0o644); err != nil {
		t.Fatal(err)
	}
	return map[string]string{
		"XDG_CONFIG_HOME": filepath.Join(root, "profile"),
		"XDG_DATA_HOME":   filepath.Join(home, ".data"),
	}
}

func opencodeLogin(id, integration string, active bool, value any) map[string]any {
	return map[string]any{"id": id, "integrationID": integration, "label": id, "active": active, "value": value}
}

// A reload brings the session's logins to the profile's through OpenCode's
// own import, without losing an OAuth refresh the session made: the
// refreshed login stays, a provider's newly selected login takes over, and a
// login the profile dropped is gone.
func TestLiveOpencodeReloadKeepsSessionRefreshesAndTheProfilesSelection(t *testing.T) {
	acpDriveSkipUnlessIgnoredTestsRequested(t, "requires MJ_OPENCODE_TEST_BINARY naming an OpenCode 2 executable")
	opencode, ok := os.LookupEnv("MJ_OPENCODE_TEST_BINARY")
	if !ok {
		t.Fatal("set an OpenCode 2 executable")
	}
	root := t.TempDir()
	oauth := func(refresh string, expires uint64) map[string]any {
		return map[string]any{"type": "oauth", "methodID": "chatgpt-browser", "refresh": refresh,
			"access": "access", "expires": expires}
	}
	key := func(key string) map[string]any { return map[string]any{"type": "key", "key": key} }
	importLogins := func(environment map[string]string) {
		staged := filepath.Join(core.HarnessKindOpenCode.HomeFromEnvironment(environment["XDG_CONFIG_HOME"]),
			core.HarnessKindOpenCode.CredentialFileName())
		command := exec.Command(opencode, "auth", "import", "--standalone", staged)
		command.Env = os.Environ()
		for name, value := range environment {
			command.Env = append(command.Env, name+"="+value)
		}
		command.Env = append(command.Env, "OPENCODE_DISABLE_AUTOUPDATE=1")
		if err := command.Run(); err != nil {
			t.Fatalf("import: %v", err)
		}
	}
	first := []any{
		opencodeLogin("cred_openai", "openai", true, oauth("refresh-1", 100)),
		opencodeLogin("cred_deepseek_old", "deepseek", true, key("sk-old")),
		opencodeLogin("cred_xai", "xai", true, key("sk-xai")),
	}
	environment := opencodeSessionHome(t, root, first)
	if _, err := importedLoginsPrepare(environment); err != nil {
		t.Fatal(err)
	}
	importLogins(environment)
	databasePath := filepath.Join(environment["XDG_DATA_HOME"], "opencode", "opencode.db")
	// The session refreshes its OpenAI login in place, as OpenCode does.
	refreshed, err := json.Marshal(oauth("refresh-2", 200))
	if err != nil {
		t.Fatal(err)
	}
	database, err := sql.Open("sqlite", opencodeDatabaseURI(databasePath))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := database.Exec("UPDATE credential SET value = ?1 WHERE id = 'cred_openai'", string(refreshed)); err != nil {
		t.Fatal(err)
	}
	database.Close()

	second := []any{
		opencodeLogin("cred_openai", "openai", true, oauth("refresh-1", 100)),
		opencodeLogin("cred_deepseek_old", "deepseek", false, key("sk-old")),
		opencodeLogin("cred_deepseek_new", "deepseek", true, key("sk-new")),
	}
	environment = opencodeSessionHome(t, root, second)
	if _, err := importedLoginsPrepare(environment); err != nil {
		t.Fatal(err)
	}
	importLogins(environment)

	database, err = sql.Open("sqlite", opencodeDatabaseURI(databasePath))
	if err != nil {
		t.Fatal(err)
	}
	defer database.Close()
	query, err := database.Query("SELECT id, active, value FROM credential ORDER BY id")
	if err != nil {
		t.Fatal(err)
	}
	defer query.Close()
	type row struct {
		id     string
		active bool
		secret any
	}
	var rows []row
	for query.Next() {
		var id, value string
		var active sql.NullBool
		if err := query.Scan(&id, &active, &value); err != nil {
			t.Fatal(err)
		}
		var decoded map[string]any
		if err := json.Unmarshal([]byte(value), &decoded); err != nil {
			t.Fatal(err)
		}
		secret, ok := decoded["refresh"]
		if !ok {
			secret = decoded["key"]
		}
		rows = append(rows, row{id: id, active: active.Valid && active.Bool, secret: secret})
	}
	want := []row{
		{id: "cred_deepseek_new", active: true, secret: "sk-new"},
		{id: "cred_deepseek_old", active: false, secret: "sk-old"},
		{id: "cred_openai", active: true, secret: "refresh-2"},
	}
	if !reflect.DeepEqual(rows, want) {
		t.Fatalf("rows = %#v, want %#v", rows, want)
	}
}

// A session home that reaches another database through a link, at the
// database, its directory or the data directory above it, is refused, and
// the other database keeps its logins.
func TestASessionDatabaseReachedThroughALinkIsRefusedAndLeftAlone(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix only")
	}
	for _, link := range []string{".data/opencode/opencode.db", ".data/opencode", ".data"} {
		root := t.TempDir()
		other := filepath.Join(root, "other", ".data", "opencode", "opencode.db")
		if err := os.MkdirAll(filepath.Dir(other), 0o755); err != nil {
			t.Fatal(err)
		}
		database, err := sql.Open("sqlite", opencodeDatabaseURI(other))
		if err != nil {
			t.Fatal(err)
		}
		for _, statement := range []string{
			"CREATE TABLE credential (id text, integration_id text, value text)",
			"INSERT INTO credential VALUES ('cred', 'deepseek', '{}')",
		} {
			if _, err := database.Exec(statement); err != nil {
				t.Fatal(err)
			}
		}
		environment := opencodeSessionHome(t, root, []any{})
		home := core.HarnessKindOpenCode.HomeFromEnvironment(environment["XDG_CONFIG_HOME"])
		planted := filepath.Join(home, link)
		if _, err := os.Stat(planted); err == nil {
			if err := os.RemoveAll(planted); err != nil {
				t.Fatal(err)
			}
		}
		if err := os.MkdirAll(filepath.Dir(planted), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.Symlink(filepath.Join(root, "other", link), planted); err != nil {
			t.Fatal(err)
		}

		_, err = importedLoginsPrepare(environment)

		if err == nil || !strings.Contains(err.Error(), "refusing to change OpenCode logins") {
			t.Fatalf("%s: %v", link, err)
		}
		var rows int64
		if err := database.QueryRow("SELECT count(*) FROM credential").Scan(&rows); err != nil {
			t.Fatal(err)
		}
		if rows != 1 {
			t.Fatalf("%s: rows = %d", link, rows)
		}
		database.Close()
	}
}
