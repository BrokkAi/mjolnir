// Port of the tests in mj-worker-mcp/src/subagent_mcp.rs that run on every
// platform. The ones that serve a fake worker socket are in
// subagent_mcp_unix_test.go.

package workermcp

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

var subagentParentWithoutOracle = ParentTools{Oracle: false, SubtleSubagents: true, OrchestratorsEnabled: false}

var subagentStraightforwardOnly = ParentTools{Oracle: false, SubtleSubagents: false, OrchestratorsEnabled: false}

// subagentValue is serde_json's Value indexing: each step takes an object member by
// string key or an array element by int index; a missing step is nil.
func subagentValue(value any, path ...any) any {
	for _, step := range path {
		switch step := step.(type) {
		case string:
			value, _ = subagentMCPGet(value, step)
		case int:
			items, ok := value.([]any)
			if !ok || step >= len(items) {
				return nil
			}
			value = items[step]
		}
	}
	return value
}

// subagentHas is `value[...].get(key).is_some()`.
func subagentHas(value any, path ...any) bool {
	parent := subagentValue(value, path[:len(path)-1]...)
	_, ok := subagentMCPGet(parent, path[len(path)-1].(string))
	return ok
}

func subagentText(value any, path ...any) string {
	text, _ := subagentValue(value, path...).(string)
	return text
}

// subagentEqualJSON compares a value with JSON text, as serde_json's Value equality.
func subagentEqualJSON(value any, text string) bool {
	want, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		panic(err)
	}
	return jsonvalue.Equal(value, want)
}

// subagentToolDefinitions is Rust's test-only tool_definitions: a parent's
// tools with the default options.
func subagentToolDefinitions() []jsonvalue.Object {
	return toolDefinitionsWithOptions(nil, DefaultParentTools())
}

// toolDefinitionsWithOptions is Rust's test-only tool_definitions_with_options:
// a parent's tools.
func toolDefinitionsWithOptions(_ *core.HarnessKind, options ParentTools) []jsonvalue.Object {
	return roleToolDefinitions(core.SubagentMcpRoleParent, options)
}

// childToolDefinitions is Rust's test-only child_tool_definitions.
func childToolDefinitions() []jsonvalue.Object {
	return roleToolDefinitions(core.SubagentMcpRoleChild, DefaultParentTools())
}

func subagentToolNamed(tools []jsonvalue.Object, name string) jsonvalue.Object {
	for _, tool := range tools {
		if subagentText(tool, "name") == name {
			return tool
		}
	}
	return nil
}

func subagentToolNames(tools []jsonvalue.Object) []string {
	var names []string
	for _, tool := range tools {
		names = append(names, subagentText(tool, "name"))
	}
	return names
}

func subagentSpawnDefinition(options ParentTools) jsonvalue.Object {
	return subagentToolNamed(toolDefinitionsWithOptions(nil, options), "spawn_worker")
}

// subagentCall is Rust's test-only call: one call with the default parent
// tools and their reply budgets.
func subagentCall(socket string, harness *core.HarnessKind, role core.SubagentMcpRole, params string, progress *Progress) (any, bool, error) {
	return callWithOptions(socket, harness, role, json.RawMessage(params), progress, DefaultParentTools())
}

// callWithOptions is Rust's test-only call_with_options: one call that reads
// the report budget a child or orchestrator has from the launch file, as
// the server does.
func callWithOptions(socket string, harness *core.HarnessKind, role core.SubagentMcpRole, params json.RawMessage, progress *Progress, options ParentTools) (any, bool, error) {
	launch, hasLaunch, err := readLaunchSettings(socket)
	if err != nil {
		return nil, false, err
	}
	var reportBudget *childReportBudget
	if hasLaunch && (role == core.SubagentMcpRoleChild || role == core.SubagentMcpRoleOrchestrator) {
		if reportBudget, err = childReportBudgetFromLaunch(launch); err != nil {
			return nil, false, err
		}
	}
	budget := func(action core.SubagentToolAction) time.Duration { return replyTimeoutFor(action, harness) }
	return callWithBudgetAndOptions(socket, role, params, progress, budget, options, reportBudget)
}

// subagentCallWithBudget is Rust's test-only call_with_budget.
func subagentCallWithBudget(socket string, _ *core.HarnessKind, role core.SubagentMcpRole, params string, progress *Progress, budget time.Duration) (any, bool, error) {
	return callWithBudgetAndOptions(socket, role, json.RawMessage(params), progress,
		func(core.SubagentToolAction) time.Duration { return budget }, DefaultParentTools(), nil)
}

// testCallWithOptions is the tests' test_call_with_options: one call with
// options and no report budget.
func testCallWithOptions(socket string, _ *core.HarnessKind, role core.SubagentMcpRole, params string, progress *Progress, budget time.Duration, options ParentTools) (any, bool, error) {
	return callWithBudgetAndOptions(socket, role, json.RawMessage(params), progress,
		func(core.SubagentToolAction) time.Duration { return budget }, options, nil)
}

// subagentRun is Rust's test-only run: the server with the default parent
// tools.
func subagentRun(reader io.Reader, writer io.Writer, socket string, harness *core.HarnessKind, role core.SubagentMcpRole) error {
	return runWithOptions(reader, writer, socket, harness, role, DefaultParentTools())
}

// subagentContractWriter collects a server's output, which the server writes from
// several goroutines.
type subagentContractWriter struct {
	mu      sync.Mutex
	written bytes.Buffer
}

func (w *subagentContractWriter) Write(data []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.written.Write(data)
}

func (w *subagentContractWriter) String() string {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.written.String()
}

const subagentInitializeAndList = `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}` + "\n" +
	`{"jsonrpc":"2.0","id":2,"method":"tools/list"}` + "\n"

