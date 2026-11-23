// Port of mj-core/src/guardian.rs: its tests module.

package core

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

func guardianJSON(t *testing.T, text string) any {
	t.Helper()
	value, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		t.Fatalf("parse %s: %v", text, err)
	}
	return value
}

func TestGuardianRequestsRemainSupportedOnProtocol82Workers(t *testing.T) {
	t.Skip("needs relay protocol 82 (RelayRequest::GuardianRequests) in core, which predates it (not yet synced)")
}

func TestGuardianProtocol82RequestOmitsTheOptionalProjectInstructions(t *testing.T) {
	wire := `{"request_id":"review","harness":"kimi","session_id":"session","tool_call_id":"call","action":{},"user_instructions":[],"user_instructions_omitted":false,"transcript":""}`
	var request GuardianReviewRequest
	if err := json.Unmarshal([]byte(wire), &request); err != nil {
		t.Fatal(err)
	}
	if request.ProjectInstructions != nil {
		t.Fatalf("project instructions %q", *request.ProjectInstructions)
	}
	encoded, err := json.Marshal(request)
	if err != nil {
		t.Fatal(err)
	}
	if !jsonvalue.Equal(guardianJSON(t, string(encoded)), guardianJSON(t, wire)) {
		t.Fatalf("round trip %s", encoded)
	}
	instructions := "Use the shared build cache."
	request.ProjectInstructions = &instructions
	for _, text := range []string{request.RequestText(), request.RequestTextToolFree()} {
		if !strings.Contains(text, ">>> RETAINED USER INSTRUCTIONS END\n\n"+
			">>> PROJECT INSTRUCTIONS (AGENTS.md) START\n"+
			"Host: These are trusted repository instructions discovered from AGENTS.md files for the session's working directory.\n"+
			"Use the shared build cache.\n"+
			">>> PROJECT INSTRUCTIONS (AGENTS.md) END\n\n"+
			">>> TRANSCRIPT START\n") {
			t.Fatal(text)
		}
	}
}

