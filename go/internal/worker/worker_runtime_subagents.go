// Port of mj-worker/src/worker_runtime/subagents.rs.

// Parent-worker queue behind the Mjolnir sub-agent MCP server.

//go:build unix

package worker

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"maps"
	"net"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/workermcp"
)

// SubagentSocket is the worker root's sub-agent MCP socket.
const SubagentSocket = "subagents.sock"

// opencodeMailboxPlugin is the worker root directory holding the OpenCode
// mailbox plugin.
const opencodeMailboxPlugin = "opencode-mailbox"

// configureOpencodeMailboxHook registers a worker-owned OpenCode 2 plugin
// without editing the user's home.
func configureOpencodeMailboxHook(root string, environment map[string]string) error {
	directory := filepath.Join(root, opencodeMailboxPlugin)
	if err := os.MkdirAll(directory, 0o777); err != nil {
		return fmt.Errorf("create OpenCode plugin directory %s: %w", directory, err)
	}
	worker, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locate worker for OpenCode mailbox transport: %w", err)
	}
	workerJSON, err := jsonvalue.Marshal(worker)
	if err != nil {
		return err
	}
	socketJSON, err := jsonvalue.Marshal(filepath.Join(root, "control.sock"))
	if err != nil {
		return err
	}
	script := strings.ReplaceAll(opencodeMailboxScript, "__MJ_WORKER__", string(workerJSON))
	script = strings.ReplaceAll(script, "__MJ_SOCKET__", string(socketJSON))
	path := filepath.Join(directory, "index.js")
	if err := os.WriteFile(path, []byte(script), 0o666); err != nil {
		return fmt.Errorf("write OpenCode mailbox hook %s: %w", path, err)
	}
	manifest := `{"name":"mjolnir-mailbox","type":"module","main":"index.js"}`
	if err := os.WriteFile(filepath.Join(directory, "package.json"), []byte(manifest), 0o666); err != nil {
		return fmt.Errorf("write OpenCode mailbox plugin manifest: %w", err)
	}
	return configureOpencodePlugin(directory, environment)
}

// subagentsCodexConfigEnv is subagents.rs's own CODEX_CONFIG_ENV; it has the
// same value as codexConfigEnv.
const subagentsCodexConfigEnv = "CODEX_CONFIG"
const codexMailboxHookMarker = "MJOLNIR_MAILBOX_HOOK=1 "
const claudeMailboxHookMarker = "MJOLNIR_MAILBOX_HOOK=1 "

// configureClaudeSubagentMCP registers this launch's `mj-agents` server, and
// the `mj-review` server when the review gate is on, in the session's
// private Claude profile, and allows every tool they list, before Claude
// starts.
//
// `mj-agents` is always loaded. `mj-review` has no `alwaysLoad`, so Claude
// defers its tools until they are needed at push time. A review server the
// launch no longer offers is removed.
//
// The worker owns this registration, as it owns Codex's, and writes it on
// every start. A profile is staged once, but a worker upgrade or recovery
// replaces the launch configuration and binary without restaging it, so a
// registration written only at staging went stale: a profile staged before
// top-level sessions could message each other never got `send_message`.
//
// Paths are absolute on the target, so Claude can launch the server from the
// checkout. Without a role the server is removed. Rules the person already
// has are kept. A home that is not a private staged directory may be the
// person's own, so it is never edited: a delegating parent or a child cannot
// start without its tools there, while a message-only session starts without
// `send_message`. Returns whether the role servers were registered.
// oracle and orchestratorsEnabled are the launch's tool capabilities, which
// decide the tools the role's server lists and so the rules allowed here.
func configureClaudeSubagentMCP(root string, sessionID string, home string, role *core.SubagentMcpRole, oracle bool, orchestratorsEnabled bool, subtleSubagents bool, reviewTools bool) (bool, error) {
	if !filepath.IsAbs(root) {
		return false, errors.New("Claude MCP worker root must be absolute")
	}
	configPath := filepath.Join(home, ".claude.json")
	settingsPath := filepath.Join(home, "settings.json")
	if !(privateStagedHome(root, sessionID, home) && regularFileOrAbsent(configPath) && regularFileOrAbsent(settingsPath)) {
		if role == nil {
			return false, nil
		}
		switch *role {
		case core.SubagentMcpRoleMessageOnly:
			slog.Warn("skipping the Claude send_message server: the profile is not a private staged directory", "path", home)
			return false, nil
		default:
			return false, fmt.Errorf("cannot register the Claude %s tools: %s is not a private staged profile", role.ID(), home)
		}
	}

	err := editProfileJSON(configPath, "staged Claude configuration", func(config *jsonvalue.Object) error {
		existing, hasServers := config.Get("mcpServers")
		if role == nil && !hasServers {
			return nil
		}
		servers := jsonvalue.Object{}
		if hasServers {
			object, ok := existing.(jsonvalue.Object)
			if !ok {
				return fmt.Errorf("mcpServers in %s must be a JSON object", configPath)
			}
			servers = object
		}
		defer func() { config.Set("mcpServers", servers) }()
		if role == nil {
			servers.SwapDelete(core.SubagentMCPServer)
			servers.SwapDelete(core.ReviewMCPServer)
			return nil
		}
		args := []any{
			"worker",
			"subagent-mcp",
			"--socket",
			filepath.Join(root, SubagentSocket),
			// The harness selects the matching MCP server instructions.
			"--harness",
			core.HarnessKindClaude.ID(),
			"--role",
			role.ID(),
		}
		if !subtleSubagents {
			// Only the non-default value is written, as for Codex.
			args = append(args, "--subtle-subagents", "false")
		}
		servers.Set(core.SubagentMCPServer, jsonvalue.Object{
			{Key: "type", Value: "stdio"},
			// Delegation is part of the session's core toolset, like
			// Claude's native Agent tool, rather than optional discovery.
			{Key: "alwaysLoad", Value: true},
			{Key: "command", Value: filepath.Join(root, "hel")},
			{Key: "args", Value: args},
		})
		if reviewTools {
			servers.Set(core.ReviewMCPServer, jsonvalue.Object{
				{Key: "type", Value: "stdio"},
				// No `alwaysLoad`: the tools are needed only before a push,
				// so Claude loads them on demand.
				{Key: "command", Value: filepath.Join(root, "hel")},
				{Key: "args", Value: subagentsStringArray(role.ReviewServerArgs(filepath.Join(root, SubagentSocket), core.HarnessKindClaude.ID()))},
			})
		} else {
			servers.SwapDelete(core.ReviewMCPServer)
		}
		return nil
	})
	if err != nil {
		return false, err
	}

	if role == nil {
		return false, nil
	}
	// In any mode but Auto, Claude would otherwise ask a person before a
	// child's mandatory `handback` or a parent's `spawn` (R11-1).
	err = editProfileJSON(settingsPath, "staged Claude settings", func(settings *jsonvalue.Object) error {
		permissions, err := subagentsObjectEntry(settings, "permissions",
			fmt.Sprintf("permissions in %s must be a JSON object", settingsPath))
		if err != nil {
			return err
		}
		allow, err := subagentsArrayEntry(&permissions, "allow",
			fmt.Sprintf("permissions.allow in %s must be a JSON array", settingsPath))
		if err != nil {
			return err
		}
		var rules []string
		for _, tool := range role.ToolNames(oracle, orchestratorsEnabled) {
			rules = append(rules, fmt.Sprintf("mcp__%s__%s", core.SubagentMCPServer, tool))
		}
		if reviewTools {
			for _, tool := range core.ReviewMCPTools {
				rules = append(rules, fmt.Sprintf("mcp__%s__%s", core.ReviewMCPServer, tool))
			}
		}
		for _, rule := range rules {
			if !slices.ContainsFunc(allow, func(existing any) bool { return jsonvalue.Equal(existing, rule) }) {
				allow = append(allow, rule)
			}
		}
		permissions.Set("allow", allow)
		settings.Set("permissions", permissions)
		return nil
	})
	if err != nil {
		return false, err
	}
	return true, nil
}

// subagentsStringArray is a string list as a JSON array value.
func subagentsStringArray(values []string) []any {
	array := make([]any, len(values))
	for index, value := range values {
		array[index] = value
	}
	return array
}

// subagentsObjectEntry is serde_json's
// `map.entry(key).or_insert(Object).as_object_mut()`: the object under key,
// a new empty one when it is missing, and an error naming `invalid` when the
// key holds something else. Set the result back under key after editing.
func subagentsObjectEntry(object *jsonvalue.Object, key string, invalid string) (jsonvalue.Object, error) {
	value, ok := object.Get(key)
	if !ok {
		created := jsonvalue.Object{}
		object.Set(key, created)
		return created, nil
	}
	nested, ok := value.(jsonvalue.Object)
	if !ok {
		return nil, errors.New(invalid)
	}
	return nested, nil
}

