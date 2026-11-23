// Port of mj-worker/src/worker_runtime/json_gh_rules.rs.

// Session-owned native permissions for OpenCode: the GitHub CLI approval it
// reads last among its configuration documents, the tools Mjolnir turns
// off, and the Mjolnir guardian's permission hook.
//
// Rust edits the JSONC documents through jsonc_parser's CST, keeping their
// comments and layout; this port uses hujson's syntax tree for the same.
// Text the document already holds is kept byte for byte; the layout of what
// is appended follows the neighbouring entries rather than jsonc_parser's
// own formatter.

//go:build unix

package worker

import (
	"bytes"
	"errors"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
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

// jsonGhRulesMember is jsonc_parser's `CstObject::get(key)`: the value of
// the first member named key.
func jsonGhRulesMember(object *hujson.Object, key string) *hujson.Value {
	for index := range object.Members {
		if name, ok := jsonGhRulesStringValue(&object.Members[index].Name); ok && name == key {
			return &object.Members[index].Value
		}
	}
	return nil
}

// jsonGhRulesArrayValue is `CstObject::array_value(key)`: the member's
// array, absent when the member is missing or holds something else.
func jsonGhRulesArrayValue(object *hujson.Object, key string) (*hujson.Array, bool) {
	value := jsonGhRulesMember(object, key)
	if value == nil {
		return nil, false
	}
	array, ok := value.Value.(*hujson.Array)
	return array, ok
}

// jsonGhRulesArrayValueOrCreate is `CstObject::array_value_or_create(key)`:
// the member's array, a new empty array member when the key is missing, and
// absent when the member holds something other than an array.
func jsonGhRulesArrayValueOrCreate(object *hujson.Object, key string) (*hujson.Array, bool) {
	if value := jsonGhRulesMember(object, key); value != nil {
		array, ok := value.Value.(*hujson.Array)
		return array, ok
	}
	member := hujson.ObjectMember{
		Name:  hujson.Value{Value: hujson.String(key)},
		Value: hujson.Value{BeforeExtra: hujson.Extra(" "), Value: &hujson.Array{}},
	}
	if count := len(object.Members); count > 0 {
		last := &object.Members[count-1]
		member.Name.BeforeExtra = jsonGhRulesSeparator(last.Name.BeforeExtra)
		if last.Value.AfterExtra != nil {
			// Keep the document's trailing comma.
			member.Value.AfterExtra = hujson.Extra{}
		}
	}
	object.Members = append(object.Members, member)
	return object.Members[len(object.Members)-1].Value.Value.(*hujson.Array), true
}

// jsonGhRulesSeparator is the space before an entry appended after one
// whose leading space was `before`: the same line break and indentation
// when the entries sit one per line, else a single space.
func jsonGhRulesSeparator(before hujson.Extra) hujson.Extra {
	index := bytes.LastIndexByte(before, '\n')
	if index < 0 {
		return hujson.Extra(" ")
	}
	rest := before[index+1:]
	indentation := rest[:len(rest)-len(bytes.TrimLeft(rest, " \t"))]
	return hujson.Extra("\n" + string(indentation))
}

// jsonGhRulesAppend is `CstArray::append`: value becomes the last element,
// laid out like the elements before it, and a trailing comma is kept.
func jsonGhRulesAppend(array *hujson.Array, value hujson.ValueTrimmed) {
	element := hujson.Value{Value: value}
	if count := len(array.Elements); count > 0 {
		last := &array.Elements[count-1]
		element.BeforeExtra = jsonGhRulesSeparator(last.BeforeExtra)
		if last.AfterExtra != nil {
			element.AfterExtra = hujson.Extra{}
		}
	}
	array.Elements = append(array.Elements, element)
}

// jsonGhRulesRemove is `CstNode::remove` of an array element: the element
// goes, and a trailing comma stays with the new last element.
func jsonGhRulesRemove(array *hujson.Array, index int) {
	removed := array.Elements[index]
	array.Elements = slices.Delete(array.Elements, index, index+1)
	if index == len(array.Elements) && index > 0 {
		previous := &array.Elements[index-1]
		if removed.AfterExtra != nil {
			if previous.AfterExtra == nil {
				previous.AfterExtra = hujson.Extra{}
			}
		} else {
			previous.AfterExtra = nil
		}
	}
}

// jsonGhRulesObject is an inline object of string members, in order.
func jsonGhRulesObject(members [][2]string) *hujson.Object {
	object := &hujson.Object{}
	for index, member := range members {
		name := hujson.Value{Value: hujson.String(member[0])}
		if index > 0 {
			name.BeforeExtra = hujson.Extra(" ")
		}
		object.Members = append(object.Members, hujson.ObjectMember{
			Name:  name,
			Value: hujson.Value{BeforeExtra: hujson.Extra(" "), Value: hujson.String(member[1])},
		})
	}
	return object
}

func configureOpencodePlugin(directory string, environment map[string]string) error {
	original, ok := environment[opencodeContent]
	if !ok {
		original = "{}"
	}
	document, object, err := jsonGhRulesDocument(original)
	if err != nil {
		return fmt.Errorf("%s: %w", opencodeContent, err)
	}
	plugins, ok := jsonGhRulesArrayValueOrCreate(object, "plugins")
	if !ok {
		return errors.New("OPENCODE_CONFIG_CONTENT plugins must be an array")
	}
	present := slices.ContainsFunc(plugins.Elements, func(node hujson.Value) bool {
		entry, ok := jsonGhRulesStringValue(&node)
		return ok && entry == directory
	})
	if !present {
		jsonGhRulesAppend(plugins, hujson.String(directory))
	}
	environment[opencodeContent] = document.String()
	return nil
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
	encodedRoots := make([]any, len(roots))
	for index, root := range roots {
		encodedRoots[index] = root
	}
	rootsJSON, err := jsonvalue.Marshal(encodedRoots)
	if err != nil {
		return err
	}
	for _, plugin := range [][2]string{{guardianPlugin, "session"}, {guardianReviewerPlugin, "reviewer"}} {
		directory := filepath.Join(root, plugin[0])
		mode := plugin[1]
		if err := os.MkdirAll(directory, 0o777); err != nil {
			return fmt.Errorf("create OpenCode guardian plugin directory %s: %w", directory, err)
		}
		modeJSON, err := jsonvalue.Marshal(mode)
		if err != nil {
			return err
		}
		script := strings.ReplaceAll(opencodeGuardianScript, "__MJ_GUARDIAN_MODE__", string(modeJSON))
		script = strings.ReplaceAll(script, "__MJ_GUARDIAN_ROOTS__", string(rootsJSON))
		path := filepath.Join(directory, "index.js")
		if err := os.WriteFile(path, []byte(script), 0o666); err != nil {
			return fmt.Errorf("write OpenCode guardian hook %s: %w", path, err)
		}
		manifest := fmt.Sprintf(`{"name":"mjolnir-guardian-%s","type":"module","main":"index.js"}`, mode)
		if err := os.WriteFile(filepath.Join(directory, "package.json"), []byte(manifest), 0o666); err != nil {
			return fmt.Errorf("write OpenCode guardian plugin manifest: %w", err)
		}
	}
	return configureOpencodePlugin(filepath.Join(root, guardianPlugin), environment)
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
	original, ok := environment[opencodeContent]
	if !ok {
		return nil, errors.New("this OpenCode session has no Mjolnir configuration document")
	}
	document, object, err := jsonGhRulesDocument(original)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", opencodeContent, err)
	}
	plugins, ok := jsonGhRulesArrayValue(object, "plugins")
	if !ok {
		return nil, errors.New("this OpenCode session registers no Mjolnir plugins")
	}
	var sessionHook string
	found := false
	for index := range plugins.Elements {
		entry, ok := jsonGhRulesStringValue(&plugins.Elements[index])
		if !ok {
			continue
		}
		if name, ok := workerRuntimeFileName(entry); ok && name == guardianPlugin {
			sessionHook, found = entry, true
			break
		}
	}
	if !found {
		return nil, errors.New("this OpenCode session was launched without the guardian's plugin; restart it")
	}
	directory, hasDirectory := workerRuntimePathParent(sessionHook)
	reviewerHook := guardianReviewerPlugin
	if hasDirectory {
		reviewerHook = core.Join(directory, guardianReviewerPlugin)
	}
	for index := 0; index < len(plugins.Elements); {
		entry, ok := jsonGhRulesStringValue(&plugins.Elements[index])
		mjolnir := false
		if ok {
			parent, hasParent := workerRuntimePathParent(entry)
			name, hasName := workerRuntimeFileName(entry)
			sameDirectory := hasParent == hasDirectory && (!hasParent || workerRuntimePathEqual(parent, directory))
			mjolnir = sameDirectory && hasName &&
				slices.Contains([]string{opencodeMailboxPlugin, guardianPlugin, guardianReviewerPlugin}, name)
		}
		if mjolnir {
			jsonGhRulesRemove(plugins, index)
			continue
		}
		index++
	}
	jsonGhRulesAppend(plugins, hujson.String(reviewerHook))
	rules, ok := jsonGhRulesArrayValueOrCreate(object, "permissions")
	if !ok {
		return nil, errors.New("OPENCODE_CONFIG_CONTENT permissions must be an array")
	}
	for _, rule := range reviewerRules {
		jsonGhRulesAppend(rules, jsonGhRulesObject([][2]string{
			{"action", rule.action}, {"resource", rule.resource}, {"effect", rule.effect},
		}))
	}
	reviewer := maps.Clone(environment)
	reviewer[opencodeContent] = document.String()
	return reviewer, nil
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
	elements := rules.Elements
	finalRules := len(elements) >= len(wanted)
	if finalRules {
		tail := elements[len(elements)-len(wanted):]
		for index, rule := range wanted {
			object, ok := tail[index].Value.(*hujson.Object)
			if !ok {
				finalRules = false
				break
			}
			for _, expected := range [][2]string{{"action", rule.action}, {"resource", rule.resource}, {"effect", rule.effect}} {
				value, ok := jsonGhRulesStringValue(jsonGhRulesMember(object, expected[0]))
				if !ok || value != expected[1] {
					finalRules = false
					break
				}
			}
			if !finalRules {
				break
			}
		}
	}
	if finalRules {
		return
	}
	for _, rule := range wanted {
		jsonGhRulesAppend(rules, jsonGhRulesObject([][2]string{
			{"action", rule.action}, {"resource", rule.resource}, {"effect", rule.effect},
		}))
	}
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
	original, ok := environment[opencodeContent]
	if !ok {
		original = "{}"
	}
	document, object, err := jsonGhRulesDocument(original)
	if err != nil {
		return fmt.Errorf("%s: %w", opencodeContent, err)
	}
	rules, ok := jsonGhRulesArrayValueOrCreate(object, "permissions")
	if !ok {
		return errors.New("OPENCODE_CONFIG_CONTENT permissions must be an array")
	}
	// OpenCode 2.2.7 concatenates native permission arrays across config
	// documents. Keeping this as its own document preserves even scalar
	// legacy defaults.
	appendRules(rules, opencodeRules(policy, denials))
	if updated := document.String(); updated != original {
		environment[opencodeContent] = updated
	}
	return nil
}
