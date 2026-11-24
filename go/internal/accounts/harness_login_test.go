// Port of the tests in mj-accounts/src/harness_login.rs.

package accounts

import (
	"io/fs"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// `mj login` for a harness whose CLI is not installed names the missing
// program and how to install it, instead of a bare ENOENT.
//
// Hard-won: 613642d3f7: Launch finding J-12 found login exposed a bare ENOENT for missing Codex; this checks the program, installation command, and profile login remedy.
func TestAMissingLoginProgramIsNamedWithHowToInstallIt(t *testing.T) {
	message := loginSpawnError(fs.ErrNotExist, "codex", core.HarnessKindCodex, "codex").Error()
	for _, want := range []string{"`codex` is not installed", "npm install -g @openai/codex", "mj login --profile codex"} {
		if !strings.Contains(message, want) {
			t.Fatalf("%q lacks %q", message, want)
		}
	}

	other := loginSpawnError(fs.ErrPermission, "codex", core.HarnessKindCodex, "codex")
	if strings.Contains(other.Error(), "not installed") {
		t.Fatalf("%q", other)
	}
}

// Hard-won: 594e6f27: Claude auth status accepted a made-up setup token by exit code alone.
func TestTheVerificationReadsWhichCredentialClaudeCodeActuallyUsed(t *testing.T) {
	for _, status := range []struct {
		stdout string
		method string
		ok     bool
	}{
		{`{"loggedIn":true,"authMethod":"oauth_token"}`, "oauth_token", true},
		{`{"loggedIn":true,"authMethod":"claudeai"}`, "claudeai", true},
		// Output that names no method leaves the exit code as the only
		// check.
		{"Logged in as someone\n", "", false},
		{`{"loggedIn":true}`, "", false},
	} {
		if method, ok := reportedAuthMethod([]byte(status.stdout)); method != status.method || ok != status.ok {
			t.Fatalf("%q: %q, %t", status.stdout, method, ok)
		}
	}
}