// subagentsArrayEntry is `map.entry(key).or_insert(Array).as_array_mut()`
// for a JSON object. Set the result back under key after editing.
func subagentsArrayEntry(object *jsonvalue.Object, key string, invalid string) ([]any, error) {
	value, ok := object.Get(key)
	if !ok {
		object.Set(key, []any{})
		return []any{}, nil
	}
	array, ok := value.([]any)
	if !ok {
		return nil, errors.New(invalid)
	}
	return array, nil
}

// configureKimiSubagentMCP binds stdio servers to a runtime through Kimi's
// private MCP profile.
func configureKimiSubagentMCP(root string, sessionID string, home string, role core.SubagentMcpRole, reviewTools bool) error {
	path := filepath.Join(home, "mcp.json")
	if !(filepath.IsAbs(root) && privateStagedHome(root, sessionID, home) && regularFileOrAbsent(path)) {
		return fmt.Errorf("cannot register the Kimi %s tools outside a private staged profile", role.ID())
	}
	command := filepath.Join(root, "hel")
	socket := filepath.Join(root, SubagentSocket)
	return editProfileJSON(path, "staged Kimi MCP configuration", func(config *jsonvalue.Object) error {
		servers, err := subagentsObjectEntry(config, "mcpServers", "Kimi mcpServers must be an object")
		if err != nil {
			return err
		}
		servers.Set(core.SubagentMCPServer, jsonvalue.Object{
			{Key: "transport", Value: "stdio"},
			{Key: "command", Value: command},
			{Key: "args", Value: []any{"worker", "subagent-mcp", "--socket", socket, "--harness", "kimi", "--role", role.ID()}},
			{Key: "runtime_id", Value: "local"},
		})
		// The review tools ride the same socket and harness channel as
		// `mj-agents`; without review the entry leaves, as Claude's does.
		if reviewTools {
			servers.Set(core.ReviewMCPServer, jsonvalue.Object{
				{Key: "transport", Value: "stdio"},
				{Key: "command", Value: command},
				{Key: "args", Value: subagentsStringArray(role.ReviewServerArgs(socket, "kimi"))},
				{Key: "runtime_id", Value: "local"},
			})
		} else {
			servers.SwapDelete(core.ReviewMCPServer)
		}
		config.Set("mcpServers", servers)
		return nil
	})
}

// editProfileJSON rewrites a staged profile's JSON file, writing only when
// its content changes. A missing file starts as an empty object. The object
// keeps its member order, as serde_json's preserve_order map does.
func editProfileJSON(path string, label string, edit func(object *jsonvalue.Object) error) error {
	body, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		body = []byte("{}")
	} else if err != nil {
		return fmt.Errorf("read %s %s: %w", label, path, err)
	}
	original, err := jsonvalue.Parse(body)
	if err != nil {
		return fmt.Errorf("parse %s %s: %w", label, path, err)
	}
	object, ok := original.(jsonvalue.Object)
	if !ok {
		return fmt.Errorf("%s %s must be a JSON object", label, path)
	}
	value := object.Clone()
	if err := edit(&value); err != nil {
		return err
	}
	if !subagentsJSONEqual(value, original) {
		resolved, err := workerRuntimeMarshalPretty(value)
		if err != nil {
			return err
		}
		resolved = append(resolved, '\n')
		if err := core.AtomicWrite(path, resolved); err != nil {
			return fmt.Errorf("write %s %s: %w", label, path, err)
		}
	}
	return nil
}

// subagentsJSONEqual is serde_json's Value equality with preserve_order: an
// object's members compare as a map, in any order.
func subagentsJSONEqual(a, b any) bool {
	switch a := a.(type) {
	case jsonvalue.Object:
		other, ok := b.(jsonvalue.Object)
		if !ok {
			return false
		}
		keys := a.Keys()
		if len(keys) != len(other.Keys()) {
			return false
		}
		for _, key := range keys {
			left, _ := a.Get(key)
			right, ok := other.Get(key)
			if !ok || !subagentsJSONEqual(left, right) {
				return false
			}
		}
		return true
	case []any:
		other, ok := b.([]any)
		if !ok || len(a) != len(other) {
			return false
		}
		for index := range a {
			if !subagentsJSONEqual(a[index], other[index]) {
				return false
			}
		}
		return true
	}
	return jsonvalue.Equal(a, b)
}

// privateStagedHome reports whether `home` is a profile this session staged
// for itself: `<root>/profile` on this machine, `profiles/<session>` in a
// container or under a remote login (the controller's
// `removable_profile_root`). An earlier local session's `<root>/profile` may
// link to the person's own home, so each directory is checked without
// following links. Only the local profile's parent and root are resolved for
// comparison, allowing symlinked ancestors without accepting a linked
// profile, profiles, or root directory.
func privateStagedHome(root string, sessionID string, home string) bool {
	name, hasName := workerRuntimeFileName(home)
	parent, hasParent := workerRuntimePathParent(home)
	localProfile := false
	if hasName && name == "profile" && hasParent {
		canonicalParent, parentErr := workerRuntimeCanonicalize(parent)
		canonicalRoot, rootErr := workerRuntimeCanonicalize(root)
		localProfile = parentErr == nil && rootErr == nil && canonicalParent == canonicalRoot
	}
	var stagedDirectories []string
	switch {
	case localProfile:
		stagedDirectories = []string{root, home}
	case hasName && name == sessionID && hasParent:
		if parentName, ok := workerRuntimeFileName(parent); ok && parentName == "profiles" {
			stagedDirectories = []string{parent, home}
		}
	}
	if len(stagedDirectories) == 0 {
		return false
	}
	for _, path := range stagedDirectories {
		metadata, err := os.Lstat(path)
		if err != nil || !metadata.IsDir() {
			return false
		}
	}
	return true
}

// regularFileOrAbsent reports whether a profile file may be edited in place:
// only when it is a regular file or does not exist yet; a link may lead out
// of the private profile.
func regularFileOrAbsent(path string) bool {
	info, err := os.Lstat(path)
	if err != nil {
		return errors.Is(err, fs.ErrNotExist)
	}
	return info.Mode().IsRegular()
}

// dirOrAbsent reports whether a profile subdirectory may be created or
// traversed: only when it is a real directory or does not exist yet; a
// symlink may lead out of the private profile.
func dirOrAbsent(path string) bool {
	info, err := os.Lstat(path)
	if err != nil {
		return errors.Is(err, fs.ErrNotExist)
	}
	return info.IsDir()
}

// configureAntigravityMailboxHook installs the Mjolnir mailbox plugin hook
// in the session-private Antigravity home
// (`<home>/config/plugins/mjolnir-mailbox/{plugin.json,hooks.json}`) so
// `agy acp` discovers and merges it alongside any user `config/hooks.json`.
func configureAntigravityMailboxHook(root string, sessionID string, home string) (bool, error) {
	configDir := filepath.Join(home, "config")
	pluginsDir := filepath.Join(configDir, "plugins")
	pluginDir := filepath.Join(pluginsDir, "mjolnir-mailbox")
	pluginJSONPath := filepath.Join(pluginDir, "plugin.json")
	hooksJSONPath := filepath.Join(pluginDir, "hooks.json")

	safe := filepath.IsAbs(root) &&
		privateStagedHome(root, sessionID, home) &&
		dirOrAbsent(configDir) &&
		dirOrAbsent(pluginsDir) &&
		dirOrAbsent(pluginDir) &&
		regularFileOrAbsent(pluginJSONPath) &&
		regularFileOrAbsent(hooksJSONPath)
	if !safe {
		slog.Warn("skipping Antigravity mailbox hook setup: profile path is not a private staged directory", "path", home)
		return false, nil
	}

	if err := os.MkdirAll(pluginDir, 0o777); err != nil {
		return false, fmt.Errorf("create Antigravity plugin dir %s: %w", pluginDir, err)
	}

	worker, err := os.Executable()
	if err != nil {
		return false, fmt.Errorf("locate worker for Antigravity mailbox hook: %w", err)
	}
	socket := filepath.Join(root, "control.sock")
	hook := func(event string) jsonvalue.Object {
		command := fmt.Sprintf("MJOLNIR_MAILBOX_HOOK=1 %s worker mailbox-hook --socket %s --event %s --format antigravity",
			core.PosixQuote(worker), core.PosixQuote(socket), event)
		return jsonvalue.Object{
			{Key: "type", Value: "command"},
			{Key: "command", Value: command},
			{Key: "timeout", Value: core.MailboxHookTimeoutSecs},
		}
	}

	pluginBytes, err := workerRuntimeMarshalPretty(jsonvalue.Object{{Key: "name", Value: "mjolnir-mailbox"}})
	if err != nil {
		return false, err
	}
	pluginBytes = append(pluginBytes, '\n')
	if existing, err := os.ReadFile(pluginJSONPath); err != nil || !bytes.Equal(existing, pluginBytes) {
		if err := core.AtomicWrite(pluginJSONPath, pluginBytes); err != nil {
			return false, fmt.Errorf("write Antigravity mailbox plugin manifest %s: %w", pluginJSONPath, err)
		}
	}

	hooksBytes, err := workerRuntimeMarshalPretty(jsonvalue.Object{
		{Key: "mjolnir-mailbox", Value: jsonvalue.Object{
			{Key: "PreInvocation", Value: []any{hook("PostToolBatch")}},
			{Key: "Stop", Value: []any{hook("Stop")}},
		}},
	})
	if err != nil {
		return false, err
	}
	hooksBytes = append(hooksBytes, '\n')
	if existing, err := os.ReadFile(hooksJSONPath); err != nil || !bytes.Equal(existing, hooksBytes) {
		if err := core.AtomicWrite(hooksJSONPath, hooksBytes); err != nil {
			return false, fmt.Errorf("write Antigravity mailbox plugin hooks %s: %w", hooksJSONPath, err)
		}
	}

	return true, nil
}

