// Port of mj-transcript/src/transcript.rs.
//
// Transcript projection and presentation implementation.
//
// The Rust module re-exports all of mj_core::transcript (`pub use
// mj_core::transcript::*`). Go has no re-exports: callers use package core
// for those items (core.ChatEntry, core.ToolStatus, core.SanitizeTerminalText,
// core.HarnessTurnItemPrefix, ...).
//
// ACP types come from acp-go's unstable schema, as in core's transcript.go:
// core.OpaqueToolCall.Decode returns an unstable acp.ToolCall and
// core.ToolCallPresentation holds an unstable acp.ToolKind.

package transcript

import (
	"encoding/json"
	"fmt"
	"strings"
	"unicode"
	"unicode/utf8"

	acp "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const toolSummarySourceBytes = 64 * 1024
const toolSummaryMaxNesting = 16

// unnamedCommandSummary stands for a command whose name is an expansion,
// such as `$E cargo test`.
const unnamedCommandSummary = "…"

// ToolSummaryVersion is the parser-rule version stored with cached tool
// summaries.
const ToolSummaryVersion uint8 = 8

// CompactToolCallForRetention reduces a tool call to what a reader still
// needs, once a verified checkpoint holds the whole of it.
//
// Tool output is where a projection's bytes are: on one measured session,
// 561 MB of 635 MiB. Behind a checkpoint nothing reads it — the checkpoint
// archive carries the complete transcript, and restoring it brings the output
// back — so what stays here is what the transcript still shows: which tool
// ran, on what, with what result, and how many lines each edit changed.
//
// Returns whether anything changed, so a caller can skip the write. body is
// a pointer to the interface so a Tool body, held by value, can be replaced.
func CompactToolCallForRetention(body *core.TranscriptBody) bool {
	panic("unported: mj-transcript/src/transcript.rs compact_tool_call_for_retention")
}

// dropDiffBody drops a diff's patch text in place. item is one element of a
// tool call's content array, as a jsonvalue value.
//
// Round-trip through ToolCallContent, not Diff: the variant tag lives on the
// enum, and writing back a bare Diff would strip it and make the whole tool
// call unreadable.
func dropDiffBody(item *any) bool {
	panic("unported: mj-transcript/src/transcript.rs drop_diff_body")
}

// toolSummarySource is where a tool summary is read from. Rust enum
// ToolSummarySource { Shell(String), Argv { executable, arguments } }.
type toolSummarySource interface {
	isToolSummarySource()
}

type toolSummarySourceShell struct {
	script string
}

type toolSummarySourceArgv struct {
	executable string
	arguments  []string
}

func (toolSummarySourceShell) isToolSummarySource() {}
func (toolSummarySourceArgv) isToolSummarySource()  {}

// ToolCallUpdateChangesPresentation reports whether a partial update changes
// inputs used to derive the tool summary. acp-go's ToolCallUpdate carries
// the update fields inline (Rust's ToolCallUpdateFields).
func ToolCallUpdateChangesPresentation(call *acp.ToolCall, fields *acp.ToolCallUpdate) bool {
	panic("unported: mj-transcript/src/transcript.rs tool_call_update_changes_presentation")
}

// UpdateToolCall applies a partial ACP update to a complete call: each field
// present in the update replaces the call's. It is the Rust ACP crate's
// ToolCall::update, which acp-go does not provide.
func UpdateToolCall(call *acp.ToolCall, fields *acp.ToolCallUpdate) {
	panic("unported: mj-transcript/src/transcript.rs ToolCall::update (agent-client-protocol)")
}

// ToolCallPresentation computes the stable presentation metadata for one
// complete ACP call.
func ToolCallPresentation(call *acp.ToolCall) core.ToolCallPresentation {
	panic("unported: mj-transcript/src/transcript.rs tool_call_presentation")
}

// MaterializedToolCallPresentation uses cached presentation data when it was
// produced by current parser rules, otherwise rebuilds it from the complete
// stored call. This lets parser fixes repair existing transcripts while
// current summaries remain cheap to load.
//
// Retention drops a finished call's raw input and output, so a summary made
// from them is kept: rebuilt from what remains, it could only get worse.
func MaterializedToolCallPresentation(stored *core.ToolCallPresentation, call *acp.ToolCall) core.ToolCallPresentation {
	panic("unported: mj-transcript/src/transcript.rs materialized_tool_call_presentation")
}

// UpdateToolCallPresentation applies the presentation-relevant portion of a
// partial ACP update to cached metadata. ACP updates replace only fields that
// are present, so a title update must not erase a summary selected from an
// earlier raw command. A nil rawInput or rawOutput is an absent field.
func UpdateToolCallPresentation(previous *core.ToolCallPresentation, title string, kind *acp.ToolKind, rawInput, rawOutput json.RawMessage) core.ToolCallPresentation {
	panic("unported: mj-transcript/src/transcript.rs update_tool_call_presentation")
}

// commandSource reads the command a raw input or output names. nil is none.
func commandSource(raw json.RawMessage) toolSummarySource {
	panic("unported: mj-transcript/src/transcript.rs command_source")
}

func isShellInterpreter(value string) bool {
	executable := value
	if index := strings.LastIndexByte(value, '/'); index >= 0 {
		executable = value[index+1:]
	}
	switch executable {
	case "sh", "bash", "dash", "zsh":
		return true
	}
	return false
}

func shellScriptArgument(arguments []string) (string, bool) {
	index, ok := shellScriptIndex(arguments)
	if !ok {
		return "", false
	}
	return arguments[index], true
}

// shellScriptIndex is where the script a shell runs with `-c` is among its
// arguments.
func shellScriptIndex(arguments []string) (int, bool) {
	for index, argument := range arguments {
		if argument == "--" {
			return 0, false
		}
		if argument == "-c" || argument == "--command" ||
			(strings.HasPrefix(argument, "-") && !strings.HasPrefix(argument, "--") && strings.Contains(argument[1:], "c")) {
			if index+1 < len(arguments) {
				return index + 1, true
			}
			return 0, false
		}
	}
	return 0, false
}

// shellNode is one node of a shell script parsed with the tree-sitter-bash
// grammar, standing in for tree_sitter::Node. Kind is the grammar's node kind
// ("command", "raw_string", "concatenation", ...), and byte offsets index the
// source that was parsed. The Go parser behind it is chosen with the bodies:
// the summaries must match the Rust tests, which follow tree-sitter-bash
// 0.25.1's tree shapes and error recovery.
type shellNode interface {
	Kind() string
	StartByte() int
	EndByte() int
	HasError() bool
	Parent() (shellNode, bool)
	Children() []shellNode
	NamedChildCount() int
	NamedChild(index int) (shellNode, bool)
	ChildByFieldName(name string) (shellNode, bool)
	ChildrenByFieldName(name string) []shellNode
}

// parseShell parses source with the bash grammar and returns the root node.
// ok is false when no tree could be built (Rust's Parser::set_language or
// Parser::parse failing).
func parseShell(source string) (root shellNode, ok bool) {
	panic("unported: mj-transcript/src/transcript.rs Parser::parse (tree-sitter-bash)")
}

// nodeText is the source text a node spans (Rust's &source[node.byte_range()]).
func nodeText(node shellNode, source string) string {
	return source[node.StartByte():node.EndByte()]
}

// wrappedShellScript is the script of a command that is nothing but a shell
// running one `-c` script, such as `/bin/bash -lc "python3 x.py"`. Codex
// reports some commands with the shell it wraps every command in, as one
// string. The wrapper says no more there than it does in an argv call, which
// drops it the same way.
func wrappedShellScript(command string) (string, bool) {
	panic("unported: mj-transcript/src/transcript.rs wrapped_shell_script")
}

// shellLiteralText is the text a shell passes on for a quoted or plain word.
// Codex quotes a script holding both kinds of quote as several quoted pieces
// in a row, such as `"rg '"'^fn'"' src"`, so a concatenation is the text of
// its pieces.
func shellLiteralText(node shellNode, source string) (string, bool) {
	panic("unported: mj-transcript/src/transcript.rs shell_literal_text")
}

// unescapeWord is the text of an unquoted word, where a backslash escapes any
// character and a backslash before a newline joins the lines.
func unescapeWord(text string) string {
	var unescaped strings.Builder
	characters := []rune(text)
	for i := 0; i < len(characters); i++ {
		if characters[i] != '\\' {
			unescaped.WriteRune(characters[i])
			continue
		}
		i++
		if i < len(characters) && characters[i] != '\n' {
			unescaped.WriteRune(characters[i])
		}
	}
	return unescaped.String()
}

// unescapeDoubleQuoted is the text a shell reads from inside double quotes,
// where a backslash escapes only `$`, a backquote, `"`, a backslash, and a
// newline.
func unescapeDoubleQuoted(text string) string {
	panic("unported: mj-transcript/src/transcript.rs unescape_double_quoted")
}

func presentationFromSource(source toolSummarySource, sourceKind core.ToolSummarySourceKind, toolKind acp.ToolKind, title string) core.ToolCallPresentation {
	panic("unported: mj-transcript/src/transcript.rs presentation_from_source")
}

func presentationFromTitle(title string, toolKind acp.ToolKind) core.ToolCallPresentation {
	panic("unported: mj-transcript/src/transcript.rs presentation_from_title")
}

func titleSource(title string) string {
	panic("unported: mj-transcript/src/transcript.rs title_source")
}

func firstMeaningfulToken(value string) (string, bool) {
	panic("unported: mj-transcript/src/transcript.rs first_meaningful_token")
}

func boundSummarySource(source string) string {
	if len(source) <= toolSummarySourceBytes {
		return source
	}
	end := toolSummarySourceBytes
	for !utf8.RuneStart(source[end]) {
		end--
	}
	return source[:end]
}

func summarizeShell(source string, nesting int) (string, bool) {
	panic("unported: mj-transcript/src/transcript.rs summarize_shell")
}

type shellCommandToken struct {
	start   int
	end     int
	summary string
}

type shellOperatorToken struct {
	start int
	kind  string
}

type shellSubshell struct {
	start int
	end   int
}

type shellToken struct {
	start int
	text  string
	order uint8
}

func collectShellTokens(node shellNode, source string, nesting int, commands *[]shellCommandToken, operators *[]shellOperatorToken, subshells *[]shellSubshell) bool {
	panic("unported: mj-transcript/src/transcript.rs collect_shell_tokens")
}

// collectAssignmentSubstitutions: an assignment that is a statement of its
// own, such as `T=$(git grep -l x)`, runs the commands substituted into its
// value. An assignment before a command is part of that command and never
// gets here.
func collectAssignmentSubstitutions(node shellNode, source string, nesting int, commands *[]shellCommandToken, operators *[]shellOperatorToken, subshells *[]shellSubshell) bool {
	panic("unported: mj-transcript/src/transcript.rs collect_assignment_substitutions")
}

func shellSeparatorBetween(previous, next *shellCommandToken, operators []shellOperatorToken) shellOperatorToken {
	var semicolon *shellOperatorToken
	for i := range operators {
		operator := &operators[i]
		if operator.start < previous.end || operator.start >= next.start {
			continue
		}
		if operator.kind != ";" {
			return *operator
		}
		if semicolon == nil {
			semicolon = operator
		}
	}
	if semicolon != nil {
		return *semicolon
	}
	return shellOperatorToken{start: previous.end, kind: ";"}
}

// invocationArgument is one argument of a parsed invocation. value is its
// text without matching quotes; literal is false when the text contains an
// expansion, so value is not what the program receives.
type invocationArgument struct {
	value   string
	literal bool
}

func summarizeCommandNode(node shellNode, source string, nesting int) (string, bool) {
	panic("unported: mj-transcript/src/transcript.rs summarize_command_node")
}

func shellCommandName(node shellNode, source string) (string, bool) {
	if containsDynamicShellNode(node) {
		return "", false
	}
	return normalizeCommandName(nodeText(node, source))
}

func shellArgumentValue(node shellNode, source string) (string, bool) {
	if containsDynamicShellNode(node) {
		return "", false
	}
	text := strings.TrimSpace(nodeText(node, source))
	if text == "" {
		return "", false
	}
	return stripMatchingQuotes(text), true
}

func containsDynamicShellNode(node shellNode) bool {
	switch node.Kind() {
	case "expansion", "simple_expansion", "command_substitution", "process_substitution", "arithmetic_expansion":
		return true
	}
	for _, child := range node.Children() {
		if containsDynamicShellNode(child) {
			return true
		}
	}
	return false
}

func summarizeInvocation(executable string, arguments []string) (string, bool) {
	literals := make([]invocationArgument, len(arguments))
	for i, value := range arguments {
		literals[i] = invocationArgument{value: stripMatchingQuotes(value), literal: true}
	}
	return summarizeInvocationWithLiterals(executable, literals, 0)
}

func summarizeInvocationWithLiterals(executable string, arguments []invocationArgument, nesting int) (string, bool) {
	panic("unported: mj-transcript/src/transcript.rs summarize_invocation_with_literals")
}

// summarizeShellInvocation: a shell is named without its directory, and the
// script it runs with `-c` is summarized in its own right, so
// `/bin/bash -c "python x.py"` reads `bash -c python`. The script is parsed
// even when it contains an expansion, because only its command names matter
// here.
func summarizeShellInvocation(shell string, arguments []invocationArgument, nesting int) string {
	values := make([]string, len(arguments))
	for i, argument := range arguments {
		values[i] = argument.value
	}
	script, ok := shellScriptArgument(values)
	if !ok {
		return shell
	}
	if inner, ok := summarizeShell(script, nesting+1); ok {
		return fmt.Sprintf("%s -c %s", shell, inner)
	}
	return shell + " -c"
}

func isSummaryExecutable(basename string) bool {
	switch basename {
	case "git", "gh", "cargo", "rustup", "npm", "pnpm", "yarn", "bun", "uv", "pip", "pip3",
		"docker", "podman", "nice", "timeout", "ssh", "mbx", "mj", "tmux":
		return true
	}
	return false
}

func allowsSecondVerb(basename, firstVerb string) bool {
	in := func(verbs ...string) bool {
		for _, verb := range verbs {
			if firstVerb == verb {
				return true
			}
		}
		return false
	}
	switch basename {
	// Only commands whose next word is always a subcommand: `git config`
	// and `git reflog` also take a key or ref there.
	case "git":
		return in("bisect", "bundle", "hook", "lfs", "maintenance", "notes", "remote",
			"sparse-checkout", "stash", "submodule", "worktree")
	case "gh":
		return in("alias", "auth", "cache", "codespace", "config", "extension", "gist",
			"gpg-key", "issue", "label", "org", "pr", "project", "release", "repo", "ruleset",
			"run", "search", "secret", "ssh-key", "variable", "workflow")
	case "docker":
		return in("buildx", "compose", "config", "context", "container", "image", "manifest",
			"network", "node", "plugin", "secret", "service", "stack", "swarm", "system",
			"trust", "volume")
	case "podman":
		return in("artifact", "container", "farm", "generate", "image", "machine", "manifest",
			"network", "play", "pod", "secret", "system", "volume")
	case "uv":
		return in("cache", "pip", "python", "run", "tool")
	case "npm":
		return in("run", "run-script")
	case "pnpm", "yarn", "bun":
		return firstVerb == "run"
	case "mbx":
		return in("cache", "explain", "settings")
	case "mj":
		return in("daemon", "import", "recover", "review", "setup", "workspaces")
	case "rustup":
		return in("component", "override", "target", "toolchain")
	}
	return false
}

func knownLeadingOptionArguments(basename string, arguments []invocationArgument, index int) (int, bool) {
	panic("unported: mj-transcript/src/transcript.rs known_leading_option_arguments")
}

func stripMatchingQuotes(value string) string {
	if len(value) >= 2 && value[0] == '"' && value[len(value)-1] == '"' {
		return value[1 : len(value)-1]
	}
	if len(value) >= 2 && value[0] == '\'' && value[len(value)-1] == '\'' {
		return value[1 : len(value)-1]
	}
	return value
}

func isShellOperator(node shellNode) bool {
	parentKind := func() (string, bool) {
		parent, ok := node.Parent()
		if !ok {
			return "", false
		}
		return parent.Kind(), true
	}
	switch node.Kind() {
	case ";":
		return true
	case "&&", "||":
		kind, ok := parentKind()
		return ok && kind == "list"
	case "|", "|&":
		kind, ok := parentKind()
		return ok && kind == "pipeline"
	case "&":
		kind, ok := parentKind()
		return !ok || (kind != "binary_expression" && kind != "unary_expression" && kind != "postfix_expression")
	}
	return false
}

func normalizeCommandName(text string) (string, bool) {
	text = strings.TrimSpace(text)
	if strings.ContainsAny(text, "$`") {
		return "", false
	}
	text = stripMatchingQuotes(text)
	if text == "" {
		return "", false
	}
	return text, true
}

func joinShellTokens(tokens []string) string {
	output := ""
	for _, token := range tokens {
		switch token {
		case "(":
			if output != "" && !strings.HasSuffix(output, " ") {
				output += " "
			}
			output += "("
		case ")":
			output = strings.TrimRightFunc(output, unicode.IsSpace) + ")"
		default:
			if output != "" && !strings.HasSuffix(output, " ") && !strings.HasSuffix(output, "(") {
				output += " "
			}
			output += token
		}
	}
	return output
}

// ToolInputText reads string arguments as text, including their real line
// breaks. Other values keep their JSON structure; the renderer wraps rather
// than clips it.
func ToolInputText(input json.RawMessage) string {
	panic("unported: mj-transcript/src/transcript.rs tool_input_text")
}

// ToolContentDetails is the readable detail of a tool call's content: its
// text blocks and the output of the terminals it ran.
func ToolContentDetails(content []acp.ToolCallContent, terminalOutputs []core.TerminalOutputRecord, rawOutput json.RawMessage, status core.ToolStatus) []string {
	panic("unported: mj-transcript/src/transcript.rs tool_content_details")
}

// ToolErrorDetail reads a textual diagnostic from an ACP provider-specific
// `rawOutput.error`. Other raw output is deliberately not exposed here:
// successful result payloads can contain arbitrary data and belong to the
// tool-specific view. nil is none.
func ToolErrorDetail(rawOutput json.RawMessage) *string {
	panic("unported: mj-transcript/src/transcript.rs tool_error_detail")
}

// rawOutputTerminalDetail is the output codex reports for a terminal it ran
// itself. Codex names its own server-side terminal, which hel never opened
// and has no record for, and puts the text in `rawOutput`; reading it here
// keeps such a call from rendering as a bare terminal id.
func rawOutputTerminalDetail(rawOutput json.RawMessage) (string, bool) {
	panic("unported: mj-transcript/src/transcript.rs raw_output_terminal_detail")
}

// TerminalOutputDetail is one terminal's output followed by how it ended.
func TerminalOutputDetail(record *core.TerminalOutputRecord) string {
	summary := terminalExitSummary(record)
	if record.Output.IsEmpty() {
		return summary
	}
	return record.Output.Text() + "\n" + summary
}

// terminalExitSummary is how a terminal ended, in one line.
func terminalExitSummary(record *core.TerminalOutputRecord) string {
	var summary string
	switch {
	case record.Signal != nil:
		summary = "killed by " + *record.Signal
	case record.ExitCode != nil:
		summary = fmt.Sprintf("exited %d", *record.ExitCode)
	default:
		summary = "exit status unavailable"
	}
	if record.Truncated {
		summary += " · output truncated"
	}
	return summary
}

func ToolDiffPaths(content []acp.ToolCallContent) []string {
	var paths []string
	for _, item := range content {
		if item.Kind == acp.ToolCallContentKindDiff && item.Diff != nil {
			paths = append(paths, item.Diff.Path)
		}
	}
	return paths
}

func ToolLocationDetails(locations []acp.ToolCallLocation) []string {
	details := make([]string, 0, len(locations))
	for _, location := range locations {
		if location.Line != nil {
			details = append(details, fmt.Sprintf("%s:%d", location.Path, *location.Line))
		} else {
			details = append(details, location.Path)
		}
	}
	return details
}

// pushStreamedEntry appends streamed agent or thought text to the
// transcript, merging it into the entry it continues so a message arrives as
// one entry rather than one per chunk.
func pushStreamedEntry(entries *[]core.ChatEntry, seq uint64, recordedAtMs *int64, role core.ChatRole, messageID *string, text string) {
	panic("unported: mj-transcript/src/transcript.rs push_streamed_entry")
}

// ApplySessionUpdateToEntries applies the transcript-visible part of one ACP
// session update. Returns the update again (ok true) when it changes the
// session surface rather than the transcript, so the chat view handles those
// without decoding twice.
func ApplySessionUpdateToEntries(entries *[]core.ChatEntry, seq uint64, recordedAtMs *int64, update acp.SessionUpdate) (acp.SessionUpdate, bool) {
	panic("unported: mj-transcript/src/transcript.rs apply_session_update_to_entries")
}

// markToolEnd records when a tool call finished, the time of the event that
// finished it.
func markToolEnd(entry *core.ChatEntry, recordedAtMs *int64) {
	entry.ToolEndedAtMs = nil
	if entry.ToolStatus != nil && (*entry.ToolStatus == core.ToolStatusCompleted || *entry.ToolStatus == core.ToolStatusFailed) {
		entry.ToolEndedAtMs = recordedAtMs
	}
}

// ApplyRuntimeEventToEntries applies the transcript-visible part of one
// persisted runtime event. Returns the event again when it only configures
// the session surface, which is the chat view's business rather than the
// transcript's; nil when the transcript took it.
func ApplyRuntimeEventToEntries(entries *[]core.ChatEntry, seq uint64, recordedAtMs *int64, runtime core.RuntimeEvent) core.RuntimeEvent {
	panic("unported: mj-transcript/src/transcript.rs apply_runtime_event_to_entries")
}

// TranscriptItemText is one transcript item flattened to the text a reader
// would see.
//
// A caller that wants the structure reads the body itself; this is the plain
// reading, built from the same flatteners every other surface uses so that a
// tool call reads as the command it ran rather than as JSON.
func TranscriptItemText(item *core.TranscriptItem) string {
	return TranscriptItemTextWithOutputs(item, core.TerminalOutputsOf([]core.TranscriptItem{*item}))
}

func TranscriptItemTextWithOutputs(item *core.TranscriptItem, outputs *core.TerminalOutputs[core.TerminalOutputRecord]) string {
	panic("unported: mj-transcript/src/transcript.rs transcript_item_text_with_outputs")
}

func computeToolDiffstats(content []acp.ToolCallContent) []string {
	var diffstats []string
	for _, item := range content {
		if item.Kind == acp.ToolCallContentKindDiff && item.Diff != nil {
			diffstats = append(diffstats, formatDiffstat(item.Diff))
		}
	}
	return diffstats
}

// MaterializedToolDiffstats is the diffstat of a finished tool item; ok is
// false for other items, unfinished calls, and calls without diffs.
func MaterializedToolDiffstats(item *core.TranscriptItem) ([]string, bool) {
	panic("unported: mj-transcript/src/transcript.rs materialized_tool_diffstats")
}

// formatDiffstat: a diff recorded since `diff` landed already carries its
// counts, so this is a lookup. An older record still holds both file copies
// and is diffed here on demand. core.PatchOf takes the stable schema's Diff.
func formatDiffstat(diff *acp.Diff) string {
	panic("unported: mj-transcript/src/transcript.rs format_diffstat")
}
