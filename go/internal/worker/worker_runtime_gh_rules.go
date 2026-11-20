// Port of mj-worker/src/worker_runtime/gh_rules.rs.

// The GitHub CLI approval every harness reads from its own configuration.
//
// An agent's issue, pull-request and review work is ordinary session work,
// so Mjolnir trusts `gh` in the configuration file the harness already reads
// rather than interrupting the person once per invocation.
// core.HarnessKind.GhApproval names where each harness keeps that rule; this
// module writes it on every worker start so a profile staged by an older
// release (or a changed execution policy) converges too.
//
// A TOML table (Rust's toml::Table) is a map[string]any here.

//go:build unix

package worker

import (
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ghPatterns are Kimi's and Grok's command patterns.
var ghPatterns = [2]string{"Bash(gh *)", "Bash(gh)"}

// codexGhRule is Codex's Starlark execpolicy rule: any command whose first
// word is `gh`.
const codexGhRule = "prefix_rule(pattern=[\"gh\"], decision=\"allow\")\n"

// musePrefixes is the Muse adapter's variable that lists command prefixes
// it approves.
const musePrefixes = "MUSE_ACP_ALLOW_COMMAND_PREFIXES"

// configureNativeRules installs this session's native permissions: where
// the selected harness accepts Mjolnir's `gh` approval, and the tools
// OpenCode must not offer.
//
// `root` and `sessionID` identify the session's private staged profile: a
// home that is not one may be the person's own, so it is never edited.
func configureNativeRules(root string, sessionID string, home string, harness core.HarnessKind, policy core.ExecutionPolicy, denials openCodeDenials, environment map[string]string) error {
	panic("unported: mj-worker/src/worker_runtime/gh_rules.rs configure_native_rules")
}

// ghRulesConfigureCodex writes the managed Codex rule, creating its `rules/`
// directory when the profile has none. Codex loads every `*.rules` file
// there and its own `default.rules` stays untouched.
func ghRulesConfigureCodex(home string, relative string) error {
	panic("unported: mj-worker/src/worker_runtime/gh_rules.rs configure_codex")
}

// configureTomlGhRules adds narrow GitHub CLI approvals to the staged TOML
// configuration, keeping every rule the person configured.
func configureTomlGhRules(home string, harness core.HarnessKind) error {
	panic("unported: mj-worker/src/worker_runtime/gh_rules.rs configure_toml_gh_rules")
}

// configureMusePrefixes adds `gh` to the prefixes muse-acp approves before
// sending a permission request. Muse keeps command rules in its adapter,
// not its settings file.
func configureMusePrefixes(environment map[string]string) error {
	panic("unported: mj-worker/src/worker_runtime/gh_rules.rs configure_muse_prefixes")
}

func permissionTable(config map[string]any, path string) (map[string]any, error) {
	value, ok := config["permission"]
	if !ok {
		table := map[string]any{}
		config["permission"] = table
		return table, nil
	}
	table, ok := value.(map[string]any)
	if !ok {
		return nil, fmt.Errorf("permission in %s must be a table", path)
	}
	return table, nil
}

func addKimiRules(config map[string]any, path string) (bool, error) {
	panic("unported: mj-worker/src/worker_runtime/gh_rules.rs add_kimi_rules")
}

func addGrokRules(config map[string]any, path string) (bool, error) {
	panic("unported: mj-worker/src/worker_runtime/gh_rules.rs add_grok_rules")
}