// subagentLines is Rust's str::lines: lines split at \n with a trailing \r
// removed, and no empty last line for a final newline.
func subagentLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for i, line := range lines {
		lines[i] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

// subagentReplies parses every output line.
func subagentReplies(t *testing.T, output string) []any {
	t.Helper()
	var replies []any
	for _, line := range subagentLines(output) {
		reply, err := jsonvalue.Parse([]byte(line))
		if err != nil {
			t.Fatalf("%v: %s", err, line)
		}
		replies = append(replies, reply)
	}
	return replies
}

func subagentReplyByID(t *testing.T, replies []any, id int) any {
	t.Helper()
	for _, reply := range replies {
		if jsonvalue.Equal(subagentValue(reply, "id"), id) {
			return reply
		}
	}
	t.Fatalf("no reply with id %d in %v", id, replies)
	return nil
}

// subagentMCPExchange initializes server and lists its tools.
func subagentMCPExchange(t *testing.T, server func(io.Reader, io.Writer) error) (any, []any) {
	t.Helper()
	output := &subagentContractWriter{}
	if err := server(strings.NewReader(subagentInitializeAndList), output); err != nil {
		t.Fatal(err)
	}
	replies := subagentReplies(t, output.String())
	initialize := subagentValue(subagentReplyByID(t, replies, 1), "result")
	tools, _ := subagentValue(subagentReplyByID(t, replies, 2), "result", "tools").([]any)
	return initialize, tools
}

func subagentListedNames(tools []any) []string {
	var names []string
	for _, tool := range tools {
		names = append(names, subagentText(tool, "name"))
	}
	return names
}

func subagentListedTool(tools []any, name string) any {
	for _, tool := range tools {
		if subagentText(tool, "name") == name {
			return tool
		}
	}
	return nil
}

func subagentWriteLaunch(t *testing.T, directory string, launch jsonvalue.Object) {
	t.Helper()
	encoded, err := jsonvalue.Marshal(launch)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(directory, "launch.json"), encoded, 0o666); err != nil {
		t.Fatal(err)
	}
}

func TestSpawnOfferingBothTaskTypesKeepsItsDescription(t *testing.T) {
	spawn := subagentSpawnDefinition(DefaultParentTools())
	if got := subagentText(spawn, "description"); got != "Start a child in your target and filesystem using the task type's configured model and effort. Use `task_type` `straightforward` for bounded work with one clear path: mapping code, running exact build, test, or gate commands, and small mechanical edits. Use `subtle` for multi-file implementation (even from a settled design), multi-stage operational runs or troubleshooting, ambiguous diagnosis, design choices, and work with a high risk of subtle bugs. A subtle task gets a smarter, more expensive agent. Judge by scope and branching, not brief length or command runtime. Escalate stalled or incomplete `straightforward` work to `subtle` after one correction. Returns child_session_id and report_dir immediately; registration does not mean startup succeeded. Reports and startup errors arrive as messages from Mjolnir; use list_agents for status. Reports are short and point to files in report_dir. Supply the assignment and file, symbol, line-range or earlier-report pointers in instructions. Use send_message for follow-up work when an idle child's context helps. Mjolnir selects an enabled profile with the most quota, API-priced profiles last, supporting the task type's configured model and effort. An unavailable model, effort or login is an error, never replaced by another selection. Only children holding processes count against the live children limit; finished children are parked. Close children you no longer need, including failed children after reading the error. Failed-start cleanup must finish before a replacement can use its slot. A child whose record is in error, lost or stopped cannot be re-prompted; a child whose last turn failed can: send_message resumes it. A login the provider has refused stays unavailable until repaired with `mj login`." {
		t.Fatal(got)
	}
	taskType := subagentValue(spawn, "inputSchema", "properties", "task_type")
	if !subagentEqualJSON(subagentValue(taskType, "enum"), `["straightforward","subtle"]`) {
		t.Fatal(subagentValue(taskType, "enum"))
	}
	description := subagentText(taskType, "description")
	if !strings.Contains(description, "`straightforward` is for bounded work with a single clear path") ||
		!strings.Contains(description, "multi-file implementation, multi-stage operational runs or troubleshooting") ||
		strings.Contains(description, "`orchestrator`") {
		t.Fatal(description)
	}
}

func TestSpawnWithOnlyStraightforwardOffersNoTaskType(t *testing.T) {
	spawn := subagentSpawnDefinition(subagentStraightforwardOnly)
	description := subagentText(spawn, "description")
	for _, absent := range []string{"task_type", "subtle", "task type"} {
		if strings.Contains(description, absent) {
			t.Fatalf("%q in %s", absent, description)
		}
	}
	if !strings.HasPrefix(description, "Start a child in your target and filesystem using the configured model and effort. Delegate straightforward work: ") {
		t.Fatal(description)
	}
	if !strings.Contains(description, "Keep multi-file implementation, troubleshooting, ambiguous diagnosis, design choices, and changes with a high risk of hard-to-detect bugs yourself.") {
		t.Fatal(description)
	}
	if subagentHas(spawn, "inputSchema", "properties", "task_type") {
		t.Fatal("task_type is offered")
	}
	if !subagentEqualJSON(subagentValue(spawn, "inputSchema", "required"), `["task_name","instructions"]`) {
		t.Fatal(subagentValue(spawn, "inputSchema", "required"))
	}
}

func TestObservationsAreReadOnly(t *testing.T) {
	var readOnly []string
	for _, tool := range subagentToolDefinitions() {
		if subagentValue(tool, "annotations", "readOnlyHint") == true {
			readOnly = append(readOnly, subagentText(tool, "name"))
		}
	}
	if !slices.Equal(readOnly, []string{"list_agents", "fetch"}) {
		t.Fatal(readOnly)
	}
}

func TestHandbackIsReadOnlySoPlanModeReviewersCanReport(t *testing.T) {
	tools := childToolDefinitions()
	if subagentText(tools[0], "name") != "handback" || subagentValue(tools[0], "annotations", "readOnlyHint") != true {
		t.Fatalf("%v", tools[0])
	}
}

func TestSpawnOfferingBothTaskTypesRequiresOne(t *testing.T) {
	_, _, err := subagentCall("unused.sock", nil, core.SubagentMcpRoleParent,
		`{"name":"spawn_worker","arguments":{"task_name":"t","instructions":"i"}}`, ProgressSilent(time.Second))
	if err == nil || !strings.Contains(err.Error(), "missing field `task_type`") {
		t.Fatal(err)
	}
}

