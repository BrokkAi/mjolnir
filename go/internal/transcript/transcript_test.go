// Port of the tests in mj-transcript/src/transcript.rs.

package transcript

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	acp "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// transcriptTestCall builds an ACP call as the Rust ToolCall builder does.
type transcriptTestCall struct{ call acp.ToolCall }

func newTranscriptTestCall(id, title string) *transcriptTestCall {
	return &transcriptTestCall{call: acp.ToolCall{ToolCallID: acp.ToolCallId(id), Title: title}}
}

func (b *transcriptTestCall) kind(kind acp.ToolKind) *transcriptTestCall {
	b.call.Kind = &kind
	return b
}

func (b *transcriptTestCall) status(status acp.ToolCallStatus) *transcriptTestCall {
	b.call.Status = &status
	return b
}

func (b *transcriptTestCall) rawInput(value any) *transcriptTestCall {
	b.call.RawInput = transcriptTestJSON(value)
	return b
}

func (b *transcriptTestCall) rawOutput(value any) *transcriptTestCall {
	b.call.RawOutput = transcriptTestJSON(value)
	return b
}

func (b *transcriptTestCall) build() *acp.ToolCall {
	call := b.call
	return &call
}

// transcriptTestJSON encodes a test value; a string is taken as JSON text.
func transcriptTestJSON(value any) json.RawMessage {
	if text, ok := value.(string); ok {
		return json.RawMessage(text)
	}
	encoded, err := json.Marshal(value)
	if err != nil {
		panic(err)
	}
	return encoded
}

// assertTranscriptGolden compares output with the checked-in expected file
// copied from the Rust crate. It never rewrites the file: the expected
// output is the Rust build's.
func assertTranscriptGolden(t *testing.T, name, actual string) {
	t.Helper()
	path := filepath.Join("testdata", "tests", "golden", name+".txt")
	expected, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("could not read golden %s: %v", path, err)
	}
	want := strings.TrimSuffix(string(expected), "\n")
	got := strings.TrimSuffix(actual, "\n")
	if want != got {
		wantLines, gotLines := strings.Split(want, "\n"), strings.Split(got, "\n")
		for i := 0; i < max(len(wantLines), len(gotLines)); i++ {
			var w, g string
			if i < len(wantLines) {
				w = wantLines[i]
			}
			if i < len(gotLines) {
				g = gotLines[i]
			}
			if w != g {
				t.Fatalf("golden output differs at %s line %d\nexpected: %q\nactual:   %q", path, i+1, w, g)
			}
		}
	}
}

func TestStandaloneTerminalRetentionPreservesMultibyteErrorAndOutcome(t *testing.T) {
	firstError := "error " + strings.Repeat("界", 100)
	exitCode := uint32(137)
	signal := "SIGKILL"
	var body core.TranscriptBody = core.TranscriptBodyTerminalOutputOf(core.TerminalOutputRecord{
		TerminalID: "term-1",
		Output:     core.NewPackedText(fmt.Sprintf("   \n  %s\n%s\n", firstError, strings.Repeat("stdout\n", 10_000))),
		Truncated:  true,
		ExitCode:   &exitCode,
		Signal:     &signal,
	})

	if !CompactTranscriptBodyForRetention(&body) {
		t.Fatal("the first compaction changed nothing")
	}
	if CompactTranscriptBodyForRetention(&body) {
		t.Fatal("a second compaction changed the row again")
	}
	compact, err := json.Marshal(body)
	if err != nil {
		t.Fatal(err)
	}
	if len(compact) >= 1024 {
		t.Fatalf("compact row is %d bytes", len(compact))
	}
	decoded, err := core.DecodeTranscriptBody(compact)
	if err != nil {
		t.Fatal(err)
	}
	terminal, ok := decoded.(core.TranscriptBodyTerminalOutput)
	if !ok {
		t.Fatalf("expected a terminal record, got %T", decoded)
	}
	record := terminal.Record
	if !record.Output.IsEmpty() {
		t.Fatalf("output kept: %q", record.Output.Text())
	}
	if record.ExitCode == nil || *record.ExitCode != 137 {
		t.Fatalf("exit code %v", record.ExitCode)
	}
	if record.Signal == nil || *record.Signal != "SIGKILL" {
		t.Fatalf("signal %v", record.Signal)
	}
	if !record.Truncated {
		t.Fatal("truncation lost")
	}
	if terminal.OutputLineCount == nil || *terminal.OutputLineCount != 10_003 {
		t.Fatalf("line count %v", terminal.OutputLineCount)
	}
	if terminal.FirstErrorLine == nil || *terminal.FirstErrorLine != firstError[:255] {
		t.Fatalf("first error line %v", terminal.FirstErrorLine)
	}
	if !terminal.DetailTrimmed {
		t.Fatal("detail not marked trimmed")
	}
}

