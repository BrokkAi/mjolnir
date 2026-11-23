// Port of the tests in mj-worker/src/worker_runtime/subagents.rs (its
// `tests` and `review_gate_tests` modules) and their test-only helpers.

//go:build unix

package worker

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// configureCodexMCP registers the owned server in Codex's session-private
// profile (Rust's test-only configure_codex_mcp).
func configureCodexMCP(root string, home string, role *core.SubagentMcpRole, policy core.ExecutionPolicy) (bool, error) {
	return configureCodexMCPWithMailboxes(root, "", home, role, policy, true, true, false, false)
}

// snapshot is the queued requests and the completed results (Rust's
// test-only SubagentEndpoint::snapshot).
func (e *SubagentEndpoint) snapshot() ([]core.SubagentToolRequest, []core.SubagentToolResult) {
	e.state.mu.Lock()
	defer e.state.mu.Unlock()
	requests := []core.SubagentToolRequest{}
	for _, id := range subagentsSortedKeys(e.state.state.requests) {
		requests = append(requests, e.state.state.requests[id])
	}
	results := []core.SubagentToolResult{}
	for _, id := range subagentsSortedKeys(e.state.state.completions) {
		result, err := e.readResult(&e.state.state, id)
		if err != nil {
			panic(err)
		}
		if result != nil {
			results = append(results, *result)
		}
	}
	return requests, results
}

func subagentsSortedKeys[V any](m map[string]V) []string {
	keys := make([]string, 0, len(m))
	for key := range m {
		keys = append(keys, key)
	}
	slices.Sort(keys)
	return keys
}

// recordCollection counts one daemon read of the queue (Rust's test-only
// record_collection).
func (e *SubagentEndpoint) recordCollection() {
	e.state.mu.Lock()
	defer e.state.mu.Unlock()
	e.state.state.collections.count++
	now := time.Now()
	e.state.state.collections.last = &now
}

// enqueue queues request and returns any result already recorded for it.
func (e *SubagentEndpoint) enqueue(request core.SubagentToolRequest) (*core.SubagentToolResult, error) {
	_, result, _, err := e.enqueueMarked(request)
	return result, err
}

// enqueueMarked queues request; it also returns whether it was accepted and
// the daemon reads that happened before it joined the queue.
func (e *SubagentEndpoint) enqueueMarked(request core.SubagentToolRequest) (bool, *core.SubagentToolResult, uint64, error) {
	return e.enqueueMarkedAtOrigin(request, nil)
}

func subagentsTestRequest(id string) core.SubagentToolRequest {
	return core.SubagentToolRequest{RequestID: id, CreatedAtMs: 1, Action: core.SubagentToolActionListAgents{}}
}

func subagentsTestDone(id string) core.SubagentToolResult {
	return core.SubagentToolResult{RequestID: id, CompletedAtMs: 2, IsError: false, Message: "done"}
}

func subagentsRole(role core.SubagentMcpRole) *core.SubagentMcpRole { return &role }

func subagentsMust(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func subagentsWrite(t *testing.T, path string, body string) {
	t.Helper()
	subagentsMust(t, os.WriteFile(path, []byte(body), 0o666))
}

func subagentsRead(t *testing.T, path string) string {
	t.Helper()
	body, err := os.ReadFile(path)
	subagentsMust(t, err)
	return string(body)
}

// subagentsJSONFile decodes a JSON file as generic values.
func subagentsJSONFile(t *testing.T, path string) map[string]any {
	t.Helper()
	var value map[string]any
	subagentsMust(t, json.Unmarshal([]byte(subagentsRead(t, path)), &value))
	return value
}

// subagentsGet walks keys through nested objects; nil when one is missing.
func subagentsGet(value any, keys ...any) any {
	for _, key := range keys {
		switch key := key.(type) {
		case string:
			object, _ := value.(map[string]any)
			value = object[key]
		case int:
			array, _ := value.([]any)
			if key >= len(array) {
				return nil
			}
			value = array[key]
		}
	}
	return value
}

func subagentsStrings(value any) []string {
	var strings []string
	for _, item := range value.([]any) {
		strings = append(strings, item.(string))
	}
	return strings
}

func subagentsExecutable(t *testing.T) string {
	t.Helper()
	worker, err := os.Executable()
	subagentsMust(t, err)
	return worker
}

// subagentsSocketPair is Rust's UnixStream::pair.
func subagentsSocketPair(t *testing.T) (net.Conn, net.Conn) {
	t.Helper()
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM, 0)
	subagentsMust(t, err)
	conn := func(fd int, name string) net.Conn {
		file := os.NewFile(uintptr(fd), name)
		defer file.Close()
		connection, err := net.FileConn(file)
		subagentsMust(t, err)
		return connection
	}
	return conn(fds[0], "client"), conn(fds[1], "server")
}

// subagentsServeOnce runs subagentsServeOne on its own goroutine; the
// channel delivers its result.
func subagentsServeOnce(stream net.Conn, endpoint *SubagentEndpoint) <-chan error {
	served := make(chan error, 1)
	go func() { served <- subagentsServeOne(context.Background(), stream, endpoint) }()
	return served
}

// Hard-won: #1473: OpenCode must retain each message before acknowledging the relay lease.
func TestOpencodeMailboxPluginRetainsBeforeAckAndDeduplicatesRetries(t *testing.T) {
	root := t.TempDir()
	environment := map[string]string{
		"OPENCODE_CONFIG_CONTENT": "{ // keep user plugins\n\"plugins\": [\"user-plugin\"], \"permissions\": [] }",
	}
	subagentsMust(t, configureOpencodeMailboxHook(root, environment))
	once := make(map[string]string)
	for key, value := range environment {
		once[key] = value
	}
	subagentsMust(t, configureOpencodeMailboxHook(root, environment))
	if !reflect.DeepEqual(environment, once) {
		t.Fatalf("a repeat registration changed the environment:\n%v\n%v", once, environment)
	}
	if !strings.Contains(environment["OPENCODE_CONFIG_CONTENT"], "user-plugin") {
		t.Fatal(environment["OPENCODE_CONFIG_CONTENT"])
	}
	transport := filepath.Join(root, "transport.mjs")
	subagentsWrite(t, transport, `#!/usr/bin/env node
import { createConnection } from "node:net";
let input = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", chunk => input += chunk);
process.stdin.on("end", () => {
  const connection = createConnection(process.argv.at(-1));
  let reply = "";
  connection.setEncoding("utf8");
  connection.on("connect", () => connection.write(JSON.stringify({
    request_id: "test", protocol_version: 1, request: JSON.parse(input),
  }) + "\n"));
  connection.on("data", chunk => reply += chunk);
  connection.on("end", () => {
    if (reply) process.stdout.write(JSON.stringify(JSON.parse(reply).payload));
    else process.exitCode = 1;
  });
});
`)
	subagentsMust(t, os.Chmod(transport, 0o755))
	pluginPath := filepath.Join(root, "opencode-mailbox/index.js")
	workerJSON, err := jsonvalue.Marshal(subagentsExecutable(t))
	subagentsMust(t, err)
	transportJSON, err := jsonvalue.Marshal(transport)
	subagentsMust(t, err)
	script := strings.ReplaceAll(subagentsRead(t, pluginPath), string(workerJSON), string(transportJSON))
	subagentsWrite(t, pluginPath, script)
	command := exec.Command("node", "--input-type=module", "-")
	command.Env = append(os.Environ(), "MJ_HOOK_PLUGIN="+pluginPath, "MJ_HOOK_SOCKET="+filepath.Join(root, "control.sock"))
	output, err := core.RunWithInput(command, []byte(`
import assert from "node:assert/strict";
import { createServer } from "node:net";
import { pathToFileURL } from "node:url";
const plugin = (await import(pathToFileURL(process.env.MJ_HOOK_PLUGIN))).default;
const retained = new Map();
const first = {key: "oracle", text: "mailbox:" + "界\u{1f680}".repeat(20 * 1024)};
const second = {key: "child", text: "second message"};
let contexts = [first];
let drains = 0;
let acknowledgements = 0;
let rejectRetention = true;
let loseAcknowledgement = true;
const server = createServer(connection => {
  let input = "";
  connection.setEncoding("utf8");
  connection.on("data", chunk => {
    input += chunk;
    if (!input.includes("\n")) return;
    const request = JSON.parse(input.slice(0, input.indexOf("\n")));
    let payload;
    if (request.request.method === "drain_mailbox") {
      drains++;
      payload = {type: "mailbox_drained", data: {lease_id: "lease-" + drains, contexts}};
    } else {
      assert.equal(request.request.method, "ack_mailbox");
      assert.ok(contexts.every(context => [...retained.values()].includes(context.text)),
        "every message must already be retained when the relay is acknowledged");
      acknowledgements++;
      if (loseAcknowledgement) {
        loseAcknowledgement = false;
        connection.end();
        return;
      }
      payload = {type: "mailbox_acknowledged", data: {acknowledged: true}};
    }
    connection.end(JSON.stringify({request_id: request.request_id,
      protocol_version: request.protocol_version, result: "ok", payload}) + "\n");
  });
});
await new Promise(resolve => server.listen(process.env.MJ_HOOK_SOCKET, resolve));
let hook;
let disposed = false;
try {
  const cleanup = await plugin.setup({
    session: {
      get: async ({sessionID}) => ({parentID: sessionID === "native-child" ? "lead" : undefined}),
      synthetic: async ({id, text, delivery, resume}) => {
        assert.equal(delivery, "steer");
        assert.equal(resume, false);
        if (rejectRetention) {
          rejectRetention = false;
          throw new Error("retention failed");
        }
        if (retained.has(id)) assert.equal(retained.get(id), text);
        retained.set(id, text);
      },
    },
    tool: {hook: async (name, callback) => {
      assert.equal(name, "execute.after");
      hook = callback;
      return {dispose: () => {disposed = true}};
    }},
  });
  const event = {sessionID: "lead", status: "completed", result: {content: "tool output", output: {ok: true}}};
  await assert.rejects(hook(event), /retention failed/);
  assert.equal(acknowledgements, 0);
  await assert.rejects(hook(event), /mailbox request exited/);
  assert.equal(retained.size, 1);
  // An expired unacknowledged lease can include newer events on retry.
  contexts = [first, second];
  await hook(event);
  assert.equal(retained.size, 2);
  assert.deepEqual(event.result, {content: "tool output", output: {ok: true}});
  const child = {sessionID: "native-child", status: "completed", result: {content: "child output"}};
  await hook(child);
  assert.equal(drains, 3);
  assert.equal(acknowledgements, 2);
  await cleanup();
  assert.ok(disposed);
} finally {
  await new Promise(resolve => server.close(resolve));
}
console.log("OpenCode durable mailbox bridge passed");
`))
	subagentsMust(t, err)
	if !output.Status.Success() {
		t.Fatalf("%s", output.Stderr)
	}
	if got := strings.TrimSpace(string(output.Stdout)); got != "OpenCode durable mailbox bridge passed" {
		t.Fatalf("stdout = %q", got)
	}
}