func TestReviewToolArgumentsAreValidatedWithClearErrors(t *testing.T) {
	refused := func(name, arguments string) string {
		t.Helper()
		_, _, err := callReview("unused.sock",
			json.RawMessage(fmt.Sprintf(`{"name":%s,"arguments":%s}`, strconv.Quote(name), arguments)),
			ProgressSilent(time.Millisecond), func(core.SubagentToolAction) time.Duration { return time.Millisecond })
		if err == nil {
			t.Fatalf("%s %s was accepted", name, arguments)
		}
		return err.Error()
	}
	for _, check := range []struct{ name, arguments, want string }{
		{"override_review", `{"overrides":[]}`, "needs at least one entry"},
		{"override_review", `{"overrides":[{"finding":"  ","reason":"fine"}]}`, "overrides[1].finding cannot be empty"},
		{"override_review", `{"overrides":[{"finding":"ok","reason":"ok"},{"finding":"second","reason":" \n"}]}`, "overrides[2].reason cannot be empty"},
		{"override_review", `{"overrides":[{"finding":"only a finding"}]}`, "missing field `reason`"},
		{"override_review", `{"overrides":[{"finding":"f","reason":"r","extra":1}]}`, "unknown field `extra`"},
		{"override_review", `{}`, "missing field `overrides`"},
		// A model that remembers the old single-reason shape is told the
		// new one.
		{"override_review", `{"reason":"unreasonable"}`, "now takes `overrides`"},
		{"set_baseline", `{"commit":" "}`, "commit cannot be empty"},
		{"set_baseline", `{"commit":"abc","path":" "}`, "path cannot be empty"},
		{"set_baseline", `{}`, "missing field `commit`"},
		{"set_baseline", `{"commit":"abc","extra":true}`, "unknown field `extra`"},
		{"request_review", `{"extra":true}`, "unknown field"},
		{"request_review", `{"path":"/workspace"}`, "now takes `paths`"},
		{"spawn_worker", `{}`, "unknown review tool"},
	} {
		if got := refused(check.name, check.arguments); !strings.Contains(got, check.want) {
			t.Fatalf("%s %s: %q lacks %q", check.name, check.arguments, got, check.want)
		}
	}
}

func TestTheReviewServerListsToolsWithoutInitializationGuidance(t *testing.T) {
	claude := core.HarnessKindClaude
	initialize, tools := subagentMCPExchange(t, func(reader io.Reader, writer io.Writer) error {
		return runReview(reader, writer, "unused.sock", &claude, core.SubagentMcpRoleParent)
	})
	if subagentText(initialize, "serverInfo", "name") != "mj-review" {
		t.Fatal(initialize)
	}
	if names := subagentListedNames(tools); !slices.Equal(names, core.ReviewMCPTools) {
		t.Fatal(names)
	}
	if subagentHas(initialize, "instructions") {
		t.Fatal("the review server sent instructions")
	}
	for _, role := range []core.SubagentMcpRole{core.SubagentMcpRoleChild, core.SubagentMcpRoleFetchOnly} {
		initialize, _ := subagentMCPExchange(t, func(reader io.Reader, writer io.Writer) error {
			return runReview(reader, writer, "unused.sock", &claude, role)
		})
		if subagentHas(initialize, "instructions") {
			t.Fatalf("the review server sent instructions to %s", role)
		}
	}

	overrideReview := subagentListedTool(tools, "override_review")
	if !strings.HasPrefix(subagentText(overrideReview, "description"), "Record before-push review findings you will not fix.") {
		t.Fatal(subagentValue(overrideReview, "description"))
	}
	schema := subagentValue(overrideReview, "inputSchema")
	if !subagentEqualJSON(subagentValue(schema, "required"), `["overrides"]`) {
		t.Fatal(subagentValue(schema, "required"))
	}
	overrides := subagentValue(schema, "properties", "overrides")
	if subagentText(overrides, "type") != "array" || !subagentEqualJSON(subagentValue(overrides, "minItems"), "1") {
		t.Fatal(overrides)
	}
	if !subagentEqualJSON(subagentValue(overrides, "items", "required"), `["finding","reason"]`) {
		t.Fatal(subagentValue(overrides, "items", "required"))
	}
	if subagentText(overrides, "items", "properties", "finding", "description") != "The finding, in enough words that a later reviewer recognizes it." {
		t.Fatal(overrides)
	}
	if subagentText(overrides, "items", "properties", "reason", "description") != "Why this finding does not need fixing." {
		t.Fatal(overrides)
	}
	baseline := subagentListedTool(tools, "set_baseline")
	if !subagentEqualJSON(subagentValue(baseline, "inputSchema", "required"), `["commit"]`) || !subagentHas(baseline, "inputSchema", "properties", "path") {
		t.Fatal(baseline)
	}
	if !strings.HasPrefix(subagentText(baseline, "description"), "Mark commits through `commit` as reviewed") {
		t.Fatal(subagentValue(baseline, "description"))
	}
	request := subagentValue(subagentListedTool(tools, "request_review"), "inputSchema", "properties")
	if subagentText(request, "paths", "type") != "array" || subagentHas(request, "path") {
		t.Fatal(request)
	}
}

