// Port of mj-worker-staging/src/worker_binary/staging.rs.

package workerstaging

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ApplyClaudeSetupToken hands a Claude worker the profile's long-lived
// setup token, when it has one. Whether the environment now authenticates
// Claude with a setup token.
//
// Claude Code reads `CLAUDE_CODE_OAUTH_TOKEN` ahead of the `/login`
// credentials file, and a setup token does not rotate, so a container copy
// cannot lose the single-use refresh race with the host. A profile that
// sets the variable itself stays authoritative.
func ApplyClaudeSetupToken(environment map[string]string, kind core.HarnessKind, tokenPath string) bool {
	panic("unported: mj-worker-staging/src/worker_binary/staging.rs apply_claude_setup_token")
}

// StageClaudeKeychainLogin makes a session's staged `.credentials.json`
// exactly the host's Claude login when the host keeps it in the macOS
// Keychain (HarnessKind.LoginStore), or removes it when there is none.
//
// Every session runs with `CLAUDE_CONFIG_DIR` set to its staged home, so
// Claude Code looks in a Keychain item named after that directory, which
// never exists, and then in the home's `.credentials.json`. That file is
// where the login reaches every target, local ones included. On a Mac the
// Keychain item is the only login: a `.credentials.json` that StageProfile
// copied from the profile home (earlier releases left session copies
// there) is replaced by the Keychain login or removed, never staged as a
// login. The copy omits the refresh token like every other copy: the daemon
// renews the Keychain login and the credential sync pushes the result
// (`copies_omit_refresh_token`). A Keychain that cannot be read (locked)
// stages no login, as Claude Code would see none; the sync pushes it once
// the Keychain opens.
//
// The setup token takes precedence: when that environment variable is
// present, Claude never reads this file.
func StageClaudeKeychainLogin(profile *core.HarnessProfile, host core.HarnessHost, destination string, environment map[string]string, executor core.CommandExecutor) error {
	panic("unported: mj-worker-staging/src/worker_binary/staging.rs stage_claude_keychain_login")
}

// ConfigureKimiHistoryMCP: Kimi's runtime-aware engine cannot infer a
// runtime identity from an ACP stdio server. Add Hel's history server to
// the session-private profile instead, where Kimi's native schema can bind
// it to the target's local runtime. historySocket is nil when the session
// has none.
func ConfigureKimiHistoryMCP(profileStage, workerRoot string, historySocket *string) error {
	panic("unported: mj-worker-staging/src/worker_binary/staging.rs configure_kimi_history_mcp")
}

// ConfigureClaudeMailboxHook adds the worker-owned mailbox drain to
// Claude's session-private settings. The command is finalized on the worker
// because remote worker roots are home-relative while staging runs on the
// controller.
func ConfigureClaudeMailboxHook(profileStage, workerRoot string) error {
	panic("unported: mj-worker-staging/src/worker_binary/staging.rs configure_claude_mailbox_hook")
}

// StageManagedSkills writes Mjolnir's managed skills into a staged profile
// home.
//
// Reserve the same directory and honor the same target scope as
// core.SessionSkills, so later sync preserves this tree.
func StageManagedSkills(kind core.HarnessKind, profileStage string, scope core.SkillsScope) error {
	panic("unported: mj-worker-staging/src/worker_binary/staging.rs stage_managed_skills")
}

// ApplyStagedExecutionSetting writes the enforcement table's staged
// setting, if the harness has one. Muse composes a session's permission
// profile from its settings file and nothing on the ACP wire overrides that
// choice, so the profile has to be staged.
func ApplyStagedExecutionSetting(kind core.HarnessKind, policy core.ExecutionPolicy, profileStage string) error {
	panic("unported: mj-worker-staging/src/worker_binary/staging.rs apply_staged_execution_setting")
}

// EditStagedJSONObject reads a staged JSON settings file (treating a
// missing file as an empty object), lets edit change its root object, and
// writes it back atomically, pretty-printed with keys in order as
// serde_json writes them. label names the file in every error message.
func EditStagedJSONObject(path, label string, edit func(root map[string]any) error) error {
	panic("unported: mj-worker-staging/src/worker_binary/staging.rs edit_staged_json_object")
}