func TestAKimiReviewerRegistersHandbackInItsPrivateRuntimeProfile(t *testing.T) {
	root := t.TempDir()
	home := filepath.Join(root, "profile")
	subagentsMust(t, os.Mkdir(home, 0o777))
	subagentsWrite(t, filepath.Join(home, "mcp.json"), `{"mcpServers":{"existing":{"command":"keep"}}}`)
	subagentsMust(t, configureKimiSubagentMCP(root, "reviewer", home, core.SubagentMcpRoleChild, false))
	config := subagentsJSONFile(t, filepath.Join(home, "mcp.json"))
	if subagentsGet(config, "mcpServers", "existing", "command") != "keep" {
		t.Fatalf("config = %v", config)
	}
	if subagentsGet(config, "mcpServers", "mj-review") != nil {
		t.Fatalf("config = %v", config)
	}
	server := subagentsGet(config, "mcpServers", "mj-agents")
	if subagentsGet(server, "runtime_id") != "local" || subagentsGet(server, "transport") != "stdio" {
		t.Fatalf("server = %v", server)
	}
	if subagentsGet(server, "command") != filepath.Join(root, "hel") {
		t.Fatalf("server = %v", server)
	}
	expected := []string{"worker", "subagent-mcp", "--socket", filepath.Join(root, SubagentSocket), "--harness", "kimi", "--role", "child"}
	if got := subagentsStrings(subagentsGet(server, "args")); !slices.Equal(got, expected) {
		t.Fatalf("args = %q", got)
	}
	outside := t.TempDir()
	if err := configureKimiSubagentMCP(root, "reviewer", outside, core.SubagentMcpRoleChild, false); err == nil {
		t.Fatal("a home outside the staged profile must be refused")
	}
}

func TestAKimiPrimaryWithReviewRegistersAndRetiresTheReviewServer(t *testing.T) {
	root := t.TempDir()
	home := filepath.Join(root, "profile")
	subagentsMust(t, os.Mkdir(home, 0o777))
	role := core.SubagentMcpRoleMessageOnly
	subagentsMust(t, configureKimiSubagentMCP(root, "primary", home, role, true))
	config := subagentsJSONFile(t, filepath.Join(home, "mcp.json"))
	server := subagentsGet(config, "mcpServers", "mj-review")
	if subagentsGet(server, "runtime_id") != "local" || subagentsGet(server, "transport") != "stdio" {
		t.Fatalf("server = %v", server)
	}
	expected := []string{"worker", "subagent-mcp", "--socket", filepath.Join(root, SubagentSocket), "--harness", "kimi", "--review-server"}
	if got := subagentsStrings(subagentsGet(server, "args")); !slices.Equal(got, expected) {
		t.Fatalf("args = %q", got)
	}
	// A later launch without review removes the registration again.
	subagentsMust(t, configureKimiSubagentMCP(root, "primary", home, role, false))
	config = subagentsJSONFile(t, filepath.Join(home, "mcp.json"))
	if subagentsGet(config, "mcpServers", "mj-review") != nil || subagentsGet(config, "mcpServers", "mj-agents") == nil {
		t.Fatalf("config = %v", config)
	}
}

// Hard-won: a602a316a: a symlinked data ancestor disabled Claude messaging and native steering.
func TestClaudeProfileRepairAcceptsSymlinkedAncestorsWithDifferentPathSpellings(t *testing.T) {
	temp := t.TempDir()
	data := filepath.Join(temp, "data")
	alias := filepath.Join(temp, "data-link")
	stagedRoot := filepath.Join(data, "workers/session")
	subagentsMust(t, os.MkdirAll(filepath.Join(stagedRoot, "profile"), 0o777))
	subagentsMust(t, os.Symlink(data, alias))
	aliasRoot := filepath.Join(alias, "workers/session")
	owner, err := WorkerRootOwnerAcquire(aliasRoot)
	subagentsMust(t, err)
	defer owner.Release()
	canonicalRoot := owner.Root()

	for _, pair := range [][2]string{
		{canonicalRoot, filepath.Join(aliasRoot, "profile")},
		{aliasRoot, filepath.Join(canonicalRoot, "profile")},
	} {
		root, home := pair[0], pair[1]
		subagentsWrite(t, filepath.Join(home, ".claude.json"), "{}")
		subagentsWrite(t, filepath.Join(home, "settings.json"), "{}")
		_, err := configureClaudeSubagentMCP(root, "session", home, subagentsRole(core.SubagentMcpRoleMessageOnly), true, true, false)
		subagentsMust(t, err)
		hooked, err := configureClaudeMailboxHook(root, "session", home)
		subagentsMust(t, err)
		if !hooked {
			t.Fatalf("%s: the hook must be installed", home)
		}
		config := subagentsJSONFile(t, filepath.Join(home, ".claude.json"))
		if subagentsGet(config, "mcpServers", "mj-agents", "command") != filepath.Join(root, "hel") {
			t.Fatalf("config = %v", config)
		}
		settings := subagentsJSONFile(t, filepath.Join(home, "settings.json"))
		if got := subagentsStrings(subagentsGet(settings, "permissions", "allow")); !slices.Equal(got, []string{
			"mcp__mj-agents__send_message", "mcp__mj-agents__fetch", "mcp__mj-agents__ask_oracle",
		}) {
			t.Fatalf("allow = %q", got)
		}
		command := subagentsGet(settings, "hooks", "PostToolBatch", 0, "hooks", 0, "command").(string)
		if !strings.HasSuffix(command, fmt.Sprintf("--socket %s --event PostToolBatch", core.PosixQuote(filepath.Join(root, "control.sock")))) {
			t.Fatalf("command = %s", command)
		}
	}
}

// Hard-won: 3c8973b87: the check expected <root>/profile/.claude, so it refused every real Claude home and steering was never offered.
func TestClaudeMailboxHookResolvesTheWorkerSocketAndPreservesProfileSettings(t *testing.T) {
	workerRoot := t.TempDir()
	stagedHome := filepath.Join(workerRoot, "profile")
	subagentsMust(t, os.MkdirAll(stagedHome, 0o777))
	remoteRoot := ".local/share/hel/workers/session"
	path := filepath.Join(stagedHome, "settings.json")
	userHook := map[string]any{"hooks": []any{map[string]any{"type": "command", "command": "user-hook"}}}
	relativeCommand := fmt.Sprintf("MJOLNIR_MAILBOX_HOOK=1 %s worker mailbox-hook --socket %s --event PostToolBatch",
		core.PosixQuote(remoteRoot+"/hel"), core.PosixQuote(remoteRoot+"/control.sock"))
	initial, err := json.Marshal(map[string]any{
		"model": "opus",
		"hooks": map[string]any{"PostToolBatch": []any{
			userHook,
			map[string]any{"hooks": []any{map[string]any{"type": "command", "command": relativeCommand, "timeout": 10}}},
		}},
	})
	subagentsMust(t, err)
	subagentsWrite(t, path, string(initial))

	hooked, err := configureClaudeMailboxHook(workerRoot, "session", stagedHome)
	subagentsMust(t, err)
	if !hooked {
		t.Fatal("the hook must be installed")
	}
	first := subagentsRead(t, path)
	hooked, err = configureClaudeMailboxHook(workerRoot, "session", stagedHome)
	subagentsMust(t, err)
	if !hooked || subagentsRead(t, path) != first {
		t.Fatal("a repeat repair must leave the settings as they are")
	}

	var settings map[string]any
	subagentsMust(t, json.Unmarshal([]byte(first), &settings))
	if settings["model"] != "opus" {
		t.Fatalf("settings = %v", settings)
	}
	groups := subagentsGet(settings, "hooks", "PostToolBatch").([]any)
	if !reflect.DeepEqual(groups[0], userHook) {
		t.Fatalf("user hook = %v", groups[0])
	}
	var mailboxHooks []map[string]any
	for _, group := range groups {
		hooks, _ := subagentsGet(group, "hooks").([]any)
		for _, hook := range hooks {
			command, _ := subagentsGet(hook, "command").(string)
			if strings.HasPrefix(command, claudeMailboxHookMarker) {
				mailboxHooks = append(mailboxHooks, hook.(map[string]any))
			}
		}
	}
	if len(mailboxHooks) != 1 {
		t.Fatalf("mailbox hooks = %v", mailboxHooks)
	}
	if mailboxHooks[0]["timeout"] != float64(core.MailboxHookTimeoutSecs) {
		t.Fatalf("timeout = %v", mailboxHooks[0]["timeout"])
	}
	expectedCommand := fmt.Sprintf("%s%s worker mailbox-hook --socket %s --event PostToolBatch",
		claudeMailboxHookMarker, core.PosixQuote(subagentsExecutable(t)), core.PosixQuote(filepath.Join(workerRoot, "control.sock")))
	if mailboxHooks[0]["command"] != expectedCommand {
		t.Fatalf("command = %v", mailboxHooks[0]["command"])
	}

	subagentsWrite(t, path, `{"model":"opus"}`)
	hooked, err = configureClaudeMailboxHook(workerRoot, "session", stagedHome)
	subagentsMust(t, err)
	if !hooked {
		t.Fatal("the hook must be installed")
	}
	repaired := subagentsJSONFile(t, path)
	if repaired["model"] != "opus" {
		t.Fatalf("repaired = %v", repaired)
	}
	if command, _ := subagentsGet(repaired, "hooks", "PostToolBatch", 0, "hooks", 0, "command").(string); !strings.HasPrefix(command, claudeMailboxHookMarker) {
		t.Fatalf("command = %v", command)
	}

	// An earlier local session ran from the person's own home, and its
	// staged path links there. Neither path may be edited.
	sourceHome := t.TempDir()
	sourceSettings := filepath.Join(sourceHome, "settings.json")
	subagentsWrite(t, sourceSettings, `{"user":"source"}`)
	legacyRoot := t.TempDir()
	linkedHome := filepath.Join(legacyRoot, "profile")
	subagentsMust(t, os.Symlink(sourceHome, linkedHome))
	for _, home := range []string{sourceHome, linkedHome} {
		hooked, err := configureClaudeMailboxHook(legacyRoot, "session", home)
		subagentsMust(t, err)
		if hooked {
			t.Fatalf("%s must not be edited", home)
		}
	}
	if got := subagentsRead(t, sourceSettings); got != `{"user":"source"}` {
		t.Fatalf("source = %s", got)
	}
}