func TestALiveToolCallRecordsWhenItStartedAndEnded(t *testing.T) {
	var entries []core.ChatEntry
	call := newTranscriptTestCall("tool-1", "cargo test").status(acp.ToolCallStatusInProgress).build()
	started, finished := int64(1_000), int64(91_000)
	ApplySessionUpdateToEntries(&entries, 1, &started, acp.SessionUpdate{Kind: acp.SessionUpdateKindToolCall, ToolCall: call})
	if entries[0].RecordedAtMs == nil || *entries[0].RecordedAtMs != 1_000 {
		t.Fatalf("recorded at %v", entries[0].RecordedAtMs)
	}
	if entries[0].ToolEndedAtMs != nil {
		t.Fatalf("ended at %v", *entries[0].ToolEndedAtMs)
	}

	completed := acp.ToolCallStatusCompleted
	finish := acp.SessionUpdate{Kind: acp.SessionUpdateKindToolCallUpdate, ToolCallUpdate: &acp.ToolCallUpdate{
		ToolCallID: "tool-1",
		Status:     &completed,
	}}
	ApplySessionUpdateToEntries(&entries, 2, &finished, finish)
	if *entries[0].RecordedAtMs != 1_000 {
		t.Fatalf("recorded at %d", *entries[0].RecordedAtMs)
	}
	if entries[0].ToolEndedAtMs == nil || *entries[0].ToolEndedAtMs != 91_000 {
		t.Fatalf("ended at %v", entries[0].ToolEndedAtMs)
	}
}

// I1-18: Claude's question tool is titled with the question itself. Its row
// showed only "Which"; it must show the whole question.
// Hard-won: e570799ccc: a question-tool row displayed only its first word.
func TestAQuestionToolIsSummarizedByItsWholeQuestion(t *testing.T) {
	call := newTranscriptTestCall("ask", "Which file name should I use for the new file?").
		rawInput(`{"questions": [{"question": "Which file name should I use for the new file?", "header": "File name"}]}`).build()
	if got := ToolCallPresentation(call).Summary; got != "Which file name should I use for the new file?" {
		t.Fatalf("summary %q", got)
	}
	read := newTranscriptTestCall("read", "Read src/lib.rs").build()
	if got := ToolCallPresentation(read).Summary; got != "Read" {
		t.Fatalf("summary %q", got)
	}
	stale := core.ToolCallPresentation{
		Summary:        "Which",
		Source:         core.NewPackedText(call.Title),
		SourceKind:     core.ToolSummarySourceKindTitle,
		ToolKind:       acp.ToolKindOther,
		SummaryVersion: 1,
	}
	if got := MaterializedToolCallPresentation(&stale, call).Summary; got != "Which file name should I use for the new file?" {
		t.Fatalf("stored rows written by the old rules are repaired: %q", got)
	}
}

func transcriptExecuteSummary(command string) string {
	call := newTranscriptTestCall("argv", "Bash").kind(acp.ToolKindExecute).rawInput(command).build()
	return ToolCallPresentation(call).Summary
}

// transcriptCommand is the JSON {"command": command}.
func transcriptCommand(command any) string {
	return string(transcriptTestJSON(map[string]any{"command": command}))
}

func TestUnknownLeadingOptionsMakeVerbPositionAmbiguous(t *testing.T) {
	if got := transcriptExecuteSummary(transcriptCommand([]string{"git", "--mystery", "status"})); got != "git" {
		t.Fatalf("summary %q", got)
	}
	if got := transcriptExecuteSummary(transcriptCommand([]string{"cargo", "--mystery", "test"})); got != "cargo" {
		t.Fatalf("summary %q", got)
	}
}

func transcriptAssertCommandSummaries(t *testing.T, cases [][2]string) {
	t.Helper()
	for _, c := range cases {
		if got := transcriptExecuteSummary(transcriptCommand(c[0])); got != c[1] {
			t.Errorf("%s: summary %q, want %q", c[0], got, c[1])
		}
	}
}

func TestGitCommandsWithNestedSubcommandsShowBothVerbs(t *testing.T) {
	transcriptAssertCommandSummaries(t, [][2]string{
		{"git worktree add ../x mj/session", "git worktree add"},
		{"git -C repo worktree list", "git worktree list"},
		{"git stash push -u -m tag", "git stash push"},
		{"git stash", "git stash"},
		{"git remote -v", "git remote"},
		{"git bisect good abc123", "git bisect good"},
		{"git stash -- src", "git stash"},
		{"git log origin/master", "git log"},
		{"git config user.name", "git config"},
	})
}

