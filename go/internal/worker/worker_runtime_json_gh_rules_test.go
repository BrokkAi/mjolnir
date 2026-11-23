// Port of the tests in mj-worker/src/worker_runtime/json_gh_rules.rs.

//go:build unix

package worker

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/tailscale/hujson"
)

func jsonGhRulesConfigure(policy core.ExecutionPolicy, denials openCodeDenials, environment map[string]string) error {
	return configureOpencodeRules(policy, denials, environment)
}

// jsonGhRulesTestRules is each rule of the document's permissions as the
// strings under keys.
func jsonGhRulesTestRules(t *testing.T, text string, keys ...string) [][]string {
	t.Helper()
	_, object, err := jsonGhRulesDocument(text)
	if err != nil {
		t.Fatal(err)
	}
	rules, ok := jsonGhRulesArrayValue(object, "permissions")
	if !ok {
		t.Fatal("permissions must be an array")
	}
	var described [][]string
	for index := range rules.Elements {
		rule := rules.Elements[index].Value.(*hujson.Object)
		var values []string
		for _, key := range keys {
			value, ok := jsonGhRulesStringValue(jsonGhRulesMember(rule, key))
			if !ok {
				t.Fatalf("rule %d has no %s", index, key)
			}
			values = append(values, value)
		}
		described = append(described, values)
	}
	return described
}

func TestOpencodeNativeOverlayPreservesJsoncAndLegacyDefaults(t *testing.T) {
	original := `{
  // retain the user's scalar default and native rule order
  "permission": "ask",
  "permissions": [
    {"action":"bash","resource":"gh *","effect":"allow"},
    {"action":"bash","resource":"*","effect":"deny"},
    {"action":"bash","resource":"rm *","effect":"deny"},
    {"action":"read","resource":".env","effect":"ask"},
  ],
  "providers": {"z":{}, "a":{}},
}`
	environment := map[string]string{opencodeContent: original}
	if err := jsonGhRulesConfigure(core.ExecutionPolicyConfiguredApprovals, openCodeDenials{nativeSubagent: false}, environment); err != nil {
		t.Fatal(err)
	}
	updated := environment[opencodeContent]
	for _, kept := range []string{
		"// retain the user's scalar default and native rule order",
		`"permission": "ask"`,
		`"providers": {"z":{}, "a":{}}`,
	} {
		if !strings.Contains(updated, kept) {
			t.Fatalf("lost %q:\n%s", kept, updated)
		}
	}
	var resources, actions []string
	for _, rule := range jsonGhRulesTestRules(t, updated, "resource", "action") {
		resources = append(resources, rule[0])
		actions = append(actions, rule[1])
	}
	// The `gh` allows come last, after every rule this document supplies, so
	// an earlier blanket deny cannot shadow them; the denials follow them and
	// cover only their own tools.
	if !slices.Equal(resources, []string{
		"gh *", "*", "rm *", ".env", "gh", "gh *", "*", "*", "*", "*", "*", "*", "*", "*",
		"*", "*", "*", "*", "*",
	}) {
		t.Fatalf("resources = %q", resources)
	}
	if !slices.Equal(actions[len(actions)-15:], []string{
		"shell",
		"shell",
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
		"execute",
	}) {
		t.Fatalf("actions = %q", actions)
	}
	if err := jsonGhRulesConfigure(core.ExecutionPolicyConfiguredApprovals, openCodeDenials{nativeSubagent: false}, environment); err != nil {
		t.Fatal(err)
	}
	if environment[opencodeContent] != updated {
		t.Fatalf("a repeat launch changed the document:\n%s", environment[opencodeContent])
	}
}