func TestListTargetsSchemaAndCallsRequireAnOrchestratorEnabledParent(t *testing.T) {
	directory := t.TempDir()
	socket := filepath.Join(directory, "control.sock")
	for _, check := range []struct {
		role             core.SubagentMcpRole
		enabled, offered bool
	}{
		{core.SubagentMcpRoleParent, true, true},
		{core.SubagentMcpRoleParent, false, false},
		{core.SubagentMcpRoleChild, true, false},
		{core.SubagentMcpRoleOrchestrator, true, false},
	} {
		role := check.role
		subagentWriteLaunch(t, directory, subagentObject("supervisors_enabled", check.enabled))
		_, tools := subagentMCPExchange(t, func(reader io.Reader, writer io.Writer) error {
			return runWithOptions(reader, writer, socket, nil, role, DefaultParentTools())
		})
		listTargets := subagentListedTool(tools, "list_targets")
		if (listTargets != nil) != check.offered {
			t.Fatalf("%s: list_targets listed %v", role, listTargets != nil)
		}
		if listTargets != nil {
			if subagentValue(listTargets, "annotations", "readOnlyHint") != true {
				t.Fatal(listTargets)
			}
			if !subagentEqualJSON(subagentValue(listTargets, "inputSchema"), `{"type":"object","properties":{},"additionalProperties":false}`) {
				t.Fatal(listTargets)
			}
		}
		spawn := subagentListedTool(tools, "spawn_orchestrator")
		if (spawn != nil) != check.offered {
			t.Fatalf("%s: spawn_orchestrator listed %v", role, spawn != nil)
		}
		options := DefaultParentTools()
		options.OrchestratorsEnabled = check.enabled
		want := "unknown sub-agent tool"
		if check.offered {
			want = "unknown field"
		}
		for _, field := range []string{"task_type", "working_directory", "profile", "model", "effort", "cpus", "memory"} {
			if spawn != nil {
				if subagentHas(spawn, "inputSchema", "properties", field) {
					t.Fatalf("spawn_orchestrator offers %s", field)
				}
				if subagentValue(spawn, "inputSchema", "additionalProperties") != false {
					t.Fatal(spawn)
				}
			}
			_, _, err := testCallWithOptions(socket, nil, role,
				fmt.Sprintf(`{"name":"spawn_orchestrator","arguments":{"task_name":"t","instructions":"i",%s:"unexpected"}}`, strconv.Quote(field)),
				ProgressSilent(time.Millisecond), time.Millisecond, options)
			if err == nil || !strings.Contains(err.Error(), want) {
				t.Fatalf("%s %s: %v", role, field, err)
			}
		}
		if role == core.SubagentMcpRoleParent || role == core.SubagentMcpRoleOrchestrator {
			_, _, err := testCallWithOptions(socket, nil, role,
				`{"name":"spawn_worker","arguments":{"task_name":"t","instructions":"i","task_type":"orchestrator"}}`,
				ProgressSilent(time.Millisecond), time.Millisecond, options)
			if err == nil || !strings.Contains(err.Error(), "unknown variant `orchestrator`") {
				t.Fatalf("%s: %v", role, err)
			}
		}
		_, _, err := testCallWithOptions(socket, nil, role,
			`{"name":"list_targets","arguments":{"target":"unexpected"}}`,
			ProgressSilent(time.Millisecond), time.Millisecond, options)
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Fatalf("%s list_targets: %v", role, err)
		}
	}
}

func TestDelegationServersNoLongerCarryReviewToolsOrGuidance(t *testing.T) {
	claude := core.HarnessKindClaude
	for _, role := range []core.SubagentMcpRole{
		core.SubagentMcpRoleParent,
		core.SubagentMcpRoleChild,
		core.SubagentMcpRoleMessageOnly,
		core.SubagentMcpRoleFetchOnly,
	} {
		initialize, tools := subagentMCPExchange(t, func(reader io.Reader, writer io.Writer) error {
			return runWithOptions(reader, writer, "unused.sock", &claude, role, DefaultParentTools())
		})
		if subagentText(initialize, "serverInfo", "name") != "mj-agents" {
			t.Fatal(initialize)
		}
		names := subagentListedNames(tools)
		for _, name := range []string{"send_message", "fetch"} {
			if !slices.Contains(names, name) {
				t.Fatalf("%s lacks %s", role, name)
			}
		}
		if subagentHas(initialize, "instructions") {
			t.Fatalf("%s sent instructions", role)
		}
		for _, name := range names {
			if slices.Contains(core.ReviewMCPTools, name) {
				t.Fatalf("%s lists %s", role, name)
			}
		}
		if role != core.SubagentMcpRoleParent {
			for _, name := range []string{"spawn_worker", "list_agents", "close"} {
				if slices.Contains(names, name) {
					t.Fatalf("%s lists %s", role, name)
				}
			}
		}
		for _, name := range core.ReviewMCPTools {
			_, _, err := testCallWithOptions("unused.sock", nil, role,
				fmt.Sprintf(`{"name":%s,"arguments":{}}`, strconv.Quote(name)),
				ProgressSilent(time.Millisecond), time.Millisecond, DefaultParentTools())
			if err == nil || !strings.Contains(err.Error(), "unknown sub-agent tool") {
				t.Fatalf("%s %s: %v", role, name, err)
			}
		}
	}
}

// Hard-won: 5fc728617: the brief writer received the reviewer's findings-only MCP contract.
func TestReviewerMcpInstructionsPreserveTheFullRubricBudget(t *testing.T) {
	brief := core.ReviewChildRoleBriefWriter
	for _, role := range []*core.ReviewChildRole{nil, &brief} {
		root := t.TempDir()
		reviewer := subagentObject()
		if role != nil {
			encoded, err := json.Marshal(*role)
			if err != nil {
				t.Fatal(err)
			}
			text, _ := strconv.Unquote(string(encoded))
			reviewer = subagentObject("role", text)
		}
		subagentWriteLaunch(t, root, subagentObject("reviewer_launch", reviewer, "environment", subagentObject()))
		output := &subagentContractWriter{}
		claude := core.HarnessKindClaude
		if err := subagentRun(strings.NewReader(subagentInitializeAndList), output,
			filepath.Join(root, "subagents.sock"), &claude, core.SubagentMcpRoleChild); err != nil {
			t.Fatal(err)
		}
		text := output.String()
		replies := subagentReplies(t, text)
		if subagentHas(replies[0], "result", "instructions") {
			t.Fatal("the child server sent instructions")
		}
		handback := subagentValue(replies[1], "result", "tools", 0)
		instructions := subagentText(handback, "description")
		if subagentText(handback, "inputSchema", "properties", "message", "description") != instructions {
			t.Fatal(handback)
		}
		if !strings.Contains(instructions, strconv.Itoa(core.SubagentKindReviewer.MaxHandbackChars())) {
			t.Fatal(instructions)
		}
		if role != nil {
			if !strings.Contains(instructions, "relation brief") || strings.Contains(instructions, "plain-text findings") || strings.Contains(text, "No findings.") {
				t.Fatal(instructions)
			}
		} else if !strings.Contains(instructions, "plain-text findings") || !strings.Contains(instructions, "No findings.") {
			t.Fatal(instructions)
		}
	}
}