func TestProjectToolsAndScriptRunnersShowTheirSubcommands(t *testing.T) {
	transcriptAssertCommandSummaries(t, [][2]string{
		{"mbx nextest run -p brokk-mj-tui", "mbx nextest"},
		{"mbx +1.91 build bifrost", "mbx build"},
		{"mbx cache stats --json", "mbx cache stats"},
		{"mj daemon restart", "mj daemon restart"},
		{"mj -i test --workspace hel daemon status", "mj daemon status"},
		{"mj transcript abc123", "mj transcript"},
		{"tmux -L mjv-e send-keys -t 0 q", "tmux send-keys"},
		{"tmux -Lmjv capture-pane -p", "tmux capture-pane"},
		{"tmux -S /tmp/sock -2 new-session -d", "tmux new-session"},
		{"uv run pytest -q", "uv run pytest"},
		{"uv run ./scripts/check.py", "uv run check.py"},
		{"uv run --with x pytest", "uv run"},
		{"npm run build", "npm run build"},
		{"npm run-script test:e2e", "npm run-script test:e2e"},
		{"pnpm run lint", "pnpm run lint"},
		{"npm test", "npm test"},
	})
}

func TestANestedShellIsNamedWithoutItsPathAndShowsItsScript(t *testing.T) {
	transcriptAssertCommandSummaries(t, [][2]string{
		{"cd dir && bash -c 'python x.py'", "cd && bash -c python"},
		{"/bin/bash -c \"python x.py\" | tee log", "bash -c python | tee"},
		{"true; /usr/bin/zsh -lc 'git status && cargo test'", "true ; zsh -c git status && cargo test"},
		{"bash -c \"$script\"", "bash -c"},
		{"/bin/bash script.sh", "bash"},
		{"bash -c 'bash -c \"uv run pytest\"'", "bash -c uv run pytest"},
	})
	// An argv shell call already stands for its script.
	if got := transcriptExecuteSummary(transcriptCommand([]string{"/bin/bash", "-lc", "python x.py"})); got != "python" {
		t.Fatalf("summary %q", got)
	}
	if got := transcriptExecuteSummary(transcriptCommand([]string{"/bin/zsh", "script.sh"})); got != "zsh" {
		t.Fatalf("summary %q", got)
	}
}