// configureClaudeMailboxHook installs the mailbox command hook in the
// session-private Claude settings and resolves staged remote paths on the
// worker target before Claude reads them.
//
// The Claude home is the staged profile directory itself: `<root>/profile`
// on this machine, `profiles/<session>` in a container or under a remote
// login (the controller's `removable_profile_root`).
func configureClaudeMailboxHook(root string, sessionID string, home string) (bool, error) {
	private := privateStagedHome(root, sessionID, home) && regularFileOrAbsent(filepath.Join(home, "settings.json"))
	if !private {
		slog.Warn("skipping Claude mailbox hook repair: settings are not in a private staged directory", "path", home)
		return false, nil
	}
	path := filepath.Join(home, "settings.json")
	body, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		body = []byte("{}")
	} else if err != nil {
		return false, fmt.Errorf("read %s: %w", path, err)
	}
	parsed, err := jsonvalue.Parse(body)
	if err != nil {
		return false, fmt.Errorf("parse staged Claude settings %s: %w", path, err)
	}
	settings, ok := parsed.(jsonvalue.Object)
	if !ok {
		return false, fmt.Errorf("staged Claude settings %s must be a JSON object", path)
	}
	if !filepath.IsAbs(root) {
		return false, errors.New("Claude worker root must be absolute")
	}
	worker, err := os.Executable()
	if err != nil {
		return false, fmt.Errorf("locate worker for Claude mailbox hook: %w", err)
	}
	socket := filepath.Join(root, "control.sock")
	command := fmt.Sprintf("MJOLNIR_MAILBOX_HOOK=1 %s worker mailbox-hook --socket %s --event PostToolBatch",
		core.PosixQuote(worker), core.PosixQuote(socket))
	hooks, err := subagentsObjectEntry(&settings, "hooks", fmt.Sprintf("hooks in %s must be a JSON object", path))
	if err != nil {
		return false, err
	}
	groups, err := subagentsArrayEntry(&hooks, "PostToolBatch", fmt.Sprintf("hooks.PostToolBatch in %s must be a JSON array", path))
	if err != nil {
		return false, err
	}
	found := false
	for groupIndex, group := range groups {
		groupObject, ok := group.(jsonvalue.Object)
		if !ok {
			continue
		}
		value, _ := groupObject.Get("hooks")
		handlers, ok := value.([]any)
		if !ok {
			continue
		}
		handlerIndex := slices.IndexFunc(handlers, func(handler any) bool {
			return subagentsIsMailboxHandler(handler, claudeMailboxHookMarker)
		})
		if handlerIndex < 0 {
			continue
		}
		handler := handlers[handlerIndex].(jsonvalue.Object)
		handler.Set("command", command)
		handler.Set("timeout", core.MailboxHookTimeoutSecs)
		handlers[handlerIndex] = handler
		groupObject.Set("hooks", handlers)
		groups[groupIndex] = groupObject
		found = true
		break
	}
	if !found {
		groups = append(groups, jsonvalue.Object{
			{Key: "hooks", Value: []any{jsonvalue.Object{
				{Key: "type", Value: "command"},
				{Key: "command", Value: command},
				{Key: "timeout", Value: core.MailboxHookTimeoutSecs},
			}}},
		})
	}
	hooks.Set("PostToolBatch", groups)
	settings.Set("hooks", hooks)
	resolved, err := workerRuntimeMarshalPretty(settings)
	if err != nil {
		return false, err
	}
	resolved = append(resolved, '\n')
	if string(resolved) != string(body) {
		if err := core.AtomicWrite(path, resolved); err != nil {
			return false, fmt.Errorf("write staged Claude settings %s: %w", path, err)
		}
	}
	return true, nil
}

// subagentsIsMailboxHandler is whether handler is a command hook whose
// command starts with marker.
func subagentsIsMailboxHandler(handler any, marker string) bool {
	object, ok := handler.(jsonvalue.Object)
	if !ok {
		return false
	}
	kind, _ := object.Get("type")
	command, _ := object.Get("command")
	text, isText := command.(string)
	return kind == "command" && isText && strings.HasPrefix(text, marker)
}

// configureClaudeSubagentSettings turns off Claude Code behavior that costs a
// managed child inference, tools or network work while doing nothing for a
// headless coding task (audited against Claude Code 2.1.326 with
// claude-agent-acp 0.108.0). The bridge asks for predicted next prompts,
// which can spend an extra model request on a turn; claude.ai connectors are
// enumerated, connected and listed at startup on an eligible login. The
// built-in Concise output style replaces the default prompt's opening
// narration and closing recaps, and any inherited Explanatory or Learning
// style, while keeping outcomes and decisions in the child's reports.
// Project instructions, skills, plugins and configured MCP servers do not
// depend on either. A child overrides the inherited user settings, which
// still apply to top-level sessions; an explicitly exported
// `CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION` is a deliberate choice and still
// wins.
func configureClaudeSubagentSettings(root string, sessionID string, home string) error {
	path := filepath.Join(home, "settings.json")
	if !(privateStagedHome(root, sessionID, home) && regularFileOrAbsent(path)) {
		return nil
	}
	return editProfileJSON(path, "staged Claude settings", func(settings *jsonvalue.Object) error {
		settings.Set("promptSuggestionEnabled", false)
		settings.Set("disableClaudeAiConnectors", true)
		settings.Set("outputStyle", "Concise")
		return nil
	})
}

// excludeDefaultClaudeInstructions keeps the person's `~/.claude/CLAUDE.md`
// out of the session's project instructions. The session's configuration
// lives in the staged profile, so Claude Code finds that file only as an
// ancestor of any checkout under the home directory: it loads the person's
// instructions a second time as project files, and the project then counts
// as having a CLAUDE.md, so its AGENTS.md is not loaded
// (anthropics/claude-code#80580). `userHome` is the HOME the harness runs
// with. Exclusions the person already has are kept.
func excludeDefaultClaudeInstructions(root string, sessionID string, home string, userHome *string) error {
	if userHome == nil || !filepath.IsAbs(*userHome) {
		return nil
	}
	defaultConfig := filepath.Join(*userHome, ".claude")
	if workerRuntimePathEqual(defaultConfig, home) {
		return nil
	}
	path := filepath.Join(home, "settings.json")
	if !(privateStagedHome(root, sessionID, home) && regularFileOrAbsent(path)) {
		return nil
	}
	excluded := filepath.Join(defaultConfig, "CLAUDE.md")
	return editProfileJSON(path, "staged Claude settings", func(settings *jsonvalue.Object) error {
		excludes, err := subagentsArrayEntry(settings, "claudeMdExcludes",
			fmt.Sprintf("claudeMdExcludes in %s must be a JSON array", path))
		if err != nil {
			return err
		}
		if !slices.ContainsFunc(excludes, func(entry any) bool { return entry == excluded }) {
			excludes = append(excludes, excluded)
		}
		settings.Set("claudeMdExcludes", excludes)
		return nil
	})
}