func TestInitializeAndToolListDeliverConciseParentAndChildContracts(t *testing.T) {
	for _, role := range []core.SubagentMcpRole{
		core.SubagentMcpRoleParent,
		core.SubagentMcpRoleFetchOnly,
		core.SubagentMcpRoleMessageOnly,
		core.SubagentMcpRoleChild,
	} {
		for _, harness := range core.HarnessKindAll {
			initialize, tools := subagentMCPExchange(t, func(reader io.Reader, writer io.Writer) error {
				return subagentRun(reader, writer, "unused.sock", &harness, role)
			})
			if subagentHas(initialize, "instructions") {
				t.Fatalf("%s %v sent instructions", role, harness)
			}
			for _, tool := range tools {
				description := subagentText(tool, "description")
				if len(description) > 1800 {
					t.Fatalf("%s %v %s description is %d bytes", role, harness, subagentText(tool, "name"), len(description))
				}
				if strings.Contains(description, core.DelegationRouting) {
					t.Fatalf("%s carries the delegation routing", subagentText(tool, "name"))
				}
			}
			fetch := subagentListedTool(tools, "fetch")
			if !subagentEqualJSON(subagentValue(fetch, "inputSchema", "required"), `["session_id","path"]`) || subagentValue(fetch, "annotations", "readOnlyHint") != true {
				t.Fatal(fetch)
			}
			names := subagentListedNames(tools)
			switch role {
			case core.SubagentMcpRoleFetchOnly:
				if !slices.Equal(names, []string{"fetch", "send_message"}) {
					t.Fatal(names)
				}
			case core.SubagentMcpRoleChild:
				if !slices.Equal(names, []string{"handback", "fetch", "send_message"}) {
					t.Fatal(names)
				}
			case core.SubagentMcpRoleMessageOnly:
				if !slices.Equal(names, []string{"send_message", "fetch"}) {
					t.Fatal(names)
				}
				sendMessage := tools[0]
				if !subagentEqualJSON(subagentValue(sendMessage, "inputSchema", "required"), `["session_id","message"]`) || subagentValue(sendMessage, "inputSchema", "additionalProperties") != false {
					t.Fatal(sendMessage)
				}
				description := subagentText(sendMessage, "description")
				for _, behavior := range []string{"another Mjolnir session", "stopped (suspended)", "full session ID"} {
					if !strings.Contains(description, behavior) {
						t.Fatal(description)
					}
				}
				if strings.Contains(description, "child_session_id") {
					t.Fatal(description)
				}
			case core.SubagentMcpRoleParent:
				spawn := subagentListedTool(tools, "spawn_worker")
				props := subagentValue(spawn, "inputSchema", "properties")
				if subagentHas(spawn, "inputSchema", "properties", "files") || subagentHas(spawn, "inputSchema", "properties", "context") {
					t.Fatal(props)
				}
				keys := props.(jsonvalue.Object).Keys()
				slices.Sort(keys)
				if !slices.Equal(keys, []string{"instructions", "no_github_credentials", "task_name", "task_type", "working_directory"}) {
					t.Fatal(keys)
				}
				if !subagentEqualJSON(subagentValue(props, "task_type", "enum"), `["straightforward","subtle"]`) {
					t.Fatal(subagentValue(props, "task_type", "enum"))
				}
				if !strings.Contains(subagentText(spawn, "description"), "Judge by scope and branching, not brief length or command runtime") {
					t.Fatal(subagentValue(spawn, "description"))
				}
				if subagentValue(spawn, "inputSchema", "additionalProperties") != false {
					t.Fatal(spawn)
				}
				sendMessages := 0
				for _, name := range names {
					if name == "send_message" {
						sendMessages++
					}
					if name == "send_input" || name == "interrupt" {
						t.Fatal(names)
					}
				}
				if sendMessages != 1 {
					t.Fatal(names)
				}
				sendMessage := subagentListedTool(tools, "send_message")
				if !subagentEqualJSON(subagentValue(sendMessage, "inputSchema", "required"), `["session_id","message"]`) || subagentValue(sendMessage, "inputSchema", "additionalProperties") != false {
					t.Fatal(sendMessage)
				}
				description := subagentText(sendMessage, "description")
				for _, behavior := range []string{
					"another Mjolnir session",
					"stopped (suspended)",
					"next tool boundary",
					"without turn cancellation",
					"idle recipient wakes",
					"queues a turn",
					"full session ID",
				} {
					if !strings.Contains(description, behavior) {
						t.Fatal(description)
					}
				}
				if strings.Contains(description, "child_session_id") {
					t.Fatal(description)
				}
			}
		}
	}
}

func TestSendMessageRejectsTheOldPublicTargetName(t *testing.T) {
	_, _, err := subagentCallWithBudget("unused.sock", nil, core.SubagentMcpRoleParent,
		`{"name":"send_message","arguments":{"child_session_id":"peer","message":"hello"}}`,
		ProgressSilent(time.Second), time.Second)
	if err == nil || err.Error() != "unknown parameter `child_session_id`; use `session_id`" {
		t.Fatal(err)
	}
}

func TestLegacyMailboxFlagKeepsPeerToolsAvailable(t *testing.T) {
	codex := core.HarnessKindCodex
	for _, role := range []core.SubagentMcpRole{core.SubagentMcpRoleParent, core.SubagentMcpRoleMessageOnly} {
		initialize, tools := subagentMCPExchange(t, func(reader io.Reader, writer io.Writer) error {
			return runWithOptions(reader, writer, "unused.sock", &codex, role, subagentParentWithoutOracle)
		})
		if !slices.Contains(subagentListedNames(tools), "send_message") {
			t.Fatalf("%s lacks send_message", role)
		}
		if subagentHas(initialize, "instructions") {
			t.Fatalf("%s sent instructions", role)
		}
	}
}