// TestOpencodeRulesApplyInEveryApprovalModeAndStayIdempotent: both approval
// modes keep the denials, and a guardian launch also keeps the `gh` allows,
// in one suffix that never grows on a repeat launch.
func TestOpencodeRulesApplyInEveryApprovalModeAndStayIdempotent(t *testing.T) {
	// The table names the document OpenCode accepts the approval in; the
	// writer must supply that same one.
	if core.HarnessKindOpenCode.GhApproval() != (core.GhApprovalSessionConfigDocument{Value: opencodeContent}) {
		t.Fatal("OpenCode takes its approval in OPENCODE_CONFIG_CONTENT")
	}
	for _, policy := range []core.ExecutionPolicy{core.ExecutionPolicyUnconstrained, core.ExecutionPolicyConfiguredApprovals} {
		for _, nativeSubagent := range []bool{false, true} {
			denials := openCodeDenials{nativeSubagent: nativeSubagent}
			environment := map[string]string{}
			if err := jsonGhRulesConfigure(policy, denials, environment); err != nil {
				t.Fatal(err)
			}
			first := environment[opencodeContent]
			var expected [][]string
			if policy == core.ExecutionPolicyConfiguredApprovals {
				expected = append(expected,
					[]string{"shell", "allow"},
					[]string{"shell", "allow"},
					[]string{"worktree_create", "deny"},
					[]string{"worktree_remove", "deny"},
					[]string{"session_move", "deny"},
					[]string{"artifact_create", "deny"},
					[]string{"artifact_update", "deny"},
					[]string{"artifact_delete", "deny"},
					[]string{"schedule_create", "deny"},
					[]string{"schedule_pause", "deny"},
					[]string{"schedule_resume", "deny"},
					[]string{"schedule_delete", "deny"},
					[]string{"console_watch_watch", "deny"},
					[]string{"console_watch_unwatch", "deny"},
				)
			}
			expected = append(expected, []string{"execute", "deny"})
			if nativeSubagent {
				expected = append(expected, []string{"subagent", "deny"})
			}
			got := jsonGhRulesTestRules(t, environment[opencodeContent], "action", "effect")
			if !slices.EqualFunc(got, expected, slices.Equal) {
				t.Fatalf("%v %v: %q", policy, nativeSubagent, got)
			}
			if err := jsonGhRulesConfigure(policy, denials, environment); err != nil {
				t.Fatal(err)
			}
			if environment[opencodeContent] != first {
				t.Fatalf("a repeat launch changed the document:\n%s", environment[opencodeContent])
			}
		}
	}
}

// TestOpencodeGhApprovalFollowsAProfilesOwnLaterRules: a profile may supply
// its own `OPENCODE_CONFIG_CONTENT` with a blanket ask. OpenCode takes the
// last matching rule, so the managed allows must follow it; the profile's
// own rule and text stay.
func TestOpencodeGhApprovalFollowsAProfilesOwnLaterRules(t *testing.T) {
	original := `{
  // keep the person's rule order
  "permissions": [
    {"action":"bash","resource":"*","effect":"ask"},
    {"action":"read","resource":".env","effect":"ask"},
  ],
  "providers": {"z":{}, "a":{}},
}`
	environment := map[string]string{opencodeContent: original}
	if err := jsonGhRulesConfigure(core.ExecutionPolicyConfiguredApprovals, openCodeDenials{nativeSubagent: false}, environment); err != nil {
		t.Fatal(err)
	}
	updated := environment[opencodeContent]
	if !strings.Contains(updated, "// keep the person's rule order") || !strings.Contains(updated, `"providers": {"z":{}, "a":{}}`) {
		t.Fatalf("lost the profile's text:\n%s", updated)
	}
	expected := [][]string{
		{"*", "ask"},
		{".env", "ask"},
		{"gh", "allow"},
		{"gh *", "allow"},
	}
	for range 13 {
		expected = append(expected, []string{"*", "deny"})
	}
	got := jsonGhRulesTestRules(t, updated, "resource", "effect")
	if !slices.EqualFunc(got, expected, slices.Equal) {
		t.Fatalf("rules = %q", got)
	}
}

// jsonGhRulesReadHTTPRequest reads one HTTP/1.1 request: whether it is a
// POST, and its body.
func jsonGhRulesReadHTTPRequest(reader *bufio.Reader) (bool, []byte, error) {
	line, err := reader.ReadString('\n')
	if err != nil {
		return false, nil, err
	}
	isPost := strings.HasPrefix(line, "POST")
	length := 0
	for {
		line, err := reader.ReadString('\n')
		if err != nil {
			return false, nil, err
		}
		if strings.TrimSpace(line) == "" {
			break
		}
		if value, ok := strings.CutPrefix(strings.ToLower(line), "content-length:"); ok {
			length, _ = strconv.Atoi(strings.TrimSpace(value))
		}
	}
	body := make([]byte, length)
	_, err = io.ReadFull(reader, body)
	return isPost, body, err
}