// Hard-won: 3c8973b87: a remote profile outside the worker root kept its home-relative hook command.
func TestClaudeMailboxHookRepairsARemoteProfileOutsideTheWorkerRoot(t *testing.T) {
	login := t.TempDir()
	root := filepath.Join(login, ".local/share/hel/workers/worker-7")
	home := filepath.Join(login, ".local/share/hel/profiles/session-7")
	subagentsMust(t, os.MkdirAll(root, 0o777))
	subagentsMust(t, os.MkdirAll(home, 0o777))
	// The controller wrote a home-relative command, which Claude would run
	// from the checkout.
	subagentsWrite(t, filepath.Join(home, "settings.json"),
		`{"hooks":{"PostToolBatch":[{"hooks":[{"type":"command","command":"MJOLNIR_MAILBOX_HOOK=1 '.local/share/hel/workers/worker-7/hel' worker mailbox-hook"}]}]}}`)

	hooked, err := configureClaudeMailboxHook(root, "session-7", home)
	subagentsMust(t, err)
	if !hooked {
		t.Fatal("the hook must be repaired")
	}
	settings := subagentsJSONFile(t, filepath.Join(home, "settings.json"))
	command := subagentsGet(settings, "hooks", "PostToolBatch", 0, "hooks", 0, "command").(string)
	if !strings.HasSuffix(command, fmt.Sprintf("--socket %s --event PostToolBatch", core.PosixQuote(filepath.Join(root, "control.sock")))) {
		t.Fatalf("command = %s", command)
	}

	// Another session's profile, or one that is not under `profiles`, is
	// not this session's.
	hooked, err = configureClaudeMailboxHook(root, "session-8", home)
	subagentsMust(t, err)
	if hooked {
		t.Fatal("another session's profile must not be edited")
	}
	elsewhere := filepath.Join(login, "session-7")
	subagentsMust(t, os.Mkdir(elsewhere, 0o777))
	hooked, err = configureClaudeMailboxHook(root, "session-7", elsewhere)
	subagentsMust(t, err)
	if hooked {
		t.Fatal("a profile outside `profiles` must not be edited")
	}
}

func TestClaudeChildSettingsExcludeTheDefaultInstructionsAndDropNoiseOnce(t *testing.T) {
	login := t.TempDir()
	root := filepath.Join(login, "worker")
	home := filepath.Join(root, "profile")
	subagentsMust(t, os.MkdirAll(home, 0o777))
	subagentsWrite(t, filepath.Join(home, "settings.json"),
		`{"model":"opus","claudeMdExcludes":["/work/CLAUDE.md"],"promptSuggestionEnabled":true,"outputStyle":"Explanatory"}`)

	for range 2 {
		subagentsMust(t, excludeDefaultClaudeInstructions(root, "session", home, &login))
		subagentsMust(t, configureClaudeSubagentSettings(root, "session", home))
	}
	settings := subagentsJSONFile(t, filepath.Join(home, "settings.json"))
	expected := map[string]any{
		"model":            "opus",
		"claudeMdExcludes": []any{"/work/CLAUDE.md", filepath.Join(login, ".claude/CLAUDE.md")},
		// The child overrides the person's explicit setting.
		"promptSuggestionEnabled":   false,
		"disableClaudeAiConnectors": true,
		"outputStyle":               "Concise",
	}
	if !reflect.DeepEqual(settings, expected) {
		t.Fatalf("settings = %v", settings)
	}

	// A home that is not this session's staged profile is never edited.
	own := filepath.Join(login, ".claude")
	subagentsMust(t, os.MkdirAll(own, 0o777))
	subagentsMust(t, excludeDefaultClaudeInstructions(root, "session", own, &login))
	subagentsMust(t, configureClaudeSubagentSettings(root, "session", own))
	if _, err := os.Stat(filepath.Join(own, "settings.json")); err == nil {
		t.Fatal("the person's own settings must not be written")
	}
}

// Hard-won: #1302: a nested staging link allowed writes to the source profile.
func TestClaudeMailboxRepairRejectsNestedSymlinks(t *testing.T) {
	root := t.TempDir()
	outside := t.TempDir()
	home := filepath.Join(root, "profile")
	subagentsMust(t, os.Symlink(outside, home))
	path := filepath.Join(outside, "settings.json")
	// Check both missing hooks and a hook that the startup repair would
	// refresh.
	for _, settings := range []string{`{}`, `{"hooks":{"PostToolBatch":[{"hooks":[{"type":"command","command":"MJOLNIR_MAILBOX_HOOK=1 old"}]}]}}`} {
		subagentsWrite(t, path, settings)
		hooked, err := configureClaudeMailboxHook(root, "session", home)
		subagentsMust(t, err)
		if hooked || subagentsRead(t, path) != settings {
			t.Fatal("a linked profile must not be edited")
		}
	}
	subagentsMust(t, os.Remove(home))
	subagentsMust(t, os.Mkdir(home, 0o777))
	subagentsMust(t, os.Symlink(path, filepath.Join(home, "settings.json")))
	before := subagentsRead(t, path)
	hooked, err := configureClaudeMailboxHook(root, "session", home)
	subagentsMust(t, err)
	if hooked || subagentsRead(t, path) != before {
		t.Fatal("linked settings must not be edited")
	}

	// A root that is itself a link stays unsafe, even when the home is
	// spelled through the real directory. Unrelated profiles stay unsafe.
	subagentsMust(t, os.Remove(filepath.Join(home, "settings.json")))
	linkedRoot := filepath.Join(outside, "worker-link")
	subagentsMust(t, os.Symlink(root, linkedRoot))
	unrelatedHome := filepath.Join(outside, "profile")
	subagentsMust(t, os.Mkdir(unrelatedHome, 0o777))
	for _, pair := range [][2]string{
		{linkedRoot, home},
		{linkedRoot, filepath.Join(linkedRoot, "profile")},
		{root, unrelatedHome},
	} {
		root, home := pair[0], pair[1]
		for _, name := range []string{"settings.json", ".claude.json"} {
			subagentsWrite(t, filepath.Join(home, name), "{}")
		}
		hooked, err := configureClaudeMailboxHook(root, "session", home)
		subagentsMust(t, err)
		if hooked {
			t.Fatalf("%s %s must not be edited", root, home)
		}
		_, err = configureClaudeSubagentMCP(root, "session", home, subagentsRole(core.SubagentMcpRoleMessageOnly), true, true, false)
		subagentsMust(t, err)
		for _, name := range []string{"settings.json", ".claude.json"} {
			if got := subagentsRead(t, filepath.Join(home, name)); got != "{}" {
				t.Fatalf("%s = %s", name, got)
			}
		}
	}
}

func TestCodexThreadMailboxHookPreservesOverridesAndTrustsOnlyItsHandler(t *testing.T) {
	workerRoot := t.TempDir()
	userHook := map[string]any{"hooks": []any{map[string]any{"type": "command", "command": "user-hook"}}}
	initial, err := json.Marshal(map[string]any{
		"model": "session-model",
		"hooks": map[string]any{"PostToolUse": []any{userHook}},
	})
	subagentsMust(t, err)
	environment := map[string]string{subagentsCodexConfigEnv: string(initial)}

	subagentsMust(t, configureCodexMailboxHook(workerRoot, environment))
	first := environment[subagentsCodexConfigEnv]
	subagentsMust(t, configureCodexMailboxHook(workerRoot, environment))
	if environment[subagentsCodexConfigEnv] != first {
		t.Fatal("a repeat installation must not change CODEX_CONFIG")
	}

	var config map[string]any
	subagentsMust(t, json.Unmarshal([]byte(first), &config))
	if config["model"] != "session-model" {
		t.Fatalf("config = %v", config)
	}
	groups := subagentsGet(config, "hooks", "PostToolUse").([]any)
	if !reflect.DeepEqual(groups[0], userHook) || len(groups) != 2 {
		t.Fatalf("groups = %v", groups)
	}
	hook := subagentsGet(groups[1], "hooks", 0)
	if subagentsGet(hook, "type") != "command" || subagentsGet(hook, "timeout") != float64(core.MailboxHookTimeoutSecs) {
		t.Fatalf("hook = %v", hook)
	}
	expectedCommand := fmt.Sprintf("%s%s worker mailbox-hook --socket %s --event PostToolUse",
		codexMailboxHookMarker, core.PosixQuote(subagentsExecutable(t)), core.PosixQuote(filepath.Join(workerRoot, "control.sock")))
	if subagentsGet(hook, "command") != expectedCommand {
		t.Fatalf("command = %v", subagentsGet(hook, "command"))
	}
	expectedHash, err := codexMailboxHookHash(expectedCommand)
	subagentsMust(t, err)
	state := subagentsGet(config, "hooks", "state", "/<session-flags>/config.toml:post_tool_use:1:0")
	if subagentsGet(state, "enabled") != true || subagentsGet(state, "trusted_hash") != expectedHash {
		t.Fatalf("state = %v", state)
	}
	if states := subagentsGet(config, "hooks", "state").(map[string]any); len(states) != 1 {
		t.Fatalf("states = %v", states)
	}
	if _, ok := config["bypass_hook_trust"]; ok {
		t.Fatal("the global trust bypass must stay off")
	}
}

func TestMailboxHooksReplaceOldHandlersAndPreserveUserHandlers(t *testing.T) {
	claudeRoot := t.TempDir()
	claudeHome := filepath.Join(claudeRoot, "profile")
	subagentsMust(t, os.MkdirAll(claudeHome, 0o777))
	claudeSettingsPath := filepath.Join(claudeHome, "settings.json")
	subagentsWrite(t, claudeSettingsPath, `{"hooks":{"PostToolBatch":[{"hooks":[{"type":"command","command":"user-hook"},{"type":"command","command":"MJOLNIR_MAILBOX_HOOK=1 old-worker worker mailbox-hook"}]}]}}`)
	_, err := configureClaudeMailboxHook(claudeRoot, "session", claudeHome)
	subagentsMust(t, err)
	claude := subagentsJSONFile(t, claudeSettingsPath)
	claudeHooks := subagentsGet(claude, "hooks", "PostToolBatch", 0, "hooks").([]any)
	if len(claudeHooks) != 2 || subagentsGet(claudeHooks[0], "command") != "user-hook" {
		t.Fatalf("hooks = %v", claudeHooks)
	}
	if command := subagentsGet(claudeHooks[1], "command").(string); !strings.HasPrefix(command, claudeMailboxHookMarker) {
		t.Fatalf("command = %s", command)
	}

	codexRoot := t.TempDir()
	userHook := map[string]any{"hooks": []any{map[string]any{"type": "command", "command": "user-hook"}}}
	initial, err := json.Marshal(map[string]any{"hooks": map[string]any{
		"PostToolUse": []any{userHook, map[string]any{"hooks": []any{
			map[string]any{"type": "command", "command": "MJOLNIR_MAILBOX_HOOK=1 old-worker worker mailbox-hook"},
		}}},
		"state": map[string]any{"/<session-flags>/config.toml:post_tool_use:1:0": map[string]any{"enabled": true}},
	}})
	subagentsMust(t, err)
	environment := map[string]string{subagentsCodexConfigEnv: string(initial)}
	subagentsMust(t, configureCodexMailboxHook(codexRoot, environment))
	var codex map[string]any
	subagentsMust(t, json.Unmarshal([]byte(environment[subagentsCodexConfigEnv]), &codex))
	groups := subagentsGet(codex, "hooks", "PostToolUse").([]any)
	if len(groups) != 2 || !reflect.DeepEqual(groups[0], userHook) {
		t.Fatalf("groups = %v", groups)
	}
	if command := subagentsGet(groups[1], "hooks", 0, "command").(string); !strings.HasPrefix(command, codexMailboxHookMarker) {
		t.Fatalf("command = %s", command)
	}
	if states := subagentsGet(codex, "hooks", "state").(map[string]any); len(states) != 1 {
		t.Fatalf("states = %v", states)
	}
}