func TestParentRequiresTaskTypeAndRefusesHiddenToolsAndSelectorOverrides(t *testing.T) {
	tools := subagentToolDefinitions()
	names := subagentToolNames(tools)
	if slices.Contains(names, "list_profiles") || !slices.Contains(names, "send_message") ||
		slices.Contains(names, "send_input") || slices.Contains(names, "interrupt") {
		t.Fatal(names)
	}
	spawn := subagentToolNamed(tools, "spawn_worker")
	if !subagentEqualJSON(subagentValue(spawn, "inputSchema", "required"), `["task_name","instructions","task_type"]`) {
		t.Fatal(subagentValue(spawn, "inputSchema", "required"))
	}
	calls := []string{`{"name":"list_profiles","arguments":{}}`}
	for _, key := range []string{"profile_id", "model", "effort"} {
		if subagentHas(spawn, "inputSchema", "properties", key) {
			t.Fatalf("spawn offers %s", key)
		}
		calls = append(calls, fmt.Sprintf(`{"name":"spawn_worker","arguments":{"task_name":"probe","instructions":"report","task_type":"straightforward",%s:"override"}}`, strconv.Quote(key)))
	}
	for _, params := range calls {
		_, _, err := subagentCall("unused.sock", nil, core.SubagentMcpRoleParent, params, ProgressSilent(time.Second))
		if err == nil || !strings.Contains(err.Error(), "unknown sub-agent tool") && !strings.Contains(err.Error(), "unknown field") {
			t.Fatalf("%s: %v", params, err)
		}
	}
}

func subagentDescription(t *testing.T, name string) string {
	t.Helper()
	tool := subagentToolNamed(subagentToolDefinitions(), name)
	if tool == nil {
		t.Fatalf("no %s tool", name)
	}
	return subagentText(tool, "description")
}

// #1160: a parent read a child's failed login as the child's report and
// gave up on the profile. The tools say what a refused login looks like and
// what to do about it.
// Hard-won: #1160: a refused provider login was mistaken for a child report.
func TestListAndSpawnSayWhatARefusedLoginLooksLike(t *testing.T) {
	wait := subagentDescription(t, "list_agents")
	for _, want := range []string{"failure kind login_invalid", "`mj login`", "not about the task"} {
		if !strings.Contains(wait, want) {
			t.Fatal(wait)
		}
	}
	if spawn := subagentDescription(t, "spawn_worker"); !strings.Contains(spawn, "login the provider has refused") {
		t.Fatal(spawn)
	}
}

// #1161: a child that handed back is parked. The parent has to know that
// such a child holds no processes, that send_message starts it again and
// can take a while, and that the cap counts live children.
// Hard-won: #1161: idle sub-agent process trees exhausted the container.
func TestTheToolsExplainParkedChildrenAndTheLiveChildLimit(t *testing.T) {
	sendMessage := subagentDescription(t, "send_message")
	for _, want := range []string{
		"unique case-insensitive hexadecimal prefix",
		"at least 8 characters",
		"resolved full session ID",
		"parked child is restarted",
		"parked child starts a turn",
		"tens of seconds",
	} {
		if !strings.Contains(sendMessage, want) {
			t.Fatalf("%q: %s", want, sendMessage)
		}
	}
	if wait := subagentDescription(t, "list_agents"); !strings.Contains(wait, "parked true") {
		t.Fatal(wait)
	}
	spawn := subagentDescription(t, "spawn_worker")
	if !strings.Contains(spawn, "live children") || !strings.Contains(spawn, "finished children are parked") {
		t.Fatal(spawn)
	}
	standing := core.StandingInstructions(core.SubagentMcpRoleParent, core.HarnessKindClaude, nil)
	if !strings.Contains(standing, "Finished children are parked") || !strings.Contains(standing, "send_message resumes one") {
		t.Fatal(standing)
	}
}

// Hard-won: cc7e6a2a: oversized handbacks overwhelmed parent context.
func TestTheChildIsToldTheSameReportRulesEverywhere(t *testing.T) {
	// Exercise the instructions the harness actually receives, not just the
	// constants and tool constructors used to build them.
	directory := t.TempDir()
	subagentWriteLaunch(t, directory, subagentObject("environment", subagentObject(
		core.ChildReportDirEnv, filepath.Join(directory, "reports"),
		core.ChildReportLimitEnv, "11184810",
	)))
	output := &subagentContractWriter{}
	if err := subagentRun(strings.NewReader(subagentInitializeAndList), output,
		filepath.Join(directory, "subagents.sock"), nil, core.SubagentMcpRoleChild); err != nil {
		t.Fatal(err)
	}
	responses := subagentReplies(t, output.String())
	initialized := subagentReplyByID(t, responses, 1)
	listed := subagentReplyByID(t, responses, 2)
	tools, _ := subagentValue(listed, "result", "tools").([]any)
	handback := subagentListedTool(tools, "handback")
	description := subagentText(handback, "description")
	if !strings.Contains(description, "Report directory limit: 11184810 bytes") || len(description) > 1800 {
		t.Fatal(description)
	}
	if subagentHas(initialized, "result", "instructions") {
		t.Fatal("the child server sent instructions")
	}
	prompt := core.HandbackPromptNote("/workspace/reports/child")
	for _, instructions := range []string{prompt, description} {
		if !strings.Contains(instructions, core.HandbackReportRules) {
			t.Fatal(instructions)
		}
	}
}

