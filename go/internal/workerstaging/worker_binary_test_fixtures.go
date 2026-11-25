// Port of mj-worker-staging/src/worker_binary/test_fixtures.rs.

// Stand-ins the staging tests, and the tests of the packages above that
// stage a worker, build on. Rust keeps this module public (doc-hidden) so
// the crates above can use it; Go test files cannot be imported by other
// packages, so it is an ordinary file here too. Failures panic, as Rust's
// unwrap does.

package workerstaging

import (
	"bytes"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"

	// The SQLite driver WriteOpencodeDatabase opens.
	_ "modernc.org/sqlite"
)

func StampedWorker(body []byte) []byte {
	bytes := append([]byte{}, body...)
	return append(bytes, core.WorkerBuildStamp()...)
}

// PodmanInstallFixture is a stamped worker, launch config, ownership record
// and profile stage in a temporary directory, for an SSH Podman target.
// Close removes the directory (Rust: dropping the TempDir).
type PodmanInstallFixture struct {
	root         string
	WorkerBinary string
	LaunchConfig string
	Ownership    string
	ProfileStage string
	Locator      core.TargetsTargetLocator
}

// Close removes the fixture's directory.
func (f *PodmanInstallFixture) Close() {
	_ = os.RemoveAll(f.root)
}

// NewPodmanInstallFixture is Rust's `podman_install_fixture` (renamed: Go
// cannot have a function and a type both named PodmanInstallFixture).
func NewPodmanInstallFixture() *PodmanInstallFixture {
	root, err := os.MkdirTemp("", ".tmp")
	fixtureMust(err)
	workerBinary := filepath.Join(root, "hel")
	fixtureMust(os.WriteFile(workerBinary, StampedWorker([]byte("worker-binary-bytes")), 0o666))
	launchConfig := filepath.Join(root, "launch.json")
	fixtureMust(os.WriteFile(launchConfig, []byte("{}"), 0o666))
	ownership := filepath.Join(root, "ownership.json")
	fixtureMust(os.WriteFile(ownership, []byte("{}"), 0o666))
	profileStage := filepath.Join(root, "profile")
	fixtureMust(os.MkdirAll(profileStage, 0o777))
	return &PodmanInstallFixture{
		root:         root,
		WorkerBinary: workerBinary,
		LaunchConfig: launchConfig,
		Ownership:    ownership,
		ProfileStage: profileStage,
		Locator: core.TargetsTargetLocator{
			Kind:        core.TargetsTargetLocatorSshPodman,
			SSH:         &core.SshTarget{Destination: "user@example.test"},
			ContainerID: strings.Repeat("a", 64),
		},
	}
}

func fixtureMust(err error) {
	if err != nil {
		panic(err)
	}
}

// WriteOpencodeDatabase writes an OpenCode 2 database holding one session
// and one API-key login with key, in the `credential` table layout OpenCode
// 2.1 creates, replacing any file at path.
func WriteOpencodeDatabase(path, key string) {
	if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		panic(fmt.Sprintf("remove %s: %s", path, rustIOError(err)))
	}
	parent, _ := pathParent(path)
	fixtureMust(os.MkdirAll(parent, 0o777))
	database, err := sql.Open("sqlite", path)
	fixtureMust(err)
	defer database.Close()
	for _, statement := range []string{
		"PRAGMA journal_mode = WAL",
		`CREATE TABLE credential (id text PRIMARY KEY, integration_id text, label text NOT NULL,
                 value text NOT NULL, connector_id text, method_id text, active integer,
                 time_created integer NOT NULL, time_updated integer NOT NULL)`,
		"CREATE TABLE session_v2 (id text PRIMARY KEY, title text)",
		"INSERT INTO session_v2 VALUES ('ses_fixture', 'native history')",
	} {
		_, err := database.Exec(statement)
		fixtureMust(err)
	}
	_, err = database.Exec("INSERT INTO credential VALUES ('cred_fixture', 'deepseek', 'DeepSeek', ?1, NULL, NULL, 1, 1, 1)",
		fmt.Sprintf(`{"type":"key","key":"%s"}`, key))
	fixtureMust(err)
}

// LoginBytes is a login as each harness writes it. A higher generation is a
// fresher copy of the same grant, which is what the credential sync orders
// copies by.
func LoginBytes(kind core.HarnessKind, generation int64) []byte {
	access := fmt.Sprintf("access-%d", generation)
	refresh := fmt.Sprintf("refresh-%d", generation)
	var login any
	switch kind {
	case core.HarnessKindCodex:
		login = map[string]any{
			"auth_mode":    "chatgpt",
			"tokens":       map[string]any{"access_token": access, "refresh_token": refresh},
			"last_refresh": fmt.Sprintf("2026-09-%02dT09:00:00Z", 10+generation),
		}
	case core.HarnessKindClaude:
		login = map[string]any{"claudeAiOauth": map[string]any{
			"accessToken":  access,
			"refreshToken": refresh,
			"expiresAt":    int64(1_790_000_000_000) + generation*1000,
		}}
	case core.HarnessKindKimi:
		login = map[string]any{
			"access_token":  access,
			"refresh_token": refresh,
			"expires_at":    int64(1_790_000_000) + generation,
		}
	case core.HarnessKindGrok:
		login = map[string]any{"https://auth.x.ai::1": map[string]any{
			"key":           access,
			"refresh_token": refresh,
			"expires_at":    fmt.Sprintf("2026-10-%02dT09:00:00Z", 10+generation),
		}}
	case core.HarnessKindMuse:
		login = map[string]any{
			"schema_version": 1,
			"providers": map[string]any{"meta": map[string]any{
				"access_token":  access,
				"refresh_token": refresh,
				"expires_at":    int64(1_790_000_000) + generation,
				"mechanism":     "oauth",
			}},
		}
	case core.HarnessKindOpenCode:
		// A staged OpenCode home carries its logins as `auth export` prints
		// them.
		login = []any{map[string]any{
			"id":            "cred_anthropic",
			"integrationID": "anthropic",
			"label":         "OAuth",
			"active":        true,
			"value": map[string]any{
				"type":    "oauth",
				"refresh": refresh,
				"access":  access,
				"expires": int64(1_790_000_000_000) + generation*1000,
			},
		}}
	case core.HarnessKindAntigravity:
		login = map[string]any{"token": map[string]any{
			"access_token": access, "refresh_token": "stable-refresh",
			"expiry": fmt.Sprintf("2026-09-%02dT09:00:00Z", 10+generation),
		}}
	case core.HarnessKindDsh:
		panic("DSH keeps no login; its key comes from DEEPSEEK_API_KEY")
	default:
		panic(fmt.Sprintf("no login fixture for %s", kind))
	}
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	fixtureMust(encoder.Encode(login))
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n"))
}