func TestClaudeDelegationLaunchesFromACheckoutAfterRemoteProfileRepair(t *testing.T) {
	for _, check := range []struct {
		role    core.SubagentMcpRole
		oldRoot string
	}{
		{core.SubagentMcpRoleParent, ".local/share/hel/profiles/session"},
		{core.SubagentMcpRoleParent, ".local/share/hel/workers/session"},
		{core.SubagentMcpRoleChild, "/previous-target/workers/session"},
		{core.SubagentMcpRoleMessageOnly, "/previous-target/workers/session"},
	} {
		// The fake server connects to the socket by its full path, so the
		// directory is short as Rust's tempdir is (sun_path holds 108 bytes).
		temp, err := os.MkdirTemp("", "mj-delegation")
		subagentsMust(t, err)
		t.Cleanup(func() { os.RemoveAll(temp) })
		root := filepath.Join(temp, "worker with spaces")
		home := filepath.Join(root, "profile")
		checkout := filepath.Join(temp, "checkout")
		for _, directory := range []string{root, home, checkout} {
			subagentsMust(t, os.Mkdir(directory, 0o777))
		}
		worker := filepath.Join(root, "hel")
		subagentsWrite(t, worker, `#!/usr/bin/env python3
import socket, sys
assert sys.argv[1:3] == ['worker', 'subagent-mcp']
assert sys.argv[sys.argv.index('--harness') + 1] == 'claude'
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
    connection.connect(sys.argv[sys.argv.index('--socket') + 1])
    assert connection.recv(64) == b'connected'
print(sys.argv[sys.argv.index('--role') + 1])
`)
		subagentsMust(t, os.Chmod(worker, 0o700))
		// A registration an earlier controller staged with home-relative
		// paths from another target.
		original := map[string]any{
			"userSetting": "retained",
			"mcpServers": map[string]any{
				"user-server": map[string]any{"command": "user-tool"},
				"mj-agents": map[string]any{
					"type":       "stdio",
					"alwaysLoad": true,
					"command":    filepath.Join(check.oldRoot, "hel"),
					"args": []any{"worker", "subagent-mcp", "--socket",
						filepath.Join(check.oldRoot, SubagentSocket),
						"--harness", "claude", "--role", "parent"},
				},
			},
		}
		path := filepath.Join(home, ".claude.json")
		body, err := json.Marshal(original)
		subagentsMust(t, err)
		subagentsWrite(t, path, string(body))
		configure := func() {
			_, err := configureClaudeSubagentMCP(root, "session", home, subagentsRole(check.role), true, true, false)
			subagentsMust(t, err)
		}
		configure()
		first := subagentsRead(t, path)
		configure()
		if subagentsRead(t, path) != first {
			t.Fatal("a repeat registration must leave the profile as it is")
		}
		var config map[string]any
		subagentsMust(t, json.Unmarshal([]byte(first), &config))
		if config["userSetting"] != "retained" {
			t.Fatalf("config = %v", config)
		}
		if !reflect.DeepEqual(subagentsGet(config, "mcpServers", "user-server"), subagentsGet(original, "mcpServers", "user-server")) {
			t.Fatalf("config = %v", config)
		}
		server := subagentsGet(config, "mcpServers", "mj-agents")
		if subagentsGet(server, "alwaysLoad") != true {
			t.Fatalf("server = %v", server)
		}
		socket, err := core.BindUnixListener(filepath.Join(root, SubagentSocket))
		subagentsMust(t, err)
		command := exec.Command(subagentsGet(server, "command").(string), subagentsStrings(subagentsGet(server, "args"))...)
		command.Dir = checkout
		accepted := make(chan error, 1)
		go func() {
			subagentsMust(t, socket.SetDeadline(time.Now().Add(10*time.Second)))
			connection, err := socket.Accept()
			if err != nil {
				accepted <- err
				return
			}
			defer connection.Close()
			_, err = connection.Write([]byte("connected"))
			accepted <- err
		}()
		output, err := core.RunBounded(context.Background(), command, 64*1024, 10*time.Second)
		subagentsMust(t, err)
		subagentsMust(t, <-accepted)
		socket.Close()
		if !output.Status.Success() {
			t.Fatalf("%s", output.Stderr)
		}
		if got := strings.TrimSpace(string(output.Stdout)); got != check.role.ID() {
			t.Fatalf("role = %s", got)
		}
	}
}

// subagentsClaudeAllow is the profile's permissions.allow rules.
func subagentsClaudeAllow(t *testing.T, home string) []string {
	t.Helper()
	settings := subagentsJSONFile(t, filepath.Join(home, "settings.json"))
	return subagentsStrings(subagentsGet(settings, "permissions", "allow"))
}

// Hard-won: adc0d368: an upgraded worker failed preparation on a profile staged without the message-only server.
// Hard-won: f9a3130c: a profile staged before message-only sessions kept no send_message through every upgrade.
func TestAMessageOnlySessionGetsSendMessageOnAProfileStagedWithoutIt(t *testing.T) {
	root := t.TempDir()
	home := filepath.Join(root, "profile")
	subagentsMust(t, os.Mkdir(home, 0o777))
	// What a controller from before message-only sessions staged: other
	// servers, no `mj-agents`, and no rule for it.
	path := filepath.Join(home, ".claude.json")
	subagentsWrite(t, path, `{"mcpServers":{"other":{"command":"x"}}}`)
	_, err := configureClaudeSubagentMCP(root, "session", home, subagentsRole(core.SubagentMcpRoleMessageOnly), true, true, false)
	subagentsMust(t, err)
	config := subagentsJSONFile(t, path)
	if subagentsGet(config, "mcpServers", "other", "command") != "x" {
		t.Fatalf("config = %v", config)
	}
	args := subagentsStrings(subagentsGet(config, "mcpServers", "mj-agents", "args"))
	role := slices.Index(args, "--role") + 1
	if args[role] != "message_only" {
		t.Fatalf("args = %q", args)
	}
	if allow := subagentsClaudeAllow(t, home); !slices.Equal(allow, []string{
		"mcp__mj-agents__send_message", "mcp__mj-agents__fetch", "mcp__mj-agents__ask_oracle",
	}) {
		t.Fatalf("allow = %q", allow)
	}

	// Losing the role removes the server; the person's servers stay.
	_, err = configureClaudeSubagentMCP(root, "session", home, nil, true, true, false)
	subagentsMust(t, err)
	config = subagentsJSONFile(t, path)
	if subagentsGet(config, "mcpServers", "mj-agents") != nil || subagentsGet(config, "mcpServers", "other", "command") != "x" {
		t.Fatalf("config = %v", config)
	}

	// A session with no role writes nothing it did not already have.
	bare := t.TempDir()
	bareHome := filepath.Join(bare, "profile")
	subagentsMust(t, os.Mkdir(bareHome, 0o777))
	_, err = configureClaudeSubagentMCP(bare, "session", bareHome, nil, true, true, false)
	subagentsMust(t, err)
	for _, name := range []string{".claude.json", "settings.json"} {
		if _, err := os.Stat(filepath.Join(bareHome, name)); err == nil {
			t.Fatalf("%s must not be written", name)
		}
	}
}

// TestTheClaudeProfileAllowsItsOwnSubAgentTools: launch finding R11-1: a
// Claude child on a model without Auto mode ran in Accept edits, and Claude
// asked a person before it would run the child's own `handback`, so the
// child could not report without one. The settings allow every tool the
// role's `mj-agents` server lists, whatever the mode, and keep the person's
// own settings and rules.
//
// Hard-won: 91244ea: a Claude child asked the person for permission before its own handback
func TestTheClaudeProfileAllowsItsOwnSubAgentTools(t *testing.T) {
	profile := func() string {
		root := t.TempDir()
		subagentsMust(t, os.Mkdir(filepath.Join(root, "profile"), 0o777))
		return root
	}
	configure := func(root string, role core.SubagentMcpRole, subtle bool) (map[string]any, map[string]any, []string) {
		home := filepath.Join(root, "profile")
		_, err := configureClaudeSubagentMCP(root, "session", home, &role, true, subtle, false)
		subagentsMust(t, err)
		settings := subagentsJSONFile(t, filepath.Join(home, "settings.json"))
		config := subagentsJSONFile(t, filepath.Join(home, ".claude.json"))
		return settings, config, subagentsStrings(subagentsGet(settings, "permissions", "allow"))
	}

	// Children can hand back and fetch without asking the person.
	root := profile()
	subagentsWrite(t, filepath.Join(root, "profile/settings.json"), `{"model":"opus","permissions":{"allow":["Bash(ls:*)"],"deny":["WebFetch"]}}`)
	settings, _, allow := configure(root, core.SubagentMcpRoleChild, true)
	if !slices.Equal(allow, []string{"Bash(ls:*)", "mcp__mj-agents__handback", "mcp__mj-agents__fetch", "mcp__mj-agents__send_message"}) {
		t.Fatalf("allow = %q", allow)
	}
	if !reflect.DeepEqual(subagentsGet(settings, "permissions", "deny"), []any{"WebFetch"}) || settings["model"] != "opus" {
		t.Fatalf("settings = %v", settings)
	}

	// A parent delegates without asking; a rule the person already has is
	// kept once, in its place.
	root = profile()
	subagentsWrite(t, filepath.Join(root, "profile/settings.json"), `{"permissions":{"allow":["mcp__mj-agents__wait"]}}`)
	_, config, allow := configure(root, core.SubagentMcpRoleParent, true)
	if slices.Contains(subagentsStrings(subagentsGet(config, "mcpServers", "mj-agents", "args")), "--subtle-subagents") {
		t.Fatal("the default stays off the command line")
	}
	if allow[0] != "mcp__mj-agents__wait" {
		t.Fatalf("allow = %q", allow)
	}
	count := 0
	for _, rule := range allow {
		if rule == "mcp__mj-agents__wait" {
			count++
		}
	}
	if count != 1 {
		t.Fatalf("allow = %q", allow)
	}
	for _, tool := range []string{"spawn", "list_agents", "send_message", "close", "fetch"} {
		if !slices.Contains(allow, "mcp__mj-agents__"+tool) {
			t.Fatalf("%s: %q", tool, allow)
		}
	}
	if slices.ContainsFunc(allow, func(rule string) bool { return strings.HasSuffix(rule, "__handback") }) {
		t.Fatalf("a parent has no handback: %q", allow)
	}

	// With only the straightforward type configured, the parent's server is
	// told so and offers no task type.
	root = profile()
	_, config, _ = configure(root, core.SubagentMcpRoleParent, false)
	args := subagentsStrings(subagentsGet(config, "mcpServers", "mj-agents", "args"))
	if !slices.Equal(args[len(args)-2:], []string{"--subtle-subagents", "false"}) {
		t.Fatalf("args = %q", args)
	}
}

