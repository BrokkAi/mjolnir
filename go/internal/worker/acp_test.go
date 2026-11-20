// Port of mj-worker/src/acp/tests.rs.

// Tests of the ACP runtime: fake bridges speak JSON-RPC lines over an
// in-memory duplex stream (or a Python script over a real pipe) and the tests
// assert what the runtime reports and what the bridge received. The
// #[cfg(unix)] items, including mod terminals, are in acp_unix_test.go.
//
// Go shapes used throughout:
//   - a bridge's tokio::io::DuplexStream is an io.ReadWriteCloser; closing it
//     is how a test aborts the bridge;
//   - serde_json::Value is the value encoding/json decodes into an any
//     (map[string]any, []any, float64, ...);
//   - an unbounded mpsc sender or a oneshot sender is a send-only channel the
//     test makes with enough buffer that the bridge never blocks on it.

package worker

import (
	"io"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// titlingCodexBridge is a fake Codex bridge that titles a thread the way
// codex-acp 1.13.2 does: from the text blocks of the prompt, joined with
// spaces (`params.prompt.filter((b) => b.type === "text")` in its prompt
// handler, which feeds both the fallback title and the title model). Any
// other block is left out. It reports each prompt it receives, then the
// title, then one agent message so the test knows the title has been
// handled.
func titlingCodexBridge(t *testing.T, stream io.ReadWriteCloser, prompts chan<- any) {
	panic("unported: mj-worker/src/acp/tests.rs titling_codex_bridge")
}

// codexSessionTitles drives a new Codex session through one prompt against
// titlingCodexBridge. Returns the prompt the bridge received and the titles
// the worker relayed. projectMemory nil means no project memory.
func codexSessionTitles(t *testing.T, projectMemory *string, prompt []schema.ContentBlock) (any, []string) {
	panic("unported: mj-worker/src/acp/tests.rs codex_session_titles")
}

func workerSocket(role core.SubagentMcpRole) SubagentMcpSocket {
	return SubagentMcpSocket{
		Path:                  "/worker/subagents.sock",
		Role:                  role,
		AgentMailboxesEnabled: true,
		SubtleSubagents:       true,
		ReviewTools:           false,
		ProfileRegistration:   false,
	}
}

// scriptedBridge answers `initialize` and `session/new`, then fails the
// first `session/prompt` with a JSON-RPC error and completes the second.
// Returns how many prompts it received.
func scriptedBridge(t *testing.T, stream io.ReadWriteCloser, loginPath string, loginIdentity ClaudeLoginIdentity) int {
	panic("unported: mj-worker/src/acp/tests.rs scripted_bridge")
}

// extRequestBridge answers `initialize` and `session/new`, then — while the
// prompt is in flight — sends the client an ext request and publishes the
// answer as soon as it arrives, so a silent client shows up as a timeout.
func extRequestBridge(t *testing.T, stream io.ReadWriteCloser, method string, answered chan<- map[string]any) {
	panic("unported: mj-worker/src/acp/tests.rs ext_request_bridge")
}

func answerToExtRequest(t *testing.T, method string, executionPolicy core.ExecutionPolicy) map[string]any {
	panic("unported: mj-worker/src/acp/tests.rs answer_to_ext_request")
}

func architectureForm() map[string]any {
	return map[string]any{
		"jsonrpc": "2.0",
		"id":      "ask-1",
		"method":  "elicitation/create",
		"params": map[string]any{
			"sessionId":  "scripted",
			"toolCallId": "question-tool",
			"mode":       "form",
			"message":    "Choose an architecture",
			"requestedSchema": map[string]any{
				"type":     "object",
				"required": []any{"architecture"},
				"properties": map[string]any{
					"architecture": map[string]any{
						"type":  "string",
						"title": "Architecture",
						"oneOf": []any{
							map[string]any{"const": "thin", "title": "Thin callers"},
							map[string]any{"const": "dynamic", "title": "Dynamic matrix"},
						},
					},
					"token": map[string]any{
						"type":  "string",
						"title": "Access token",
						"_meta": map[string]any{"codex": map[string]any{"isSecret": true}},
					},
				},
			},
		},
	}
}

// elicitationBridge scripts one prompt that asks the architecture question.
func elicitationBridge(t *testing.T, stream io.ReadWriteCloser, initialized, answered chan<- map[string]any, finishBeforeAnswer bool) {
	panic("unported: mj-worker/src/acp/tests.rs elicitation_bridge")
}

func answerArchitectureForm(t *testing.T, harness core.HarnessKind, finishBeforeAnswer bool) {
	panic("unported: mj-worker/src/acp/tests.rs answer_architecture_form")
}

// modeSurface is how a fake bridge offers its modes.
type modeSurface int

const (
	modeSurfaceBoth modeSurface = iota
)

// modeChangeBridge offers its modes on surface. ignoredMode is the startup
// enforcement selection a test treats as setup rather than as the mode change
// it is observing; nil means none.
func modeChangeBridge(t *testing.T, stream io.ReadWriteCloser, surface modeSurface, ignoredMode *string, observed chan<- map[string]any) {
	panic("unported: mj-worker/src/acp/tests.rs mode_change_bridge")
}

func policyIsEnforcedBeforeSessionIsReported(t *testing.T, harness core.HarnessKind, executionPolicy core.ExecutionPolicy, claudePermissionMode core.ClaudePermissionMode, resumeSession *string) {
	panic("unported: mj-worker/src/acp/tests.rs policy_is_enforced_before_session_is_reported")
}

// stubbornModeBridge is a harness that answers the mode request while still
// reporting another mode has not applied the policy, so the session must
// fail instead of running prompts under the permissions the operator did not
// ask for.
func stubbornModeBridge(t *testing.T, stream io.ReadWriteCloser) {
	panic("unported: mj-worker/src/acp/tests.rs stubborn_mode_bridge")
}

// allowAllOnlyMuseBridge is a Muse bridge whose approval selector offers only
// `allowAll`: a Guardian session has no approval mode to run in. With plan,
// it also carries Plan on its separate `mode` selector.
func allowAllOnlyMuseBridge(t *testing.T, stream io.ReadWriteCloser, plan bool) {
	panic("unported: mj-worker/src/acp/tests.rs allow_all_only_muse_bridge")
}

func guardianMuseOfferingOnlyAllowAllFails(t *testing.T, plan bool) {
	panic("unported: mj-worker/src/acp/tests.rs guardian_muse_offering_only_allow_all_fails")
}

// codexRefreshFailure is Codex's error when OpenAI rejects its refresh token
// (R14-1; reverify-14 cli/011, cli/013).
const codexRefreshFailure = "Your access token could not be refreshed. Please log out and sign in again."

// codexRefreshFailureBridge fails every prompt the way codex-acp 1.13.3
// failed one in reverify-14 (tmux/012): Codex's own account of the turn
// arrives as agent text, then the prompt fails with the same sentence and
// `codexErrorInfo: "unauthorized"`. The first prompt streams that text, the
// second streams nothing, and the third streams the sentence and then a
// thought, which ends that agent message.
func codexRefreshFailureBridge(t *testing.T, stream io.ReadWriteCloser) {
	panic("unported: mj-worker/src/acp/tests.rs codex_refresh_failure_bridge")
}

// stalledPromptBridge answers `initialize` and `session/new`, then holds
// `session/prompt` until the test completes it. Used to prove cancel waits
// for a real prompt settlement and restarts when that settlement never
// arrives.
func stalledPromptBridge(t *testing.T, stream io.ReadWriteCloser, observed chan<- string, complete <-chan struct{}) {
	panic("unported: mj-worker/src/acp/tests.rs stalled_prompt_bridge")
}

// musePolicyOptions is muse-acp 0.10's policy selectors as a fake bridge
// reports them: approvals on `approval_mode`, apart from Mode, and
// auto-review off in every session it opens. A guardian Muse session selects
// `promptUnmatched` and `on`.
func musePolicyOptions() []any {
	return []any{
		map[string]any{"id": "approval_mode", "name": "Approvals", "type": "select",
			"currentValue": "allowAll", "options": []any{
				map[string]any{"value": "allowAll", "name": "Allow all"},
				map[string]any{"value": "promptUnmatched", "name": "Prompt unmatched"},
			}},
		map[string]any{"id": "auto_review", "name": "Auto-review", "type": "select",
			"currentValue": "off", "options": []any{
				map[string]any{"value": "off", "name": "Off"},
				map[string]any{"value": "on", "name": "On"},
			}},
	}
}

// selectOption applies a `session/set_config_option` request's params to
// options, in place.
func selectOption(options []any, params map[string]any) {
	for _, option := range options {
		option, ok := option.(map[string]any)
		if !ok {
			panic("config options are a list of objects")
		}
		if option["id"] == params["configId"] {
			option["currentValue"] = params["value"]
		}
	}
}

// silentAfterPromptBridge is a bridge that accepts a prompt, optionally
// announces one tool call, and then goes silent for good. This is what a
// harness blocked in a long build looks like from Mjolnir: an open tool call
// and no protocol traffic at all.
func silentAfterPromptBridge(t *testing.T, stream io.ReadWriteCloser, observed chan<- string, openAToolCall bool) {
	silentAfterPromptBridgeWithLateReply(t, stream, observed, openAToolCall, false, false)
}

func silentAfterPromptBridgeWithLateReply(t *testing.T, stream io.ReadWriteCloser, observed chan<- string, openAToolCall, lateReply, childTraffic bool) {
	panic("unported: mj-worker/src/acp/tests.rs silent_after_prompt_bridge_with_late_reply")
}

// silentBridgeSpec is the launch of a scripted Muse bridge under
// stallPolicy: Muse marks no turn end of its own, so the watchdog covers it.
// Shared with the guardian tests.
func silentBridgeSpec(stallPolicy core.StallPolicy) LaunchSpec {
	panic("unported: mj-worker/src/acp/tests.rs silent_bridge_spec")
}

// waitForBridgePrompt reads scripted-bridge methods into methods until a
// `session/prompt` arrives, so a test can assert what the bridge saw and in
// what order.
func waitForBridgePrompt(t *testing.T, observed <-chan string, methods *[]string) {
	t.Helper()
	for {
		var method string
		select {
		case seen, ok := <-observed:
			if !ok {
				t.Fatal("the bridge must keep reporting methods")
			}
			method = seen
		case <-time.After(5 * time.Second):
			t.Fatal("the prompt must reach the bridge")
		}
		*methods = append(*methods, method)
		if method == "session/prompt" {
			return
		}
	}
}

// promptEchoingBridge answers `initialize`, `session/load`, and every
// `session/prompt` at once, reporting each prompt's text so a test can tell
// which prompts reached it.
func promptEchoingBridge(t *testing.T, stream io.ReadWriteCloser, observed chan<- string) {
	promptEchoingBridgeReporting(t, stream, observed, nil)
}

// promptEchoingBridgeReporting is promptEchoingBridge that names itself
// agentVersion in `initialize`; nil names no version.
func promptEchoingBridgeReporting(t *testing.T, stream io.ReadWriteCloser, observed chan<- string, agentVersion *string) {
	panic("unported: mj-worker/src/acp/tests.rs prompt_echoing_bridge_reporting")
}

// waitForRuntimeEvent returns the first event from events that matches,
// failing the test after the shared I/O hang guard.
func waitForRuntimeEvent(t *testing.T, events <-chan core.RuntimeEvent, matches func(core.RuntimeEvent) bool) core.RuntimeEvent {
	t.Helper()
	for {
		select {
		case event, ok := <-events:
			if !ok {
				t.Fatal("runtime event channel stays open")
			}
			if matches(event) {
				return event
			}
		case <-time.After(ioTimeout):
			t.Fatal("timed out waiting for runtime event")
		}
	}
}

func steeringBridge(t *testing.T, stream io.ReadWriteCloser, observed chan<- any, complete <-chan struct{}, steeringOutcome string) {
	panic("unported: mj-worker/src/acp/tests.rs steering_bridge")
}

func exerciseImageSteering(t *testing.T, withImages bool, steeringOutcome string) {
	panic("unported: mj-worker/src/acp/tests.rs exercise_image_steering")
}

// sessionReloadRejectingBridge is a fake bridge that rejects every attempt to
// reload a recorded session and answers `session/new` with a fresh id.
func sessionReloadRejectingBridge(t *testing.T, stream io.ReadWriteCloser, observed chan<- string, advertisedResume bool) {
	panic("unported: mj-worker/src/acp/tests.rs session_reload_rejecting_bridge")
}

// reloadFallbackSpec is the launch of a scripted bridge resuming the
// recorded session "gone". Shared with the instructions tests.
func reloadFallbackSpec(harness core.HarnessKind) LaunchSpec {
	panic("unported: mj-worker/src/acp/tests.rs reload_fallback_spec")
}

func exerciseContextClear(t *testing.T, failReplacement bool, harness core.HarnessKind) {
	panic("unported: mj-worker/src/acp/tests.rs exercise_context_clear")
}

func pendingTestQuestion(id string) core.ElicitationRequest {
	return core.ElicitationRequest{
		ID:             id,
		Message:        "",
		Title:          nil,
		Description:    nil,
		Fields:         []core.ElicitationField{},
		ToolPermission: nil,
	}
}

// typedFailureBridge is an adapter that, once asked for typed failures, ends
// the first prompt the way it ended issue 1217's: `end_turn`, no agent text,
// and the provider's error as a `sessionFailure` record on the response.
// Reports whether the worker's `initialize` asked for the records.
func typedFailureBridge(t *testing.T, stream io.ReadWriteCloser, failure any) bool {
	panic("unported: mj-worker/src/acp/tests.rs typed_failure_bridge")
}

// heldPromptBridge is a fake OpenCode that opens one session and holds each
// prompt until release fires.
func heldPromptBridge(t *testing.T, stream io.ReadWriteCloser, observed chan<- string, release <-chan struct{}) {
	panic("unported: mj-worker/src/acp/tests.rs held_prompt_bridge")
}

// acpTestsWaitFor waits until the fake bridge has seen method. Rust's
// wait_for; prefixed because test_support has its own wait_for.
func acpTestsWaitFor(t *testing.T, observed <-chan string, method string) {
	t.Helper()
	timeout := time.After(5 * time.Second)
	for {
		select {
		case seen, ok := <-observed:
			if !ok {
				t.Fatalf("the fake bridge stopped before %s", method)
			}
			if seen == method {
				return
			}
		case <-timeout:
			t.Fatalf("no %s", method)
		}
	}
}
