// Port of mj-worker/src/worker_runtime/json_gh_rules.rs.

// Session-owned native permissions for OpenCode: the GitHub CLI approval it
// reads last among its configuration documents, the tools Mjolnir turns
// off, and the Mjolnir guardian's permission hook.
//
// Rust edits the JSONC documents through jsonc_parser's CST, keeping their
// comments and layout; this port uses hujson's syntax tree for the same.

//go:build unix

package worker

import (
	"errors"
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/tailscale/hujson"
)

const opencodeContent = "OPENCODE_CONFIG_CONTENT"

var ghCommands = [2]string{"gh", "gh *"}

// openCodeDenials is what OpenCode must not offer the model, whatever the
// approval mode.
type openCodeDenials struct {
	// nativeSubagent denies its own `subagent` tool, when Mjolnir (or
	// nobody) delegates.
	nativeSubagent bool
}

// jsonGhRulesDocument parses a JSONC document whose root must be an object;
// the object is the root's value.
func jsonGhRulesDocument(text string) (*hujson.Value, *hujson.Object, error) {
	document, err := hujson.Parse([]byte(text))
	if err != nil {
		return nil, nil, fmt.Errorf("parse native gh permission configuration: %w", err)
	}
	object, ok := document.Value.(*hujson.Object)
	if !ok {
		return nil, nil, errors.New("native gh permission configuration must be an object")
	}
	return &document, object, nil
}

// jsonGhRulesStringValue is the decoded string of a string literal node.
func jsonGhRulesStringValue(node *hujson.Value) (string, bool) {
	if node == nil {
		return "", false
	}
	literal, ok := node.Value.(hujson.Literal)
	if !ok || literal.Kind() != '"' {
		return "", false
	}
	return literal.String(), true
}

func configureOpencodePlugin(directory string, environment map[string]string) error {
	panic("unported: mj-worker/src/worker_runtime/json_gh_rules.rs configure_opencode_plugin")
}

// jsonGhRulesRule is one native permission rule, as OpenCode 2.2.7 lists
// them in `permissions`: (action, resource, effect).
type jsonGhRulesRule struct {
	action   string
	resource string
	effect   string
}

// permissionlessTools are the OpenCode 2.2.7 tools that change something
// without consulting its permissions, so neither its rules nor the
// guardian's hook would ever see them. Under configured approvals they are
// denied; OpenCode then does not offer them. `worktree_remove` consults only
// `edit`, which an inside-the-workspace removal passes, so it goes too.
// Mjolnir sessions use none of them.
var permissionlessTools = [12]string{
	"worktree_create",
	"worktree_remove",
	"session_move",
	"artifact_create",
	"artifact_update",
	"artifact_delete",
	"schedule_create",
	"schedule_pause",
	"schedule_resume",
	"schedule_delete",
	"console_watch_watch",
	"console_watch_unwatch",
}

// The guardian's OpenCode plugin, one directory per mode. Only the session
// one is registered for the session; the reviewer's environment swaps in the
// other (see opencodeReviewerEnvironment).
const guardianPlugin = "opencode-guardian"
const guardianReviewerPlugin = "opencode-guardian-reviewer"

// configureOpencodeGuardian stages the Mjolnir guardian's OpenCode plugin
// and registers its session mode.
//
// OpenCode 2.2.7's default agent allows every tool (`{"action": "*",
// "resource": "*", "effect": "allow"}` in `packages/schema/src/agent.ts`),
// so under configured approvals nothing would ever ask and the guardian
// (`HarnessKind::guardian`) would review nothing. Rules cannot fix that
// without overriding denies: OpenCode takes the last matching rule, and a
// document Mjolnir supplies last would overrule Plan mode's edit deny, its
// read-only agents and the person's own deny rules. The plugin's
// `permission.evaluate` hook runs after every rule instead, and only turns
// an allowed action that can reach outside the workspace into an ask.
// `roots` is the session's working directory followed by its attached
// directories.
func configureOpencodeGuardian(root string, roots []string, environment map[string]string) error {
	panic("unported: mj-worker/src/worker_runtime/json_gh_rules.rs configure_opencode_guardian")
}