// Hard-won: 5fc951f2e: skipped shared-home registration must report tools unavailable.
func TestTheClaudeRegistrationNeverEditsAProfileThatMayBeThePersonsOwn(t *testing.T) {
	root := t.TempDir()
	own := t.TempDir()
	subagentsWrite(t, filepath.Join(own, ".claude.json"), "{}")
	// An earlier local session linked its profile to the person's home.
	subagentsMust(t, os.Symlink(own, filepath.Join(root, "profile")))
	home := filepath.Join(root, "profile")
	for _, role := range []core.SubagentMcpRole{core.SubagentMcpRoleParent, core.SubagentMcpRoleChild} {
		if _, err := configureClaudeSubagentMCP(root, "session", home, &role, true, true, false); err == nil {
			t.Fatalf("%s cannot start without its tools", role)
		}
	}
	registered, err := configureClaudeSubagentMCP(root, "session", home, subagentsRole(core.SubagentMcpRoleMessageOnly), true, true, false)
	subagentsMust(t, err)
	if registered {
		t.Fatal("a message-only session starts without its server there")
	}
	if got := subagentsRead(t, filepath.Join(own, ".claude.json")); got != "{}" {
		t.Fatalf("own = %s", got)
	}
	if _, err := os.Stat(filepath.Join(own, "settings.json")); err == nil {
		t.Fatal("the person's settings must not be written")
	}

	// Invalid JSON in a private profile is reported, not replaced.
	root = t.TempDir()
	home = filepath.Join(root, "profile")
	subagentsMust(t, os.Mkdir(home, 0o777))
	for _, broken := range []string{"{", "[]", `{"mcpServers":[]}`} {
		subagentsWrite(t, filepath.Join(home, ".claude.json"), broken)
		if _, err := configureClaudeSubagentMCP(root, "session", home, subagentsRole(core.SubagentMcpRoleMessageOnly), true, true, false); err == nil {
			t.Fatalf("%s must be refused", broken)
		}
		if got := subagentsRead(t, filepath.Join(home, ".claude.json")); got != broken {
			t.Fatalf("broken = %s", got)
		}
	}
}

// subagentsTOMLFile decodes a TOML file as generic values.
func subagentsTOMLFile(t *testing.T, path string) map[string]any {
	t.Helper()
	table, err := core.TOMLTableFromStr(subagentsRead(t, path))
	subagentsMust(t, err)
	return table
}

// Hard-won: #1287: direct exposure still allowed blocking waits inside code-mode exec cells.
func TestCodexDelegationProfilePreservesOtherServersAndHandlesUpgradesAndDisable(t *testing.T) {
	for _, role := range []core.SubagentMcpRole{
		core.SubagentMcpRoleParent,
		core.SubagentMcpRoleParent,
		core.SubagentMcpRoleMessageOnly,
		core.SubagentMcpRoleChild,
	} {
		for _, policy := range []core.ExecutionPolicy{core.ExecutionPolicyConfiguredApprovals, core.ExecutionPolicyUnconstrained} {
			root := t.TempDir()
			home := filepath.Join(root, "profile")
			subagentsMust(t, os.Mkdir(home, 0o777))
			path := filepath.Join(home, "config.toml")
			rules := filepath.Join(home, "rules")
			subagentsMust(t, os.Mkdir(rules, 0o777))
			userRule := "prefix_rule(pattern=[\"git\"], decision=\"prompt\")\n"
			subagentsWrite(t, filepath.Join(rules, "user.rules"), userRule)
			originalText := `
model = "parent-model"
[features]
hooks = true
apps = true
[features.code_mode]
enabled = false
excluded_tool_namespaces = ["mcp__user_hidden"]
direct_only_tool_namespaces = ["mcp__user_direct"]
[mcp_servers.user-server]
command = "user-tool"
omit_tools_from = ["direct"]
[mcp_servers.mj-agents]
url = "https://obsolete.invalid"
enabled = false
`
			subagentsWrite(t, path, originalText)
			worker := subagentsExecutable(t)
			socket := filepath.Join(root, SubagentSocket)
			staged, err := configureCodexMCP(root, home, &role, policy)
			subagentsMust(t, err)
			if !staged {
				t.Fatal("the private profile must be configured")
			}
			first := subagentsRead(t, path)
			staged, err = configureCodexMCP(root, home, &role, policy)
			subagentsMust(t, err)
			if !staged || subagentsRead(t, path) != first {
				t.Fatal("a repeat configuration must leave the profile as it is")
			}
			if got := subagentsRead(t, filepath.Join(rules, "user.rules")); got != userRule {
				t.Fatalf("user rule = %s", got)
			}
			config, err := core.TOMLTableFromStr(first)
			subagentsMust(t, err)
			original, err := core.TOMLTableFromStr(originalText)
			subagentsMust(t, err)
			if config["model"] != original["model"] || subagentsGet(config, "features", "hooks") != subagentsGet(original, "features", "hooks") {
				t.Fatalf("config = %v", config)
			}
			// A top-level session keeps the user's features.
			if subagentsGet(config, "features", "apps") != true || subagentsGet(config, "features", "plugins") != nil {
				t.Fatalf("features = %v", config["features"])
			}
			codeMode := subagentsGet(config, "features", "code_mode")
			if subagentsGet(codeMode, "enabled") != false {
				t.Fatalf("code mode = %v", codeMode)
			}
			if !reflect.DeepEqual(subagentsGet(codeMode, "excluded_tool_namespaces"), subagentsGet(original, "features", "code_mode", "excluded_tool_namespaces")) {
				t.Fatalf("code mode = %v", codeMode)
			}
			if got := subagentsStrings(subagentsGet(codeMode, "direct_only_tool_namespaces")); !slices.Equal(got, []string{"mcp__user_direct", "mcp__mj_agents"}) {
				t.Fatalf("namespaces = %q", got)
			}
			if !reflect.DeepEqual(subagentsGet(config, "mcp_servers", "user-server"), subagentsGet(original, "mcp_servers", "user-server")) {
				t.Fatalf("servers = %v", config["mcp_servers"])
			}
			server := subagentsGet(config, "mcp_servers", "mj-agents")
			if subagentsGet(server, "command") != worker {
				t.Fatalf("server = %v", server)
			}
			if got := subagentsStrings(subagentsGet(server, "omit_tools_from")); !slices.Equal(got, []string{"deferred"}) {
				t.Fatalf("server = %v", server)
			}
			if subagentsGet(server, "url") != nil || subagentsGet(server, "enabled") != nil {
				t.Fatalf("server = %v", server)
			}
			expectedArgs := []string{"worker", "subagent-mcp", "--socket", socket, "--harness", "codex", "--role", role.ID(), "--agent-mailboxes-enabled", "true"}
			if got := subagentsStrings(subagentsGet(server, "args")); !slices.Equal(got, expectedArgs) {
				t.Fatalf("args = %q", got)
			}
			approval, hasApproval := subagentsGet(server, "default_tools_approval_mode").(string)
			if hasApproval != (policy == core.ExecutionPolicyConfiguredApprovals) || (hasApproval && approval != "approve") {
				t.Fatalf("server = %v", server)
			}
			if role == core.SubagentMcpRoleChild {
				child := func() {
					staged, err := configureCodexMCPWithMailboxes(root, "", home, &role, policy, true, true, false, true)
					subagentsMust(t, err)
					if !staged {
						t.Fatal("the private profile must be configured")
					}
				}
				child()
				stagedText := subagentsRead(t, path)
				child()
				if subagentsRead(t, path) != stagedText {
					t.Fatal("a repeat child configuration must leave the profile as it is")
				}
				config, err := core.TOMLTableFromStr(stagedText)
				subagentsMust(t, err)
				// The child overrides even the user's explicit setting.
				for _, feature := range codexSubagentDisabledFeatures {
					if subagentsGet(config, "features", feature) != false {
						t.Fatalf("%s = %v", feature, subagentsGet(config, "features", feature))
					}
				}
				if subagentsGet(config, "features", "hooks") != true {
					t.Fatalf("features = %v", config["features"])
				}
				if !reflect.DeepEqual(subagentsGet(config, "mcp_servers", "user-server"), subagentsGet(original, "mcp_servers", "user-server")) {
					t.Fatalf("servers = %v", config["mcp_servers"])
				}
			}
			staged, err = configureCodexMCP(root, home, nil, policy)
			subagentsMust(t, err)
			if !staged {
				t.Fatal("the private profile must be configured")
			}
			disabled := subagentsTOMLFile(t, path)
			if subagentsGet(disabled, "mcp_servers", "mj-agents") != nil {
				t.Fatalf("servers = %v", disabled["mcp_servers"])
			}
			if got := subagentsRead(t, filepath.Join(rules, "user.rules")); got != userRule {
				t.Fatalf("user rule = %s", got)
			}
			if !reflect.DeepEqual(subagentsGet(disabled, "mcp_servers", "user-server"), subagentsGet(original, "mcp_servers", "user-server")) {
				t.Fatalf("servers = %v", disabled["mcp_servers"])
			}
		}
	}
}

func TestCodexDelegationProfileHandlesMissingAndInvalidConfiguration(t *testing.T) {
	root := t.TempDir()
	home := filepath.Join(root, "profile")
	subagentsMust(t, os.Mkdir(home, 0o777))
	path := filepath.Join(home, "config.toml")
	configure := func(role *core.SubagentMcpRole) error {
		_, err := configureCodexMCP(root, home, role, core.ExecutionPolicyUnconstrained)
		return err
	}
	subagentsMust(t, configure(nil))
	if _, err := os.Stat(path); err == nil {
		t.Fatal("no role writes no configuration")
	}
	subagentsMust(t, configure(subagentsRole(core.SubagentMcpRoleChild)))
	config := subagentsTOMLFile(t, path)
	if subagentsGet(config, "mcp_servers", "mj-agents", "omit_tools_from", 0) != "deferred" {
		t.Fatalf("config = %v", config)
	}
	if subagentsGet(config, "features", "code_mode", "direct_only_tool_namespaces", 0) != "mcp__mj_agents" {
		t.Fatalf("config = %v", config)
	}
	for _, enabled := range []bool{false, true} {
		subagentsWrite(t, path, fmt.Sprintf("[features]\ncode_mode = %v\n", enabled))
		subagentsMust(t, configure(subagentsRole(core.SubagentMcpRoleParent)))
		config := subagentsTOMLFile(t, path)
		if subagentsGet(config, "features", "code_mode", "enabled") != enabled {
			t.Fatalf("config = %v", config)
		}
		if subagentsGet(config, "features", "code_mode", "direct_only_tool_namespaces", 0) != "mcp__mj_agents" {
			t.Fatalf("config = %v", config)
		}
	}
	for _, broken := range []string{
		"[",
		"mcp_servers = 3",
		"features = 3",
		"[features]\ncode_mode = 3",
		"[features.code_mode]\ndirect_only_tool_namespaces = 3",
	} {
		subagentsWrite(t, path, broken)
		if err := configure(subagentsRole(core.SubagentMcpRoleParent)); err == nil {
			t.Fatalf("%q must be refused", broken)
		}
		if got := subagentsRead(t, path); got != broken {
			t.Fatalf("broken = %q", got)
		}
	}
	remote := filepath.Join(root, "profiles/session")
	subagentsMust(t, os.MkdirAll(remote, 0o777))
	for _, policy := range []core.ExecutionPolicy{core.ExecutionPolicyConfiguredApprovals, core.ExecutionPolicyUnconstrained} {
		for _, role := range []*core.SubagentMcpRole{
			nil,
			subagentsRole(core.SubagentMcpRoleParent),
			subagentsRole(core.SubagentMcpRoleMessageOnly),
			subagentsRole(core.SubagentMcpRoleChild),
		} {
			staged, err := configureCodexMCPWithMailboxes(root, "session", remote, role, policy, true, true, false, false)
			subagentsMust(t, err)
			if !staged {
				t.Fatal("the session's remote profile is private")
			}
		}
	}
	staged, err := configureCodexMCPWithMailboxes(root, "different-session", remote, nil, core.ExecutionPolicyConfiguredApprovals, true, true, false, false)
	subagentsMust(t, err)
	if staged {
		t.Fatal("another session's profile is not this session's")
	}
}

