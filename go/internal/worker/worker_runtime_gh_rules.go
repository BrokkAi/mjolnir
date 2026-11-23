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
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"

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
	switch approval := harness.GhApproval().(type) {
	case core.GhApprovalSessionConfigDocument:
		// OpenCode reads its documents in order and takes the last matching
		// rule, so its approval and denials are asserted in the document
		// Mjolnir supplies last. Nothing outside the launch environment is
		// read or written for it, so no staged home is involved.
		return configureOpencodeRules(policy, denials, environment)
	case core.GhApprovalAdapterEnvironment:
		// Muse keeps command prefixes in its adapter, not its settings.
		if policy == core.ExecutionPolicyConfiguredApprovals {
			return configureMusePrefixes(environment)
		}
		return nil
	case core.GhApprovalSessionOptions, core.GhApprovalLaunchFile, core.GhApprovalAcpRequest:
		// A session's own request options, and the destinations another part
		// of the launch owns, are written where their contents are decided:
		// `session_request_meta` for Claude, `stage_antigravity_grants` for
		// agy's project grants, and the ACP permission callback for DSH.
		return nil
	case core.GhApprovalStagedFile:
		// Codex reads the rule under every policy, so changing the execution
		// policy never leaves an older profile behind.
		if harness != core.HarnessKindCodex && policy != core.ExecutionPolicyConfiguredApprovals {
			return nil
		}
		if !privateStagedHome(root, sessionID, home) {
			return nil
		}
		switch harness {
		case core.HarnessKindCodex:
			return ghRulesConfigureCodex(home, approval.Value)
		case core.HarnessKindKimi, core.HarnessKindGrok:
			return configureTomlGhRules(home, harness)
		}
		return nil
	}
	return nil
}

// ghRulesConfigureCodex writes the managed Codex rule, creating its `rules/`
// directory when the profile has none. Codex loads every `*.rules` file
// there and its own `default.rules` stays untouched.
func ghRulesConfigureCodex(home string, relative string) error {
	rule := filepath.Join(home, relative)
	directory, ok := workerRuntimePathParent(rule)
	if !ok {
		return errors.New("the Codex rule path must name a file")
	}
	metadata, err := os.Lstat(directory)
	switch {
	case err == nil:
		if !metadata.IsDir() {
			return fmt.Errorf("Codex rules directory %s must be a real directory", directory)
		}
	case errors.Is(err, fs.ErrNotExist):
		if err := os.Mkdir(directory, 0o777); err != nil {
			return fmt.Errorf("create Codex rules directory %s: %w", directory, err)
		}
	default:
		return fmt.Errorf("inspect Codex rules %s: %w", directory, err)
	}
	if !regularFileOrAbsent(rule) {
		return fmt.Errorf("managed Codex rule %s must be a regular file", rule)
	}
	if err := core.AtomicWrite(rule, []byte(codexGhRule)); err != nil {
		return fmt.Errorf("write managed Codex rule %s: %w", rule, err)
	}
	return nil
}

// configureTomlGhRules adds narrow GitHub CLI approvals to the staged TOML
// configuration, keeping every rule the person configured.
func configureTomlGhRules(home string, harness core.HarnessKind) error {
	path := filepath.Join(home, "config.toml")
	metadata, err := os.Lstat(path)
	switch {
	case err == nil:
		if !metadata.Mode().IsRegular() {
			return fmt.Errorf("harness configuration %s must be a regular file", path)
		}
	case errors.Is(err, fs.ErrNotExist):
	default:
		return fmt.Errorf("inspect %s: %w", path, err)
	}
	config, err := readStagedTOML(path, "parse staged harness configuration")
	if err != nil {
		return err
	}
	changed := false
	switch harness {
	case core.HarnessKindKimi:
		changed, err = addKimiRules(config, path)
	case core.HarnessKindGrok:
		changed, err = addGrokRules(config, path)
	}
	if err != nil {
		return err
	}
	if changed {
		text, err := core.TOMLTableToString(config)
		if err != nil {
			return err
		}
		if err := core.AtomicWrite(path, []byte(text)); err != nil {
			return fmt.Errorf("write staged harness configuration %s: %w", path, err)
		}
	}
	return nil
}

// readStagedTOML reads a staged TOML document as a table; a missing file is
// an empty table. `parse` names the document in a parse error.
func readStagedTOML(path string, parse string) (map[string]any, error) {
	text, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return map[string]any{}, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", path, err)
	}
	config, err := core.TOMLTableFromStr(string(text))
	if err != nil {
		return nil, fmt.Errorf("%s %s: %w", parse, path, err)
	}
	return config, nil
}

// configureMusePrefixes adds `gh` to the prefixes muse-acp approves before
// sending a permission request. Muse keeps command rules in its adapter,
// not its settings file.
func configureMusePrefixes(environment map[string]string) error {
	var prefixes [][]string
	if value, ok := environment[musePrefixes]; ok {
		if strings.TrimSpace(value) == "null" {
			return errors.New("parse Muse native command prefixes: invalid type: null, expected a sequence")
		}
		if err := json.Unmarshal([]byte(value), &prefixes); err != nil {
			return fmt.Errorf("parse Muse native command prefixes: %w", err)
		}
	}
	for _, prefix := range prefixes {
		if len(prefix) == 0 || slices.Contains(prefix, "") {
			return errors.New("Muse native command prefixes must contain nonempty argv words")
		}
	}
	if !slices.ContainsFunc(prefixes, func(prefix []string) bool { return slices.Equal(prefix, []string{"gh"}) }) {
		prefixes = append(prefixes, []string{"gh"})
	}
	encoded, err := workerRuntimeMarshalJSON(prefixes)
	if err != nil {
		return err
	}
	environment[musePrefixes] = string(encoded)
	return nil
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

// stagedTOMLArray is `table.entry(key).or_insert(Array).as_array_mut()`: the
// array under key, a new empty one when the key is missing, and false when
// the key holds something else.
func stagedTOMLArray(table map[string]any, key string) ([]any, bool) {
	value, ok := table[key]
	if !ok {
		table[key] = []any{}
		return []any{}, true
	}
	array, ok := value.([]any)
	return array, ok
}

func addKimiRules(config map[string]any, path string) (bool, error) {
	permission, err := permissionTable(config, path)
	if err != nil {
		return false, err
	}
	rules, ok := stagedTOMLArray(permission, "rules")
	if !ok {
		return false, fmt.Errorf("permission.rules in %s must be an array", path)
	}
	changed := false
	for _, pattern := range ghPatterns {
		exists := slices.ContainsFunc(rules, func(rule any) bool {
			table, ok := rule.(map[string]any)
			return ok && table["decision"] == "allow" && table["pattern"] == pattern
		})
		if !exists {
			rules = append(rules, map[string]any{"decision": "allow", "pattern": pattern})
			changed = true
		}
	}
	permission["rules"] = rules
	return changed, nil
}

func addGrokRules(config map[string]any, path string) (bool, error) {
	permission, err := permissionTable(config, path)
	if err != nil {
		return false, err
	}
	allow, ok := stagedTOMLArray(permission, "allow")
	if !ok {
		return false, fmt.Errorf("permission.allow in %s must be an array", path)
	}
	changed := false
	for _, pattern := range ghPatterns {
		if !slices.ContainsFunc(allow, func(value any) bool { return value == pattern }) {
			allow = append(allow, pattern)
			changed = true
		}
	}
	permission["allow"] = allow
	return changed, nil
}