func configureCodexMCPProfile(root string, sessionID string, home string, role *core.SubagentMcpRole, policy core.ExecutionPolicy, subtleSubagents bool, reviewTools bool, managedSubagent bool) (bool, error) {
	if !privateStagedHome(root, sessionID, home) {
		return false, nil
	}
	// The staged copy is session-private, so re-serializing it through a
	// structured table (which drops comments and formatting) is acceptable.
	path := filepath.Join(home, "config.toml")
	config, err := readStagedTOML(path, "parse staged Codex configuration")
	if err != nil {
		return false, err
	}
	changed, err := configureCodexMCPServers(config, path, root, role, policy, subtleSubagents, reviewTools)
	if err != nil {
		return false, err
	}
	if managedSubagent {
		disabled, err := disableCodexSubagentFeatures(config, path)
		if err != nil {
			return false, err
		}
		changed = disabled || changed
	}
	if changed {
		text, err := core.TOMLTableToString(config)
		if err != nil {
			return false, err
		}
		if err := core.AtomicWrite(path, []byte(text)); err != nil {
			return false, fmt.Errorf("write staged Codex configuration %s: %w", path, err)
		}
	}
	return true, nil
}

// codexSubagentDisabledFeatures are the Codex features that cost a managed
// child context, tools, network work or startup latency while doing nothing
// for a headless coding task (audited against Codex 0.170.0). `apps` adds
// hosted connector tools and guidance and can hold the first tool catalog on
// a cold connector fetch; `plugins` adds guidance and a background
// marketplace sync, though the staged home carries no plugin cache;
// `remote_plugin` and `tool_suggest` fetch plugin suggestions on the first
// turn and add an install tool; `image_generation` adds a ~500-token tool
// that spends image quota. AGENTS.md injection, staged skills and configured
// MCP servers do not depend on any of these. Flags that add nothing to a
// headless session (browser and computer use, skill search) are left alone.
// A child overrides the inherited user configuration, which still applies to
// top-level sessions; a project configuration or profile session override
// that enables a feature is a deliberate choice for that scope and still
// wins.
var codexSubagentDisabledFeatures = [5]string{
	"apps",
	"plugins",
	"remote_plugin",
	"tool_suggest",
	"image_generation",
}

// stagedTOMLTable is `table.entry(key).or_insert(Table).as_table_mut()`: the
// table under key, a new empty one when the key is missing, and false when
// the key holds something else. A Go map is shared, so edits need no
// write-back.
func stagedTOMLTable(table map[string]any, key string) (map[string]any, bool) {
	value, ok := table[key]
	if !ok {
		created := map[string]any{}
		table[key] = created
		return created, true
	}
	nested, ok := value.(map[string]any)
	return nested, ok
}

// disableCodexSubagentFeatures edits a decoded TOML table; it reports
// whether the configuration changed.
func disableCodexSubagentFeatures(config map[string]any, path string) (bool, error) {
	features, ok := stagedTOMLTable(config, "features")
	if !ok {
		return false, fmt.Errorf("features in %s must be a table", path)
	}
	changed := false
	for _, feature := range codexSubagentDisabledFeatures {
		if value, ok := features[feature]; !ok || value != false {
			features[feature] = false
			changed = true
		}
	}
	return changed, nil
}

// configureCodexMCPServers returns whether the configuration changed.
func configureCodexMCPServers(config map[string]any, path string, root string, role *core.SubagentMcpRole, policy core.ExecutionPolicy, subtleSubagents bool, reviewTools bool) (bool, error) {
	name := core.SubagentMCPServer
	if _, ok := config["mcp_servers"]; role == nil && !ok {
		return false, nil
	}
	servers, ok := stagedTOMLTable(config, "mcp_servers")
	if !ok {
		return false, fmt.Errorf("mcp_servers in %s must be a table", path)
	}
	if role == nil {
		_, removedAgents := servers[name]
		_, removedReview := servers[core.ReviewMCPServer]
		delete(servers, name)
		delete(servers, core.ReviewMCPServer)
		return removedAgents || removedReview, nil
	}
	worker, err := os.Executable()
	if err != nil {
		return false, fmt.Errorf("locate worker for Codex delegation: %w", err)
	}
	socket := filepath.Join(root, SubagentSocket)
	args := []any{
		"worker",
		"subagent-mcp",
		"--socket",
		socket,
		"--harness",
		"codex",
		"--role",
		role.ID(),
	}
	if !subtleSubagents {
		// Written only when off, so a launch offering both types keeps the
		// arguments an older worker accepts.
		args = append(args, "--subtle-subagents", "false")
	}
	server := map[string]any{
		"command": worker,
		"args":    args,
		// Expose the tools up front, including when code mode is enabled.
		"omit_tools_from": []any{"deferred"},
	}
	if policy == core.ExecutionPolicyConfiguredApprovals {
		server["default_tools_approval_mode"] = "approve"
	}
	servers[name] = server
	if reviewTools {
		// Unlike `mj-agents`, no `omit_tools_from`: Codex may defer these
		// tools until a search finds them, which suits tools needed only
		// before a push.
		review := map[string]any{
			"command": worker,
			"args":    subagentsStringArray(role.ReviewServerArgs(socket, "codex")),
		}
		if policy == core.ExecutionPolicyConfiguredApprovals {
			review["default_tools_approval_mode"] = "approve"
		}
		servers[core.ReviewMCPServer] = review
	} else {
		delete(servers, core.ReviewMCPServer)
	}

	// Keep delegation tools outside the code-mode executor so Codex suspends
	// inference until the actual MCP call returns.
	features, ok := stagedTOMLTable(config, "features")
	if !ok {
		return false, errors.New("features in staged Codex configuration must be a table")
	}
	codeMode, ok := features["code_mode"]
	if !ok {
		codeMode = map[string]any{}
		features["code_mode"] = codeMode
	}
	if enabled, isBool := codeMode.(bool); isBool {
		codeMode = map[string]any{"enabled": enabled}
		features["code_mode"] = codeMode
	}
	codeModeTable, ok := codeMode.(map[string]any)
	if !ok {
		return false, errors.New("features.code_mode in staged Codex configuration must be a table or boolean")
	}
	namespaces, ok := stagedTOMLArray(codeModeTable, "direct_only_tool_namespaces")
	if !ok {
		return false, errors.New("features.code_mode.direct_only_tool_namespaces must be an array")
	}
	const namespace = "mcp__mj_agents"
	if !slices.ContainsFunc(namespaces, func(value any) bool { return value == namespace }) {
		namespaces = append(namespaces, namespace)
	}
	codeModeTable["direct_only_tool_namespaces"] = namespaces
	return true, nil
}

func configureCodexProfile(root string, sessionID string, home string, environment map[string]string, role *core.SubagentMcpRole, policy core.ExecutionPolicy, subtleSubagents bool, reviewTools bool, managedSubagent bool) (bool, error) {
	if err := configureCodexMailboxHook(root, environment); err != nil {
		return false, err
	}
	return configureCodexMCPProfile(root, sessionID, home, role, policy, subtleSubagents, reviewTools, managedSubagent)
}

// configureCodexMailboxHook puts the mailbox hook in codex-acp's per-thread
// config override. Its state trusts only this generated handler; using
// Codex's global trust bypass would also enable untrusted handlers from the
// user's and project's config layers.
func configureCodexMailboxHook(root string, environment map[string]string) error {
	if !filepath.IsAbs(root) {
		return errors.New("Codex worker root must be absolute")
	}
	worker, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locate worker for Codex mailbox hook: %w", err)
	}
	socket := filepath.Join(root, "control.sock")
	command := fmt.Sprintf("%s%s worker mailbox-hook --socket %s --event PostToolUse",
		codexMailboxHookMarker, core.PosixQuote(worker), core.PosixQuote(socket))
	config := jsonvalue.Object{}
	if existing, ok := environment[subagentsCodexConfigEnv]; ok {
		value, err := jsonvalue.Parse([]byte(existing))
		object, isObject := value.(jsonvalue.Object)
		if err != nil || !isObject {
			return fmt.Errorf("%s must be a JSON object for mailbox hooks", subagentsCodexConfigEnv)
		}
		config = object
	}
	hooks, err := subagentsObjectEntry(&config, "hooks", "hooks in CODEX_CONFIG must be a JSON object")
	if err != nil {
		return err
	}
	groups, err := subagentsArrayEntry(&hooks, "PostToolUse", "hooks.PostToolUse in CODEX_CONFIG must be a JSON array")
	if err != nil {
		return err
	}
	groupIndex, handlerIndex := -1, -1
	for index, group := range groups {
		groupObject, ok := group.(jsonvalue.Object)
		if !ok {
			continue
		}
		value, _ := groupObject.Get("hooks")
		handlers, ok := value.([]any)
		if !ok {
			continue
		}
		position := slices.IndexFunc(handlers, func(handler any) bool {
			return subagentsIsMailboxHandler(handler, codexMailboxHookMarker)
		})
		if position < 0 {
			continue
		}
		handler := handlers[position].(jsonvalue.Object)
		handler.Set("command", command)
		handler.Set("timeout", core.MailboxHookTimeoutSecs)
		handlers[position] = handler
		groupObject.Set("hooks", handlers)
		groups[index] = groupObject
		groupIndex, handlerIndex = index, position
		break
	}
	if groupIndex < 0 {
		groups = append(groups, jsonvalue.Object{
			{Key: "hooks", Value: []any{jsonvalue.Object{
				{Key: "type", Value: "command"},
				{Key: "command", Value: command},
				{Key: "timeout", Value: core.MailboxHookTimeoutSecs},
			}}},
		})
		groupIndex, handlerIndex = len(groups)-1, 0
	}
	hooks.Set("PostToolUse", groups)
	hash, err := codexMailboxHookHash(command)
	if err != nil {
		return err
	}
	state, err := subagentsObjectEntry(&hooks, "state", "hooks.state in CODEX_CONFIG must be a JSON object")
	if err != nil {
		return err
	}
	state.Set(fmt.Sprintf("/<session-flags>/config.toml:post_tool_use:%d:%d", groupIndex, handlerIndex),
		jsonvalue.Object{{Key: "enabled", Value: true}, {Key: "trusted_hash", Value: hash}})
	hooks.Set("state", state)
	config.Set("hooks", hooks)
	encoded, err := jsonvalue.Marshal(config)
	if err != nil {
		return err
	}
	environment[subagentsCodexConfigEnv] = string(encoded)
	return nil
}