// Hard-won: a602a316a5: a linked profiles parent let private-profile writes reach a source profile.
func TestLegacyCodexHomesStayOnAcpWithoutWritingTheUsersProfile(t *testing.T) {
	root := t.TempDir()
	source := t.TempDir()
	path := filepath.Join(source, "config.toml")
	original := "model = \"original\"\n"
	subagentsWrite(t, path, original)
	configure := func(home string, role *core.SubagentMcpRole) bool {
		staged, err := configureCodexMCP(root, home, role, core.ExecutionPolicyConfiguredApprovals)
		subagentsMust(t, err)
		return staged
	}
	sourceUntouched := func() {
		t.Helper()
		if got := subagentsRead(t, path); got != original {
			t.Fatalf("source = %s", got)
		}
		if _, err := os.Stat(filepath.Join(source, "rules")); err == nil {
			t.Fatal("no rules may be written to the source")
		}
	}
	if configure(source, subagentsRole(core.SubagentMcpRoleParent)) {
		t.Fatal("the person's home is not a staged profile")
	}
	staged := filepath.Join(root, "profile")
	subagentsMust(t, os.Symlink(source, staged))
	for _, home := range []string{source, staged} {
		for _, role := range []*core.SubagentMcpRole{subagentsRole(core.SubagentMcpRoleParent), subagentsRole(core.SubagentMcpRoleChild), nil} {
			if configure(home, role) {
				t.Fatalf("%s must not be configured", home)
			}
			sourceUntouched()
		}
	}
	subagentsMust(t, os.Remove(staged))
	subagentsMust(t, os.Mkdir(staged, 0o777))
	if configure(source, subagentsRole(core.SubagentMcpRoleParent)) {
		t.Fatal("the person's home is not a staged profile")
	}
	if got := subagentsRead(t, path); got != original {
		t.Fatalf("source = %s", got)
	}
	if !configure(staged, subagentsRole(core.SubagentMcpRoleParent)) {
		t.Fatal("the real staged profile is configured")
	}
	if info, err := os.Stat(filepath.Join(staged, "config.toml")); err != nil || !info.Mode().IsRegular() {
		t.Fatal("the staged configuration must be written")
	}
	sourceUntouched()

	sourceSession := filepath.Join(source, "session")
	subagentsMust(t, os.Mkdir(sourceSession, 0o777))
	sourceConfig := filepath.Join(sourceSession, "config.toml")
	subagentsWrite(t, sourceConfig, original)
	profiles := filepath.Join(root, "profiles")
	subagentsMust(t, os.Symlink(source, profiles))
	remoteHome := filepath.Join(profiles, "session")
	for _, role := range []*core.SubagentMcpRole{nil, subagentsRole(core.SubagentMcpRoleParent), subagentsRole(core.SubagentMcpRoleChild)} {
		if privateStagedHome(root, "session", remoteHome) {
			t.Fatal("a linked profiles directory is not private")
		}
		staged, err := configureCodexMCPWithMailboxes(root, "session", remoteHome, role, core.ExecutionPolicyConfiguredApprovals, true, true, false, false)
		subagentsMust(t, err)
		if staged {
			t.Fatal("a linked profiles directory must not be configured")
		}
		if got := subagentsRead(t, sourceConfig); got != original {
			t.Fatalf("source = %s", got)
		}
		if _, err := os.Stat(filepath.Join(sourceSession, "rules")); err == nil {
			t.Fatal("no rules may be written to the source")
		}
	}

	// A link above the real profiles directory still names a private home.
	subagentsMust(t, os.Remove(profiles))
	subagentsMust(t, os.MkdirAll(remoteHome, 0o777))
	ancestor := filepath.Join(source, "worker-alias")
	subagentsMust(t, os.Symlink(root, ancestor))
	if !privateStagedHome(root, "session", filepath.Join(ancestor, "profiles/session")) {
		t.Fatal("a linked ancestor still names a private home")
	}
}

func TestClosedMutationAdmissionRejectsChangesButKeepsObservationsOpen(t *testing.T) {
	root := t.TempDir()
	endpoint, err := SubagentEndpointOpen(root)
	subagentsMust(t, err)
	if !endpoint.state.state.mutatingAdmissionOpen {
		t.Fatal("admission starts open")
	}
	subagentsMust(t, endpoint.SetMutatingAdmission(false))
	reopened, err := SubagentEndpointOpen(root)
	subagentsMust(t, err)
	if reopened.state.state.mutatingAdmissionOpen {
		t.Fatal("a closed gate survives reopening")
	}

	mutations := []core.SubagentToolAction{
		core.SubagentToolActionSpawn{
			NoGithubCredentials: false,
			TaskType:            core.SubagentTaskTypeStraightforward,
			TaskName:            "task",
			Instructions:        "work",
			WorkingDirectory:    "",
		},
		core.SubagentToolActionSendInput{ChildSessionID: "child", Message: "continue"},
		core.SubagentToolActionSendMessage{ChildSessionID: "child", Message: "note"},
		core.SubagentToolActionLegacyInterruptAgent{ChildSessionID: "child"},
		core.SubagentToolActionCloseAgent{ChildSessionID: "child"},
	}
	for index, action := range mutations {
		request := subagentsTestRequest(fmt.Sprintf("mutation-%d", index))
		request.Action = action
		accepted, result, _, err := reopened.enqueueMarked(request)
		subagentsMust(t, err)
		if accepted || result == nil || !result.IsError {
			t.Fatalf("%T must be refused: %v %v", action, accepted, result)
		}
	}
	for _, pair := range []struct {
		id     string
		action core.SubagentToolAction
	}{
		{"list", core.SubagentToolActionListAgents{}},
		{"wait", core.SubagentToolActionLegacyWaitAgents{}},
	} {
		request := subagentsTestRequest(pair.id)
		request.Action = pair.action
		accepted, result, _, err := reopened.enqueueMarked(request)
		subagentsMust(t, err)
		if !accepted || result != nil {
			t.Fatalf("%s must be accepted", pair.id)
		}
	}
	if queued, _ := reopened.snapshot(); len(queued) != 2 {
		t.Fatalf("queued = %v", queued)
	}
	subagentsMust(t, reopened.SetMutatingAdmission(true))
	request := subagentsTestRequest("reopened")
	request.Action = core.SubagentToolActionSendMessage{ChildSessionID: "child", Message: "note"}
	accepted, _, _, err := reopened.enqueueMarked(request)
	subagentsMust(t, err)
	if !accepted {
		t.Fatal("a reopened gate accepts mutations")
	}
}

func TestAcceptedMutationRetryRemainsAttachedAfterAdmissionCloses(t *testing.T) {
	root := t.TempDir()
	endpoint, err := SubagentEndpointOpen(root)
	subagentsMust(t, err)
	accepted := subagentsTestRequest("accepted-before-close")
	accepted.Action = core.SubagentToolActionCloseAgent{ChildSessionID: "child"}
	admitted, _, _, err := endpoint.enqueueMarked(accepted)
	subagentsMust(t, err)
	if !admitted {
		t.Fatal("an open gate accepts the mutation")
	}
	subagentsMust(t, endpoint.SetMutatingAdmission(false))

	admitted, result, _, err := endpoint.enqueueMarked(accepted)
	subagentsMust(t, err)
	if !admitted || result != nil {
		t.Fatal("the retry stays attached to the queued operation")
	}
	queued, _ := endpoint.snapshot()
	if len(queued) != 1 || queued[0].RequestID != "accepted-before-close" {
		t.Fatalf("queued = %v", queued)
	}

	newRequest := subagentsTestRequest("new-after-close")
	newRequest.Action = core.SubagentToolActionCloseAgent{ChildSessionID: "child"}
	admitted, result, _, err = endpoint.enqueueMarked(newRequest)
	subagentsMust(t, err)
	if admitted || result == nil || !result.IsError {
		t.Fatal("a new mutation is refused while the gate is closed")
	}
}

func TestAdmissionStampsWorkerTurnAndReplayCannotRetargetIt(t *testing.T) {
	root := t.TempDir()
	durable, err := DurableRelayOpen(root, "session-123", "test")
	subagentsMust(t, err)
	relay := NewRelayCell(durable)
	func() {
		owner := relay.Lock()
		defer owner.Unlock()
		_, err := owner.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: nil})
		subagentsMust(t, err)
		submitRelay(owner.DurableRelay, "original-turn", prompt("first"))
		_, err = owner.ClaimPendingCommands(true)
		subagentsMust(t, err)
	}()
	endpoint, err := SubagentEndpointOpen(root)
	subagentsMust(t, err)
	endpoint.relay = relay
	report := subagentsTestRequest("handback-request")
	report.Action = core.SubagentToolActionHandback{Message: "original report"}
	forged := "untrusted-forged-turn"
	report.OriginatingCommandID = &forged
	_, err = endpoint.enqueue(report)
	subagentsMust(t, err)
	func() {
		owner := relay.Lock()
		defer owner.Unlock()
		_, err := owner.RecordCommandCompleted("original-turn", core.RelayCommandOutcomePrompt{StopReason: "end_turn"})
		subagentsMust(t, err)
		submitRelay(owner.DurableRelay, "replacement-turn", prompt("second"))
		_, err = owner.ClaimPendingCommands(true)
		subagentsMust(t, err)
	}()
	_, err = endpoint.enqueue(report)
	subagentsMust(t, err)
	reopened, err := SubagentEndpointOpen(root)
	subagentsMust(t, err)
	pending, _ := reopened.snapshot()
	if len(pending) != 1 || pending[0].OriginatingCommandID == nil || *pending[0].OriginatingCommandID != "original-turn" {
		t.Fatalf("pending = %v", pending)
	}
}