// reviewerRules are the rules a guardian reviewer's OpenCode runs under,
// appended last so they win: it may read, and nothing else. Denied tools
// are not even offered. The reviewer's hook denies the rest whatever an
// earlier per-agent rule says, since those follow these.
var reviewerRules = [8]jsonGhRulesRule{
	{"*", "*", "deny"},
	{"read", "*", "allow"},
	{"read", "*.env", "deny"},
	{"read", "*.env.*", "deny"},
	{"read", "*.env.example", "allow"},
	{"glob", "*", "allow"},
	{"grep", "*", "allow"},
	{"external_directory", "*", "allow"},
}

// opencodeReviewerEnvironment is the environment a guardian reviewer's
// OpenCode starts with: the session's, with the plugins Mjolnir registered
// for the session (its mailbox and the session hook) replaced by the
// reviewer's hook, so the reviewer never drains the session's mailbox, and
// with reviewerRules appended. The person's own plugins stay, since a
// provider may need one to sign in; the reviewer's hook is registered after
// them. A session launched without the guardian's plugin cannot have a
// contained reviewer, so that is an error.
func opencodeReviewerEnvironment(environment map[string]string) (map[string]string, error) {
	panic("unported: mj-worker/src/worker_runtime/json_gh_rules.rs opencode_reviewer_environment")
}

// opencodeRules are the rules this launch appends after every rule a
// document already supplies, so they win: OpenCode takes the last rule that
// matches, and a project document or a profile's own
// `OPENCODE_CONFIG_CONTENT` follows the staged file. Both the `gh` approval
// and the denials are one suffix, so one idempotence check covers them.
func opencodeRules(policy core.ExecutionPolicy, denials openCodeDenials) []jsonGhRulesRule {
	var rules []jsonGhRulesRule
	if policy == core.ExecutionPolicyConfiguredApprovals {
		// OpenCode 2.2.7 names its shell tool's action `shell`; only its
		// legacy `permission` object still maps the old `bash` key onto it.
		for _, command := range ghCommands {
			rules = append(rules, jsonGhRulesRule{"shell", command, "allow"})
		}
		for _, tool := range permissionlessTools {
			rules = append(rules, jsonGhRulesRule{tool, "*", "deny"})
		}
	}
	// With `execute` allowed, OpenCode 2.2.7 offers MCP tools only through a
	// JavaScript `execute` tool ("Code Mode"). Denying it lists the
	// `mj-agents` tools as direct `mj-agents_<tool>` tools, which the
	// delegation policy text and the owned-server approvals name.
	rules = append(rules, jsonGhRulesRule{"execute", "*", "deny"})
	if denials.nativeSubagent {
		rules = append(rules, jsonGhRulesRule{"subagent", "*", "deny"})
	}
	return rules
}

// appendRules appends `wanted` after the rules the document already
// supplies, unless a previous launch already appended exactly them.
// OpenCode takes the last rule that matches, so an earlier identical rule
// may be shadowed by later restrictions.
func appendRules(rules *hujson.Array, wanted []jsonGhRulesRule) {
	panic("unported: mj-worker/src/worker_runtime/json_gh_rules.rs append_rules")
}

// configureOpencodeRules adds this launch's `gh` approval, and the tools
// Mjolnir turns off, to `OPENCODE_CONFIG_CONTENT`.
//
// OpenCode merges its configuration documents in order and takes the last
// matching permission rule. The staged `opencode.json` is read first, and a
// project's own `opencode.json` or a profile's `OPENCODE_CONFIG_CONTENT`
// follows it, so only the document Mjolnir supplies there is guaranteed to
// win. The staged file keeps everything else the profile configured.
func configureOpencodeRules(policy core.ExecutionPolicy, denials openCodeDenials, environment map[string]string) error {
	panic("unported: mj-worker/src/worker_runtime/json_gh_rules.rs configure_opencode_rules")
}