// codexMailboxHookHash matches Codex 0.159.1's hook identity hash for the
// generated command hook.
func codexMailboxHookHash(command string) (string, error) {
	// Rust passes the identity through a TOML value, whose tables are
	// sorted, before canonicalizing it; canonicalizing sorts it the same way.
	identity := jsonvalue.Object{
		{Key: "event_name", Value: "post_tool_use"},
		{Key: "hooks", Value: []any{jsonvalue.Object{
			{Key: "type", Value: "command"},
			{Key: "command", Value: command},
			{Key: "timeout", Value: core.MailboxHookTimeoutSecs},
			{Key: "async", Value: false},
		}}},
	}
	serialized, err := jsonvalue.Marshal(canonicalizeJSON(identity))
	if err != nil {
		return "", err
	}
	digest := sha256.Sum256(serialized)
	return "sha256:" + hex.EncodeToString(digest[:]), nil
}

func canonicalizeJSON(value any) any {
	switch value := value.(type) {
	case jsonvalue.Object:
		entries := slices.Clone(value)
		slices.SortStableFunc(entries, func(left, right jsonvalue.Member) int { return strings.Compare(left.Key, right.Key) })
		canonical := jsonvalue.Object{}
		for _, entry := range entries {
			canonical.Set(entry.Key, canonicalizeJSON(entry.Value))
		}
		return canonical
	case []any:
		items := make([]any, len(value))
		for i, item := range value {
			items[i] = canonicalizeJSON(item)
		}
		return items
	}
	return value
}

const subagentQueue = "subagents.json"

const maxSocketTasks = 96
const longSocketTasks = 32
const controlSocketTasks = 32
const socketIODeadline = 2 * time.Second

// socketAdmission is two tokio semaphores: a buffered channel holds one
// token per admitted socket task.
type socketAdmission struct {
	long    chan struct{}
	control chan struct{}
}

func defaultSocketAdmission() *socketAdmission {
	return &socketAdmission{
		long:    make(chan struct{}, longSocketTasks),
		control: make(chan struct{}, controlSocketTasks),
	}
}

// workerReplyMargin is how much sooner than the MCP server gives up this
// worker answers a request. The MCP server waits workermcp.ReplyTimeout;
// answering first lets it tell the model what this worker knows (whether the
// daemon has picked the request up) instead of only that nothing came back
// (#1197).
const workerReplyMargin = 10 * time.Second

// subagentsReplyBudget is how long a socket call waits for its result:
// workermcp.ReplyTimeout less workerReplyMargin. A variable only so the test
// that Rust runs on tokio's paused clock can shorten it.
var subagentsReplyBudget = workermcp.ReplyTimeout - workerReplyMargin

// queueState is the durable sub-agent queue (serde deny_unknown_fields; its
// wire form is queueStateWire).
type queueState struct {
	requests           map[string]core.SubagentToolRequest
	results            map[string]core.SubagentToolResult
	completions        map[string]core.SubagentCompletion
	callerAcknowledged map[string]struct{}
	daemonAcknowledged map[string]struct{}
	// legacyDeliveredToWaiter decodes queue files from older workers without
	// using their delivery decisions; it is never written.
	legacyDeliveredToWaiter map[string]bool
	// mutatingAdmissionOpen is protected by the same lock as `requests`,
	// making gate closure atomic with queue admission. Old queue files
	// default to the open state.
	mutatingAdmissionOpen bool
	// collections are the daemon's reads of this queue since this worker
	// started. It lives under the queue's lock so a request is either in a
	// read or after it. Not serialized.
	collections subagentsCollections
	// generation is not serialized.
	generation uint64
	// liveWaiters are waiters currently registered in this worker process.
	// Registration and completion share the queue lock to decide the
	// fallback race once. Not serialized.
	liveWaiters map[string]int
}

// queueStateWire is queueState's serde form, fields in declaration order.
type queueStateWire struct {
	Requests                map[string]core.SubagentToolRequest `json:"requests"`
	Results                 map[string]core.SubagentToolResult  `json:"results"`
	Completions             map[string]core.SubagentCompletion  `json:"completions"`
	CallerAcknowledged      []string                            `json:"caller_acknowledged"`
	DaemonAcknowledged      []string                            `json:"daemon_acknowledged"`
	LegacyDeliveredToWaiter map[string]bool                     `json:"delivered_to_waiter,omitempty"`
	MutatingAdmissionOpen   bool                                `json:"mutating_admission_open"`
}

func defaultQueueState() queueState {
	return queueState{
		requests:                map[string]core.SubagentToolRequest{},
		results:                 map[string]core.SubagentToolResult{},
		completions:             map[string]core.SubagentCompletion{},
		callerAcknowledged:      map[string]struct{}{},
		daemonAcknowledged:      map[string]struct{}{},
		legacyDeliveredToWaiter: map[string]bool{},
		mutatingAdmissionOpen:   true,
		liveWaiters:             map[string]int{},
	}
}

func defaultMutatingAdmission() bool { return true }

// clone is Rust's derive(Clone): every map is copied.
func (s *queueState) clone() queueState {
	return queueState{
		requests:                subagentsCloneMap(s.requests),
		results:                 subagentsCloneMap(s.results),
		completions:             subagentsCloneMap(s.completions),
		callerAcknowledged:      subagentsCloneMap(s.callerAcknowledged),
		daemonAcknowledged:      subagentsCloneMap(s.daemonAcknowledged),
		legacyDeliveredToWaiter: subagentsCloneMap(s.legacyDeliveredToWaiter),
		mutatingAdmissionOpen:   s.mutatingAdmissionOpen,
		collections:             s.collections,
		generation:              s.generation,
		liveWaiters:             subagentsCloneMap(s.liveWaiters),
	}
}

// subagentsCloneMap is maps.Clone that never returns nil, so the copy can
// take writes.
func subagentsCloneMap[K comparable, V any](m map[K]V) map[K]V {
	clone := make(map[K]V, len(m))
	maps.Copy(clone, m)
	return clone
}

// MarshalJSON writes the serialized fields; delivered_to_waiter is never
// written.
func (s queueState) MarshalJSON() ([]byte, error) {
	wire := queueStateWire{
		Requests:              s.requests,
		Results:               s.results,
		Completions:           s.completions,
		CallerAcknowledged:    subagentsSortedSet(s.callerAcknowledged),
		DaemonAcknowledged:    subagentsSortedSet(s.daemonAcknowledged),
		MutatingAdmissionOpen: s.mutatingAdmissionOpen,
	}
	if wire.Requests == nil {
		wire.Requests = map[string]core.SubagentToolRequest{}
	}
	if wire.Results == nil {
		wire.Results = map[string]core.SubagentToolResult{}
	}
	if wire.Completions == nil {
		wire.Completions = map[string]core.SubagentCompletion{}
	}
	return workerRuntimeMarshalJSON(wire)
}