// Codex reports a command that went through its approval path as one string
// with its shell wrapper. The same script reads the same whether the wrapper
// is there or not.
func TestACommandThatIsOnlyAShellWrapperStandsForItsScript(t *testing.T) {
	script := "python3 - <<'PY'\nprint(\"x\")\nPY"
	wrapped := fmt.Sprintf("/bin/bash -lc \"%s\"", strings.ReplaceAll(strings.ReplaceAll(script, `\`, `\\`), `"`, `\"`))
	presentation := func(command string) core.ToolCallPresentation {
		return ToolCallPresentation(newTranscriptTestCall("call", "Bash").kind(acp.ToolKindExecute).
			rawInput(transcriptCommand(command)).build())
	}
	if got := presentation(script).Summary; got != "python3" {
		t.Fatalf("summary %q", got)
	}
	if got := presentation(wrapped).Summary; got != "python3" {
		t.Fatalf("summary %q", got)
	}
	if got := presentation(wrapped).Source.Text(); got != script {
		t.Fatalf("source %q", got)
	}

	for _, c := range [][2]string{
		{"bash -c 'python x.py'", "python"},
		{"/usr/bin/zsh -lc 'git status && cargo test'", "git status && cargo test"},
		{"bash -c \"cd $dir && make\"", "cd && make"},
		{"bash -c 'cargo test' arg0", "cargo test"},
		// Codex's quoting of a script with both kinds of quote.
		{"/bin/bash -lc \"rg -n '\"'^fn'\"' src | head\"", "rg | head"},
		// An environment assignment is part of what the command does.
		{"FOO=1 bash -c 'cargo test'", "bash -c cargo test"},
	} {
		if got := presentation(c[0]).Summary; got != c[1] {
			t.Errorf("%s: summary %q, want %q", c[0], got, c[1])
		}
	}
}

// Retention drops a finished call's raw input. Rebuilding its summary from
// the title alone would lose what the input said.
func TestASummaryWhoseInputRetentionDroppedIsKept(t *testing.T) {
	compacted := newTranscriptTestCall("call", "Bash").kind(acp.ToolKindExecute).build()
	stale := core.ToolCallPresentation{
		Summary:        "cargo test",
		Source:         core.NewPackedText("cargo test --workspace"),
		SourceKind:     core.ToolSummarySourceKindRawInput,
		ToolKind:       acp.ToolKindExecute,
		SummaryVersion: 0,
	}
	if got := MaterializedToolCallPresentation(&stale, compacted); !transcriptPresentationEqual(got, stale) {
		t.Fatalf("presentation %+v", got)
	}
	titled := stale
	titled.SourceKind = core.ToolSummarySourceKindTitle
	if got := MaterializedToolCallPresentation(&titled, compacted).Summary; got != "Bash" {
		t.Fatalf("summary %q", got)
	}
}

func transcriptPresentationEqual(a, b core.ToolCallPresentation) bool {
	return a.Summary == b.Summary && a.Source.Equal(b.Source) && a.SourceKind == b.SourceKind &&
		a.ToolKind == b.ToolKind && a.SummaryVersion == b.SummaryVersion
}

func TestQuotedAndDynamicShellVerbsAreDistinguished(t *testing.T) {
	quoted := newTranscriptTestCall("quoted", "Bash").kind(acp.ToolKindExecute).rawInput(`{"command": "git \"status\""}`).build()
	if got := ToolCallPresentation(quoted).Summary; got != "git status" {
		t.Fatalf("summary %q", got)
	}
	dynamic := newTranscriptTestCall("dynamic", "Bash").kind(acp.ToolKindExecute).rawInput(`{"command": "git \"$verb\""}`).build()
	if got := ToolCallPresentation(dynamic).Summary; got != "git" {
		t.Fatalf("summary %q", got)
	}
	dynamicName := newTranscriptTestCall("dynamic-name", "Bash").kind(acp.ToolKindExecute).rawInput(`{"command": "$command status"}`).build()
	if got := ToolCallPresentation(dynamicName).Summary; got != "Bash" {
		t.Fatalf("summary %q", got)
	}
}

// One command named by an expansion does not hide the rest of the script,
// and an assignment on its own reads as what it substitutes.
func TestShellSummaryKeepsTheCommandsAroundAnUnnamedOne(t *testing.T) {
	transcriptAssertCommandSummaries(t, [][2]string{
		{"T=$(git grep -l 'facts' -- tests | head -1); echo \"file: $T\"; " +
			"S=$(echo $T | cut -d/ -f2); E=\"env -u CARGO\"; " +
			"$E X=1 cargo nextest run --test $S 2>&1 | grep -E 'FAIL'; " +
			"$E cargo clippy 2>&1 | grep -E '^error' | head -3",
			"git grep | head ; echo ; echo | cut ; … | grep ; … | grep | head"},
		{"export T=\"$(git rev-parse HEAD)\"", "git rev-parse"},
		{"A=1 B=$(date)", "date"},
		{"cd dir && $runner test", "cd && …"},
		{"FOO=bar env -i echo $(printf hi)", "env"},
		{"E=x", "Bash"},
	})
}

func TestShellSummarySkipsAssignmentsArgumentsAndNestedSubstitutions(t *testing.T) {
	transcriptAssertCommandSummaries(t, [][2]string{
		{"FOO=bar env -i bash -c \"echo $(printf hi)\"", "env"},
		{"(cd dir && python x.py) | cat", "(cd && python) | cat"},
	})
}

func TestShellSummaryKeepsListPipelineAndBackgroundOperators(t *testing.T) {
	simple := newTranscriptTestCall("call-1", "Bash").kind(acp.ToolKindExecute).
		rawInput(transcriptCommand("cd dir && python x.py | cat | wc ; print ok")).build()
	presentation := ToolCallPresentation(simple)
	if presentation.Summary != "cd && python | cat | wc ; print" {
		t.Fatalf("summary %q", presentation.Summary)
	}
	if presentation.SourceKind != core.ToolSummarySourceKindRawInput {
		t.Fatalf("source kind %q", presentation.SourceKind)
	}
	transcriptAssertCommandSummaries(t, [][2]string{
		{"'printf' '%s' hi >out |& sed s/hi/bye/ || echo failed & wait; cat <in", "printf |& sed || echo & wait ; cat"},
	})
}

func TestShellSummaryRemovesStructuralLoopAndGroupSeparators(t *testing.T) {
	transcriptAssertCommandSummaries(t, [][2]string{
		{"for file in a b; do rm \"$file\"; done; mkdir -p out; nice -n 10 python3 script.py", "rm ; mkdir ; nice python3"},
		{"if test -f foo; then rm foo; fi; { mkdir bar; echo done; }", "test ; rm ; mkdir ; echo"},
		{"echo start; case x in a|b) echo branch;; esac; echo done", "echo ; echo ; echo"},
	})
}

func TestShellSummaryHandlesALoopWithALeadingPipelineAndNice(t *testing.T) {
	transcriptAssertCommandSummaries(t, [][2]string{
		{"cd /tmp && for spec in a b; do set -- $spec; rm \"$spec\"; mkdir -p \"$spec\"; nice -n 10 ./bin/bifrost \"$spec\"; echo \"$spec\"; done; python3 script.py",
			"cd && set ; rm ; mkdir ; nice ./bin/bifrost ; echo ; python3"},
	})
}

func TestShellSummaryInsertsASeparatorAfterAHeredoc(t *testing.T) {
	transcriptAssertCommandSummaries(t, [][2]string{
		{"python3 <<'PYEOF'\nprint(\"x\")\nPYEOF\ngrep -n x file | head", "python3 ; grep | head"},
	})
}

func TestMaterializedSummaryFromAnOlderParserIsRepaired(t *testing.T) {
	source := "python3 <<'PYEOF'\nprint(\"x\")\nPYEOF\ngrep -n x file | head"
	call := newTranscriptTestCall("heredoc", "Bash").kind(acp.ToolKindExecute).rawInput(transcriptCommand(source)).build()
	stale := core.ToolCallPresentation{
		Summary:        "python3 grep | head",
		Source:         core.NewPackedText(source),
		SourceKind:     core.ToolSummarySourceKindRawInput,
		ToolKind:       acp.ToolKindExecute,
		SummaryVersion: 0,
	}

	repaired := MaterializedToolCallPresentation(&stale, call)
	if repaired.Summary != "python3 ; grep | head" {
		t.Fatalf("summary %q", repaired.Summary)
	}
	if repaired.SummaryVersion != ToolSummaryVersion {
		t.Fatalf("version %d", repaired.SummaryVersion)
	}

	current := repaired
	current.Summary = "stored current summary"
	if got := MaterializedToolCallPresentation(&current, call).Summary; got != "stored current summary" {
		t.Fatalf("summary %q", got)
	}
}

func TestExecuteSummaryBoundsTheRetainedSourceBeforeParsing(t *testing.T) {
	command := "echo " + strings.Repeat("argument", toolSummarySourceBytes)
	call := newTranscriptTestCall("bounded", "Bash").kind(acp.ToolKindExecute).rawInput(transcriptCommand(command)).build()

	presentation := ToolCallPresentation(call)
	if presentation.Source.Len() != toolSummarySourceBytes {
		t.Fatalf("source length %d", presentation.Source.Len())
	}
	if presentation.Summary != "echo" {
		t.Fatalf("summary %q", presentation.Summary)
	}
}

func TestTitleWrappersAndMalformedShellFallBackSafely(t *testing.T) {
	wrapped := newTranscriptTestCall("wrapped", "Running: ls -la").kind(acp.ToolKindExecute).build()
	if got := ToolCallPresentation(wrapped).Summary; got != "ls" {
		t.Fatalf("summary %q", got)
	}
	malformed := newTranscriptTestCall("bad", "Bash").kind(acp.ToolKindExecute).rawInput(`{"command": "cd && ("}`).build()
	if got := ToolCallPresentation(malformed).Summary; got != "Bash" {
		t.Fatalf("summary %q", got)
	}
}

func TestExplicitEmptyRawInputDropsAStaleRawSummary(t *testing.T) {
	initial := newTranscriptTestCall("call", "Bash").kind(acp.ToolKindExecute).rawInput(`{"command": "python script.py"}`).build()
	previous := ToolCallPresentation(initial)
	emptyInput := json.RawMessage(`{"command": null}`)
	updated := UpdateToolCallPresentation(&previous, "Running: ls -la", nil, emptyInput, nil)
	if updated.Summary != "ls" || updated.SourceKind != core.ToolSummarySourceKindTitle {
		t.Fatalf("updated %+v", updated)
	}

	output := json.RawMessage(`{"command": "cat result.txt"}`)
	updated = UpdateToolCallPresentation(&previous, "Running: ls -la", nil, emptyInput, output)
	if updated.Summary != "cat" || updated.SourceKind != core.ToolSummarySourceKindRawOutput {
		t.Fatalf("updated %+v", updated)
	}

	outputInitial := newTranscriptTestCall("output", "Bash").kind(acp.ToolKindExecute).rawOutput(`{"command": "python result.py"}`).build()
	outputPrevious := ToolCallPresentation(outputInitial)
	emptyOutput := json.RawMessage(`{"command": null}`)
	updated = UpdateToolCallPresentation(&outputPrevious, "Running: ls -la", nil, nil, emptyOutput)
	if updated.Summary != "ls" || updated.SourceKind != core.ToolSummarySourceKindTitle {
		t.Fatalf("updated %+v", updated)
	}
}

func transcriptRenderedToolRow(call *acp.ToolCall) string {
	presentation := ToolCallPresentation(call)
	item := core.TranscriptItem{
		StableID:        "tool:golden",
		Position:        1,
		CreatedAtMs:     1,
		LastChangedAtMs: 1,
		Body: core.TranscriptBodyTool{
			Call:         core.OpaqueToolCallFromValue(transcriptTestJSON(call)),
			Presentation: &presentation,
		},
	}
	return TranscriptItemText(&item)
}

func transcriptAppendToolRow(output *strings.Builder, label string, call *acp.ToolCall) {
	if output.Len() != 0 {
		output.WriteByte('\n')
	}
	fmt.Fprintf(output, "=== %s (transcript row) ===\n", label)
	output.WriteString(transcriptRenderedToolRow(call))
	output.WriteByte('\n')
}

func TestGoldenTranscriptToolRowSummary(t *testing.T) {
	var output strings.Builder
	// Hard-won: #1416: untrusted User, PlanProposal, and System text could emit terminal controls.
	hostile := "safe\x1b]52;c;Y2xpcGJvYXJk\x07\x1b[31mred\x1b[0m"
	for _, row := range []struct {
		label string
		body  core.TranscriptBody
	}{
		{"user message", core.TranscriptBodyUser{Content: []json.RawMessage{transcriptTestJSON(map[string]any{"type": "text", "text": hostile})}}},
		{"plan proposal", core.TranscriptBodyPlanProposal{ProposalID: "golden", Plan: hostile}},
		{"system message", core.TranscriptBodySystem{Text: hostile}},
	} {
		item := core.TranscriptItem{
			StableID:        row.label + ":golden",
			Position:        1,
			CreatedAtMs:     1,
			LastChangedAtMs: 1,
			Body:            row.body,
		}
		fmt.Fprintf(&output, "=== %s (transcript row) ===\n", row.label)
		fmt.Fprintf(&output, "%s\n", TranscriptItemText(&item))
	}
	diff := acp.Diff{Path: "/workspace/new.txt", NewText: "one\ntwo\n"}
	fmt.Fprintf(&output, "=== new file diff detail (transcript row) ===\n")
	fmt.Fprintf(&output, "%s\n", formatDiffstat(&diff))

	terminal := func(exitCode *uint32, signal *string, truncated bool, text string) string {
		record := core.TerminalOutputRecord{
			TerminalID: "term-1",
			Output:     core.NewPackedText(text),
			Truncated:  truncated,
			ExitCode:   exitCode,
			Signal:     signal,
		}
		item := core.TranscriptItem{
			StableID:        "terminal:golden",
			Position:        1,
			CreatedAtMs:     1,
			LastChangedAtMs: 1,
			Body:            core.TranscriptBodyTerminalOutput{Record: record},
		}
		return TranscriptItemText(&item)
	}
	code := func(code uint32) *uint32 { return &code }
	signal := func(signal string) *string { return &signal }
	for _, row := range []struct{ label, row string }{
		{"successful terminal", terminal(code(0), nil, false, "out")},
		{"truncated terminal", terminal(code(1), nil, true, "out")},
		{"signal termination", terminal(nil, signal("SIGKILL"), false, "out")},
		{"released terminal", terminal(nil, nil, false, "out")},
		{"silent signal termination", terminal(nil, signal("SIGTERM"), false, "")},
	} {
		fmt.Fprintf(&output, "\n=== %s (transcript row) ===\n%s\n", row.label, row.row)
	}

	transcriptAppendToolRow(&output, "shell command",
		newTranscriptTestCall("shell", "Terminal").kind(acp.ToolKindExecute).
			rawInput(transcriptCommand([]string{"bash", "-lc", "cd dir && python x.py | cat"})).build())
	transcriptAppendToolRow(&output, "ordinary argv command",
		newTranscriptTestCall("argv", "Execute").kind(acp.ToolKindExecute).
			rawInput(transcriptCommand([]string{"python", "-c", "print(1)"})).build())

	updateCall := newTranscriptTestCall("shell", "Bash").kind(acp.ToolKindExecute).rawInput(`{"command": "cargo test"}`).build()
	original := ToolCallPresentation(updateCall)
	outputUpdate := acp.ToolCallUpdate{RawOutput: transcriptTestJSON(map[string]any{"output": strings.Repeat("x", 128*1024)})}
	fmt.Fprintf(&output, "\n=== output-only update (transcript row) ===\n")
	fmt.Fprintf(&output, "presentation changed: %t\n", ToolCallUpdateChangesPresentation(updateCall, &outputUpdate))
	fmt.Fprintf(&output, "%s\n", original.Summary)
	completed := acp.ToolCallStatusCompleted
	statusUpdate := acp.ToolCallUpdate{Status: &completed}
	fmt.Fprintf(&output, "status-only update changes presentation: %t\n", ToolCallUpdateChangesPresentation(updateCall, &statusUpdate))
	outputCall := newTranscriptTestCall("output", "Bash").kind(acp.ToolKindExecute).build()
	fmt.Fprintf(&output, "output-only call changes presentation: %t\n", ToolCallUpdateChangesPresentation(outputCall, &outputUpdate))
	changedInput := json.RawMessage(`{"command": "cargo check"}`)
	inputUpdate := acp.ToolCallUpdate{RawInput: changedInput}
	fmt.Fprintf(&output, "changed command presentation: %t\n", ToolCallUpdateChangesPresentation(updateCall, &inputUpdate))
	execute := acp.ToolKindExecute
	fmt.Fprintf(&output, "updated row: %s\n", UpdateToolCallPresentation(&original, "Bash", &execute, changedInput, nil).Summary)

	for _, row := range []struct {
		label   string
		command []string
	}{
		{"git registered verb", []string{"git", "--no-pager", "status", "--short"}},
		{"cargo registered verb", []string{"cargo", "+nightly", "test", "--package", "hel"}},
		{"gh registered namespace", []string{"gh", "--hostname", "github.example", "pr", "list"}},
		{"docker registered namespace", []string{"docker", "--context", "work", "compose", "up"}},
		{"podman registered namespace", []string{"podman", "machine", "list"}},
		{"uv registered namespace", []string{"uv", "--project", "app", "pip", "install", "ruff"}},
		{"rustup registered namespace", []string{"rustup", "toolchain", "list"}},
		{"npm registered namespace", []string{"npm", "--prefix", "web", "run", "build"}},
	} {
		transcriptAppendToolRow(&output, row.label,
			newTranscriptTestCall("argv", "Bash").kind(acp.ToolKindExecute).rawInput(transcriptCommand(row.command)).build())
	}

	shell := newTranscriptTestCall("string", "Bash").kind(acp.ToolKindExecute).
		rawInput(transcriptCommand("git --no-pager status --short")).build()
	argv := newTranscriptTestCall("argv", "Bash").kind(acp.ToolKindExecute).
		rawInput(transcriptCommand([]string{"git", "--no-pager", "status", "--short"})).build()
	fmt.Fprintf(&output, "\n=== shell and argv invocation depth (transcript rows) ===\n")
	fmt.Fprintf(&output, "shell: %s\n", transcriptRenderedToolRow(shell))
	fmt.Fprintf(&output, "argv: %s\n", transcriptRenderedToolRow(argv))

	for _, row := range [][2]string{
		{"sudo wrapper", "sudo -n git status"},
		{"env wrapper", "env FOO=bar git status"},
		{"command wrapper", "command git status"},
	} {
		transcriptAppendToolRow(&output, row[0],
			newTranscriptTestCall("wrapper", "Bash").kind(acp.ToolKindExecute).rawInput(transcriptCommand(row[1])).build())
	}

	for _, row := range []struct {
		label   string
		command []string
	}{
		{"unlisted build tool", []string{"mytool", "build", "src"}},
		{"unlisted script", []string{"mytool", "./script.sh"}},
		{"python script", []string{"python", "script.py"}},
		{"gh API namespace", []string{"gh", "api", "graphql"}},
		{"docker run namespace", []string{"docker", "run", "ubuntu"}},
		{"registered future git verb", []string{"git", "future-verb"}},
		{"absolute git with attached directory", []string{"/usr/bin/git", "-Crepo", "status"}},
		{"git short global option", []string{"git", "-c", "core.pager=cat", "status"}},
		{"gh attached repository option", []string{"gh", "-Rorg/repo", "pr", "list"}},
		{"cargo color option", []string{"cargo", "--color=always", "test"}},
		{"cargo config option", []string{"cargo", "--config", "build.jobs=2", "test"}},
	} {
		transcriptAppendToolRow(&output, row.label,
			newTranscriptTestCall("argv", "Bash").kind(acp.ToolKindExecute).rawInput(transcriptCommand(row.command)).build())
	}

	for _, command := range [][]string{
		{"nice", "-n", "10", "python3", "script.py"},
		{"nice", "--adjustment", "10", "python3", "script.py"},
		{"nice", "--adjustment=10", "python3", "script.py"},
		{"nice", "-10", "python3", "script.py"},
		{"nice", "-n10", "python3", "script.py"},
		{"nice", "--", "python3", "script.py"},
	} {
		transcriptAppendToolRow(&output, "nice command options",
			newTranscriptTestCall("nice", "Bash").kind(acp.ToolKindExecute).rawInput(transcriptCommand(command)).build())
	}
	transcriptAppendToolRow(&output, "nice shell command",
		newTranscriptTestCall("nice-shell", "Bash").kind(acp.ToolKindExecute).
			rawInput(transcriptCommand("nice -n 10 python3 script.py")).build())
	for _, row := range []struct {
		label   string
		command any
	}{
		{"timeout shell command", "timeout git status"},
		{"timeout argv command", []string{"timeout", "30s", "git", "-C", "repo", "worktree", "list"}},
		{"timeout command options", []string{
			"timeout",
			"--foreground",
			"--preserve-status",
			"--verbose",
			"--signal",
			"TERM",
			"--kill-after=5s",
			"1.5m",
			"git",
			"status",
		}},
		{"timeout attached short options", "timeout -sTERM -k5s -- 30 git status"},
		{"nested command wrappers", "nice -n 10 timeout 5m nice -- cargo +nightly test -q"},
		{"recursive script runner", "uv run /usr/bin/timeout 5m git status"},
		{"wrapped shell command", "timeout 1h /bin/bash -lc 'git status && cargo test'"},
		{"timeout in a pipeline", "timeout 10 git status | cat && timeout 2m gh pr list"},
		{"timeout unknown option", "nice timeout --mystery 10 git status"},
		{"timeout dynamic duration", "timeout \"$duration\" git status"},
		{"timeout dynamic command", "timeout 30 \"$command\" status"},
		{"wrapped dynamic verb", "timeout 30 git \"$verb\""},
		{"timeout without command", []string{"timeout", "30"}},
		{"script name is not an invocation", "timeout 30 npm run git status"},
		{"ordinary command arguments are not invocations", "timeout 30 python git status"},
		{"ssh remote command", "ssh builder git status --short"},
		{"ssh remote argv", []string{"ssh", "builder", "git", "-C", "repo", "worktree", "list"}},
		{"ssh quoted remote script", "ssh builder 'cd repo && git status | cat'"},
		{"ssh quoted remote argv script", []string{"ssh", "builder", "git status && cargo test -q"}},
		{"ssh connection options", "/usr/bin/ssh -vvT -p 2222 -o BatchMode=yes -i /keys/build builder git status"},
		{"ssh attached options after destination", "ssh builder -Tp2222 -oBatchMode=yes -- uv run pytest -q"},
		{"ssh wrapped remote shell", "timeout 30 ssh -- builder 'bash -lc \"git status && cargo test\"'"},
		{"ssh without remote command", "ssh builder"},
		{"ssh dynamic destination", "ssh \"$host\" git status"},
		{"ssh dynamic remote script", "ssh builder \"cd $dir && git status\""},
		{"ssh dynamic remote command", "ssh builder \"$script\""},
		{"ssh unknown connection option", "ssh --mystery value builder git status"},
		{"ssh unknown option after destination", "ssh builder --mystery value git status"},
		{"ssh without destination", []string{"ssh", "-p", "2222"}},
		{"ssh nesting budget", strings.Repeat("ssh builder ", 2_000) + "git status"},
	} {
		transcriptAppendToolRow(&output, row.label,
			newTranscriptTestCall("recursive", "Bash").kind(acp.ToolKindExecute).rawInput(transcriptCommand(row.command)).build())
	}
	for _, row := range []struct {
		label, command, summary string
		summaryVersion          uint8
	}{
		{"cached wrapper summary", "nice -n 10 timeout 30 git status", "nice timeout", 4},
		{"cached ssh summary", "ssh builder 'git status'", "ssh", 5},
	} {
		cached := newTranscriptTestCall("cached-wrapper", "Bash").kind(acp.ToolKindExecute).
			rawInput(transcriptCommand(row.command)).build()
		presentation := ToolCallPresentation(cached)
		presentation.Summary = row.summary
		presentation.SummaryVersion = row.summaryVersion
		item := core.TranscriptItem{
			StableID:        "tool:cached-wrapper",
			Position:        1,
			CreatedAtMs:     1,
			LastChangedAtMs: 1,
			Body: core.TranscriptBodyTool{
				Call:         core.OpaqueToolCallFromValue(transcriptTestJSON(cached)),
				Presentation: &presentation,
			},
		}
		fmt.Fprintf(&output, "\n=== %s (transcript row) ===\n%s\n", row.label, TranscriptItemText(&item))
	}
	transcriptAppendToolRow(&output, "non-execute tool title",
		newTranscriptTestCall("read", "Read src/lib.rs").kind(acp.ToolKindRead).build())

	assertTranscriptGolden(t, "transcript-tool-row-summary", output.String())
}
