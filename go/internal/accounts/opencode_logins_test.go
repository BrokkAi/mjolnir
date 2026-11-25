// Port of the tests in mj-accounts/src/opencode_logins.rs.

package accounts

import (
	"bytes"
	"database/sql"
	"os"
	"path/filepath"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// Moving to OpenCode 2 keeps every login the person had and revives none
// they removed. In the person's own home, the database's logins come with
// the one OpenCode selects for each integration; an OpenCode 1 login that
// `mj login` kept under the home is carried as OpenCode 2's own upgrade
// carries it; and the OpenCode 1 file OpenCode 2 already took over brings
// nothing back.
func TestMovingToOpencode2KeepsEveryLoginAndRevivesNone(t *testing.T) {
	const child = "MJ_TEST_OPENCODE_2_LOGINS"
	root, ok := os.LookupEnv(child)
	if !ok {
		root := t.TempDir()
		targets.NewIsolatedTest(targets.TestName(
			"mj_accounts::opencode_logins::tests",
			"TestMovingToOpencode2KeepsEveryLoginAndRevivesNone",
		)).
			Env(child, root).
			Env("XDG_CONFIG_HOME", filepath.Join(root, "config")).
			Env("XDG_DATA_HOME", filepath.Join(root, "data")).
			Run()
		return
	}
	home := filepath.Join(root, "config/opencode")
	data := filepath.Join(root, "data/opencode")
	if err := os.MkdirAll(data, 0o755); err != nil {
		t.Fatal(err)
	}
	// OpenCode 2 took the person's store over and imported its OpenCode 1
	// file, leaving two DeepSeek keys without an active flag. The person
	// has since logged out of Anthropic.
	database, err := sql.Open("sqlite", filepath.Join(data, opencodeDatabase))
	if err != nil {
		t.Fatal(err)
	}
	for _, statement := range []string{
		`CREATE TABLE migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)`,
		`INSERT INTO migration VALUES ('20260805200742_import_legacy_credentials', 1)`,
		`CREATE TABLE credential (id text PRIMARY KEY, integration_id text, label text NOT NULL,
                       value text NOT NULL, connector_id text, method_id text, active integer,
                       time_created integer NOT NULL, time_updated integer NOT NULL)`,
		`INSERT INTO credential VALUES ('cred_old', 'deepseek', 'API key',
                       '{"type":"key","key":"sk-old"}', NULL, NULL, NULL, 1, 1)`,
		`INSERT INTO credential VALUES ('cred_new', 'deepseek', 'API key',
                       '{"type":"key","key":"sk-new"}', NULL, NULL, NULL, 2, 2)`,
	} {
		if _, err := database.Exec(statement); err != nil {
			t.Fatal(err)
		}
	}
	if err := database.Close(); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(data, opencodeLegacyLogins),
		[]byte(`{"anthropic":{"type":"oauth","refresh":"r","access":"a","expires":1}}`), 0o644); err != nil {
		t.Fatal(err)
	}
	// OpenCode 1 under Mjolnir kept these under the home itself.
	mjolnir := filepath.Join(home, ".data/opencode")
	if err := os.MkdirAll(mjolnir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(mjolnir, opencodeLegacyLogins), []byte(`{"broken":{"type":"api"},
                "deepseek":{"type":"api","key":"sk-mjolnir"},
                "openai/":{"type":"oauth","refresh":"r","access":"a","expires":5,"accountId":"acct"}}`), 0o644); err != nil {
		t.Fatal(err)
	}
	profile := core.NewHarnessProfile(core.HarnessKindOpenCode, home)
	profile.Enabled = true
	profile.MjolnirGuardian = true

	logins, err := ProfileLogins(&profile)
	if err != nil || logins == nil {
		t.Fatalf("profile logins: %v, %v", logins, err)
	}

	again, err := ProfileLogins(&profile)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(again, logins) {
		t.Fatalf("a relaunch stages the same logins, so its import adds nothing:\n%s\n%s", again, logins)
	}
	got, err := jsonvalue.Parse(logins)
	if err != nil {
		t.Fatal(err)
	}
	want, err := jsonvalue.Parse([]byte(`[
                {"id": "cred_old", "integrationID": "deepseek", "label": "API key", "active": false,
                 "value": {"type": "key", "key": "sk-old"}},
                {"id": "cred_new", "integrationID": "deepseek", "label": "API key", "active": true,
                 "value": {"type": "key", "key": "sk-new"}},
                {"id": "cred_mjolnir_6f70656e6169", "integrationID": "openai", "label": "OAuth",
                 "active": true,
                 "value": {"type": "oauth", "methodID": "chatgpt-browser", "refresh": "r",
                           "access": "a", "expires": 5, "metadata": {"accountID": "acct"}}}
            ]`))
	if err != nil {
		t.Fatal(err)
	}
	if !jsonvalue.Equal(got, opencodeSortedValue(want)) {
		t.Fatalf("logins:\n%s", logins)
	}
}