// UnmarshalJSON decodes a queue file: every field defaults, unknown fields
// are refused, and mutating admission defaults to open.
func (s *queueState) UnmarshalJSON(data []byte) error {
	wire := queueStateWire{MutatingAdmissionOpen: defaultMutatingAdmission()}
	if err := workerRuntimeDecodeStrict(data, &wire); err != nil {
		return err
	}
	state := defaultQueueState()
	maps.Copy(state.requests, wire.Requests)
	maps.Copy(state.results, wire.Results)
	maps.Copy(state.completions, wire.Completions)
	for _, id := range wire.CallerAcknowledged {
		state.callerAcknowledged[id] = struct{}{}
	}
	for _, id := range wire.DaemonAcknowledged {
		state.daemonAcknowledged[id] = struct{}{}
	}
	maps.Copy(state.legacyDeliveredToWaiter, wire.LegacyDeliveredToWaiter)
	state.mutatingAdmissionOpen = wire.MutatingAdmissionOpen
	*s = state
	return nil
}

// subagentsSortedSet is a BTreeSet<String> as serde writes it: a sorted
// array, `[]` when empty.
func subagentsSortedSet(set map[string]struct{}) []string {
	sorted := slices.Sorted(maps.Keys(set))
	if sorted == nil {
		return []string{}
	}
	return sorted
}

// subagentsCollections is how often and how recently the daemon has read
// the queue.
type subagentsCollections struct {
	count uint64
	last  *time.Time
}

// queueStateCell is Rust's Arc<Mutex<QueueState>>.
type queueStateCell struct {
	mu    sync.Mutex
	state queueState
}

// subagentHandbackGate is tokio's Mutex<()> behind an Arc: a channel with
// room for one holder. references counts the handback calls that have
// taken the gate, holding it or waiting for it, as Arc::strong_count does
// for Rust's per-call clones.
type subagentHandbackGate struct {
	slot       chan struct{}
	references atomic.Int64
}

func newSubagentHandbackGate() *subagentHandbackGate {
	return &subagentHandbackGate{slot: make(chan struct{}, 1)}
}

// lock waits for the gate; the returned function releases it and drops the
// call's reference.
func (g *subagentHandbackGate) lock(ctx context.Context) (func(), error) {
	g.references.Add(1)
	select {
	case g.slot <- struct{}{}:
		return func() {
			<-g.slot
			g.references.Add(-1)
		}, nil
	case <-ctx.Done():
		g.references.Add(-1)
		return nil, ctx.Err()
	}
}

// SubagentEndpoint is the worker side of the sub-agent queue. Copies share
// the queue, as Rust's Clone does.
type SubagentEndpoint struct {
	path         string
	admission    *socketAdmission
	relay        *RelayCell
	subagentKind *core.SubagentKind
	handbackGate *subagentHandbackGate
	state        *queueStateCell
	// completed is woken whenever a result lands, so a socket call awaiting
	// its own request returns the moment the daemon completes it (tokio's
	// Notify::notify_waiters).
	completed *core.Watch[struct{}]
	// generationWatch is the queue's generation, published by every write so
	// a connection waiting for a change (`WaitForChange`) wakes.
	generationWatch *core.Watch[uint64]
}

// clone is Rust's derive(Clone): the copy shares the queue and its
// notifications, and has its own relay and kind.
func (e *SubagentEndpoint) clone() *SubagentEndpoint {
	copied := *e
	return &copied
}

// SubagentEndpointOpen opens the queue in a worker root, importing results
// an older worker kept inline.
func SubagentEndpointOpen(root string) (*SubagentEndpoint, error) {
	path := filepath.Join(root, subagentQueue)
	state := defaultQueueState()
	body, err := os.ReadFile(path)
	switch {
	case err == nil:
		if err := json.Unmarshal(body, &state); err != nil {
			return nil, fmt.Errorf("parse sub-agent queue %s: %w", path, err)
		}
	case errors.Is(err, fs.ErrNotExist):
	default:
		return nil, fmt.Errorf("read sub-agent queue %s: %w", path, err)
	}
	endpoint := &SubagentEndpoint{
		path:            path,
		admission:       defaultSocketAdmission(),
		handbackGate:    newSubagentHandbackGate(),
		state:           &queueStateCell{state: state},
		completed:       core.NewWatch(struct{}{}),
		generationWatch: core.NewWatch[uint64](0),
	}
	if err := endpoint.importLegacyResults(); err != nil {
		return nil, err
	}
	return endpoint, nil
}

// subagentsHandbackBounced is the reply to a handback the automatic check
// judged unfinished.
const subagentsHandbackBounced = "Not delivered yet: an automatic check judged that your assigned task may still have unfinished work. Compare your report with the assignment. If a required item is still undone and needs no new decision, do it, then hand back. Running low on context is not a reason to stop: your context is compacted automatically, so keep your progress in files and continue. A long wait is not a reason either: when the assignment needs the result of a command that is still running, wait for it to finish. If you were told you could stop here, you need instructions or a decision from the parent, or the task is done, call handback again with your report unchanged (put any question in it); it will be delivered without another check."

func (e *SubagentEndpoint) checkHandback(ctx context.Context, request *core.SubagentToolRequest) (*core.SubagentToolResult, error) {
	handback, ok := request.Action.(core.SubagentToolActionHandback)
	if !ok {
		return nil, nil
	}
	if e.subagentKind == nil || *e.subagentKind != core.SubagentKindWorker {
		return nil, nil
	}
	cached, err := e.cachedResult(request.RequestID)
	if err != nil {
		return nil, err
	}
	if cached != nil {
		return nil, nil
	}
	e.state.mu.Lock()
	_, queued := e.state.state.requests[request.RequestID]
	e.state.mu.Unlock()
	if queued || e.relay == nil {
		return nil, nil
	}
	type preparedCheck struct {
		client     *VerdictClient
		session    string
		generation uint64
		evidence   *core.TurnEvidence
	}
	prepared, err := func() (*preparedCheck, error) {
		owner := e.relay.Lock()
		defer owner.Unlock()
		if owner.usesClaudeHarnessTurns() {
			if err := owner.openHarnessTurnIfAllowed(); err != nil {
				return nil, err
			}
		}
		request.OriginatingCommandID = nil
		if origin, ok := owner.originatingCommandID(); ok {
			request.OriginatingCommandID = &origin
		}
		client := owner.handbackVerdictClient
		if client == nil {
			return nil, nil
		}
		check, err := owner.prepareHandbackCheck(handback.Message)
		if err != nil || check == nil {
			return nil, err
		}
		return &preparedCheck{client: client, session: check.SessionID, generation: check.Generation, evidence: &check.Evidence}, nil
	}()
	if err != nil || prepared == nil {
		return nil, err
	}
	attempt, answer, err := prepared.client.askHandbackLogged(ctx, prepared.session, prepared.generation, prepared.evidence)
	if err != nil {
		reason := "request_failed"
		var netError net.Error
		if errors.Is(err, context.DeadlineExceeded) || (errors.As(err, &netError) && netError.Timeout()) {
			reason = "timeout"
		}
		attempt.finish("delivered", reason)
		return nil, nil
	}
	bounce := core.HandbackNeedsMoreWork(&answer)
	if bounce {
		attempt.finish("bounced", "handback_unfinished")
		return &core.SubagentToolResult{
			RequestID:     request.RequestID,
			CompletedAtMs: core.EpochMillis(),
			IsError:       false,
			Message:       subagentsHandbackBounced,
		}, nil
	}
	attempt.finish("delivered", "handback_accepted")
	return nil, nil
}

func (e *SubagentEndpoint) cachedResult(requestID string) (*core.SubagentToolResult, error) {
	e.state.mu.Lock()
	defer e.state.mu.Unlock()
	return e.readResult(&e.state.state, requestID)
}

func (e *SubagentEndpoint) registerWaiter(requestID string) *liveWaiter {
	e.state.mu.Lock()
	defer e.state.mu.Unlock()
	if e.state.state.liveWaiters == nil {
		e.state.state.liveWaiters = map[string]int{}
	}
	e.state.state.liveWaiters[requestID]++
	return &liveWaiter{cell: e.state, requestID: requestID, active: true}
}

func (e *SubagentEndpoint) awaitResultRegistered(ctx context.Context, requestID string, deadline time.Time, waiter *liveWaiter) (*core.SubagentToolResult, error) {
	timer := time.NewTimer(time.Until(deadline))
	defer timer.Stop()
	for {
		// Take the wake-up before looking, so a completion between the look
		// and the wait is not missed (Notify::enable).
		notified := e.completed.Changed()
		result, err := e.cachedResult(requestID)
		if err != nil {
			return nil, err
		}
		if result != nil {
			waiter.finish()
			return result, nil
		}
		select {
		case <-notified:
		case <-timer.C:
			return waiter.finishAndTakeResult(e)
		case <-ctx.Done():
			return nil, ctx.Err()
		}
	}
}