// Hard-won: #1450: a handback from a Monitor-woken turn was refused for lack of a turn id.
func TestHandbackAdmissionOwnsTheHarnessTurnInEitherOrder(t *testing.T) {
	for _, opened := range []bool{false, true} {
		root := t.TempDir()
		durable, err := DurableRelayOpen(root, "session-123", "test")
		subagentsMust(t, err)
		relay := NewRelayCell(durable)
		func() {
			owner := relay.Lock()
			defer owner.Unlock()
			owner.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
			_, err := owner.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: nil})
			subagentsMust(t, err)
			if opened {
				subagentsMust(t, owner.openHarnessTurnIfAllowed())
			}
		}()
		endpoint, err := SubagentEndpointOpen(root)
		subagentsMust(t, err)
		endpoint.relay = relay
		report := subagentsTestRequest("harness-handback")
		report.Action = core.SubagentToolActionHandback{Message: "report"}
		_, err = endpoint.enqueue(report)
		subagentsMust(t, err)
		owner := relay.Lock()
		expected, ok := owner.originatingCommandID()
		owner.Unlock()
		if !ok || !strings.HasPrefix(expected, "harness-turn-") {
			t.Fatalf("origin = %q", expected)
		}
		queued, _ := endpoint.snapshot()
		if queued[0].OriginatingCommandID == nil || *queued[0].OriginatingCommandID != expected {
			t.Fatalf("queued = %v", queued)
		}
	}
}

func TestFailedPersistenceNeverPublishesAdmissionOrCompletion(t *testing.T) {
	root := t.TempDir()
	endpoint, err := SubagentEndpointOpen(root)
	subagentsMust(t, err)
	valid := endpoint.path
	endpoint.path = root // Atomic rename cannot replace a directory.
	if _, err := endpoint.enqueue(subagentsTestRequest("failed-admission")); err == nil {
		t.Fatal("the failed write must be reported")
	}
	if queued, _ := endpoint.snapshot(); len(queued) != 0 {
		t.Fatalf("queued = %v", queued)
	}
	endpoint.path = valid
	_, err = endpoint.enqueue(subagentsTestRequest("accepted-request"))
	subagentsMust(t, err)
	endpoint.path = root
	if _, err := endpoint.Complete(subagentsTestDone("accepted-request")); err == nil {
		t.Fatal("the failed write must be reported")
	}
	pending, results := endpoint.snapshot()
	if len(pending) != 1 || len(results) != 0 {
		t.Fatalf("pending = %v results = %v", pending, results)
	}
}

func TestLegacyQueueDeliveryBitsDecodeWithoutAffectingCompletions(t *testing.T) {
	var queue queueState
	subagentsMust(t, json.Unmarshal([]byte(`{
		"delivered_to_waiter": {"old-wait": false},
		"requests": {"old-wait": {"request_id":"old-wait", "created_at_ms":1, "action":{"action":"wait_agents"}}}
	}`), &queue))
	if _, ok := queue.requests["old-wait"].Action.(core.SubagentToolActionLegacyWaitAgents); !ok {
		t.Fatalf("action = %#v", queue.requests["old-wait"].Action)
	}
	encoded, err := json.Marshal(queue)
	subagentsMust(t, err)
	var value map[string]any
	subagentsMust(t, json.Unmarshal(encoded, &value))
	if _, ok := value["delivered_to_waiter"]; ok {
		t.Fatalf("value = %s", encoded)
	}
}

func TestQueueSurvivesReopenAndReturnsCachedCompletionIdempotently(t *testing.T) {
	directory := t.TempDir()
	endpoint, err := SubagentEndpointOpen(directory)
	subagentsMust(t, err)
	result, err := endpoint.enqueue(subagentsTestRequest("request-1"))
	subagentsMust(t, err)
	if result != nil {
		t.Fatalf("result = %v", result)
	}
	if queued, _ := endpoint.snapshot(); !reflect.DeepEqual(queued, []core.SubagentToolRequest{subagentsTestRequest("request-1")}) {
		t.Fatalf("queued = %v", queued)
	}

	completed := core.SubagentToolResult{RequestID: "request-1", CompletedAtMs: 2, IsError: false, Message: "done"}
	_, err = endpoint.Complete(completed)
	subagentsMust(t, err)

	reopened, err := SubagentEndpointOpen(directory)
	subagentsMust(t, err)
	if queued, _ := reopened.snapshot(); len(queued) != 0 {
		t.Fatalf("queued = %v", queued)
	}
	cached, err := reopened.enqueue(subagentsTestRequest("request-1"))
	subagentsMust(t, err)
	if cached == nil || *cached != completed {
		t.Fatalf("cached = %v", cached)
	}

	subagentsWrite(t, filepath.Join(directory, subagentQueue), `{"requests":{
		"legacy-message":{"originating_command_id":null,"request_id":"legacy-message","created_at_ms":1,
			"action":{"action":"send_message","params":{"child_session_id":"child","message":"continue"}}},
		"cached-input":{"originating_command_id":null,"request_id":"cached-input","created_at_ms":2,
			"action":{"action":"send_input","params":{"child_session_id":"child","message":"continue through the cached alias"}}}
	}}`)
	legacyQueue, err := SubagentEndpointOpen(directory)
	subagentsMust(t, err)
	requests, _ := legacyQueue.snapshot()
	find := func(id string) core.SubagentToolAction {
		for _, request := range requests {
			if request.RequestID == id {
				return request.Action
			}
		}
		t.Fatalf("%s is missing", id)
		return nil
	}
	if action := find("legacy-message"); action != (core.SubagentToolActionSendMessage{ChildSessionID: "child", Message: "continue"}) {
		t.Fatalf("action = %#v", action)
	}
	if action := find("cached-input"); action != (core.SubagentToolActionSendInput{ChildSessionID: "child", Message: "continue through the cached alias"}) {
		t.Fatalf("action = %#v", action)
	}
}

func TestSendInputAcknowledgesStorageWithoutCompletingTheRequest(t *testing.T) {
	directory := t.TempDir()
	endpoint, err := SubagentEndpointOpen(directory)
	subagentsMust(t, err)
	input := subagentsTestRequest("input-1")
	input.Action = core.SubagentToolActionSendInput{ChildSessionID: "starting-child", Message: strings.Repeat("x", 128*1024)}
	client, server := subagentsSocketPair(t)
	defer client.Close()
	served := subagentsServeOnce(server, endpoint.clone())
	body, err := json.Marshal(input)
	subagentsMust(t, err)
	go func() { _, _ = client.Write(append(body, '\n')) }()
	subagentsMust(t, client.SetReadDeadline(time.Now().Add(2*time.Second)))
	line, err := bufio.NewReader(client).ReadString('\n')
	subagentsMust(t, err)
	subagentsMust(t, <-served)
	var reply map[string]any
	subagentsMust(t, json.Unmarshal([]byte(line), &reply))
	if reply["acknowledgment_required"] != false {
		t.Fatalf("reply = %v", reply)
	}
	var payload map[string]any
	subagentsMust(t, json.Unmarshal([]byte(subagentsGet(reply, "result", "message").(string)), &payload))
	if payload["status"] != "queued" || payload["child_session_id"] != "starting-child" {
		t.Fatalf("payload = %v", payload)
	}
	if _, ok := payload["turn_id"]; ok {
		t.Fatalf("payload = %v", payload)
	}
	reopened, err := SubagentEndpointOpen(directory)
	subagentsMust(t, err)
	queued, results := reopened.snapshot()
	if !reflect.DeepEqual(queued, []core.SubagentToolRequest{input}) || len(results) != 0 {
		t.Fatalf("snapshot = %v %v", queued, results)
	}
	cached, err := reopened.enqueue(input)
	subagentsMust(t, err)
	if cached != nil {
		t.Fatalf("cached = %v", cached)
	}
	completed := subagentsTestDone("input-1")
	_, err = reopened.Complete(completed)
	subagentsMust(t, err)
	final, err := SubagentEndpointOpen(directory)
	subagentsMust(t, err)
	cached, err = final.enqueue(input)
	subagentsMust(t, err)
	if cached == nil || *cached != completed {
		t.Fatalf("cached = %v", cached)
	}
}

// Hard-won: 131223a: sub-agent MCP calls returned accepted placeholders instead of their daemon result.
func TestASocketWaiterIsRegisteredBeforeTheRequestBecomesVisible(t *testing.T) {
	directory := t.TempDir()
	endpoint, err := SubagentEndpointOpen(directory)
	subagentsMust(t, err)
	client, server := subagentsSocketPair(t)
	defer client.Close()
	served := subagentsServeOnce(server, endpoint.clone())
	body, err := json.Marshal(subagentsTestRequest("r1"))
	subagentsMust(t, err)
	_, err = client.Write(append(body, '\n'))
	subagentsMust(t, err)
	deadline := time.Now().Add(2 * time.Second)
	for {
		queued, _ := endpoint.snapshot()
		if slices.ContainsFunc(queued, func(request core.SubagentToolRequest) bool { return request.RequestID == "r1" }) {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("the request never became visible")
		}
		time.Sleep(time.Millisecond)
	}
	endpoint.state.mu.Lock()
	waiters := endpoint.state.state.liveWaiters["r1"]
	endpoint.state.mu.Unlock()
	if waiters != 1 {
		t.Fatalf("a daemon may complete immediately once it sees the queued request: %d waiters", waiters)
	}
	delivered, err := endpoint.Complete(subagentsTestDone("r1"))
	subagentsMust(t, err)
	if !delivered {
		t.Fatal("the live waiter receives the result")
	}
	subagentsMust(t, client.SetReadDeadline(time.Now().Add(2*time.Second)))
	line, err := bufio.NewReader(client).ReadString('\n')
	subagentsMust(t, err)
	subagentsMust(t, <-served)
	var reply socketReply
	subagentsMust(t, json.Unmarshal([]byte(line), &reply))
	if reply.Result == nil || *reply.Result != subagentsTestDone("r1") {
		t.Fatalf("reply = %v", reply)
	}
}

// TestAnUnansweredRequestIsAnsweredHereWithWhetherTheDaemonPickedItUp: #1197:
// the MCP server gives up after replyTimeout. This worker answers first, and
// says whether the daemon has read the queue since the request joined it; a
// read before the request does not count. Rust runs this on tokio's paused
// clock; here the reply budget is shortened instead.
//
// Hard-won: dfd2e32: a stalled daemon launched a queued child after the parent had moved on.
func TestAnUnansweredRequestIsAnsweredHereWithWhetherTheDaemonPickedItUp(t *testing.T) {
	budget := subagentsReplyBudget
	subagentsReplyBudget = 300 * time.Millisecond
	defer func() { subagentsReplyBudget = budget }()
	for _, daemonReadsIt := range []bool{false, true} {
		directory := t.TempDir()
		endpoint, err := SubagentEndpointOpen(directory)
		subagentsMust(t, err)
		endpoint.recordCollection()
		listener, err := core.BindUnixListener(filepath.Join(directory, "test.sock"))
		subagentsMust(t, err)
		served := make(chan error, 1)
		go func() {
			stream, err := listener.Accept()
			if err != nil {
				served <- err
				return
			}
			served <- subagentsServeOne(context.Background(), stream, endpoint.clone())
		}()
		client, err := core.ConnectUnixStream(filepath.Join(directory, "test.sock"))
		subagentsMust(t, err)
		body, err := json.Marshal(subagentsTestRequest("r-list"))
		subagentsMust(t, err)
		_, err = client.Write(append(body, '\n'))
		subagentsMust(t, err)
		started := time.Now()
		for {
			if queued, _ := endpoint.snapshot(); len(queued) != 0 {
				break
			}
			time.Sleep(time.Millisecond)
		}
		if daemonReadsIt {
			endpoint.recordCollection()
		}
		line, err := bufio.NewReader(client).ReadString('\n')
		subagentsMust(t, err)
		subagentsMust(t, <-served)
		client.Close()
		listener.Close()
		if elapsed := time.Since(started); elapsed >= replyTimeout {
			t.Fatalf("the worker must answer before the MCP server gives up, took %v", elapsed)
		}
		var reply map[string]any
		subagentsMust(t, json.Unmarshal([]byte(strings.TrimSpace(line)), &reply))
		if reply["accepted"] != true {
			t.Fatalf("reply = %v", reply)
		}
		if _, ok := reply["result"]; ok {
			t.Fatalf("reply = %v", reply)
		}
		daemon, err := json.Marshal(reply["daemon"])
		subagentsMust(t, err)
		var contact DaemonContact
		subagentsMust(t, json.Unmarshal(daemon, &contact))
		if contact.PickedUp != daemonReadsIt || contact.LastCollectedSecondsAgo == nil {
			t.Fatalf("reply = %v", reply)
		}
	}
}

