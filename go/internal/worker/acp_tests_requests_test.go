// Port of mj-worker/src/acp/tests.rs: the tests of the session requests a
// launch sends, the MCP servers and tools it registers, and the permission
// answers it gives on its own.

package worker

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"testing"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// acpTestsJV is a value as serde_json::Value holds it in the production code:
// the JSON jsonvalue reads back from value's encoding.
func acpTestsJV(t *testing.T, value any) any {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	parsed, err := jsonvalue.Parse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	return parsed
}

// acpTestsRPCError is agent_client_protocol::Error::new(code,
// message).data(data); nil data is none.
func acpTestsRPCError(code int, message string, data any) *acp.RPCError {
	rpc := &acp.RPCError{Code: code, Message: message}
	if data != nil {
		encoded, err := json.Marshal(data)
		if err != nil {
			panic(err)
		}
		rpc.Data = encoded
	}
	return rpc
}

// acpTestsOuterMessage is an error's own message without the errors it wraps:
// anyhow's `to_string()`, where `{:#}` (err.Error()) is the whole chain.
func acpTestsOuterMessage(err error) string {
	message := err.Error()
	if inner := errors.Unwrap(err); inner != nil {
		message = strings.TrimSuffix(message, ": "+inner.Error())
	}
	return message
}

// acpTestsPermissionRequest is RequestPermissionRequest::new(session,
// tool_call, options) from their JSON shapes.
func acpTestsPermissionRequest(t *testing.T, sessionID string, toolCall map[string]any, options ...map[string]any) schema.RequestPermissionRequest {
	t.Helper()
	offered := make([]any, 0, len(options))
	for _, option := range options {
		offered = append(offered, option)
	}
	encoded, err := json.Marshal(map[string]any{"sessionId": sessionID, "toolCall": toolCall, "options": offered})
	if err != nil {
		t.Fatal(err)
	}
	return acpTestsDecode[schema.RequestPermissionRequest](t, string(encoded))
}

// acpTestsOption is PermissionOption::new(id, name, kind).
func acpTestsOption(id, name, kind string) map[string]any {
	return map[string]any{"optionId": id, "name": name, "kind": kind}
}

// acpTestsSelectedOption is the optionId a permission response selects.
func acpTestsSelectedOption(t *testing.T, response any) any {
	t.Helper()
	return acpTestsGet(acpTestsPlain(t, response), "outcome", "optionId")
}

// acpTestsServerNames is the names of a request's MCP servers.
func acpTestsServerNames(request any) []string {
	names := []string{}
	for _, server := range acpTestsArray(acpTestsGet(request, "mcpServers")) {
		name, _ := acpTestsString(acpTestsGet(server, "name"))
		names = append(names, name)
	}
	return names
}

// acpTestsOpenRequests is the `session/new`, `session/resume` and
// `session/load` requests spec sends, as JSON values.
func acpTestsOpenRequests(t *testing.T, spec *LaunchSpec, includeHistoryTools bool) (newSession, resume, load any) {
	t.Helper()
	newSession = acpTestsPlain(t, newSessionRequest(spec, spec.AdditionalDirectories, includeHistoryTools, nil))
	resume = acpTestsPlain(t, resumeSessionRequest(spec, spec.AdditionalDirectories, schema.SessionId("native"), nil))
	load = acpTestsPlain(t, loadSessionRequest(spec, spec.AdditionalDirectories, schema.SessionId("native"), nil))
	return newSession, resume, load
}

// Every launch request states the servers Mjolnir owns. A bridge that opens
// the session again is a new harness process, and Codex builds the resumed
// thread's MCP set from the request alone (#1085), so a resume that sends
// none leaves the session without its delegation tools.
func TestEveryLaunchRequestRegistersOnlyAcpDeliveredMcpServers(t *testing.T) {
	spec := acpTestsSpec(core.HarnessKindCodex, "codex-acp", "/workspace/app")
	newSession, resume, load := acpTestsOpenRequests(t, &spec, true)
	if !acpTestsHas(newSession, "mcpServers") || !acpTestsJSONEqual(t, acpTestsGet(newSession, "mcpServers"), []any{}) {
		t.Fatalf("a new session always states its MCP set, even when it is empty: %v", newSession)
	}
	for _, opened := range []any{resume, load} {
		if acpTestsHas(opened, "mcpServers") && !acpTestsJSONEqual(t, acpTestsGet(opened, "mcpServers"), []any{}) {
			t.Fatalf("a session with no Mjolnir servers asks for none: %v", opened)
		}
	}

	spec.SubagentPolicy = core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}
	socket := workerSocket(core.SubagentMcpRoleParent)
	spec.SubagentMCPSocket = &socket
	for _, reviewTools := range []bool{false, true} {
		for _, launch := range []struct {
			harness             core.HarnessKind
			profileRegistration bool
		}{
			{core.HarnessKindCodex, false},
			{core.HarnessKindCodex, true},
			{core.HarnessKindClaude, true},
			{core.HarnessKindDsh, false},
			{core.HarnessKindGrok, false},
		} {
			spec.Harness = launch.harness
			spec.SubagentMCPSocket.ProfileRegistration = launch.profileRegistration
			spec.SubagentMCPSocket.ReviewTools = reviewTools
			newSession, resume, load := acpTestsOpenRequests(t, &spec, true)
			for _, request := range []any{newSession, resume, load} {
				expected := []string{}
				switch {
				case launch.profileRegistration:
				case reviewTools:
					expected = []string{"mj-agents", "mj-review"}
				default:
					expected = []string{"mj-agents"}
				}
				if names := acpTestsServerNames(request); !slices.Equal(names, expected) {
					t.Fatalf("only harnesses without profile registration receive the ACP servers: got %v, want %v: %v",
						names, expected, request)
				}
			}
		}
	}
	spec.SubagentMCPSocket.ReviewTools = false
}

// OpenCode 2.2.7 titles an MCP call `<server>_<tool>`, and server names may
// contain `_`, so another server can produce an owned tool's exact title.
// Hard-won: 01332d2da: a server named `mj-agents_send` could borrow `send_message`'s approval
func TestOpencodeMcpCallsAlwaysReachThePerson(t *testing.T) {
	spec := reloadFallbackSpec(core.HarnessKindOpenCode)
	spec.ExecutionPolicy = core.ExecutionPolicyConfiguredApprovals
	socket := workerSocket(core.SubagentMcpRoleParent)
	socket.ReviewTools = true
	spec.SubagentMCPSocket = &socket
	approvals := ownedMcpApprovalsForLaunch(&spec)
	// `mj-agents` + `send_message` and `mj-agents_send` + `message` look alike.
	for _, title := range []string{"mj-agents_spawn", "mj-agents_send_message", "mj-review_request_review"} {
		request := acpTestsPermissionRequest(t, "native",
			map[string]any{"toolCallId": "call", "title": title, "kind": "other"},
			acpTestsOption("once", "Allow once", "allow_once"),
			acpTestsOption("reject", "Reject", "reject_once"))
		if response := approvals.response(&request); response != nil {
			t.Fatalf("%s was answered without the person: %#v", title, response)
		}
	}
}

