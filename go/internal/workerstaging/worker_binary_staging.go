// Port of mj-worker-staging/src/worker_binary/staging.rs.

package workerstaging

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"os"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/accounts"
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
	if kind != core.HarnessKindClaude {
		return false
	}
	if value, ok := environment[core.ClaudeOauthTokenEnv]; ok && strings.TrimSpace(value) != "" {
		return true
	}
	delete(environment, core.ClaudeOauthTokenEnv)
	token, ok, err := core.ReadClaudeOauthToken(tokenPath)
	if err != nil {
		// A stored token Hel cannot read is worth reporting, but the session
		// still starts on the synced credentials file.
		slog.Warn("ignoring an unreadable Claude setup token", "path", tokenPath, "error", err.Error())
		return false
	}
	if !ok {
		return false
	}
	environment[core.ClaudeOauthTokenEnv] = token
	return true
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
	if profile.Kind != core.HarnessKindClaude || !profile.AuthScheme().UsesNativeLoginFile() {
		return nil
	}
	if _, ok := core.ClaudeEnvironmentAuthFrom(environment); ok {
		return nil
	}
	if _, keychain := profile.Kind.LoginStore(host, profile.Home).(core.HarnessLoginStoreMacKeychain); !keychain {
		return nil
	}
	path := pathJoin(destination, profile.Kind.CredentialFileName())
	credentials, err := accounts.ClaudeKeychainCredentials(executor)
	if err != nil {
		return err
	}
	if credentials != nil {
		copied := core.CredentialCopyForHarness(profile.Kind, credentials)
		if err := core.WriteCredentialFile(profile.Kind, path, copied); err != nil {
			return fmt.Errorf("stage Claude's macOS Keychain login at %s: %w", path, err)
		}
		return nil
	}
	if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("remove a leftover Claude login from the staged home %s: %w", path, rustIOError(err))
	}
	return nil
}

// ConfigureKimiHistoryMCP: Kimi's runtime-aware engine cannot infer a
// runtime identity from an ACP stdio server. Add Hel's history server to
// the session-private profile instead, where Kimi's native schema can bind
// it to the target's local runtime. historySocket is nil when the session
// has none.
func ConfigureKimiHistoryMCP(profileStage, workerRoot string, historySocket *string) error {
	path := pathJoin(profileStage, "mcp.json")
	return EditStagedJSONObject(path, "staged Kimi MCP configuration", func(root map[string]any) error {
		entry, ok := root["mcpServers"]
		if !ok {
			entry = map[string]any{}
			root["mcpServers"] = entry
		}
		servers, ok := entry.(map[string]any)
		if !ok {
			return fmt.Errorf("mcpServers in staged Kimi MCP configuration %s must be a JSON object", path)
		}

		// The worker root and the socket are paths on the target, which is
		// POSIX even when the controller runs on Windows.
		worker := core.Join(workerRoot, "hel")
		socketIsAbsolute := historySocket == nil || core.IsAbsolute(*historySocket)
		var server map[string]any
		if core.IsAbsolute(worker) && socketIsAbsolute {
			args := []any{"worker", "memory-mcp"}
			if historySocket != nil {
				args = append(args, "--history-socket", core.Text(*historySocket))
			}
			server = map[string]any{
				"transport":  "stdio",
				"command":    core.Text(worker),
				"args":       args,
				"runtime_id": "local",
			}
		} else {
			worker := core.Text(worker)
			workerExpression := "\"$HOME/$1\""
			if core.IsAbsolute(worker) {
				workerExpression = "\"$1\""
			}
			script := "exec " + workerExpression + " worker memory-mcp"
			args := []any{"-c", script, "mj-memory", worker}
			if historySocket != nil {
				if core.IsAbsolute(*historySocket) {
					script += " --history-socket \"$2\""
				} else {
					script += " --history-socket \"$HOME/$2\""
				}
				args[1] = script
				args = append(args, core.Text(*historySocket))
			}
			server = map[string]any{
				"transport":  "stdio",
				"command":    "sh",
				"args":       args,
				"runtime_id": "local",
			}
		}
		servers["mj-memory"] = server
		return nil
	})
}