// SetMutatingAdmission serializes an admission change with every enqueue
// and persists it before reporting success. A request that raced the close
// is either already in `requests` for the controller to drain, or is
// rejected by that enqueue.
func (e *SubagentEndpoint) SetMutatingAdmission(open bool) error {
	e.state.mu.Lock()
	defer e.state.mu.Unlock()
	if e.state.state.mutatingAdmissionOpen == open {
		return nil
	}
	next := e.state.state.clone()
	next.mutatingAdmissionOpen = open
	if err := e.persist(&next); err != nil {
		return err
	}
	e.state.state = next
	return nil
}

// daemonContact reports whether the daemon has read the queue after the read
// numbered `mark`, and how long ago it last read it.
func (e *SubagentEndpoint) daemonContact(mark uint64) workermcp.DaemonContact {
	e.state.mu.Lock()
	collections := e.state.state.collections
	e.state.mu.Unlock()
	contact := workermcp.DaemonContact{PickedUp: collections.count > mark}
	if collections.last != nil {
		seconds := uint64(time.Since(*collections.last) / time.Second)
		contact.LastCollectedSecondsAgo = &seconds
	}
	return contact
}

// enqueueMarkedAtOrigin queues `request`. It also returns whether the
// request was accepted, any result already recorded for it, and the number
// of daemon reads that happened before it joined the queue.
func (e *SubagentEndpoint) enqueueMarkedAtOrigin(request core.SubagentToolRequest, capturedOrigin *string) (bool, *core.SubagentToolResult, uint64, error) {
	// One owner selects the originating turn and keeps it selected until the
	// request is durable; a later daemon never reconstructs this fact.
	var relay *RelayGuard
	if e.relay != nil {
		relay = e.relay.Lock()
		defer relay.Unlock()
	}
	e.state.mu.Lock()
	defer e.state.mu.Unlock()
	state := &e.state.state
	mark := state.collections.count
	result, err := e.readResult(state, request.RequestID)
	if err != nil {
		return false, nil, mark, err
	}
	if result != nil {
		return true, result, mark, nil
	}
	// A retry of a request already accepted before the gate closed is not a
	// new admission. Keep it attached to the original queued operation.
	if _, ok := state.requests[request.RequestID]; ok {
		return true, nil, mark, nil
	}
	acknowledged := 0
	for id := range state.callerAcknowledged {
		if _, ok := state.daemonAcknowledged[id]; ok {
			acknowledged++
		}
	}
	if len(state.requests)+max(len(state.completions)-acknowledged, 0) >= core.RelayRetryIDRetention {
		return false, &core.SubagentToolResult{
			RequestID:     request.RequestID,
			CompletedAtMs: core.EpochMillis(),
			IsError:       true,
			Message:       "Sub-agent request was not accepted: outstanding replies must be acknowledged before more work can be queued.",
		}, mark, nil
	}
	if request.Action.MutatesChildState() && !state.mutatingAdmissionOpen {
		return false, &core.SubagentToolResult{
			RequestID:     request.RequestID,
			CompletedAtMs: core.EpochMillis(),
			IsError:       true,
			Message:       "Sub-agent request was not accepted: this session is replacing its harness (Move). Retry after the Move finishes.",
		}, mark, nil
	}
	if relay != nil {
		// A report is harness work, like output or an autonomous spawn. The
		// MCP socket can deliver it before the adapter's tool-call update, so
		// it opens the turn itself (#1450). Only an admitted request opens
		// one: a retry or a refusal must not.
		if _, handback := request.Action.(core.SubagentToolActionHandback); handback && relay.usesClaudeHarnessTurns() {
			if err := relay.openHarnessTurnIfAllowed(); err != nil {
				return false, nil, mark, err
			}
		}
		// Only the socket handler supplies a worker-captured origin. Never
		// trust an originating_command_id carried in an incoming request.
		if capturedOrigin != nil {
			request.OriginatingCommandID = capturedOrigin
		} else if origin, ok := relay.originatingCommandID(); ok {
			request.OriginatingCommandID = &origin
		} else {
			request.OriginatingCommandID = nil
		}
	}
	next := state.clone()
	if _, ok := next.requests[request.RequestID]; !ok {
		next.requests[request.RequestID] = request
	}
	if err := e.persist(&next); err != nil {
		return false, nil, mark, err
	}
	*state = next
	return true, nil, mark, nil
}

// Complete records the daemon's result for a request; it reports whether a
// live waiter in this worker receives it.
func (e *SubagentEndpoint) Complete(result core.SubagentToolResult) (bool, error) {
	requestID := result.RequestID
	e.state.mu.Lock()
	if _, ok := e.state.state.completions[requestID]; ok {
		e.state.mu.Unlock()
		return true, nil
	}
	delivered := e.state.state.liveWaiters[requestID] > 0
	if err := e.storeResult(&e.state.state, result); err != nil {
		e.state.mu.Unlock()
		return false, err
	}
	e.state.mu.Unlock()
	e.completed.SendReplace(struct{}{})
	return delivered, nil
}

func (e *SubagentEndpoint) storeResult(state *queueState, result core.SubagentToolResult) error {
	requestID := result.RequestID
	next := state.clone()
	delete(next.requests, requestID)
	if err := e.writeResult(&result); err != nil {
		return err
	}
	next.completions[requestID] = core.SubagentCompletionFrom(&result)
	if err := e.retireReceipts(&next); err != nil {
		return err
	}
	if err := e.persist(&next); err != nil {
		return err
	}
	*state = next
	return nil
}

func (e *SubagentEndpoint) persist(state *queueState) error {
	body, err := workerRuntimeMarshalPretty(state)
	if err != nil {
		return err
	}
	if err := core.AtomicWrite(e.path, body); err != nil {
		return fmt.Errorf("write sub-agent queue %s: %w", e.path, err)
	}
	state.generation++
	// The writer still holds the queue lock, so a woken waiter reads the
	// queue after this write is committed.
	e.generationWatch.SendReplace(state.generation)
	return nil
}

// generations changes after every write to the queue.
func (e *SubagentEndpoint) generations() *core.Watch[uint64] {
	return e.generationWatch
}

// liveWaiter is one waiter registered in queueState.liveWaiters. Rust's Drop
// is finish; callers defer it.
type liveWaiter struct {
	cell      *queueStateCell
	requestID string
	active    bool
}

// finishAndTakeResult ends the waiter and inspects its result under the same
// lock used by Complete, so timeout and completion have one serialized
// winner.
func (w *liveWaiter) finishAndTakeResult(endpoint *SubagentEndpoint) (*core.SubagentToolResult, error) {
	w.cell.mu.Lock()
	defer w.cell.mu.Unlock()
	liveWaiterRemoveFrom(&w.active, w.requestID, &w.cell.state)
	return endpoint.readResult(&w.cell.state, w.requestID)
}

func (w *liveWaiter) finish() {
	w.cell.mu.Lock()
	defer w.cell.mu.Unlock()
	liveWaiterRemoveFrom(&w.active, w.requestID, &w.cell.state)
}

func liveWaiterRemoveFrom(active *bool, requestID string, state *queueState) {
	if !*active {
		return
	}
	if waiters, ok := state.liveWaiters[requestID]; ok {
		if waiters <= 1 {
			delete(state.liveWaiters, requestID)
		} else {
			state.liveWaiters[requestID] = waiters - 1
		}
	}
	*active = false
}

// socketReply is the worker's answer on the sub-agent socket (serde
// deny_unknown_fields).
type socketReply struct {
	Accepted               bool                     `json:"accepted"`
	AcknowledgmentRequired bool                     `json:"acknowledgment_required"`
	Result                 *core.SubagentToolResult `json:"result,omitempty"`
	// Daemon is set only when Result is absent.
	Daemon *workermcp.DaemonContact `json:"daemon,omitempty"`
}

// UnmarshalJSON requires `accepted` and refuses unknown fields.
func (r *socketReply) UnmarshalJSON(data []byte) error {
	type plain socketReply
	var value plain
	if err := workerRuntimeDecodeStrict(data, &value, "accepted"); err != nil {
		return err
	}
	*r = socketReply(value)
	return nil
}

