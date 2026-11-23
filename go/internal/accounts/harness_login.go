// Port of mj-accounts/src/harness_login.rs.

// Harness login driver shared by the local CLI and daemon terminal sessions.
// The ACP sign-in it tries first is in harness_login_acp_login.go (Unix
// only, as in Rust).

package accounts

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"os/exec"
	"strings"
	"unicode"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// LoginOptions are the options for the harness login driver. The zero value
// is Rust's Default.
type LoginOptions struct {
	SetupToken bool
	CLI        bool
	Method     *string
}

// Login runs login with the caller's terminal standard streams. Call it in
// a terminal-owning process, never directly on the daemon event loop. It
// returns the login command's exit code.
func Login(ctx context.Context, profileID string, profile *core.HarnessProfile, options LoginOptions) (int, error) {
	panic("unported: mj-accounts/src/harness_login.rs login")
}

func configuredLoginEnvironment(profile *core.HarnessProfile) map[string]string {
	environment := maps.Clone(profile.Environment.Resolved())
	if environment == nil {
		environment = map[string]string{}
	}
	profile.Kind.ConfigureProfileHomeEnvironment(profile.Home, core.HarnessHostCurrent(), environment)
	return environment
}

// profileCredentialFingerprint is the credential fingerprint a login is
// expected to change; nil when the profile holds no login.
func profileCredentialFingerprint(profile *core.HarnessProfile) (*string, error) {
	panic("unported: mj-accounts/src/harness_login.rs profile_credential_fingerprint")
}

func reportLoginChange(profileID string, profile *core.HarnessProfile, before *string, stored bool) error {
	panic("unported: mj-accounts/src/harness_login.rs report_login_change")
}

// loginSpawnError says which program is missing and how to get it, rather
// than the bare "No such file or directory" the operating system reports.
func loginSpawnError(err error, program string, kind core.HarnessKind, profileID string) error {
	if !errors.Is(err, fs.ErrNotExist) && !errors.Is(err, exec.ErrNotFound) {
		return err
	}
	return fmt.Errorf(
		"`%s` is not installed or is not on PATH, so the %s login cannot run. %s, then run `mj login --profile %s` again.",
		program, kind.DisplayName(), kind.InstallAdvice(), profileID,
	)
}

// storeClaudeSetupToken mints a long-lived Claude subscription token and
// stores it for the profile.
//
// Claude Code has no early-refresh command, and its `/login` grant rotates:
// a container copy that reaches expiry at the same instant as the host
// spends a single-use refresh token the host has already spent, and the
// container's turn dies. `claude setup-token` mints a one-year token that
// never rotates, and Claude Code reads it from `CLAUDE_CODE_OAUTH_TOKEN`
// ahead of the credentials file, so there is nothing left to race.
func storeClaudeSetupToken(ctx context.Context, profileID string, profile *core.HarnessProfile) error {
	panic("unported: mj-accounts/src/harness_login.rs store_claude_setup_token")
}

// reportedAuthMethod is how `claude auth status` says it authenticated, when
// it says so.
//
// Exit code alone proves little: `claude auth status` reports success for
// any value of `CLAUDE_CODE_OAUTH_TOKEN`. What is worth confirming is that
// Claude Code read the variable and gave it precedence, which the reported
// method says. A build whose output carries no method is not a failure to
// report.
func reportedAuthMethod(stdout []byte) (string, bool) {
	var payload map[string]any
	if json.Unmarshal(stdout, &payload) != nil {
		return "", false
	}
	method, ok := payload["authMethod"].(string)
	return method, ok
}

// extractSetupToken is the token in `claude setup-token` output.
//
// The command prints instructions as well as the token, so the last
// whitespace-free `sk-ant-oat01-` line wins. A future format that stops
// using that prefix still leaves the token as the last thing printed, so the
// last non-empty line is the fallback.
func extractSetupToken(stdout string) (string, bool) {
	var candidates []string
	for _, line := range strings.Split(stdout, "\n") {
		if line = strings.TrimSpace(line); line != "" {
			candidates = append(candidates, line)
		}
	}
	for i := len(candidates) - 1; i >= 0; i-- {
		if strings.HasPrefix(candidates[i], "sk-ant-oat01-") && !strings.ContainsFunc(candidates[i], unicode.IsSpace) {
			return candidates[i], true
		}
	}
	if len(candidates) == 0 {
		return "", false
	}
	return candidates[len(candidates)-1], true
}