var subagentsReviewRules = [3]string{
	"mcp__mj-review__request_review",
	"mcp__mj-review__override_review",
	"mcp__mj-review__set_baseline",
}

func reviewGateStagedProfile(t *testing.T) (string, string) {
	t.Helper()
	root := t.TempDir()
	home := filepath.Join(root, "profile")
	subagentsMust(t, os.Mkdir(home, 0o777))
	return root, home
}

func reviewGateClaudeFiles(t *testing.T, home string) (map[string]any, []string) {
	t.Helper()
	config := subagentsJSONFile(t, filepath.Join(home, ".claude.json"))
	return config, subagentsClaudeAllow(t, home)
}

func reviewGateCodexServers(t *testing.T, home string) map[string]any {
	t.Helper()
	config := subagentsTOMLFile(t, filepath.Join(home, "config.toml"))
	return config["mcp_servers"].(map[string]any)
}

// Hard-won: 32d96d199: review registration required granting the delegation role.
func TestTheReviewServerIsRegisteredForEveryRoleWhenTheLaunchOffersReviewTools(t *testing.T) {
	for _, role := range []core.SubagentMcpRole{
		core.SubagentMcpRoleParent,
		core.SubagentMcpRoleChild,
		core.SubagentMcpRoleMessageOnly,
		core.SubagentMcpRoleFetchOnly,
	} {
		for _, tools := range []bool{false, true} {
			expected := tools
			root, home := reviewGateStagedProfile(t)
			_, err := configureClaudeSubagentMCP(root, "session", home, &role, true, true, tools)
			subagentsMust(t, err)
			config, allow := reviewGateClaudeFiles(t, home)
			servers := config["mcpServers"].(map[string]any)
			// Delegation tools stay always loaded and carry no review state.
			if subagentsGet(servers, "mj-agents", "alwaysLoad") != true {
				t.Fatalf("servers = %v", servers)
			}
			agents := subagentsStrings(subagentsGet(servers, "mj-agents", "args"))
			if slices.Contains(agents, "--review-gate") || slices.Contains(agents, "--review-server") {
				t.Fatalf("agents = %q", agents)
			}
			if slices.ContainsFunc(allow, func(rule string) bool {
				return strings.HasSuffix(rule, "__request_review") && strings.HasPrefix(rule, "mcp__mj-agents__")
			}) {
				t.Fatalf("allow = %q", allow)
			}
			if _, ok := servers["mj-review"]; ok != expected {
				t.Fatalf("%s %v", role, tools)
			}
			for _, rule := range subagentsReviewRules {
				if slices.Contains(allow, rule) != expected {
					t.Fatalf("%s %v: %q", role, tools, allow)
				}
			}
			socketPath := filepath.Join(root, SubagentSocket)
			reviewArgs := func(harness string) []string {
				args := []string{"worker", "subagent-mcp", "--socket", socketPath, "--harness", harness, "--review-server"}
				// Only a managed child names its role, for its instructions.
				if role == core.SubagentMcpRoleChild || role == core.SubagentMcpRoleFetchOnly {
					args = append(args, "--role", role.ID())
				}
				return args
			}
			if expected {
				review := servers["mj-review"].(map[string]any)
				if review["type"] != "stdio" {
					t.Fatalf("review = %v", review)
				}
				// No alwaysLoad: Claude defers these tools until needed.
				if _, ok := review["alwaysLoad"]; ok {
					t.Fatalf("review = %v", review)
				}
				if review["command"] != filepath.Join(root, "hel") {
					t.Fatalf("review = %v", review)
				}
				if got := subagentsStrings(review["args"]); !slices.Equal(got, reviewArgs("claude")) {
					t.Fatalf("args = %q", got)
				}
			}

			for _, policy := range []core.ExecutionPolicy{core.ExecutionPolicyConfiguredApprovals, core.ExecutionPolicyUnconstrained} {
				_, err := configureCodexMCPWithMailboxes(root, "", home, &role, policy, true, true, tools, false)
				subagentsMust(t, err)
				servers := reviewGateCodexServers(t, home)
				agents := subagentsStrings(subagentsGet(servers, "mj-agents", "args"))
				if slices.Contains(agents, "--review-gate") || slices.Contains(agents, "--review-server") {
					t.Fatalf("agents = %q", agents)
				}
				if _, ok := servers["mj-review"]; ok != expected {
					t.Fatalf("%s %v", role, tools)
				}
				if expected {
					review := servers["mj-review"].(map[string]any)
					if got := subagentsStrings(review["args"]); !slices.Equal(got, reviewArgs("codex")) {
						t.Fatalf("args = %q", got)
					}
					// Codex may defer these tools; mj-agents opts out of that.
					if _, ok := review["omit_tools_from"]; ok {
						t.Fatalf("review = %v", review)
					}
					if subagentsGet(servers, "mj-agents", "omit_tools_from") == nil {
						t.Fatalf("servers = %v", servers)
					}
					if _, ok := review["default_tools_approval_mode"]; ok != (policy == core.ExecutionPolicyConfiguredApprovals) {
						t.Fatalf("review = %v", review)
					}
				}
			}
		}
	}
}

// TestAProfileStagedWithReviewToolsUnderMjAgentsIsUpgradedOnStart: a profile
// staged by the previous build lists the review tools under `mj-agents`. The
// next worker start rewrites it, whatever the gate says.
func TestAProfileStagedWithReviewToolsUnderMjAgentsIsUpgradedOnStart(t *testing.T) {
	root, home := reviewGateStagedProfile(t)
	socket := filepath.Join(root, SubagentSocket)
	staged, err := json.Marshal(map[string]any{"mcpServers": map[string]any{
		"other": map[string]any{"command": "x"},
		"mj-agents": map[string]any{"type": "stdio", "alwaysLoad": true, "command": filepath.Join(root, "hel"),
			"args": []any{"worker", "subagent-mcp", "--socket", socket, "--role", "parent", "--review-gate"}},
	}})
	subagentsMust(t, err)
	subagentsWrite(t, filepath.Join(home, ".claude.json"), string(staged))
	subagentsWrite(t, filepath.Join(home, "settings.json"), `{"permissions":{"allow":["mcp__mj-agents__request_review","mcp__mj-agents__override_review"]}}`)
	subagentsWrite(t, filepath.Join(home, "config.toml"), fmt.Sprintf(
		"[mcp_servers.mj-agents]\ncommand = \"/old/hel\"\nargs = [\"worker\", \"subagent-mcp\", \"--socket\", \"%s\", \"--role\", \"parent\", \"--review-gate\"]\n",
		socket))
	parent := subagentsRole(core.SubagentMcpRoleParent)

	_, err = configureClaudeSubagentMCP(root, "session", home, parent, true, true, true)
	subagentsMust(t, err)
	config, allow := reviewGateClaudeFiles(t, home)
	if subagentsGet(config, "mcpServers", "other", "command") != "x" {
		t.Fatalf("config = %v", config)
	}
	if slices.Contains(subagentsStrings(subagentsGet(config, "mcpServers", "mj-agents", "args")), "--review-gate") {
		t.Fatalf("config = %v", config)
	}
	if _, ok := subagentsGet(config, "mcpServers", "mj-review").(map[string]any); !ok {
		t.Fatalf("config = %v", config)
	}
	for _, rule := range subagentsReviewRules {
		if !slices.Contains(allow, rule) {
			t.Fatalf("allow = %q", allow)
		}
	}

	_, err = configureCodexMCPWithMailboxes(root, "", home, parent, core.ExecutionPolicyConfiguredApprovals, true, true, true, false)
	subagentsMust(t, err)
	servers := reviewGateCodexServers(t, home)
	if slices.Contains(subagentsStrings(subagentsGet(servers, "mj-agents", "args")), "--review-gate") {
		t.Fatalf("servers = %v", servers)
	}
	if _, ok := servers["mj-review"]; !ok {
		t.Fatalf("servers = %v", servers)
	}

	// Turning the gate off, or losing the role, removes the review server.
	_, err = configureClaudeSubagentMCP(root, "session", home, parent, true, true, false)
	subagentsMust(t, err)
	config, _ = reviewGateClaudeFiles(t, home)
	if subagentsGet(config, "mcpServers", "mj-review") != nil {
		t.Fatalf("config = %v", config)
	}
	if _, ok := subagentsGet(config, "mcpServers", "mj-agents").(map[string]any); !ok {
		t.Fatalf("config = %v", config)
	}
	_, err = configureCodexMCPWithMailboxes(root, "", home, parent, core.ExecutionPolicyConfiguredApprovals, true, true, false, false)
	subagentsMust(t, err)
	servers = reviewGateCodexServers(t, home)
	if _, ok := servers["mj-review"]; ok {
		t.Fatalf("servers = %v", servers)
	}
	if _, ok := servers["mj-agents"]; !ok {
		t.Fatalf("servers = %v", servers)
	}

	_, err = configureClaudeSubagentMCP(root, "session", home, parent, true, true, true)
	subagentsMust(t, err)
	_, err = configureClaudeSubagentMCP(root, "session", home, nil, true, true, true)
	subagentsMust(t, err)
	config, _ = reviewGateClaudeFiles(t, home)
	if subagentsGet(config, "mcpServers", "mj-review") != nil || subagentsGet(config, "mcpServers", "mj-agents") != nil {
		t.Fatalf("config = %v", config)
	}
	if subagentsGet(config, "mcpServers", "other", "command") != "x" {
		t.Fatalf("config = %v", config)
	}
	_, err = configureCodexMCPWithMailboxes(root, "", home, parent, core.ExecutionPolicyConfiguredApprovals, true, true, true, false)
	subagentsMust(t, err)
	_, err = configureCodexMCPWithMailboxes(root, "", home, nil, core.ExecutionPolicyConfiguredApprovals, true, true, true, false)
	subagentsMust(t, err)
	servers = reviewGateCodexServers(t, home)
	if len(servers) != 0 {
		t.Fatalf("servers = %v", servers)
	}
}