// agy 1.5.11 titles an MCP call `mcp_<server>_<tool>` and names no server in
// the request, so a title cannot tell an owned tool from a lookalike. Its own
// `mcp(<server>/*)` grant can: the worker stages one per registered owned
// server in agy's project file and answers none of agy's requests itself.
// `tests/e2e/antigravity_owned_mcp_grants.py` runs the same golden file
// through the real agy.
// Hard-won: 14ded1285: agy's Auto mode still asked a person before every mj MCP tool call, including handback.
func TestAntigravityOwnedMcpServersAreGrantedInItsProjectFile(t *testing.T) {
	home := t.TempDir()
	project := filepath.Join(home, "config", "projects", "default-cli-project.json")
	if err := os.MkdirAll(filepath.Dir(project), 0o755); err != nil {
		t.Fatal(err)
	}
	// What interactive agy writes, plus a grant of the person's own and one an
	// earlier launch left for a server this launch no longer registers.
	if err := os.WriteFile(project, []byte(`{
  "id": "default-cli-project",
  "name": "CLI Project",
  "projectResources": {},
  "permissionGrants": {
    "permissionGrants": {
      "allow": [
        "command(gh pr list)",
        "read_file(/personal-notes/)",
        "mcp(mj-review/*)"
      ],
      "deny": [
        "command(rm)"
      ]
    },
    "v2Migrated": true
  }
}`), 0o644); err != nil {
		t.Fatal(err)
	}
	spec := reloadFallbackSpec(core.HarnessKindAntigravity)
	spec.ExecutionPolicy = core.ExecutionPolicyConfiguredApprovals
	spec.Environment["MJ_ANTIGRAVITY_HOME"] = home
	userHome := filepath.Join(home, "user-home")
	homeVariable := "HOME"
	if runtime.GOOS == "windows" {
		homeVariable = "USERPROFILE"
	}
	spec.Environment[homeVariable] = userHome
	socket := workerSocket(core.SubagentMcpRoleChild)
	spec.SubagentMCPSocket = &socket
	spec.ProjectMemory = &core.ProjectMemoryLaunchConfig{
		HistorySocket:   acpTestsStringPointer("/worker/history.sock"),
		ProjectKey:      "project",
		Root:            "/memory",
		BaselineRoot:    "/baseline",
		RepositoryRoots: map[string]string{},
		MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
	}
	// A configured server never gets a grant, even one named like ours.
	spec.ExtraMCPServers = []ReviewerMcpServer{NewReviewerMcpServer(core.ReviewMcpServer{
		Name:    "mj-agents_send",
		Command: "analyzer",
		Args:    []string{},
	})}
	readRule := func(directory string) string {
		encoded, err := json.Marshal(fmt.Sprintf("read_file(%s)", directory+string(filepath.Separator)))
		if err != nil {
			t.Fatal(err)
		}
		return string(encoded)
	}
	read := func() string {
		t.Helper()
		contents, err := os.ReadFile(project)
		if err != nil {
			t.Fatal(err)
		}
		return string(contents)
	}
	if err := stageAntigravityGrants(&spec); err != nil {
		t.Fatal(err)
	}
	staged := read()
	golden, err := os.ReadFile(filepath.Join("testdata", "acp", "antigravity_project_grants.json"))
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.ReplaceAll(staged, readRule(userHome), `"read_file(@USER_HOME@)"`); got != string(golden) {
		t.Fatalf("staged project file:\n%s\nwant:\n%s", got, golden)
	}
	// Every worker start stages again.
	if err := stageAntigravityGrants(&spec); err != nil {
		t.Fatal(err)
	}
	if read() != staged {
		t.Fatalf("staging again changed the project file:\n%s", read())
	}
	// A later launch of this profile with another home replaces the earlier
	// home's grant rather than leaving both homes readable.
	otherHome := filepath.Join(home, "other-home")
	spec.Environment[homeVariable] = otherHome
	if err := stageAntigravityGrants(&spec); err != nil {
		t.Fatal(err)
	}
	if want := strings.ReplaceAll(staged, readRule(userHome), readRule(otherHome)); read() != want {
		t.Fatalf("another home's grant:\n%s\nwant:\n%s", read(), want)
	}
	spec.Environment[homeVariable] = userHome
	if err := stageAntigravityGrants(&spec); err != nil {
		t.Fatal(err)
	}
	if read() != staged {
		t.Fatalf("returning to the first home:\n%s", read())
	}
	// The worker itself answers nothing for agy.
	request := acpTestsPermissionRequest(t, "native",
		map[string]any{"toolCallId": "call", "title": "Run mcp_mj-agents_handback", "kind": "other"},
		acpTestsOption("allow", "Allow", "allow_once"))
	approvals := ownedMcpApprovalsForLaunch(&spec)
	if response := approvals.response(&request); response != nil {
		t.Fatalf("the worker answered agy's request: %#v", response)
	}
	// Missing or root-valued launch homes must fail visibly, never grant `/`.
	delete(spec.Environment, homeVariable)
	if err := stageAntigravityGrants(&spec); err == nil || !strings.Contains(acpTestsOuterMessage(err), homeVariable) {
		t.Fatalf("a missing home must name %s: %v", homeVariable, err)
	}
	spec.Environment[homeVariable] = filepath.VolumeName(userHome) + string(filepath.Separator)
	if err := stageAntigravityGrants(&spec); err == nil {
		t.Fatal("a root home must not be granted")
	}
	// Unconstrained targets run agy with its skip-permissions flag instead.
	spec.ExecutionPolicy = core.ExecutionPolicyUnconstrained
	if err := os.WriteFile(project, []byte("unchanged"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := stageAntigravityGrants(&spec); err != nil {
		t.Fatal(err)
	}
	if read() != "unchanged" {
		t.Fatalf("an unconstrained launch rewrote the project file: %s", read())
	}
}

func TestGuardianPreapprovesTrustedGhAndOwnedMcpOnEverySessionOpen(t *testing.T) {
	// A DSH permission request, titled and offering options as dsh-acp 0.2.0
	// does, and the option the worker selects for it without asking.
	dshAnswer := func(approvals *OwnedMcpApprovals, title string) any {
		request := acpTestsPermissionRequest(t, "native",
			map[string]any{"toolCallId": "call", "title": title},
			acpTestsOption("allow_once", "Allow once", "allow_once"),
			acpTestsOption("allow_always", "Allow for this session", "allow_always"),
			acpTestsOption("reject_once", "Deny", "reject_once"))
		response := approvals.response(&request)
		if response == nil {
			return nil
		}
		return acpTestsSelectedOption(t, response)
	}
	for _, harness := range []core.HarnessKind{core.HarnessKindCodex, core.HarnessKindClaude, core.HarnessKindDsh} {
		for _, policy := range []core.ExecutionPolicy{core.ExecutionPolicyConfiguredApprovals, core.ExecutionPolicyUnconstrained} {
			for _, role := range []core.SubagentMcpRole{
				core.SubagentMcpRoleParent,
				core.SubagentMcpRoleOrchestrator,
				core.SubagentMcpRoleMessageOnly,
				core.SubagentMcpRoleFetchOnly,
				core.SubagentMcpRoleChild,
			} {
				spec := reloadFallbackSpec(harness)
				spec.ExecutionPolicy = policy
				socket := workerSocket(role)
				socket.ReviewTools = true
				oracle := role == core.SubagentMcpRoleOrchestrator
				orchestratorsEnabled := role == core.SubagentMcpRoleParent
				socket.Oracle = oracle
				socket.OrchestratorsEnabled = orchestratorsEnabled
				spec.SubagentMCPSocket = &socket
				spec.ProjectMemory = &core.ProjectMemoryLaunchConfig{
					HistorySocket:   acpTestsStringPointer("/worker/history.sock"),
					ProjectKey:      "project",
					Root:            "/memory",
					BaselineRoot:    "/baseline",
					RepositoryRoots: map[string]string{},
					MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
				}
				// The running executable can have a different canonical path.
				spec.Command = "/canonical/worker/hel"
				spec.ExtraMCPServers = []ReviewerMcpServer{
					NewReviewerMcpServer(core.ReviewMcpServer{Name: "mj-tools", Command: "tool-server", Args: []string{}}),
					// A name resembling our namespace must not imply ownership.
					NewReviewerMcpServer(core.ReviewMcpServer{Name: "mj-third-party", Command: "analyzer", Args: []string{}}),
				}
				guardian := policy == core.ExecutionPolicyConfiguredApprovals
				// The launch's review_tools flag, not the role, decides.
				offersReview := true
				newSession, resume, load := acpTestsOpenRequests(t, &spec, true)
				for _, request := range []any{newSession, load, resume} {
					servers := acpTestsArray(acpTestsGet(request, "mcpServers"))
					if harness != core.HarnessKindClaude {
						var delegation any
						for _, server := range servers {
							if acpTestsGet(server, "name") == "mj-agents" {
								delegation = server
								break
							}
						}
						if delegation == nil {
							t.Fatalf("no delegation server: %v", request)
						}
						for _, arg := range acpTestsArray(acpTestsGet(delegation, "args")) {
							if arg == "--review-gate" || arg == "--review-server" {
								t.Fatalf("the delegation server must not serve review: %v", request)
							}
						}
						var review any
						for _, server := range servers {
							if acpTestsGet(server, "name") == "mj-review" {
								review = server
								break
							}
						}
						if (review != nil) != offersReview {
							t.Fatalf("review server present %v, want %v: %v", review != nil, offersReview, request)
						}
						if review != nil {
							var args []string
							for _, arg := range acpTestsArray(acpTestsGet(review, "args")) {
								text, _ := acpTestsString(arg)
								args = append(args, text)
							}
							if !slices.Equal(args[:2], []string{"worker", "subagent-mcp"}) {
								t.Fatalf("review args %v", args)
							}
							if !slices.Contains(args, "--socket") || !slices.Contains(args, harness.ID()) {
								t.Fatalf("review args %v", args)
							}
							if role == core.SubagentMcpRoleChild || role == core.SubagentMcpRoleFetchOnly || role == core.SubagentMcpRoleOrchestrator {
								if !slices.Equal(args[len(args)-3:], []string{"--review-server", "--role", role.ID()}) {
									t.Fatalf("review args %v", args)
								}
							} else {
								if args[len(args)-1] != "--review-server" || slices.Contains(args, "--role") {
									t.Fatalf("review args %v", args)
								}
							}
						}
					}
					ownedServers := 4
					if offersReview {
						ownedServers++
					}
					switch harness {
					case core.HarnessKindDsh:
						// DSH has no approval setting to send; the worker answers
						// its permission requests instead (below).
						if len(servers) != ownedServers {
							t.Fatalf("%d servers, want %d: %v", len(servers), ownedServers, request)
						}
						for _, server := range servers {
							if acpTestsHas(server, "_meta") {
								t.Fatalf("a DSH server carries _meta: %v", request)
							}
						}
						if acpTestsHas(request, "_meta") {
							t.Fatalf("a DSH request carries _meta: %v", request)
						}
					case core.HarnessKindCodex:
						if len(servers) != ownedServers {
							t.Fatalf("%d servers, want %d: %v", len(servers), ownedServers, request)
						}
						for _, server := range servers {
							name := acpTestsGet(server, "name")
							approve := guardian && (name == "mj-memory" || name == "mj-agents" || name == "mj-review")
							mode, present := acpTestsPointer(server, "/_meta/codex/defaultToolsApprovalMode")
							if present != approve || (approve && mode != "approve") {
								t.Fatalf("server %v approval mode %v: %v", name, mode, request)
							}
						}
					default:
						if len(servers) != 1 {
							t.Fatalf("review and delegation connections remain in the staged profile: %v", request)
						}
						allowed, present := acpTestsPointer(request, "/_meta/claudeCode/options/allowedTools")
						if guardian {
							expected := []string{"Bash(gh:*)", "mcp__mj-memory__*"}
							for _, tool := range role.ToolNames(oracle, orchestratorsEnabled) {
								expected = append(expected, "mcp__mj-agents__"+tool)
							}
							if offersReview {
								for _, tool := range []string{"request_review", "override_review", "set_baseline"} {
									expected = append(expected, "mcp__mj-review__"+tool)
								}
							}
							if !present || !acpTestsJSONEqual(t, allowed, expected) {
								t.Fatalf("allowed tools %v, want %v: %v", allowed, expected, request)
							}
						} else if present {
							t.Fatalf("yolo must not receive new allow rules: %v", request)
						}
					}
				}
				if harness == core.HarnessKindDsh {
					approvals := ownedMcpApprovalsForLaunch(&spec)
					offers := func(tool string) bool { return slices.Contains(role.ToolNames(oracle, orchestratorsEnabled), tool) }
					for _, check := range []struct {
						title string
						owned bool
					}{
						{"mcp  mj-agents  spawn worker", offers("spawn_worker")},
						{"mcp  mj-agents  spawn orchestrator", orchestratorsEnabled},
						{"mcp  mj-agents  ask oracle", oracle},
						{"mcp  mj-agents  list agents", offers("list_agents")},
						{"mcp  mj-agents  list targets", offers("list_targets")},
						{"mcp  mj-review  request review", offersReview},
						{"mcp  mj-review  override review", offersReview},
						{"mcp  mj-review  set baseline", offersReview},
						{"mcp  mj-review_  request review", false},
						{"mcp  mj-agents  handback", offers("handback")},
						{"mcp  mj-memory  trace file src/lib.rs", true},
						// A configured reviewer server is not Mjolnir's,
						// whatever its name.
						{"mcp  mj-tools  run", false},
						{"mcp  mj-third-party  scan", false},
						// `mj-agents_` only begins with an owned name.
						{"mcp  mj-agents   spawn", false},
						{"mcp  mj-agents  x  spawn", false},
						{"mcp  mj-agents  spawn  x", false},
						{"mcp  mj-agents  delete", false},
						{"mcp  mj-memory  delete file x", false},
						{"mcp  mj-agents  ", false},
						{"Run rm -rf build", false},
						{"write file mcp  mj-agents  spawn", false},
					} {
						var want any
						if guardian && check.owned {
							want = "allow_once"
						}
						if got := dshAnswer(&approvals, check.title); got != want {
							t.Fatalf("%v %v: %s: answered %v, want %v", policy, role, check.title, got, want)
						}
					}
				}
				spec.SubagentMCPSocket = nil
				if harness == core.HarnessKindDsh {
					// Without the socket this launch registers no delegation
					// server, so nothing named after it is approved.
					approvals := ownedMcpApprovalsForLaunch(&spec)
					if got := dshAnswer(&approvals, "mcp  mj-agents  spawn"); got != nil {
						t.Fatalf("spawn answered %v", got)
					}
					if got := dshAnswer(&approvals, "mcp  mj-review  request review"); got != nil {
						t.Fatalf("request review answered %v", got)
					}
					if got := dshAnswer(&approvals, "mcp  mj-memory  search sessions"); (got != nil) != guardian {
						t.Fatalf("search sessions answered %v under %v", got, policy)
					}
				}
				spec.ExtraMCPServers = nil
				withoutMemory := acpTestsPlain(t, newSessionRequest(&spec, spec.AdditionalDirectories, false, nil))
				if len(acpTestsArray(acpTestsGet(withoutMemory, "mcpServers"))) != 0 {
					t.Fatalf("no servers without history tools: %v", withoutMemory)
				}
				// The `gh` approval is session-scoped, so it is sent even when
				// no owned server or history tool needs an allow entry.
				var expected any
				if harness == core.HarnessKindClaude && guardian {
					expected = []any{"Bash(gh:*)"}
				}
				allowed, present := acpTestsPointer(withoutMemory, "/_meta/claudeCode/options/allowedTools")
				if present != (expected != nil) || (present && !acpTestsJSONEqual(t, allowed, expected)) {
					t.Fatalf("allowed tools %v, want %v: %v", allowed, expected, withoutMemory)
				}
				spec.ProjectMemory = nil
				absent := acpTestsPlain(t, newSessionRequest(&spec, spec.AdditionalDirectories, true, nil))
				if len(acpTestsArray(acpTestsGet(absent, "mcpServers"))) != 0 {
					t.Fatalf("no servers without project memory: %v", absent)
				}
				allowed, present = acpTestsPointer(absent, "/_meta/claudeCode/options/allowedTools")
				if present != (expected != nil) || (present && !acpTestsJSONEqual(t, allowed, expected)) {
					t.Fatalf("allowed tools %v, want %v: %v", allowed, expected, absent)
				}
			}
		}
	}

	for _, policy := range []core.ExecutionPolicy{core.ExecutionPolicyConfiguredApprovals, core.ExecutionPolicyUnconstrained} {
		spec := reloadFallbackSpec(core.HarnessKindClaude)
		spec.ExecutionPolicy = policy
		spec.ProjectMemory = nil
		spec.SubagentMCPSocket = nil
		spec.ExtraMCPServers = nil
		newSession, resume, load := acpTestsOpenRequests(t, &spec, true)
		for _, opened := range []struct {
			method  string
			request any
		}{
			{"session/new", newSession},
			{"session/load", load},
			{"session/resume", resume},
		} {
			// The protocol's optional field is serialized only when nonempty;
			// no MCP servers is represented by either omission or an empty list.
			if acpTestsHas(opened.request, "mcpServers") {
				servers, isArray := acpTestsGet(opened.request, "mcpServers").([]any)
				if !isArray || len(servers) != 0 {
					t.Fatalf("%s unexpectedly registered MCP servers: %v", opened.method, opened.request)
				}
			}
			allowed, present := acpTestsPointer(opened.request, "/_meta/claudeCode/options/allowedTools")
			if policy == core.ExecutionPolicyConfiguredApprovals {
				if !present || !acpTestsJSONEqual(t, allowed, []any{"Bash(gh:*)"}) {
					t.Fatalf("%s: %v", opened.method, opened.request)
				}
			} else if present {
				t.Fatalf("YOLO must not receive native allow rules for %s: %v", opened.method, opened.request)
			}
		}
	}
}

// Launch finding I2-1: a new Codex session prompted "Reply with the single
// word pong." was titled "Project memory instructions". codex-acp titles a
// thread from the text blocks of its first prompt, and Mjolnir sent the
// project memory as the first of them. The memory now travels as an embedded
// resource: the model still reads all of it, and the title comes from the
// user's words.
func TestACodexSessionGivenProjectMemoryIsTitledFromTheUsersPrompt(t *testing.T) {
	temp := t.TempDir()
	store := core.NewProjectMemoryStore(filepath.Join(temp, "memory"))
	memory, err := core.StartupPromptContext(&store, map[string]string{})
	if err != nil {
		t.Fatal(err)
	}
	request := "Reply with the single word pong."

	received, titles := codexSessionTitles(t, &temp, []schema.ContentBlock{
		hiddenContextBlock(memory),
		acpTestsText(request),
	})

	if !slices.Equal(titles, []string{request}) {
		t.Fatalf("titles %q, want only the user's request", titles)
	}
	expected := []any{
		map[string]any{
			"type": "resource",
			"resource": map[string]any{
				"uri":  core.HiddenPromptContextURI,
				"text": memory,
			},
		},
		map[string]any{"type": "text", "text": request},
	}
	if !acpTestsJSONEqual(t, received, expected) {
		t.Fatalf("the bridge received %v, want %v", received, expected)
	}
}

// Only Codex receives the hidden context as a resource. Every other harness
// gets the same text block as before; its bridge may not accept embedded
// resources at all.
func TestOnlyCodexReceivesHiddenContextAsAResource(t *testing.T) {
	context := "<mj-project-memory>notes</mj-project-memory>"
	user := acpTestsText("ship it")
	prompt := []schema.ContentBlock{hiddenContextBlock(context), user}

	codex := promptForHarness(core.HarnessKindCodex, slices.Clone(prompt))
	if codex[0].Resource == nil {
		t.Fatalf("Codex receives the context as a resource: %#v", codex)
	}
	if !acpTestsJSONEqual(t, codex[1], user) {
		t.Fatalf("the user's block is unchanged: %#v", codex[1])
	}
	for _, harness := range []core.HarnessKind{core.HarnessKindClaude, core.HarnessKindGrok, core.HarnessKindKimi, core.HarnessKindMuse} {
		got := promptForHarness(harness, slices.Clone(prompt))
		if want := []schema.ContentBlock{acpTestsText(context), user}; !acpTestsJSONEqual(t, got, want) {
			t.Fatalf("%v: %#v", harness, got)
		}
	}
	for _, harness := range core.HarnessKindAll {
		got := promptForHarness(harness, []schema.ContentBlock{user})
		if !acpTestsJSONEqual(t, got, []schema.ContentBlock{user}) {
			t.Fatalf("%v: %#v", harness, got)
		}
	}
}

// Hard-won: c823ba502: metadata-only live terminal output was discarded before reaching the TUI.
func TestOnlyUpdatesForToolCallsCreatedOnTheLiveConnectionAreRelayed(t *testing.T) {
	liveToolCalls := NewShared(map[string]struct{}{})
	metadataOnly := acpTestsDecode[schema.SessionUpdate](t,
		`{"sessionUpdate":"tool_call_update","toolCallId":"old-tool","_meta":{"terminal_output_delta":{"data":"replayed output"}}}`)
	if sessionUpdateIsRelayVisible(&metadataOnly, liveToolCalls, "session-1") {
		t.Fatal("metadata of a replayed tool call is not live")
	}

	delayed := acpTestsDecode[schema.SessionUpdate](t,
		`{"sessionUpdate":"tool_call_update","toolCallId":"old-tool","title":"updated"}`)
	if sessionUpdateIsRelayVisible(&delayed, liveToolCalls, "session-1") {
		t.Fatal("a delayed update of a replayed tool call is not live")
	}

	created := acpTestsDecode[schema.SessionUpdate](t, `{"sessionUpdate":"tool_call","toolCallId":"live-tool","title":"read"}`)
	if !sessionUpdateIsRelayVisible(&created, liveToolCalls, "session-1") {
		t.Fatal("a tool call created on the live connection is relayed")
	}
	visible := acpTestsDecode[schema.SessionUpdate](t,
		`{"sessionUpdate":"tool_call_update","toolCallId":"live-tool","title":"updated"}`)
	if !sessionUpdateIsRelayVisible(&visible, liveToolCalls, "session-1") {
		t.Fatal("an update of a live tool call is relayed")
	}

	for _, key := range []string{"terminal_output", "terminal_output_delta", "terminal_exit", "private"} {
		update := acpTestsDecode[schema.SessionUpdate](t, string(acpTestsMust(t)(json.Marshal(map[string]any{
			"sessionUpdate": "tool_call_update",
			"toolCallId":    "live-tool",
			"_meta": map[string]any{key: map[string]any{
				"terminal_id": "exec-1", "data": "command failed", "exit_code": 1,
			}},
		}))))
		if got := sessionUpdateIsRelayVisible(&update, liveToolCalls, "session-1"); got != (key != "private") {
			t.Fatalf("%s: visible %v", key, got)
		}
		if sessionUpdateIsRelayVisible(&metadataOnly, liveToolCalls, "session-1") {
			t.Fatal("metadata of a replayed tool call is not live")
		}
	}
}

// Suppression follows the stored policy, including None without an MCP socket.
func TestNativeDelegationFollowsPolicyIndependentlyOfTheMcpSocket(t *testing.T) {
	spec := acpTestsSpec(core.HarnessKindClaude, "/worker/mj", "/workspace/app")

	for _, harness := range []core.HarnessKind{core.HarnessKindClaude, core.HarnessKindCodex} {
		spec.Harness = harness

		spec.SubagentPolicy = core.SubagentPolicy{Mode: core.SubagentPolicyNative}
		spec.SubagentMCPSocket = nil
		meta := acpTestsPlain(t, sessionRequestMeta(&spec, true))
		if _, present := acpTestsPointer(meta, "/claudeCode/options/disallowedTools"); present {
			t.Fatalf("%v without a socket must keep its native tools: %v", harness, meta)
		}
		if _, present := acpTestsPointer(meta, "/codex/options/disallowedTools"); present {
			t.Fatalf("%v without a socket must keep its native tools: %v", harness, meta)
		}

		spec.SubagentPolicy = core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}
		socket := workerSocket(core.SubagentMcpRoleParent)
		spec.SubagentMCPSocket = &socket
		meta = acpTestsPlain(t, sessionRequestMeta(&spec, true))
		path := "/codex/options/disallowedTools"
		expected := []any{"spawn_agent"}
		if harness == core.HarnessKindClaude {
			path = "/claudeCode/options/disallowedTools"
			expected = []any{"Agent", "Task", "ListAgents", "SendMessage", "Workflow"}
		}
		if hidden, present := acpTestsPointer(meta, path); !present || !acpTestsJSONEqual(t, hidden, expected) {
			t.Fatalf("%v: %v", harness, meta)
		}
		for _, policy := range []core.SubagentPolicy{
			{Mode: core.SubagentPolicyNone},
			{Mode: core.SubagentPolicyMjolnir},
		} {
			spec.SubagentPolicy = policy
			spec.SubagentMCPSocket = nil
			meta := acpTestsPlain(t, sessionRequestMeta(&spec, true))
			if hidden, present := acpTestsPointer(meta, path); !present || !acpTestsJSONEqual(t, hidden, expected) {
				t.Fatalf("%v under %v: %v", harness, policy.Mode, meta)
			}
			if servers := extraMCP(&spec); len(servers) != 0 {
				t.Fatalf("no extra MCP without a socket: %#v", servers)
			}
		}
	}
}

func TestProjectHistoryMcpAndNonClaudeFileMemoryAreDelivered(t *testing.T) {
	spec := acpTestsSpec(core.HarnessKindCodex, "/worker/hel", "/workspace/app")
	spec.AdditionalDirectories = []string{"/workspace/api", "/profile/projects/abc/memory"}
	spec.ProjectMemory = &core.ProjectMemoryLaunchConfig{
		ProjectKey:   "abc",
		Root:         "/profile/projects/abc/memory",
		BaselineRoot: "/profile/projects/abc/.hel-memory-baseline",
		RepositoryRoots: map[string]string{
			"app": "/workspace/app",
			"api": "/workspace/api",
		},
		MCPDelivery: core.ProjectMemoryMcpDeliveryAcp,
	}
	onlyStdio := func(servers []schema.McpServer, what string) *schema.McpServerStdio {
		t.Helper()
		if len(servers) != 1 || servers[0].Stdio == nil {
			t.Fatalf("%s: %#v", what, servers)
		}
		return servers[0].Stdio
	}
	server := onlyStdio(projectHistoryMCP(&spec), "non-Claude sessions receive exactly one memory MCP server")
	if server.Name != "mj-memory" {
		t.Fatalf("server name %q", server.Name)
	}
	if server.Command != "/worker/hel" {
		t.Fatalf("server command %q", server.Command)
	}
	if !slices.Equal(server.Args, []string{"worker", "memory-mcp"}) {
		t.Fatalf("server args %q", server.Args)
	}
	request := acpTestsPlain(t, newSessionRequest(&spec, spec.AdditionalDirectories, true, nil))
	if !acpTestsJSONEqual(t, acpTestsGet(request, "additionalDirectories"), []any{"/workspace/api", "/profile/projects/abc/memory"}) {
		t.Fatalf("additional directories %v", acpTestsGet(request, "additionalDirectories"))
	}
	if slices.ContainsFunc(server.Args, func(argument string) bool { return strings.Contains(argument, "store") }) {
		t.Fatal("the model-facing service must not expose store selection")
	}

	spec.ProjectMemory.MCPDelivery = core.ProjectMemoryMcpDeliveryHarnessProfile
	if servers := projectHistoryMCP(&spec); len(servers) != 0 {
		t.Fatalf("a profile-delivered server is not sent over ACP: %#v", servers)
	}
	spec.ProjectMemory.MCPDelivery = core.ProjectMemoryMcpDeliveryAcp

	claude := spec
	claude.Harness = core.HarnessKindClaude
	if servers := projectHistoryMCP(&claude); len(servers) != 0 {
		t.Fatalf("Claude without a history socket: %#v", servers)
	}
	claude.ProjectMemory.HistorySocket = acpTestsStringPointer("/worker/control.sock")
	server = onlyStdio(projectHistoryMCP(&claude), "Claude receives history tools")
	if slices.Contains(server.Args, "--root") || !slices.Contains(server.Args, "/worker/control.sock") {
		t.Fatalf("Claude history args %q", server.Args)
	}
	claude.Harness = core.HarnessKindCodex
	server = onlyStdio(projectHistoryMCP(&claude), "Codex receives history")
	if slices.Contains(server.Args, "--root") {
		t.Fatalf("Codex history args %q", server.Args)
	}
	claude.Harness = core.HarnessKindMuse
	server = onlyStdio(projectHistoryMCP(&claude), "Muse receives history")
	if slices.Contains(server.Args, "--root") {
		t.Fatalf("Muse history args %q", server.Args)
	}
}

// Children retain handback while native delegation is disabled.
func TestAChildSocketServesHandbackAndHidesNativeTools(t *testing.T) {
	spec := acpTestsSpec(core.HarnessKindCodex, "codex-acp", "/workspace/app")
	spec.SubagentPolicy = core.SubagentPolicy{Mode: core.SubagentPolicyNone}
	socket := workerSocket(core.SubagentMcpRoleChild)
	spec.SubagentMCPSocket = &socket
	spec.Verdict = nil
	for _, harness := range []core.HarnessKind{core.HarnessKindClaude, core.HarnessKindCodex} {
		spec.Harness = harness
		meta := acpTestsPlain(t, sessionRequestMeta(&spec, true))
		pointer, expected := "/codex/options/disallowedTools", []any{"spawn_agent"}
		if harness == core.HarnessKindClaude {
			pointer = "/claudeCode/options/disallowedTools"
			expected = []any{"Agent", "Task", "ListAgents", "SendMessage", "Workflow"}
		}
		if hidden, present := acpTestsPointer(meta, pointer); !present || !acpTestsJSONEqual(t, hidden, expected) {
			t.Fatalf("%v: %v", harness, meta)
		}
	}
	spec.Harness = core.HarnessKindCodex
	servers := extraMCP(&spec)
	if len(servers) != 1 || servers[0].Stdio == nil {
		t.Fatalf("a Codex child receives the Mjolnir sub-agent MCP server: %#v", servers)
	}
	server := servers[0].Stdio
	if server.Name != "mj-agents" {
		t.Fatalf("server name %q", server.Name)
	}
	args := server.Args
	if len(args) < 2 || args[len(args)-1] != "child" || args[len(args)-2] != "--role" {
		t.Fatalf("a child's server names its role last: %q", args)
	}
}

func TestUnconstrainedPermissionPrefersAOneTimeAllowAndNeverCancels(t *testing.T) {
	request := acpTestsPermissionRequest(t, "session-1",
		map[string]any{"toolCallId": "tool-1", "title": "Run command"},
		acpTestsOption("abort", "Abort", "reject_once"),
		acpTestsOption("always", "Always allow", "allow_always"),
		acpTestsOption("allow_once", "Allow once", "allow_once"))

	response := allowPermissionResponse(&request)
	if response == nil {
		t.Fatal("an allow option is selected")
	}
	if got := acpTestsSelectedOption(t, response); got != "allow_once" {
		t.Fatalf("selected %v, want allow_once", got)
	}

	rejectedOnly := acpTestsPermissionRequest(t, "session-1",
		map[string]any{"toolCallId": "tool-2"},
		acpTestsOption("abort", "Abort", "reject_once"))
	if response := allowPermissionResponse(&rejectedOnly); response != nil {
		t.Fatalf("nothing to allow, nothing selected: %#v", response)
	}
}

// Hard-won: a1d7b48: Compaction failures were reported as finished turns, causing clients to resend unproductive prompts.
func TestOnlyAFinishedTurnThatProducedNothingCountsAsUnanswered(t *testing.T) {
	if !promptReturnedWithoutUpdates(schema.StopReasonEndTurn, 7, 7) {
		t.Fatal("a finished turn with no output is unanswered")
	}
	if promptReturnedWithoutUpdates(schema.StopReasonEndTurn, 7, 8) {
		t.Fatal("a finished turn with output is answered")
	}
	if promptReturnedWithoutUpdates(schema.StopReasonCancelled, 7, 7) {
		t.Fatal("a cancelled turn keeps its own stop reason")
	}
	// A turn that ended for a reason of its own already reports that reason.
	// Relabelling it "unanswered" would hide why it really ended.
	for _, stopReason := range []schema.StopReason{schema.StopReasonMaxTokens, schema.StopReasonMaxTurnRequests, schema.StopReasonRefusal} {
		if promptReturnedWithoutUpdates(stopReason, 7, 7) {
			t.Fatalf("%v must keep its own stop reason", stopReason)
		}
	}
}

func TestGrokPlanReviewAnswersAreUserSelected(t *testing.T) {
	review := core.NormalizedPlanReview("plan-review-grok-1", acpTestsJV(t, map[string]any{"plan_content": "Do nothing"}))
	encoded, err := json.Marshal(review)
	if err != nil {
		t.Fatal(err)
	}
	var decoded core.ElicitationRequest
	if err := json.Unmarshal(encoded, &decoded); err != nil {
		t.Fatal(err)
	}
	reencoded, err := json.Marshal(decoded)
	if err != nil {
		t.Fatal(err)
	}
	if string(reencoded) != string(encoded) {
		t.Fatalf("normalized reviews must survive the durable relay journal: %s became %s", encoded, reencoded)
	}
	if field := review.Fields[1]; field.CustomAnswerFor == nil || *field.CustomAnswerFor != "action" {
		t.Fatalf("custom answer for %v", field.CustomAnswerFor)
	}
	if field := review.Fields[1]; field.CustomAnswerOption == nil || *field.CustomAnswerOption != "revise" {
		t.Fatalf("custom answer option %v", field.CustomAnswerOption)
	}
	response := core.GrokPlanResponse(core.ElicitationResponse{
		Action:  core.ElicitationResponseAccept,
		Content: map[string]core.ElicitationValue{core.PlanReviewAction: core.ElicitationValueString("implement")},
	})
	var answer any
	if err := json.Unmarshal(response, &answer); err != nil {
		t.Fatal(err)
	}
	if !acpTestsJSONEqual(t, answer, map[string]any{"outcome": "approved"}) {
		t.Fatalf("Grok's plan answer %s", response)
	}
}

func TestSupportedPermissionPlanReviewsAreDetectedAndMappedToNativeOptions(t *testing.T) {
	for _, fixture := range []struct{ title, approvalID string }{
		{"Implement this plan?", "IMPLEMENT_PLAN_OPTION_ID"},
		{"ExitPlanMode", "default"},
		{"Review plan", "plan_approve"},
	} {
		request := acpTestsPermissionRequest(t, "session-1",
			map[string]any{"toolCallId": "tool-1", "title": fixture.title, "rawInput": map[string]any{"plan": "Do the work"}},
			acpTestsOption(fixture.approvalID, "Approve", "allow_once"),
			acpTestsOption("plan_revise", "Revise", "reject_once"))
		if !isPlanPermission(&request) {
			t.Fatalf("fixture %s", fixture.title)
		}
		review := core.NormalizedPlanReview("plan-review-1", acpTestsJV(t, request))
		if !strings.Contains(review.Message, "Do the work") {
			t.Fatalf("review message %q", review.Message)
		}
		response := permissionPlanResponse(&request, core.ElicitationResponse{
			Action:  core.ElicitationResponseAccept,
			Content: map[string]core.ElicitationValue{core.PlanReviewAction: core.ElicitationValueString("implement")},
		})
		if got := acpTestsSelectedOption(t, response); got != fixture.approvalID {
			t.Fatalf("%s: selected %v, want %s", fixture.title, got, fixture.approvalID)
		}
	}
}

// The exact `session/request_permission` Claude Code's ACP bridge sends when
// ExitPlanMode fires: a `switch_mode` tool call carrying the plan and a
// `planFilePath`, titled "Ready to code?", with generic permission-mode option
// ids. Captured from a live worker.log. None of the title/option heuristics
// match it, so the classifier must key on the tool kind and plan payload.
func TestClaudeExitPlanModeRequestIsDetectedAndMapped(t *testing.T) {
	request := acpTestsDecode[schema.RequestPermissionRequest](t, `{
        "sessionId": "7c4ce22d-9b3a-421b-b9f9-f2b8d3b73bb8",
        "toolCall": {
            "toolCallId": "toolu_01L8aaLyXndAiFoQM9yhpsRy",
            "kind": "switch_mode",
            "title": "Ready to code?",
            "content": [{
                "type": "content",
                "content": {"type": "text", "text": "# Add --version flag\n\nDo the work."}
            }],
            "rawInput": {
                "plan": "# Add --version flag\n\nDo the work.",
                "planFilePath": "/home/jonathan/.claude/plans/add-a-version-flag.md"
            }
        },
        "options": [
            {"optionId": "bypassPermissions", "name": "Yes, and bypass permissions", "kind": "allow_always"},
            {"optionId": "auto", "name": "Yes, and use \"auto\" mode", "kind": "allow_always"},
            {"optionId": "acceptEdits", "name": "Yes, and auto-accept edits", "kind": "allow_always"},
            {"optionId": "default", "name": "Yes, and manually approve edits", "kind": "allow_once"},
            {"optionId": "plan", "name": "No, keep planning", "kind": "reject_once"}
        ]
    }`)

	if !isPlanPermission(&request) {
		t.Fatal("Claude's switch_mode request must be classified as a plan review")
	}

	review := core.NormalizedPlanReview("plan-review-1", acpTestsJV(t, request))
	if !strings.Contains(review.Message, "Add --version flag") {
		t.Fatalf("review message %q", review.Message)
	}

	// Guardian implementations use Auto rather than manual edit approvals.
	implement := core.ElicitationResponse{
		Action:  core.ElicitationResponseAccept,
		Content: map[string]core.ElicitationValue{core.PlanReviewAction: core.ElicitationValueString("implement")},
	}
	var enforcement *core.ExecutionEnforcement
	if value, ok := core.HarnessKindClaude.ExecutionEnforcement(core.ExecutionPolicyConfiguredApprovals); ok {
		enforcement = &value
	}
	answer, err := policyPlanPermissionAnswer(&request, implement, core.HarnessKindClaude, enforcement)
	if err != nil {
		t.Fatal(err)
	}
	approved, ok := answer.(PlanPermissionAnswerNative)
	if !ok {
		t.Fatalf("Auto is offered by this bridge: %#v", answer)
	}
	if got := acpTestsSelectedOption(t, approved.Value); got != "auto" {
		t.Fatalf("selected %v, want auto", got)
	}

	// Declining keeps planning by selecting the reject option instead of
	// cancelling the turn.
	declined := permissionPlanResponse(&request, core.ElicitationResponse{
		Action:  core.ElicitationResponseAccept,
		Content: map[string]core.ElicitationValue{core.PlanReviewAction: core.ElicitationValueString("keep_planning")},
	})
	if got := acpTestsSelectedOption(t, declined); got != "plan" {
		t.Fatalf("selected %v, want plan", got)
	}
}

// Hard-won: 7c4a57a: Reviewers saw only “Bash” in Kimi permission forms, not the command being approved.
func TestAPermissionFormShowsTheCommandItApproves(t *testing.T) {
	// I2-5: Kimi's reviewer asked "requests permission: Bash" with no command.
	if got := permissionTitleWithCommand("Bash", acpTestsJV(t, map[string]any{"command": "ls -la"})); got != "Bash\n$ ls -la" {
		t.Fatalf("%q", got)
	}
	if got := permissionTitleWithCommand("Shell", acpTestsJV(t, map[string]any{"command": []any{"git", "status"}})); got != "Shell\n$ git status" {
		t.Fatalf("%q", got)
	}
	// A title that already names the command is not repeated.
	if got := permissionTitleWithCommand("Run ls -la", acpTestsJV(t, map[string]any{"command": "ls -la"})); got != "Run ls -la" {
		t.Fatalf("%q", got)
	}
	if got := permissionTitleWithCommand("Edit a.py", nil); got != "Edit a.py" {
		t.Fatalf("%q", got)
	}
}

func TestEchoedUserImagesDoNotReenterTheRelayJournal(t *testing.T) {
	update := acpTestsDecode[schema.SessionUpdate](t, string(acpTestsMust(t)(json.Marshal(map[string]any{
		"sessionUpdate": "user_message_chunk",
		"content":       map[string]any{"type": "image", "data": strings.Repeat("x", 700*1024), "mimeType": "image/png"},
	}))))
	if sessionUpdateIsRelayVisible(&update, NewShared(map[string]struct{}{}), "session-1") {
		t.Fatal("an echoed user image must not reach the relay")
	}
}

// Hard-won: be5abcc: An invalid ACP tool status left the UI card unsettled indefinitely.
func TestAnOutOfSpecToolStatusIsCoercedToFailedSoTheCardSettles(t *testing.T) {
	// Muse's adapter can send an ACP-illegal `cancelled` status; the whole
	// notification used to be dropped, stranding the tool card in_progress.
	update := acpTestsJV(t, map[string]any{
		"sessionUpdate": "tool_call_update",
		"toolCallId":    "item-1",
		"status":        "cancelled",
	})
	replaced, ok := coerceToolCallStatus(&update)
	if !ok || replaced != "cancelled" {
		t.Fatalf("replaced %q, %v", replaced, ok)
	}
	if status := acpTestsGet(acpTestsPlain(t, update), "status"); status != "failed" {
		t.Fatalf("status %v, want failed", status)
	}
	// And it now parses into a real v1 update rather than being discarded.
	acpTestsDecode[schema.SessionUpdate](t, string(acpTestsMust(t)(json.Marshal(update))))
}

func TestSalvageSettlesANamedToolAndIgnoresTheRest(t *testing.T) {
	// When a tool update cannot be represented at all, we still settle the
	// named tool as failed rather than drop it and strand the card.
	raw := acpTestsJV(t, map[string]any{
		"sessionUpdate": "tool_call_update",
		"toolCallId":    "item-2",
		"status":        "completed",
		"content":       []any{map[string]any{"type": "from_the_future", "payload": 7}},
	})
	salvaged := salvageToolCallUpdate(raw)
	if salvaged == nil {
		t.Fatal("a named tool update is salvageable")
	}
	if salvaged.ToolCallUpdate == nil {
		t.Fatalf("expected a tool_call_update, got %#v", salvaged)
	}
	if salvaged.ToolCallUpdate.ToolCallID != "item-2" {
		t.Fatalf("salvaged tool %q", salvaged.ToolCallUpdate.ToolCallID)
	}

	// Without a tool id there is nothing to settle, and a non-tool update is
	// left for the caller to drop.
	if salvageToolCallUpdate(acpTestsJV(t, map[string]any{"sessionUpdate": "tool_call_update", "status": "completed"})) != nil {
		t.Fatal("an update without a tool id is not salvageable")
	}
	if salvageToolCallUpdate(acpTestsJV(t, map[string]any{"sessionUpdate": "agent_message_chunk"})) != nil {
		t.Fatal("a non-tool update is not salvageable")
	}
}

// Hard-won: be5abcc: Stalled turns lacked a clear cause and recovery action for the user.
func TestTheStallMessageSaysWhatHappenedAndWhatToDo(t *testing.T) {
	silent := turnStallMessage(core.HarnessKindMuse, core.StallVerdictSilent{SilentMs: 630_000})
	if !strings.Contains(silent, "stopped responding") {
		t.Fatal(silent)
	}
	if !strings.Contains(silent, "10 minute") {
		t.Fatalf("reports the silence in minutes: %s", silent)
	}
	if !strings.Contains(silent, "no tool call was open") {
		t.Fatalf("says why the short bound applied: %s", silent)
	}
	if !strings.Contains(silent, "Resend") {
		t.Fatalf("tells the user how to continue: %s", silent)
	}
	if !strings.Contains(silent, "#1007") {
		t.Fatalf("names the known issue: %s", silent)
	}

	// A turn ended for one long tool call must name the call, how long it
	// ran, and the knob that raises or removes the limit.
	tool := turnStallMessage(core.HarnessKindMuse, core.StallVerdictToolCall{
		ToolCallID: "job_output-7",
		RunningMs:  14_460_000,
		SilentMs:   14_400_000,
	})
	if !strings.Contains(tool, "job_output-7") {
		t.Fatalf("names the tool call: %s", tool)
	}
	if !strings.Contains(tool, "241 minute") {
		t.Fatalf("how long it ran: %s", tool)
	}
	if !strings.Contains(tool, "MJ_TURN_TOOL_STALL_TIMEOUT_MS") {
		t.Fatalf("names the knob: %s", tool)
	}
}

// The message tells the user which variable raises the limit, and the code
// has to read that same variable. It did not: the lookup said
// `MJ_TURN_TOOL_CALL_TIMEOUT_MS` while the message, the documentation and the
// daemon's passthrough all said `MJ_TURN_TOOL_STALL_TIMEOUT_MS`, so setting
// the documented variable did nothing and the bound was always the four-hour
// default. Nothing caught it because every test asserted on the message.
// Hard-won: db49943: The documented timeout variable was ignored, leaving tool calls with a four-hour bound.
func TestTheToolCallBoundReadsTheVariableItsMessageAdvertises(t *testing.T) {
	message := turnStallMessage(core.HarnessKindMuse, core.StallVerdictToolCall{
		ToolCallID: "bash-1",
		RunningMs:  1,
		SilentMs:   1,
	})
	if !strings.Contains(message, toolCallStallTimeoutVariable) {
		t.Fatalf("the message names the variable the code reads: %s", message)
	}

	// And the daemon carries that same name to the workers it starts, which
	// is the only way it reaches a worker at all. Rust reads
	// mj-worker-staging/src/worker_binary/launch.rs; the Go daemon's copy is
	// that module's port.
	carried, err := os.ReadFile(filepath.Join("..", "workerstaging", "worker_binary_launch.go"))
	if errors.Is(err, os.ErrNotExist) {
		t.Skip("needs mj-worker-staging (not yet ported)")
	}
	if err != nil {
		t.Fatalf("read the launch configuration that carries the knob: %v", err)
	}
	if !strings.Contains(string(carried), toolCallStallTimeoutVariable) {
		t.Fatal("the daemon carries the variable the worker reads")
	}
}

// Hard-won: 710ffb6: Codex thread-not-found errors were mislabeled as stray ACP output.
func TestAnAgentErrorIsNotBlamedOnStrayBridgeOutput(t *testing.T) {
	// Launch finding I2-7: a missing Codex thread came back with a hint about
	// login-shell output that had nothing to do with it.
	agent := protocolFailure(rpcErrorWithData(rpcCodeInternalError, "Internal error", "thread not found: t1"), false).Error()
	if !strings.Contains(agent, "thread not found") || strings.Contains(agent, "bridge stdout") {
		t.Fatal(agent)
	}
	// Output the transport could not parse, or a connection that broke, is
	// what the hint is for.
	garbage := protocolFailure(rpcErrorWithData(rpcCodeParseError, "Parse error", "x"), false).Error()
	if !strings.Contains(garbage, "bridge stdout must contain only JSON-RPC frames") {
		t.Fatal(garbage)
	}
	closed := protocolFailure(acpTestsRPCError(rpcCodeInternalError, "Internal error", nil), true).Error()
	if !strings.Contains(closed, "bridge stdout must contain only JSON-RPC frames") {
		t.Fatal(closed)
	}
}

// R8-2: the exit record's first line names why the worker stopped. The
// bridge's stderr tail used to come first, so a resume that failed on a
// refused mode showed "ACP bridge stderr:" and the bridge's log lines.
// Hard-won: e794b25: Resume errors stored a 96-line diagnostics dump instead of the cause.
func TestAWorkerExitReasonNamesTheCauseBeforeTheBridgeStderr(t *testing.T) {
	err := fmt.Errorf("%s\n[session/create] phase=models durationMs=1\n[session/create] phase=register durationMs=1: %w",
		bridgeStderrContext,
		fmt.Errorf("select required ACP execution mode auto: %w",
			errors.New("Internal error: {\n  \"details\": \"Cannot set permission mode to auto: auto mode unavailable for this model\"\n}")))

	reason := WorkerExitReason(err)
	lines := strings.Split(reason, "\n")
	if want := "select required ACP execution mode auto: Internal error: { \"details\": \"Cannot " +
		"set permission mode to auto: auto mode unavailable for this model\" }"; lines[0] != want {
		t.Fatalf("first line %q, want %q", lines[0], want)
	}
	if len(lines) < 2 || lines[1] != bridgeStderrContext {
		t.Fatalf("second line %q", lines)
	}
	if !strings.HasSuffix(reason, "[session/create] phase=register durationMs=1") {
		t.Fatalf("reason %q", reason)
	}

	// The marker supplies the first line even when cleanup also reported
	// an error. The bridge stderr remains available below that cause.
	err = fmt.Errorf("ACP cleanup also failed: %w", fmt.Errorf("%s\nbridge diagnostic: %w",
		bridgeStderrContext, core.WorkerHarnessStartupFailure{Message: "load ACP session failed"}))
	if got, want := WorkerExitReason(err), "load ACP session failed\n"+bridgeStderrContext+"\nbridge diagnostic"; got != want {
		t.Fatalf("reason %q, want %q", got, want)
	}

	// A failure with no stderr is the chain on one line.
	err = fmt.Errorf("start worker: %w", errors.New("durable relay open failed"))
	if got := WorkerExitReason(err); got != "start worker: durable relay open failed" {
		t.Fatalf("reason %q", got)
	}
}