func TestTheModelsAnswerIsThePayloadItselfNotTheResultEnvelope(t *testing.T) {
	envelope, err := jsonvalue.Parse([]byte(`{"request_id":"r-1","completed_at_ms":7,"is_error":false,"message":"{\"status\":\"reported\",\"agents\":[]}"}`))
	if err != nil {
		t.Fatal(err)
	}
	value := modelFacing("r-1", envelope)
	if subagentText(value, "status") != "reported" || subagentText(value, "request_id") != "r-1" {
		t.Fatal(value)
	}
	if subagentHas(value, "message") {
		t.Fatalf("the payload must not stay wrapped in a stringified message: %v", value)
	}
	// A message that is not JSON, such as an error string, is left alone.
	plain, err := jsonvalue.Parse([]byte(`{"request_id":"r-2","is_error":true,"message":"child not found"}`))
	if err != nil {
		t.Fatal(err)
	}
	if !jsonvalue.Equal(modelFacing("r-2", plain), plain) {
		t.Fatal(modelFacing("r-2", plain))
	}
}

func TestAChildGetsHandbackAndCommonToolsWithoutDelegation(t *testing.T) {
	tools := childToolDefinitions()
	if names := subagentToolNames(tools); !slices.Equal(names, []string{"handback", "fetch", "send_message"}) {
		t.Fatal(names)
	}
	if !subagentEqualJSON(subagentValue(tools[0], "inputSchema", "required"), `["message"]`) {
		t.Fatal(tools[0])
	}
	if slices.Contains(subagentToolNames(subagentToolDefinitions()), "handback") {
		t.Fatal("a parent has no report to hand back")
	}
}

// A Claude profile is staged with an allow rule for each tool in ToolNames,
// so a tool listed here and missing there would make Claude ask a person
// before running it (R11-1).
// Hard-won: 91244ea1: Claude sub-agent handback stalled on an unapproved MCP tool.
func TestEachRoleListsExactlyTheToolsItsHarnessIsAllowed(t *testing.T) {
	expected := core.SubagentMcpRoleParent.ToolNames(false, false)
	if names := subagentToolNames(subagentToolDefinitions()); !slices.Equal(names, expected) {
		t.Fatal(names)
	}
	oracle := DefaultParentTools()
	oracle.Oracle = true
	oracle.OrchestratorsEnabled = true
	if names := subagentToolNames(toolDefinitionsWithOptions(nil, oracle)); !slices.Equal(names, core.SubagentMcpRoleParent.ToolNames(true, true)) {
		t.Fatal(names)
	}
	if names := subagentToolNames(childToolDefinitions()); !slices.Equal(names, core.SubagentMcpRoleChild.ToolNames(false, false)) {
		t.Fatal(names)
	}
}

func TestOptionalToolsAreReadFromTheLaunchFileAsPlainJson(t *testing.T) {
	directory := t.TempDir()
	socket := filepath.Join(directory, "control.sock")
	if _, present, err := readLaunchSettings(socket); err != nil || present {
		t.Fatalf("no launch file: %v %v", present, err)
	}
	launch := filepath.Join(directory, "launch.json")
	for _, check := range []struct {
		body     string
		expected bool
	}{
		{`{"session_id":"s"}`, false},
		{`{"session_id":"s","oracle":true}`, true},
		// A field this build does not know, or a retired one, changes
		// nothing.
		{`{"oracle":true,"future_field":1,"bifrost_binary":"x"}`, true},
		{`{"oracle":false}`, false},
	} {
		if err := os.WriteFile(launch, []byte(check.body), 0o666); err != nil {
			t.Fatal(err)
		}
		launch, present, err := readLaunchSettings(socket)
		if err != nil || !present {
			t.Fatalf("%s: %v %v", check.body, present, err)
		}
		if oracle, _ := subagentMCPGet(launch, "oracle"); (oracle == true) != check.expected {
			t.Fatalf("%s: %v", check.body, oracle)
		}
	}
}

func TestOracleToolIsGatedAndNotReadOnlyHinted(t *testing.T) {
	if slices.Contains(subagentToolNames(toolDefinitionsWithOptions(nil, DefaultParentTools())), "ask_oracle") {
		t.Fatal("the oracle is offered without being enabled")
	}
	options := DefaultParentTools()
	options.Oracle = true
	oracle := subagentToolNamed(toolDefinitionsWithOptions(nil, options), "ask_oracle")
	if subagentHas(oracle, "annotations") {
		t.Fatal(oracle)
	}
	description := subagentText(oracle, "description")
	if len(description) > 1800 ||
		!strings.HasPrefix(description, "Consult a strong read-only advisor about a decision") ||
		!strings.HasSuffix(description, "The oracle remembers your earlier questions and its answers, so a follow-up can build on them.") {
		t.Fatal(description)
	}
	if subagentText(oracle, "inputSchema", "properties", "message", "description") != "The decision, the options and your evidence, with file, symbol and line pointers. The oracle reads the shared workspace but not your conversation." {
		t.Fatal(oracle)
	}
	if !subagentEqualJSON(subagentValue(oracle, "inputSchema", "required"), `["message"]`) {
		t.Fatal(oracle)
	}
	if slices.Contains(subagentToolNames(childToolDefinitions()), "ask_oracle") {
		t.Fatal("a child is offered the oracle")
	}
}

func TestOracleIsListedForEachPrimaryRoleOnlyWhenEnabled(t *testing.T) {
	for _, role := range []core.SubagentMcpRole{
		core.SubagentMcpRoleParent,
		core.SubagentMcpRoleOrchestrator,
		core.SubagentMcpRoleMessageOnly,
		core.SubagentMcpRoleFetchOnly,
		core.SubagentMcpRoleChild,
	} {
		for _, oracle := range []bool{false, true} {
			options := DefaultParentTools()
			options.Oracle = oracle
			_, tools := subagentMCPExchange(t, func(reader io.Reader, writer io.Writer) error {
				return runWithOptions(reader, writer, "unused.sock", nil, role, options)
			})
			if got, want := slices.Contains(subagentListedNames(tools), "ask_oracle"), oracle && role != core.SubagentMcpRoleChild; got != want {
				t.Fatalf("%s oracle=%v: listed %v", role, oracle, got)
			}
		}
	}
}