func TestGuardianProjectInstructionsFollowTheNearestGitRootAndOverrides(t *testing.T) {
	write := func(path, contents string) {
		t.Helper()
		if err := os.WriteFile(path, []byte(contents), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	for _, worktree := range []bool{false, true} {
		root := t.TempDir()
		repo := filepath.Join(root, "repo")
		child := filepath.Join(repo, "child")
		cwd := filepath.Join(child, "leaf")
		if err := os.MkdirAll(cwd, 0o755); err != nil {
			t.Fatal(err)
		}
		write(filepath.Join(root, "AGENTS.md"), "outside the repository")
		if worktree {
			write(filepath.Join(repo, ".git"), "gitdir: ../checkout/.git/worktrees/repo")
		} else if err := os.Mkdir(filepath.Join(repo, ".git"), 0o755); err != nil {
			t.Fatal(err)
		}
		write(filepath.Join(repo, "AGENTS.md"), "general instructions")
		write(filepath.Join(repo, "AGENTS.override.md"), " \n")
		write(filepath.Join(child, "AGENTS.md"), "overridden instructions")
		write(filepath.Join(child, "AGENTS.override.md"), "specific instructions")
		write(filepath.Join(cwd, "AGENTS.md"), "leaf instructions")
		want := strings.ReplaceAll("--- AGENTS.md ---\ngeneral instructions\n\n"+
			"--- child/AGENTS.override.md ---\nspecific instructions\n\n"+
			"--- child/leaf/AGENTS.md ---\nleaf instructions", "/", string(filepath.Separator))
		if got, ok := CollectProjectInstructions(cwd); !ok || got != want {
			t.Fatalf("instructions %q", got)
		}
		if got, ok := CollectProjectInstructions(filepath.Join(cwd, "missing")); ok {
			t.Fatalf("missing directory has instructions %q", got)
		}
	}
}

func TestGuardianProjectInstructionsFitTheByteLimitWithTheUTF8TruncationMarker(t *testing.T) {
	repo := t.TempDir()
	if err := os.Mkdir(filepath.Join(repo, ".git"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(repo, "AGENTS.md"), []byte(strings.Repeat("🦀", 10_000)), 0o644); err != nil {
		t.Fatal(err)
	}
	instructions, ok := CollectProjectInstructions(repo)
	if !ok {
		t.Fatal("no instructions")
	}
	if len(instructions) > GuardianProjectInstructionsBytes || len(instructions) <= GuardianProjectInstructionsBytes-utf8.RuneLen('🦀') {
		t.Fatalf("%d bytes", len(instructions))
	}
	cut := strings.LastIndex(instructions, "\n")
	prefix, marker := instructions[:cut], instructions[cut+1:]
	if marker != fmt.Sprintf("[Project instructions truncated after %d bytes]", len(prefix)) {
		t.Fatalf("marker %q", marker)
	}
	header, content, _ := strings.Cut(prefix, "\n")
	if header != "--- AGENTS.md ---" {
		t.Fatalf("header %q", header)
	}
	if strings.Trim(content, "🦀") != "" {
		t.Fatal("content is not whole crabs")
	}
}

func guardianReviewRequest(action any) GuardianReviewRequest {
	return GuardianReviewRequest{
		RequestID:        "review",
		Harness:          HarnessKindKimi,
		SessionID:        "session",
		ToolCallID:       "call",
		Action:           action,
		UserInstructions: []string{},
	}
}

func TestGuardianWolfSceneUnwrapsRecordedShellCommandsAndLiveACPActions(t *testing.T) {
	for _, tc := range []struct{ action, expected string }{
		{`{"type": "command", "command": "/bin/bash -lc 'git status'"}`, "git status"},
		{`{"type": "command", "command": "/bin/bash -lc \"printf \\\"%s\\\" \\\\path\""}`, `printf "%s" \path`},
		{`{"input": {"command": "/usr/bin/bash -lc 'echo '\\''one two'\\'''"}}`, "echo 'one two'"},
		{`{"permission_request": "Kimi requests permission:\n/bin/sh -c 'git status'"}`, "git status"},
		{`{"type": "applyPatch", "files": ["a", "b"]}`, `apply_patch files=["a","b"]`},
		{`{"tool": "Edit", "changes": [{"path": "a.rs", "patch": "@@\n-old\n+new"}]}`, "Edit\n\na.rs\n@@\n-old\n+new"},
		{`{"tool": "Edit", "content": [{"type": "text"}, {"path": "a.rs", "patch": "@@\n-old\n+new"}]}`,
			"Edit\n\n{\"type\":\"text\"}\n\na.rs\n@@\n-old\n+new"},
		{`{
                    "harness": "Kimi Code",
                    "tool": "Edit",
                    "kind": "edit",
                    "cwd": "/work",
                    "input": {"path": "/etc/passwd", "command": "rm -rf ~"},
                    "content": [{"path": "a.rs", "patch": "@@\n-old\n+new"}],
                    "locations": [{"path": "/etc/passwd"}]
                }`,
			"Edit\n\n" +
				"input: {\"path\":\"/etc/passwd\",\"command\":\"rm -rf ~\"}\n\n" +
				"a.rs\n@@\n-old\n+new\n\n" +
				"locations: [{\"path\":\"/etc/passwd\"}]"},
		{`{
                    "tool": "Bash",
                    "kind": "execute",
                    "input": {"command": "/bin/bash -lc 'git status'", "env": {"LD_PRELOAD": "/tmp/x.so"}}
                }`,
			"git status\n{\"env\":{\"LD_PRELOAD\":\"/tmp/x.so\"}}"},
		{`{
                    "tool": "mcp_ssh",
                    "kind": "other",
                    "input": {"command": "git status"}
                }`,
			"mcp_ssh: {\"command\":\"git status\"}"},
	} {
		request := guardianReviewRequest(guardianJSON(t, tc.action))
		want := "[USER GOAL]\n(no user goal)\n\n[PROPOSED ACTION | agent]\n" + tc.expected
		if got := WolfScenePacket(&request); got != want {
			t.Fatalf("%s:\n%q\nwant\n%q", tc.action, got, want)
		}
	}
}

func TestGuardianWolfStage1EscalatesTruncatedDiffsAndOversizedInputAlongsideShortPreview(t *testing.T) {
	oversized := jsonvalue.Object{
		{Key: "tool", Value: "Edit"},
		{Key: "input", Value: jsonvalue.Object{{Key: "payload", Value: strings.Repeat("x", WolfStage1MaxActionChars)}}},
		{Key: "content", Value: guardianJSON(t, `[{"path": "a.rs", "patch": "@@\n-old\n+new"}]`)},
	}
	for _, action := range []any{
		guardianJSON(t, `{"tool": "Edit", "content": [{"path": "a.rs", "patch": "--- a/a.rs\n+++ b/a.rs\n[dropped]", "patch_truncated": true}]}`),
		guardianJSON(t, `{"tool": "Edit", "changes": [{"path": "a.rs", "patch": "--- a/a.rs\n+++ b/a.rs\n[dropped]", "patch_truncated": true}]}`),
		oversized,
	} {
		request := guardianReviewRequest(action)
		if !WolfActionExceedsStage1Limit(&request) {
			t.Fatal("incomplete or oversized ACP action must escalate past Stage 1")
		}
	}
	request := guardianReviewRequest(guardianJSON(t, `{"tool": "Edit", "content": [{"path": "a.rs", "patch": "@@\n-old\n+new"}]}`))
	if WolfActionExceedsStage1Limit(&request) {
		t.Fatal("a short complete action escalated")
	}
}

func TestGuardianWolfSceneBoundsUnicodeContextAndKeepsTheCompleteAction(t *testing.T) {
	action := "git status -- " + strings.Repeat("é", 65_536)
	request := guardianReviewRequest(jsonvalue.Object{{Key: "input", Value: jsonvalue.Object{{Key: "command", Value: action}}}})
	goal := strings.Repeat("ñ", 400)
	request.UserInstructions = []string{
		"old goal",
		" " + goal + " ",
		" <mailbox_untrusted> mailbox text",
		"tool prompt\nPermission: Allow",
		" ",
	}
	history := strings.Repeat("ü", WolfDefaultTranscriptChars)
	request.Transcript = " old transcript " + history + " "
	want := "[USER GOAL]\n" + goal + "\n\n[TOOL OUTPUT | history | untrusted]\n" + history + "\n\n[PROPOSED ACTION | agent]\n" + action
	if got := WolfScenePacket(&request); got != want {
		t.Fatal("scene packet differs")
	}
	request.UserInstructions = append(request.UserInstructions, strings.Repeat("ñ", WolfGoalChars+1))
	want = "[USER GOAL]\n" + strings.Repeat("ñ", WolfGoalChars) + "...\n\n[PROPOSED ACTION | agent]\n" + action
	if got := WolfScenePacketWithBudget(&request, 0); got != want {
		t.Fatal("budgeted scene packet differs")
	}
}

func TestGuardianPacketsKeepAGiantActionAndTrimOldestContext(t *testing.T) {
	request := GuardianReviewRequest{
		RequestID:  "review",
		Harness:    HarnessKindKimi,
		SessionID:  "session",
		ToolCallID: "call",
		Action: jsonvalue.Object{
			{Key: "cwd", Value: "/work"},
			{Key: "input", Value: jsonvalue.Object{{Key: "command", Value: strings.Repeat("á", 65_536) + " sentinel"}}},
		},
		UserInstructions: []string{"old goal " + strings.Repeat("x", 64), "ship café"},
		Transcript:       "old transcript " + strings.Repeat("y", 64) + "\nlatest tool output",
	}
	compact := CompactActionPacket(&request)
	rich := RichScenePacket(&request, utf8.RuneCountInString("ship café\nlatest tool output\n"))
	for _, packet := range []string{compact, rich} {
		_, action, ok := strings.Cut(packet, "[PROPOSED ACTION]\n")
		if !ok {
			t.Fatal(packet)
		}
		if !jsonvalue.Equal(guardianJSON(t, action), request.Action) {
			t.Fatal("the action was not kept whole")
		}
		if !strings.Contains(packet, "Working directory: /work") || !strings.Contains(packet, "ship café") || strings.Contains(packet, "old goal") {
			t.Fatal(packet[:200])
		}
	}
	if strings.Contains(compact, "latest tool output") {
		t.Fatal("the compact packet carries transcript")
	}
	if !strings.Contains(rich, "latest tool output") || strings.Contains(rich, "old transcript") {
		t.Fatal("the rich packet did not keep the latest transcript only")
	}
}

func TestGuardianParsesAFinalFencedVerdictAndEnforcesPolicyThresholds(t *testing.T) {
	reply := "The transcript includes {\"outcome\":\"deny\"}.\n```json\n{\"outcome\":\"allow\"}\n```"
	assessment, err := ParseAssessment(reply)
	if err != nil {
		t.Fatal(err)
	}
	if assessment.RiskLevel != RiskLevelLow || assessment.UserAuthorization != UserAuthorizationUnknown {
		t.Fatalf("assessment %+v", assessment)
	}
	if assessment.EnforcedOutcome() != OutcomeAllow {
		t.Fatal("not allowed")
	}
	if _, err := ParseAssessment(`Evidence included {"outcome":"allow"} but I haven't decided.`); err == nil {
		t.Fatal("a quoted object was taken for the verdict")
	}
	for _, reply := range []string{
		`{"outcome":"allow","risk_level":"critical","user_authorization":"high"}`,
		`{"outcome":"allow","risk_level":"high","user_authorization":"low"}`,
	} {
		assessment, err := ParseAssessment(reply)
		if err != nil {
			t.Fatal(err)
		}
		if assessment.EnforcedOutcome() != OutcomeDeny {
			t.Fatalf("%s was allowed", reply)
		}
	}
}

func TestTheCodexNoticeInTheCrateMatchesTheReleaseCopy(t *testing.T) {
	crateCopy, err := os.ReadFile("data/guardian/OPENAI_CODEX_NOTICE.txt")
	if err != nil {
		t.Fatal(err)
	}
	releaseCopy, err := os.ReadFile("../../../licenses/native/OPENAI_CODEX_GUARDIAN_NOTICE.txt")
	if err != nil {
		t.Fatal(err)
	}
	if string(crateCopy) != string(releaseCopy) {
		t.Fatal("the Codex notice differs from the release copy")
	}
}