// ConfigureClaudeMailboxHook adds the worker-owned mailbox drain to
// Claude's session-private settings. The command is finalized on the worker
// because remote worker roots are home-relative while staging runs on the
// controller.
func ConfigureClaudeMailboxHook(profileStage, workerRoot string) error {
	path := pathJoin(profileStage, "settings.json")
	worker := core.Join(workerRoot, "hel")
	socket := core.Join(workerRoot, "control.sock")
	command := fmt.Sprintf("MJOLNIR_MAILBOX_HOOK=1 %s worker mailbox-hook --socket %s --event PostToolBatch",
		core.PosixQuote(core.Text(worker)), core.PosixQuote(core.Text(socket)))
	return EditStagedJSONObject(path, "staged Claude settings", func(root map[string]any) error {
		hooksEntry, ok := root["hooks"]
		if !ok {
			hooksEntry = map[string]any{}
			root["hooks"] = hooksEntry
		}
		hooks, ok := hooksEntry.(map[string]any)
		if !ok {
			return fmt.Errorf("hooks in staged Claude settings %s must be a JSON object", path)
		}
		groupsEntry, ok := hooks["PostToolBatch"]
		if !ok {
			groupsEntry = []any{}
		}
		groups, ok := groupsEntry.([]any)
		if !ok {
			return fmt.Errorf("hooks.PostToolBatch in staged Claude settings %s must be a JSON array", path)
		}
		var existing map[string]any
	search:
		for _, group := range groups {
			group, ok := group.(map[string]any)
			if !ok {
				continue
			}
			entries, ok := group["hooks"].([]any)
			if !ok {
				continue
			}
			for _, hook := range entries {
				hook, ok := hook.(map[string]any)
				if !ok {
					continue
				}
				kind, _ := hook["type"].(string)
				text, isText := hook["command"].(string)
				if kind == "command" && isText && strings.HasPrefix(text, "MJOLNIR_MAILBOX_HOOK=1 ") {
					existing = hook
					break search
				}
			}
		}
		if existing != nil {
			existing["command"] = command
			existing["timeout"] = core.MailboxHookTimeoutSecs
		} else {
			groups = append(groups, map[string]any{
				"hooks": []any{map[string]any{
					"type":    "command",
					"command": command,
					"timeout": core.MailboxHookTimeoutSecs,
				}},
			})
		}
		hooks["PostToolBatch"] = groups
		return nil
	})
}

// StageManagedSkills writes Mjolnir's managed skills into a staged profile
// home.
//
// Reserve the same directory and honor the same target scope as
// core.SessionSkills, so later sync preserves this tree.
func StageManagedSkills(kind core.HarnessKind, profileStage string, scope core.SkillsScope) error {
	directory := pathJoin(profileStage, core.MjSkillDirectory(kind))
	if err := os.RemoveAll(directory); err != nil {
		return fmt.Errorf("remove staged mj skill %s: %w", directory, rustIOError(err))
	}
	if scope == core.SkillsScopeIsolated {
		return nil
	}
	for _, entry := range core.ManagedSkills(kind) {
		path := pathJoin(profileStage, entry.Path)
		if parent, ok := pathParent(path); ok {
			if err := os.MkdirAll(parent, 0o777); err != nil {
				return fmt.Errorf("create staged skills directory %s: %w", parent, rustIOError(err))
			}
		}
		if err := core.AtomicWrite(path, entry.Bytes); err != nil {
			return fmt.Errorf("write managed skill %s: %w", path, err)
		}
	}
	return nil
}

// ApplyStagedExecutionSetting writes the enforcement table's staged
// setting, if the harness has one. Muse composes a session's permission
// profile from its settings file and nothing on the ACP wire overrides that
// choice, so the profile has to be staged.
func ApplyStagedExecutionSetting(kind core.HarnessKind, policy core.ExecutionPolicy, profileStage string) error {
	enforcement, ok := kind.ExecutionEnforcement(policy)
	if !ok {
		return nil
	}
	setting, ok := enforcement.StagedSetting()
	if !ok {
		return nil
	}
	path := pathJoin(profileStage, setting.File)
	label := fmt.Sprintf("staged %s settings", kind.DisplayName())
	return EditStagedJSONObject(path, label, func(root map[string]any) error {
		if err := setting.Apply(root); err != nil {
			return fmt.Errorf("%s %s: %w", label, path, err)
		}
		return nil
	})
}

// EditStagedJSONObject reads a staged JSON settings file (treating a
// missing file as an empty object), lets edit change its root object, and
// writes it back atomically, pretty-printed with keys in order as
// serde_json writes them. label names the file in every error message.
func EditStagedJSONObject(path, label string, edit func(root map[string]any) error) error {
	var document any
	body, err := os.ReadFile(path)
	switch {
	case err == nil:
		decoder := json.NewDecoder(bytes.NewReader(body))
		// Numbers keep their text, as serde_json keeps an integer's value.
		decoder.UseNumber()
		err := decoder.Decode(&document)
		if err == nil {
			if _, trailing := decoder.Token(); trailing != io.EOF {
				err = errors.New("trailing characters")
			}
		}
		if err != nil {
			return fmt.Errorf("parse %s %s: %w", label, path, err)
		}
	case errors.Is(err, fs.ErrNotExist):
		document = map[string]any{}
	default:
		return fmt.Errorf("read %s %s: %w", label, path, rustIOError(err))
	}
	root, ok := document.(map[string]any)
	if !ok {
		return fmt.Errorf("%s %s must contain a JSON object", label, path)
	}
	if err := edit(root); err != nil {
		return err
	}
	written, err := upgradeSerdeJSONPretty(root)
	if err != nil {
		return err
	}
	if err := core.AtomicWrite(path, append(written, '\n')); err != nil {
		return fmt.Errorf("write %s %s: %w", label, path, err)
	}
	return nil
}