func TestEachRoleRefusesTheOtherRolesTools(t *testing.T) {
	progress := ProgressSilent(50 * time.Millisecond)
	socket := "/nonexistent/subagents.sock"
	_, _, child := subagentCallWithBudget(socket, nil, core.SubagentMcpRoleChild,
		`{"name":"spawn_worker","arguments":{"task_name":"t","instructions":"i","task_type":"straightforward"}}`, progress, 50*time.Millisecond)
	if child == nil || !strings.Contains(child.Error(), "unknown sub-agent tool") {
		t.Fatalf("a child cannot spawn: %v", child)
	}
	_, _, parent := subagentCallWithBudget(socket, nil, core.SubagentMcpRoleParent,
		`{"name":"handback","arguments":{"message":"done"}}`, progress, 50*time.Millisecond)
	if parent == nil || !strings.Contains(parent.Error(), "unknown sub-agent tool") {
		t.Fatalf("a parent cannot hand back: %v", parent)
	}
}

// Hard-won: #1331: binary artifacts silently displaced report evidence in checkpoints.
func TestHandbackRefusesOversizedReportsAndListsTheTenLargestFiles(t *testing.T) {
	directory := t.TempDir()
	reports := filepath.Join(directory, "reports")
	if err := os.MkdirAll(filepath.Join(reports, "nested"), 0o777); err != nil {
		t.Fatal(err)
	}
	for size := 1; size <= 12; size++ {
		file, err := os.Create(filepath.Join(reports, "nested", fmt.Sprintf("artifact-%02d", size)))
		if err != nil {
			t.Fatal(err)
		}
		if err := file.Truncate(int64(size)); err != nil {
			t.Fatal(err)
		}
		_ = file.Close()
	}
	socket := filepath.Join(directory, "subagents.sock")
	subagentWriteLaunch(t, directory, subagentObject("environment", subagentObject(
		core.ChildReportDirEnv, reports,
		core.ChildReportLimitEnv, "77",
	)))
	value, isError, err := subagentCall(socket, nil, core.SubagentMcpRoleChild,
		`{"name":"handback","arguments":{"message":"done"}}`, ProgressSilent(time.Second))
	if err != nil {
		t.Fatal(err)
	}
	if !isError {
		t.Fatal(value)
	}
	refusal := subagentText(value, "error")
	if !strings.Contains(refusal, "holds 78 bytes; limit is 77 bytes") {
		t.Fatal(refusal)
	}
	var files []string
	for _, line := range strings.Split(refusal, "\n") {
		if strings.HasPrefix(line, "  ") {
			files = append(files, line)
		}
	}
	if len(files) != 10 {
		t.Fatal(refusal)
	}
	if !strings.Contains(files[0], "12 bytes: nested") || !strings.HasSuffix(files[0], "artifact-12") {
		t.Fatal(refusal)
	}
	if !strings.HasSuffix(files[9], "artifact-03") || strings.Contains(refusal, "artifact-02") {
		t.Fatal(refusal)
	}
	if !strings.Contains(refusal, "Delete build outputs, binaries and downloads") || !strings.Contains(refusal, "call handback again") {
		t.Fatal(refusal)
	}
}

func TestOlderLaunchFilesWithoutBothReportSettingsSkipTheBudget(t *testing.T) {
	directory := t.TempDir()
	socket := filepath.Join(directory, "subagents.sock")
	if budget, err := subagentLaunchBudget(socket); err != nil || budget != nil {
		t.Fatalf("%v %v", budget, err)
	}
	for _, environment := range []jsonvalue.Object{
		subagentObject(),
		subagentObject(core.ChildReportDirEnv, "/missing"),
		subagentObject(core.ChildReportLimitEnv, "1"),
	} {
		subagentWriteLaunch(t, directory, subagentObject("environment", environment))
		if budget, err := subagentLaunchBudget(socket); err != nil || budget != nil {
			t.Fatalf("%v: %v %v", environment, budget, err)
		}
	}
}

// subagentLaunchBudget is the tests' read_launch_settings(socket) followed by
// ChildReportBudget::from_launch: nil when there is no launch file or no
// budget in it.
func subagentLaunchBudget(socket string) (*childReportBudget, error) {
	launch, present, err := readLaunchSettings(socket)
	if err != nil || !present {
		return nil, err
	}
	return childReportBudgetFromLaunch(launch)
}

func TestNewMcpRefusesTheOldSendInputName(t *testing.T) {
	_, _, err := subagentCall("unused.sock", nil, core.SubagentMcpRoleParent,
		`{"name":"send_input","arguments":{"child_session_id":"child","message":"keep going"}}`,
		ProgressSilent(time.Millisecond))
	if err == nil || !strings.Contains(err.Error(), "unknown sub-agent tool") {
		t.Fatal(err)
	}
}

func TestAnUnansweredHandbackTellsTheChildToCallAgain(t *testing.T) {
	value, isError := unansweredReply("request-1", core.SubagentToolActionHandback{Message: "r"}, 120*time.Second)
	if !isError {
		t.Fatal(value)
	}
	if refusal := subagentText(value, "error"); !strings.Contains(refusal, "call handback again") || strings.Contains(refusal, "list_agents") {
		t.Fatal(refusal)
	}
}

func TestSpawnTakesNoCallerChosenKey(t *testing.T) {
	spawn := subagentToolNamed(subagentToolDefinitions(), "spawn_worker")
	if subagentHas(spawn, "inputSchema", "properties", "request_key") || strings.Contains(subagentText(spawn, "description"), "request_key") {
		t.Fatal(spawn)
	}
	_, _, refused := subagentCallWithBudget("/nonexistent/subagents.sock", nil, core.SubagentMcpRoleParent,
		`{"name":"spawn_worker","arguments":{"task_name":"t","instructions":"i","task_type":"straightforward","request_key":"k"}}`,
		ProgressSilent(50*time.Millisecond), 50*time.Millisecond)
	if refused == nil || !strings.Contains(refused.Error(), "unknown field `request_key`") {
		t.Fatalf("a spawn naming request_key must be refused: %v", refused)
	}
}