// jsonGhRulesOfferedTools is the function names a chat request offered.
func jsonGhRulesOfferedTools(request map[string]any) []string {
	tools := []string{}
	list, _ := request["tools"].([]any)
	for _, tool := range list {
		function, _ := tool.(map[string]any)["function"].(map[string]any)
		if name, ok := function["name"].(string); ok {
			tools = append(tools, name)
		}
	}
	return tools
}

func jsonGhRulesChunk(delta any, finish any) string {
	encoded, _ := json.Marshal(map[string]any{
		"id": "c", "object": "chat.completion.chunk", "created": 1, "model": "m",
		"choices": []any{map[string]any{"index": 0, "delta": delta, "finish_reason": finish}},
	})
	return "data: " + string(encoded) + "\n\n"
}

func jsonGhRulesWriteModels(stream net.Conn) {
	models := `{"data":[{"id":"m","object":"model"}]}`
	fmt.Fprintf(stream, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: %d\r\nconnection: close\r\n\r\n%s", len(models), models)
}

// stubProvider serves OpenCode's provider requests from this goroutine,
// recording the tool names each chat request offered the model.
func stubProvider(listener net.Listener, offered *jsonGhRulesOffered) {
	for {
		stream, err := listener.Accept()
		if err != nil {
			return
		}
		go func() {
			defer stream.Close()
			isPost, body, err := jsonGhRulesReadHTTPRequest(bufio.NewReader(stream))
			if err != nil {
				return
			}
			if !isPost {
				jsonGhRulesWriteModels(stream)
				return
			}
			var request map[string]any
			_ = json.Unmarshal(body, &request)
			offered.push(jsonGhRulesOfferedTools(request))
			events := jsonGhRulesChunk(map[string]any{"role": "assistant", "content": "stub ok"}, nil) +
				jsonGhRulesChunk(map[string]any{}, "stop") + "data: [DONE]\n\n"
			fmt.Fprintf(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: %d\r\nconnection: close\r\n\r\n%s", len(events), events)
		}()
	}
}

// jsonGhRulesOffered is the tool lists the provider saw, shared across its
// connections.
type jsonGhRulesOffered struct {
	mu    sync.Mutex
	lists [][]string
}

func (o *jsonGhRulesOffered) push(tools []string) {
	o.mu.Lock()
	defer o.mu.Unlock()
	o.lists = append(o.lists, tools)
}

func (o *jsonGhRulesOffered) snapshot() [][]string {
	o.mu.Lock()
	defer o.mu.Unlock()
	return slices.Clone(o.lists)
}

func (o *jsonGhRulesOffered) clear() {
	o.mu.Lock()
	defer o.mu.Unlock()
	o.lists = nil
}

func jsonGhRulesProviderDocument(port int, extra map[string]any) string {
	document := map[string]any{
		"provider": map[string]any{"stub": map[string]any{
			"npm": "@ai-sdk/openai-compatible", "name": "stub",
			"options": map[string]any{"baseURL": fmt.Sprintf("http://127.0.0.1:%d/v1", port), "apiKey": "x"},
			"models": map[string]any{"m": map[string]any{
				"name": "m", "tool_call": true, "limit": map[string]any{"context": 100000, "output": 4000},
			}},
		}},
		"model": "stub/m", "small_model": "stub/m", "autoupdate": false,
	}
	for key, value := range extra {
		document[key] = value
	}
	encoded, _ := json.Marshal(document)
	return string(encoded)
}

// liveOpenCode is a real OpenCode 2.2.7 ACP process driven over stdio,
// answering every permission request with the given option and recording
// it.
type liveOpenCode struct {
	t       *testing.T
	child   *exec.Cmd
	stdin   io.WriteCloser
	replies chan map[string]any
	nextID  int
	asked   []any
}

func startLiveOpenCode(t *testing.T, root, work, content string) *liveOpenCode {
	t.Helper()
	opencode := os.Getenv("MJ_OPENCODE_TEST_BINARY")
	if opencode == "" {
		t.Fatal("set OpenCode")
	}
	child := exec.Command(opencode, "acp")
	child.Env = []string{
		"PATH=/usr/bin:/bin",
		"HOME=" + filepath.Join(root, "home"),
		"XDG_CONFIG_HOME=" + filepath.Join(root, "config"),
		"XDG_DATA_HOME=" + filepath.Join(root, "data"),
		"XDG_CACHE_HOME=" + filepath.Join(root, "cache"),
		"XDG_STATE_HOME=" + filepath.Join(root, "state"),
		opencodeContent + "=" + content,
	}
	child.Dir = work
	stdin, err := child.StdinPipe()
	if err != nil {
		t.Fatal(err)
	}
	stdout, err := child.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err := child.Start(); err != nil {
		t.Fatal(err)
	}
	replies := make(chan map[string]any, 1024)
	go func() {
		scanner := bufio.NewScanner(stdout)
		scanner.Buffer(make([]byte, 1024*1024), 64*1024*1024)
		for scanner.Scan() {
			var message map[string]any
			if json.Unmarshal(scanner.Bytes(), &message) == nil {
				replies <- message
			}
		}
	}()
	session := &liveOpenCode{t: t, child: child, stdin: stdin, replies: replies}
	t.Cleanup(session.stop)
	return session
}

func (s *liveOpenCode) stop() {
	_ = s.child.Process.Kill()
	_ = s.child.Wait()
}

func (s *liveOpenCode) call(method string, params any, answer string) map[string]any {
	s.t.Helper()
	s.nextID++
	id := float64(s.nextID)
	request, _ := json.Marshal(map[string]any{"jsonrpc": "2.0", "id": s.nextID, "method": method, "params": params})
	fmt.Fprintf(s.stdin, "%s\n", request)
	for {
		var message map[string]any
		select {
		case message = <-s.replies:
		case <-time.After(60 * time.Second):
			s.t.Fatal(method)
		}
		if message["method"] == "session/request_permission" {
			params, _ := message["params"].(map[string]any)
			s.asked = append(s.asked, params["toolCall"])
			reply, _ := json.Marshal(map[string]any{"jsonrpc": "2.0", "id": message["id"],
				"result": map[string]any{"outcome": map[string]any{"outcome": "selected", "optionId": answer}}})
			fmt.Fprintf(s.stdin, "%s\n", reply)
			continue
		}
		if message["id"] == id && message["method"] == nil {
			if message["error"] != nil {
				s.t.Fatalf("%s: %v", method, message)
			}
			result, _ := message["result"].(map[string]any)
			return result
		}
	}
}

// TestLiveOpencodeOffersMjAgentsDirectlyAndDropsDeniedTools: the
// configuration this module builds, run through the pinned OpenCode:
// `execute` is gone so `mj-agents` tools are direct tools, and `subagent` is
// gone exactly when native delegation is suppressed. A stub provider records
// what the model is offered; nothing leaves the machine.
func TestLiveOpencodeOffersMjAgentsDirectlyAndDropsDeniedTools(t *testing.T) {
	// Rust: #[ignore]; run it by setting both binaries.
	if os.Getenv("MJ_OPENCODE_TEST_BINARY") == "" || os.Getenv("MJ_WORKER_BINARY") == "" {
		t.Skip("requires MJ_OPENCODE_TEST_BINARY naming OpenCode 2.2.7 and MJ_WORKER_BINARY pointing to the current mj-worker build")
	}
	worker := os.Getenv("MJ_WORKER_BINARY")
	for _, nativeSubagent := range []bool{false, true} {
		root := t.TempDir()
		work := filepath.Join(root, "work")
		if err := os.MkdirAll(work, 0o777); err != nil {
			t.Fatal(err)
		}
		listener, err := net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			t.Fatal(err)
		}
		defer listener.Close()
		port := listener.Addr().(*net.TCPAddr).Port
		offered := &jsonGhRulesOffered{}
		go stubProvider(listener, offered)
		environment := map[string]string{opencodeContent: jsonGhRulesProviderDocument(port, nil)}
		if err := configureOpencodeRules(core.ExecutionPolicyUnconstrained, openCodeDenials{nativeSubagent: nativeSubagent}, environment); err != nil {
			t.Fatal(err)
		}
		session := startLiveOpenCode(t, root, work, environment[opencodeContent])
		session.call("initialize", map[string]any{"protocolVersion": 1, "clientCapabilities": map[string]any{}}, "once")
		socket := filepath.Join(root, "subagents.sock")
		opened := session.call("session/new", map[string]any{"cwd": work, "mcpServers": []any{map[string]any{
			"name": "mj-agents", "command": worker,
			"args": []any{"worker", "subagent-mcp", "--socket", socket, "--harness", core.HarnessKindOpenCode.ID(), "--role", "parent"},
			"env":  []any{},
		}}}, "once")
		// The only prompt, sent right after `session/new`, must already list
		// the connected server's tools. A title request may precede it and
		// offer no tools.
		session.call("session/prompt", map[string]any{"sessionId": opened["sessionId"],
			"prompt": []any{map[string]any{"type": "text", "text": "hello"}}}, "once")
		var tools []string
		for _, list := range offered.snapshot() {
			if slices.Contains(list, "mj-agents_spawn") {
				tools = list
				break
			}
		}
		session.stop()
		if !slices.Contains(tools, "mj-agents_list_agents") {
			t.Fatalf("%q of %q", tools, offered.snapshot())
		}
		if slices.Contains(tools, "execute") {
			t.Fatalf("%q", tools)
		}
		if slices.Contains(tools, "subagent") != !nativeSubagent {
			t.Fatalf("%q", tools)
		}
	}
}

// jsonGhRulesScriptCall is one tool call a scripted provider makes.
type jsonGhRulesScriptCall struct {
	name      string
	arguments map[string]any
}

// scriptedProvider serves OpenCode's provider requests from a script: the
// last user message names a script, and each tool result so far advances it
// by one call. Records the tools each request offered.
func scriptedProvider(listener net.Listener, scripts map[string][]jsonGhRulesScriptCall, offered *jsonGhRulesOffered) {
	names := make([]string, 0, len(scripts))
	for name := range scripts {
		names = append(names, name)
	}
	slices.Sort(names)
	for {
		stream, err := listener.Accept()
		if err != nil {
			return
		}
		go func() {
			defer stream.Close()
			isPost, body, err := jsonGhRulesReadHTTPRequest(bufio.NewReader(stream))
			if err != nil {
				return
			}
			if !isPost {
				jsonGhRulesWriteModels(stream)
				return
			}
			var request map[string]any
			_ = json.Unmarshal(body, &request)
			tools := jsonGhRulesOfferedTools(request)
			messages, _ := request["messages"].([]any)
			user := 0
			for index := len(messages) - 1; index >= 0; index-- {
				if message, ok := messages[index].(map[string]any); ok && message["role"] == "user" {
					user = index
					break
				}
			}
			text := ""
			if user < len(messages) {
				content, _ := json.Marshal(messages[user].(map[string]any)["content"])
				text = string(content)
			}
			step := 0
			for _, message := range messages[min(user, len(messages)):] {
				if message.(map[string]any)["role"] == "tool" {
					step++
				}
			}
			var call *jsonGhRulesScriptCall
			if len(tools) > 0 {
				for _, name := range names {
					if strings.Contains(text, name) {
						if step < len(scripts[name]) {
							call = &scripts[name][step]
						}
						break
					}
				}
			}
			offered.push(tools)
			var events string
			if call != nil {
				arguments, _ := json.Marshal(call.arguments)
				events = jsonGhRulesChunk(map[string]any{"role": "assistant", "tool_calls": []any{map[string]any{
					"index": 0, "id": fmt.Sprintf("call_%d", step), "type": "function",
					"function": map[string]any{"name": call.name, "arguments": string(arguments)},
				}}}, nil) + jsonGhRulesChunk(map[string]any{}, "tool_calls") + "data: [DONE]\n\n"
			} else {
				events = jsonGhRulesChunk(map[string]any{"role": "assistant", "content": "done"}, nil) +
					jsonGhRulesChunk(map[string]any{}, "stop") + "data: [DONE]\n\n"
			}
			fmt.Fprintf(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: %d\r\nconnection: close\r\n\r\n%s", len(events), events)
		}()
	}
}

// Hard-won: ef74ec25c: Mjolnir's appended OpenCode rules overrode Plan mode's and the person's denies.
//
// TestLiveOpencodeGuardianHookAsksOnlyBeyondTheWorkspaceAndKeepsDenies: under
// configured approvals the guardian's hook makes the pinned OpenCode ask
// about what can reach outside the workspace and nothing else, while every
// deny stands: the person's own rule and Plan mode's. The reviewer's launch
// may only read, and drops the session's other plugins.
func TestLiveOpencodeGuardianHookAsksOnlyBeyondTheWorkspaceAndKeepsDenies(t *testing.T) {
	// Rust: #[ignore]; run it by setting MJ_OPENCODE_TEST_BINARY.
	if os.Getenv("MJ_OPENCODE_TEST_BINARY") == "" {
		t.Skip("requires MJ_OPENCODE_TEST_BINARY naming OpenCode 2.2.7")
	}
	root := t.TempDir()
	work := filepath.Join(root, "work")
	if err := os.MkdirAll(filepath.Join(work, ".git/hooks"), 0o777); err != nil {
		t.Fatal(err)
	}
	// A link whose target does not exist yet, as a cloned repository can
	// carry one, sends a write outside the workspace.
	if err := os.Symlink(filepath.Join(root, "outside.txt"), filepath.Join(work, "escape")); err != nil {
		t.Fatal(err)
	}
	// The person's own plugin, which the reviewer must keep.
	person := filepath.Join(root, "person-plugin")
	if err := os.MkdirAll(person, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(person, "index.js"), []byte("export default { id: \"person\", async setup() { return () => {}; } };\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(person, "package.json"), []byte(`{"name":"person","type":"module","main":"index.js"}`), 0o666); err != nil {
		t.Fatal(err)
	}
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	port := listener.Addr().(*net.TCPAddr).Port
	scripts := map[string][]jsonGhRulesScriptCall{
		"BUILD": {
			{"shell", map[string]any{"command": "touch by-shell.txt"}},
			{"write", map[string]any{"path": "notes.txt", "content": "notes"}},
			{"write", map[string]any{"path": ".git/hooks/post-checkout", "content": "#!/bin/sh"}},
			{"write", map[string]any{"path": "escape", "content": "out"}},
			{"write", map[string]any{"path": ".GIT/config", "content": "x"}},
			{"shell", map[string]any{"command": "rm notes.txt"}},
			{"read", map[string]any{"path": "notes.txt"}},
		},
		"PLAN": {
			{"write", map[string]any{"path": "by-plan.txt", "content": "plan"}},
		},
		"REVIEW": {
			{"shell", map[string]any{"command": "touch by-reviewer.txt"}},
			{"write", map[string]any{"path": "by-reviewer.txt", "content": "x"}},
			{"read", map[string]any{"path": "notes.txt"}},
		},
	}
	offered := &jsonGhRulesOffered{}
	go scriptedProvider(listener, scripts, offered)
	environment := map[string]string{opencodeContent: jsonGhRulesProviderDocument(port, map[string]any{
		// The person's own deny, which must outlast Mjolnir's rules.
		"permissions": []any{map[string]any{"action": "shell", "resource": "rm *", "effect": "deny"}},
		"plugins":     []any{person},
	})}
	if err := configureOpencodeRules(core.ExecutionPolicyConfiguredApprovals, openCodeDenials{nativeSubagent: false}, environment); err != nil {
		t.Fatal(err)
	}
	if err := configureOpencodeGuardian(root, []string{work}, environment); err != nil {
		t.Fatal(err)
	}

	session := startLiveOpenCode(t, root, work, environment[opencodeContent])
	session.call("initialize", map[string]any{"protocolVersion": 1, "clientCapabilities": map[string]any{}}, "once")
	opened := session.call("session/new", map[string]any{"cwd": work, "mcpServers": []any{}}, "once")
	finished := session.call("session/prompt", map[string]any{"sessionId": opened["sessionId"],
		"prompt": []any{map[string]any{"type": "text", "text": "BUILD"}}}, "once")
	if finished["stopReason"] != "end_turn" {
		t.Fatalf("%v", finished)
	}
	var asked []string
	for _, call := range session.asked {
		rawInput, _ := call.(map[string]any)["rawInput"].(map[string]any)
		text, ok := rawInput["command"].(string)
		if !ok {
			text, _ = rawInput["path"].(string)
		}
		asked = append(asked, text)
	}
	// The shell command, the edit to git's hooks, the write through the
	// dangling link and the edit to `.GIT` asked; the workspace edit and the
	// read did not; the person's deny refused `rm` unasked.
	if !slices.Equal(asked, []string{"touch by-shell.txt", ".git/hooks/post-checkout", "escape", ".GIT/config"}) {
		t.Fatalf("%q: %v", asked, session.asked)
	}
	if _, err := os.Stat(filepath.Join(work, "by-shell.txt")); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(work, "notes.txt")); err != nil {
		t.Fatal("the denied rm did not run")
	}

	// Plan mode's own edit deny stands.
	plan := session.call("session/new", map[string]any{"cwd": work, "mcpServers": []any{}}, "once")
	session.call("session/set_config_option", map[string]any{"sessionId": plan["sessionId"], "configId": "mode", "value": "plan"}, "once")
	session.call("session/prompt", map[string]any{"sessionId": plan["sessionId"],
		"prompt": []any{map[string]any{"type": "text", "text": "PLAN"}}}, "once")
	if _, err := os.Stat(filepath.Join(work, "by-plan.txt")); err == nil {
		t.Fatal("Plan mode must not write")
	}
	if len(session.asked) != 4 {
		t.Fatalf("%v", session.asked)
	}
	session.stop()

	// The reviewer reads and does nothing else, without asking. Its launch
	// keeps the person's plugin and swaps Mjolnir's for its own hook.
	reviewer, err := opencodeReviewerEnvironment(environment)
	if err != nil {
		t.Fatal(err)
	}
	_, object, err := jsonGhRulesDocument(reviewer[opencodeContent])
	if err != nil {
		t.Fatal(err)
	}
	pluginArray, _ := jsonGhRulesArrayValue(object, "plugins")
	var plugins []string
	for index := range pluginArray.Elements {
		if entry, ok := jsonGhRulesStringValue(&pluginArray.Elements[index]); ok {
			plugins = append(plugins, entry)
		}
	}
	if !slices.Equal(plugins, []string{person, filepath.Join(root, guardianReviewerPlugin)}) {
		t.Fatalf("plugins = %q", plugins)
	}
	offered.clear()
	review := startLiveOpenCode(t, root, work, reviewer[opencodeContent])
	review.call("initialize", map[string]any{"protocolVersion": 1, "clientCapabilities": map[string]any{}}, "once")
	reviewSession := review.call("session/new", map[string]any{"cwd": work, "mcpServers": []any{}}, "once")
	finished = review.call("session/prompt", map[string]any{"sessionId": reviewSession["sessionId"],
		"prompt": []any{map[string]any{"type": "text", "text": "REVIEW"}}}, "once")
	if finished["stopReason"] != "end_turn" {
		t.Fatalf("a refused tool is a tool error: %v", finished)
	}
	if len(review.asked) != 0 {
		t.Fatalf("%v", review.asked)
	}
	if _, err := os.Stat(filepath.Join(work, "by-reviewer.txt")); err == nil {
		t.Fatal("the reviewer must not write")
	}
	var tools []string
	for _, list := range offered.snapshot() {
		if len(list) > 0 {
			tools = list
			break
		}
	}
	if !slices.Contains(tools, "read") {
		t.Fatalf("%q", tools)
	}
	if slices.ContainsFunc(tools, func(tool string) bool { return tool == "shell" || tool == "write" || tool == "edit" }) {
		t.Fatalf("%q", tools)
	}
}