// subagentsServe binds the sub-agent socket in `root` and serves it until
// the returned guard is released.
func subagentsServe(runtime *core.Runtime, root string, relay *RelayCell, harness core.HarnessKind, subagentKind *core.SubagentKind) (*SubagentEndpoint, *subagentSocketGuard, error) {
	endpoint, err := SubagentEndpointOpen(root)
	if err != nil {
		return nil, nil, err
	}
	// Preserve a closed gate if this worker restarts during Move recovery. A
	// replacement worker explicitly reopens admission after it reconnects.
	endpoint.relay = relay
	endpoint.subagentKind = subagentKind
	path := filepath.Join(root, SubagentSocket)
	_ = os.Remove(path)
	listener, err := core.BindUnixListener(path)
	if err != nil {
		return nil, nil, fmt.Errorf("bind sub-agent socket %s: %w", path, err)
	}
	listener.SetUnlinkOnClose(false)
	if err := os.Chmod(path, 0o600); err != nil {
		listener.Close()
		return nil, nil, err
	}
	ctx, cancel := context.WithCancel(context.Background())
	service := endpoint.clone()
	task := core.Spawn(func() struct{} {
		var tasks sync.WaitGroup
		defer tasks.Wait()
		slots := make(chan struct{}, maxSocketTasks)
		stop := context.AfterFunc(ctx, func() { listener.Close() })
		defer stop()
		for {
			select {
			case slots <- struct{}{}:
			case <-ctx.Done():
				return struct{}{}
			}
			stream, err := listener.Accept()
			if err != nil {
				<-slots
				if ctx.Err() != nil {
					return struct{}{}
				}
				slog.Warn("sub-agent socket accept failed", "error", err)
				select {
				case <-time.After(100 * time.Millisecond):
				case <-ctx.Done():
					return struct{}{}
				}
				continue
			}
			tasks.Go(func() {
				defer func() { <-slots }()
				defer func() {
					if recovered := recover(); recovered != nil {
						slog.Error("sub-agent socket task panicked", "error", fmt.Sprint(recovered))
					}
				}()
				if err := subagentsServeOne(ctx, stream, service); err != nil {
					slog.Warn("sub-agent MCP dispatch failed", "error", err.Error())
				}
			})
		}
	})
	guard := socketGuard(path)
	return endpoint, &subagentSocketGuard{cancel: cancel, task: task, socket: &guard}, nil
}

// subagentSocketGuard stops the socket's accept loop and removes the socket
// when released (Rust's Drop).
type subagentSocketGuard struct {
	cancel context.CancelFunc
	task   *core.JoinHandle[struct{}]
	socket *socketGuard
}

// Release is Rust's Drop: it aborts the accept loop and removes the socket.
func (g *subagentSocketGuard) Release() {
	g.cancel()
	if g.socket != nil {
		g.socket.Close()
		g.socket = nil
	}
}

// subagentsSocketTimeout is whether err is an expired socket deadline.
func subagentsSocketTimeout(err error) bool {
	var netError net.Error
	return errors.Is(err, os.ErrDeadlineExceeded) || (errors.As(err, &netError) && netError.Timeout())
}

func subagentsServeOne(ctx context.Context, stream net.Conn, endpoint *SubagentEndpoint) error {
	defer stream.Close()
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	// The task ends with its context; closing the stream ends every wait on
	// it, as aborting the Rust task does.
	stop := context.AfterFunc(ctx, func() { stream.Close() })
	defer stop()
	reader := bufio.NewReader(stream)
	if err := stream.SetReadDeadline(time.Now().Add(socketIODeadline)); err != nil {
		return err
	}
	line, ok, err := readBoundedLine(reader, core.RelayCommandByteBudget)
	if err != nil {
		if subagentsSocketTimeout(err) {
			return errors.New("sub-agent request read timed out")
		}
		return err
	}
	if !ok {
		return errors.New("sub-agent request ended before a frame")
	}
	if err := stream.SetReadDeadline(time.Time{}); err != nil {
		return err
	}
	// The socket request is serde's untagged enum: an acknowledgement, or a
	// tool request.
	var acknowledge struct {
		AcknowledgeResult *string `json:"acknowledge_result"`
	}
	if json.Unmarshal([]byte(line), &acknowledge) == nil && acknowledge.AcknowledgeResult != nil {
		if err := endpoint.acknowledgeReplies([]string{*acknowledge.AcknowledgeResult}, true); err != nil {
			return err
		}
		return subagentsWriteReply(ctx, stream, socketReply{Accepted: true})
	}
	var request core.SubagentToolRequest
	if err := json.Unmarshal([]byte(line), &request); err != nil {
		return fmt.Errorf("parse sub-agent request: data did not match any variant of untagged enum SocketRequest: %w", err)
	}
	// Origin attribution belongs to this worker, never to the socket caller.
	request.OriginatingCommandID = nil
	lane := endpoint.admission.control
	switch request.Action.(type) {
	case core.SubagentToolActionSpawn, core.SubagentToolActionSpawnOrchestrator:
		lane = endpoint.admission.long
	}
	select {
	case lane <- struct{}{}:
		defer func() { <-lane }()
	default:
		return subagentsWriteReply(ctx, stream, socketReply{
			Accepted: false,
			Result: &core.SubagentToolResult{
				RequestID:     request.RequestID,
				CompletedAtMs: core.EpochMillis(),
				IsError:       true,
				Message:       "Sub-agent request was not accepted: this operation's queue is full; retry later.",
			},
		})
	}
	requestID := request.RequestID
	deadlineBudget := subagentsReplyBudget
	var queuedInput *core.SubagentToolResult
	if input, ok := request.Action.(core.SubagentToolActionSendInput); ok {
		message, err := jsonvalue.Marshal(jsonvalue.Object{
			{Key: "request_id", Value: requestID},
			{Key: "child_session_id", Value: input.ChildSessionID},
			{Key: "status", Value: "queued"},
			{Key: "next_action", Value: "Input is stored, not yet confirmed delivered. Use list_agents to check delivery; do not resend it."},
		})
		if err != nil {
			return err
		}
		queuedInput = &core.SubagentToolResult{
			RequestID:     requestID,
			CompletedAtMs: core.EpochMillis(),
			IsError:       false,
			Message:       string(message),
		}
	}
	started := time.Now()
	// Register before durable enqueue: the daemon can collect and complete a
	// request as soon as the queue write becomes visible.
	var waiter *liveWaiter
	if queuedInput == nil {
		waiter = endpoint.registerWaiter(requestID)
		defer waiter.finish()
	}
	// The gate makes an in-flight check the only decision for the child's
	// assignment, so concurrent handbacks wait until that check completes.
	var releaseGate func()
	if _, handback := request.Action.(core.SubagentToolActionHandback); handback {
		release, err := endpoint.handbackGate.lock(ctx)
		if err != nil {
			return err
		}
		releaseGate = release
	}
	unlockGate := func() {
		if releaseGate != nil {
			releaseGate()
			releaseGate = nil
		}
	}
	defer unlockGate()
	// The optional classifier fails open after five seconds.
	bounced, err := endpoint.checkHandback(ctx, &request)
	if err != nil {
		return err
	}
	if bounced != nil {
		if _, err := endpoint.Complete(*bounced); err != nil {
			return err
		}
	}
	capturedOrigin := request.OriginatingCommandID
	request.OriginatingCommandID = nil
	accepted, queued, mark, err := endpoint.enqueueMarkedAtOrigin(request, capturedOrigin)
	if err != nil {
		return err
	}
	unlockGate()
	if !accepted {
		return subagentsWriteReply(ctx, stream, socketReply{Accepted: false, Result: queued})
	}
	var result *core.SubagentToolResult
	acknowledgmentRequired := false
	switch {
	case queued != nil:
		result, acknowledgmentRequired = queued, true
	case queuedInput != nil:
		result = queuedInput
	default:
		result, err = endpoint.awaitResultRegistered(ctx, requestID, started.Add(deadlineBudget), waiter)
		if err != nil {
			return err
		}
		acknowledgmentRequired = result != nil
	}
	// Without a result, say whether the daemon has picked the request up: a
	// daemon that is restarting or stalled otherwise looks the same as a slow
	// action, and the model should act differently on the two.
	var daemon *workermcp.DaemonContact
	if result == nil {
		contact := endpoint.daemonContact(mark)
		daemon = &contact
	}
	return subagentsWriteReply(ctx, stream, socketReply{
		Accepted:               true,
		AcknowledgmentRequired: acknowledgmentRequired,
		Result:                 result,
		Daemon:                 daemon,
	})
}

func subagentsWriteReply(ctx context.Context, write net.Conn, reply socketReply) error {
	body, err := workerRuntimeMarshalJSON(reply)
	if err != nil {
		return err
	}
	body = append(body, '\n')
	if err := write.SetWriteDeadline(time.Now().Add(socketIODeadline)); err != nil {
		return err
	}
	if _, err := write.Write(body); err != nil {
		if subagentsSocketTimeout(err) {
			return errors.New("sub-agent response write timed out")
		}
		return fmt.Errorf("answer sub-agent request: %w", err)
	}
	return nil
}
