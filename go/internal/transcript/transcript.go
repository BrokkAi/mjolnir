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
	"bytes"
	"cmp"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"slices"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"
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
	tool, ok := (*body).(core.TranscriptBodyTool)
	if !ok {
		return false
	}
	changed := false
	tool.Call.Edit(func(value *any) {
		object, ok := (*value).(jsonvalue.Object)
		if !ok {
			return
		}
		changed = len(tool.TerminalOutputs) != 0 || len(tool.TerminalRefs) != 0
		tool.TerminalOutputs = nil
		tool.TerminalRefs = nil
		// serde_json's Map::remove under preserve_order is swap_remove.
		for _, field := range []string{"rawInput", "rawOutput", "_meta"} {
			if object.SwapDelete(field) {
				changed = true
			}
		}
		*value = object
		member, ok := object.Get("content")
		if !ok {
			return
		}
		content, ok := member.([]any)
		if !ok {
			return
		}
		// Diffs stay, because the transcript still shows their stat. Their
		// patch text does not, and neither do the two file copies an older
		// record holds instead of a patch: core.DropPatchText turns those
		// into the counts formatDiffstat reads before dropping them.
		retained := make([]any, 0, len(content))
		for _, item := range content {
			if kind, ok := transcriptGetString(item, "type"); ok && kind == "diff" {
				retained = append(retained, item)
			}
		}
		if len(retained) != len(content) {
			changed = true
		}
		for i := range retained {
			if dropDiffBody(&retained[i]) {
				changed = true
			}
		}
		object.Set("content", retained)
		*value = object
	})
	*body = tool
	return changed
}

// transcriptGetString is value.get(key).and_then(Value::as_str) on a
// jsonvalue value.
func transcriptGetString(value any, key string) (string, bool) {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return "", false
	}
	member, ok := object.Get(key)
	if !ok {
		return "", false
	}
	text, ok := member.(string)
	return text, ok
}

// dropDiffBody drops a diff's patch text in place. item is one element of a
// tool call's content array, as a jsonvalue value.
//
// Round-trip through ToolCallContent, not Diff: the variant tag lives on the
// enum, and writing back a bare Diff would strip it and make the whole tool
// call unreadable.
func dropDiffBody(item *any) bool {
	encoded, err := jsonvalue.Marshal(*item)
	if err != nil {
		slog.Warn("skipping unreadable tool content during retention", "error", err)
		return false
	}
	var content schema.ToolCallContent
	if err := json.Unmarshal(encoded, &content); err != nil {
		// Content this cannot read is content it must not rewrite.
		slog.Warn("skipping unreadable tool content during retention", "error", err)
		return false
	}
	if content.Diff == nil {
		return false
	}
	if !core.DropPatchText(content.Diff) {
		return false
	}
	rewritten, err := json.Marshal(content)
	if err == nil {
		var value any
		if value, err = jsonvalue.Parse(rewritten); err == nil {
			*item = value
			return true
		}
	}
	slog.Warn("could not rewrite a diff during retention", "error", err)
	return false
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
	kind := toolCallKind(call)
	return (fields.Title != nil && *fields.Title != call.Title) ||
		(fields.Kind != nil && *fields.Kind != kind) ||
		(fields.RawInput != nil && !rawValuePresentAndEqual(fields.RawInput, call.RawInput)) ||
		(kind == acp.ToolKindExecute &&
			fields.RawOutput != nil && !rawValuePresentAndEqual(fields.RawOutput, call.RawOutput) &&
			commandSource(call.RawInput) == nil)
}

// rawValuePresentAndEqual is Rust's `Some(value) == other`: other is
// present and the same JSON value, objects compared without regard to member
// order as serde_json compares them.
func rawValuePresentAndEqual(value, other json.RawMessage) bool {
	if other == nil {
		return false
	}
	left, err := jsonvalue.Parse(value)
	if err != nil {
		return false
	}
	right, err := jsonvalue.Parse(other)
	if err != nil {
		return false
	}
	return jsonValueEqual(left, right)
}

// jsonValueEqual is serde_json::Value equality on jsonvalue values: object
// members compare by key whatever their order, as Rust's Map does.
func jsonValueEqual(a, b any) bool {
	switch x := a.(type) {
	case jsonvalue.Object:
		y, ok := b.(jsonvalue.Object)
		if !ok || len(x) != len(y) {
			return false
		}
		for _, member := range x {
			other, ok := y.Get(member.Key)
			if !ok || !jsonValueEqual(member.Value, other) {
				return false
			}
		}
		return true
	case []any:
		y, ok := b.([]any)
		if !ok || len(x) != len(y) {
			return false
		}
		for i := range x {
			if !jsonValueEqual(x[i], y[i]) {
				return false
			}
		}
		return true
	}
	switch b.(type) {
	case jsonvalue.Object, []any:
		return false
	}
	return jsonvalue.Equal(a, b)
}

// toolCallKind is the call's kind, ToolKind's default (other) when absent.
func toolCallKind(call *acp.ToolCall) acp.ToolKind {
	if call.Kind == nil {
		return acp.ToolKindOther
	}
	return *call.Kind
}

// toolCallStatus is the call's status, ToolCallStatus's default (pending)
// when absent.
func toolCallStatus(call *acp.ToolCall) acp.ToolCallStatus {
	if call.Status == nil {
		return acp.ToolCallStatusPending
	}
	return *call.Status
}

// UpdateToolCall applies a partial ACP update to a complete call: each field
// present in the update replaces the call's. It is the Rust ACP crate's
// ToolCall::update, which acp-go does not provide.
func UpdateToolCall(call *acp.ToolCall, fields *acp.ToolCallUpdate) {
	if fields.Title != nil {
		call.Title = *fields.Title
	}
	if fields.Name != nil {
		name := *fields.Name
		call.Name = &name
	}
	if fields.Kind != nil {
		kind := *fields.Kind
		call.Kind = &kind
	}
	if fields.Status != nil {
		status := *fields.Status
		call.Status = &status
	}
	if fields.Content != nil {
		call.Content = fields.Content
	}
	if fields.Locations != nil {
		call.Locations = fields.Locations
	}
	if fields.RawInput != nil {
		call.RawInput = fields.RawInput
	}
	if fields.RawOutput != nil {
		call.RawOutput = fields.RawOutput
	}
}

// ToolCallPresentation computes the stable presentation metadata for one
// complete ACP call.
func ToolCallPresentation(call *acp.ToolCall) core.ToolCallPresentation {
	kind := toolCallKind(call)
	if kind == acp.ToolKindExecute {
		if source := commandSource(call.RawInput); source != nil {
			return presentationFromSource(source, core.ToolSummarySourceKindRawInput, kind, call.Title)
		}
		if source := commandSource(call.RawOutput); source != nil {
			return presentationFromSource(source, core.ToolSummarySourceKindRawOutput, kind, call.Title)
		}
	}
	return presentationFromTitle(call.Title, kind)
}

// MaterializedToolCallPresentation uses cached presentation data when it was
// produced by current parser rules, otherwise rebuilds it from the complete
// stored call. This lets parser fixes repair existing transcripts while
// current summaries remain cheap to load.
//
// Retention drops a finished call's raw input and output, so a summary made
// from them is kept: rebuilt from what remains, it could only get worse.
func MaterializedToolCallPresentation(stored *core.ToolCallPresentation, call *acp.ToolCall) core.ToolCallPresentation {
	if stored != nil {
		keep := stored.SummaryVersion >= ToolSummaryVersion
		switch stored.SourceKind {
		case core.ToolSummarySourceKindRawInput:
			keep = keep || call.RawInput == nil
		case core.ToolSummarySourceKindRawOutput:
			keep = keep || call.RawOutput == nil
		}
		if keep {
			return *stored
		}
	}
	return ToolCallPresentation(call)
}

// UpdateToolCallPresentation applies the presentation-relevant portion of a
// partial ACP update to cached metadata. ACP updates replace only fields that
// are present, so a title update must not erase a summary selected from an
// earlier raw command. A nil rawInput or rawOutput is an absent field.
func UpdateToolCallPresentation(previous *core.ToolCallPresentation, title string, kind *acp.ToolKind, rawInput, rawOutput json.RawMessage) core.ToolCallPresentation {
	nextKind := acp.ToolKindOther
	if kind != nil {
		nextKind = *kind
	} else if previous != nil {
		nextKind = previous.ToolKind
	}

	if nextKind == acp.ToolKindExecute {
		if rawInput != nil {
			if source := commandSource(rawInput); source != nil {
				return presentationFromSource(source, core.ToolSummarySourceKindRawInput, nextKind, title)
			}
		}
		if rawOutput != nil {
			if source := commandSource(rawOutput); source != nil &&
				(rawInput != nil || !(previous != nil && previous.SourceKind == core.ToolSummarySourceKindRawInput)) {
				return presentationFromSource(source, core.ToolSummarySourceKindRawOutput, nextKind, title)
			}
		}
		if previous != nil && previous.ToolKind == acp.ToolKindExecute &&
			((rawInput == nil && previous.SourceKind == core.ToolSummarySourceKindRawInput) ||
				(rawInput == nil && rawOutput == nil && previous.SourceKind == core.ToolSummarySourceKindRawOutput)) {
			kept := *previous
			kept.ToolKind = nextKind
			return kept
		}
	}

	return presentationFromTitle(title, nextKind)
}

// commandSource reads the command a raw input or output names. nil is none.
func commandSource(raw json.RawMessage) toolSummarySource {
	if raw == nil {
		return nil
	}
	value, err := jsonvalue.Parse(raw)
	if err != nil {
		return nil
	}
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil
	}
	command, ok := object.Get("command")
	if !ok {
		return nil
	}
	switch command := command.(type) {
	case string:
		if strings.TrimSpace(command) == "" {
			return nil
		}
		return toolSummarySourceShell{script: command}
	case []any:
		argv := make([]string, len(command))
		for i, value := range command {
			text, ok := value.(string)
			if !ok {
				return nil
			}
			argv[i] = text
		}
		if len(argv) == 0 {
			return nil
		}
		first := strings.TrimSpace(argv[0])
		if first == "" {
			return nil
		}
		if isShellInterpreter(first) {
			if script, ok := shellScriptArgument(argv[1:]); ok {
				return toolSummarySourceShell{script: script}
			}
		}
		return toolSummarySourceArgv{executable: first, arguments: slices.Clone(argv[1:])}
	}
	return nil
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
	root, ok := parseShell(command)
	if !ok {
		return "", false
	}
	if root.HasError() || root.NamedChildCount() != 1 {
		return "", false
	}
	node, ok := root.NamedChild(0)
	if !ok || node.Kind() != "command" {
		return "", false
	}
	nameNode, ok := node.ChildByFieldName("name")
	if !ok {
		return "", false
	}
	name, ok := shellCommandName(nameNode, command)
	if !ok {
		return "", false
	}
	arguments := node.ChildrenByFieldName("argument")
	// Only the name and its arguments: an environment assignment before the
	// shell is part of what the command does.
	if !isShellInterpreter(name) || node.NamedChildCount() != len(arguments)+1 {
		return "", false
	}
	words := make([]string, len(arguments))
	for i, argument := range arguments {
		words[i] = nodeText(argument, command)
	}
	index, ok := shellScriptIndex(words)
	if !ok {
		return "", false
	}
	return shellLiteralText(arguments[index], command)
}

// shellLiteralText is the text a shell passes on for a quoted or plain word.
// Codex quotes a script holding both kinds of quote as several quoted pieces
// in a row, such as `"rg '"'^fn'"' src"`, so a concatenation is the text of
// its pieces.
func shellLiteralText(node shellNode, source string) (string, bool) {
	text := nodeText(node, source)
	switch node.Kind() {
	case "raw_string":
		if len(text) < 2 || text[0] != '\'' || text[len(text)-1] != '\'' {
			return "", false
		}
		return text[1 : len(text)-1], true
	case "string":
		if len(text) < 2 || text[0] != '"' || text[len(text)-1] != '"' {
			return "", false
		}
		return unescapeDoubleQuoted(text[1 : len(text)-1]), true
	case "word":
		return unescapeWord(text), true
	case "concatenation":
		end := node.StartByte()
		var joined strings.Builder
		for _, piece := range node.Children() {
			// Every byte must belong to a piece this reads.
			if piece.StartByte() != end {
				return "", false
			}
			end = piece.EndByte()
			pieceText, ok := shellLiteralText(piece, source)
			if !ok {
				return "", false
			}
			joined.WriteString(pieceText)
		}
		if end != node.EndByte() {
			return "", false
		}
		return joined.String(), true
	}
	return "", false
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
	var unescaped strings.Builder
	characters := []rune(text)
	for i := 0; i < len(characters); i++ {
		character := characters[i]
		if character == '\\' && i+1 < len(characters) {
			switch characters[i+1] {
			case '$', '`', '"', '\\':
				i++
				unescaped.WriteRune(characters[i])
				continue
			case '\n':
				i++
				continue
			}
		}
		unescaped.WriteRune(character)
	}
	return unescaped.String()
}

func presentationFromSource(source toolSummarySource, sourceKind core.ToolSummarySourceKind, toolKind acp.ToolKind, title string) core.ToolCallPresentation {
	var text, summary string
	switch source := source.(type) {
	case toolSummarySourceShell:
		bounded := boundSummarySource(source.script)
		script, wrapped := wrappedShellScript(bounded)
		inner, summarized := "", false
		if wrapped {
			inner, summarized = summarizeShell(script, 1)
		}
		if summarized {
			text, summary = script, inner
		} else {
			var ok bool
			if summary, ok = summarizeShell(bounded, 0); !ok {
				if summary, ok = firstMeaningfulToken(title); !ok {
					summary = "tool"
				}
			}
			text = bounded
		}
	case toolSummarySourceArgv:
		joined := strings.Join(append([]string{source.executable}, source.arguments...), " ")
		text = boundSummarySource(joined)
		var ok bool
		if summary, ok = summarizeInvocation(source.executable, source.arguments); !ok {
			if summary, ok = firstMeaningfulToken(title); !ok {
				summary = "tool"
			}
		}
	}
	return core.ToolCallPresentation{
		Summary:        summary,
		Source:         core.NewPackedText(text),
		SourceKind:     sourceKind,
		ToolKind:       toolKind,
		SummaryVersion: ToolSummaryVersion,
	}
}

func presentationFromTitle(title string, toolKind acp.ToolKind) core.ToolCallPresentation {
	source := titleSource(title)
	bounded := boundSummarySource(source)
	var summary string
	var ok bool
	switch {
	case toolKind == acp.ToolKindExecute:
		if summary, ok = summarizeShell(bounded, 0); !ok {
			if summary, ok = firstMeaningfulToken(bounded); !ok {
				summary = "tool"
			}
		}
	case strings.HasSuffix(strings.TrimRightFunc(bounded, unicode.IsSpace), "?"):
		// A question tool (Claude's AskUserQuestion) is titled with the
		// question; its first word alone ("Which") says nothing.
		summary = strings.TrimSpace(bounded)
	default:
		if summary, ok = firstMeaningfulToken(bounded); !ok {
			summary = "tool"
		}
	}
	return core.ToolCallPresentation{
		Summary:        summary,
		Source:         core.NewPackedText(bounded),
		SourceKind:     core.ToolSummarySourceKindTitle,
		ToolKind:       toolKind,
		SummaryVersion: ToolSummaryVersion,
	}
}

func titleSource(title string) string {
	title = strings.TrimSpace(title)
	if rest, ok := strings.CutPrefix(title, "Running:"); ok {
		title = strings.TrimSpace(rest)
	} else if rest, ok := strings.CutPrefix(title, "Starting background:"); ok {
		title = strings.TrimSpace(rest)
	}
	if inner, ok := strings.CutPrefix(title, "Execute `"); ok {
		if inner, ok := strings.CutSuffix(inner, "`"); ok {
			return inner
		}
	}
	return title
}

func firstMeaningfulToken(value string) (string, bool) {
	fields := strings.Fields(value)
	if len(fields) == 0 {
		return "", false
	}
	token := strings.TrimFunc(fields[0], func(character rune) bool {
		return !rustIsAlphanumeric(character) && character != '/' && character != '.' && character != '_'
	})
	if token == "" {
		return "", false
	}
	return strings.Trim(token, "'\"`"), true
}

// rustIsAlphanumeric is Rust's char::is_alphanumeric: the Alphabetic or
// Numeric property.
func rustIsAlphanumeric(c rune) bool {
	return unicode.In(c, unicode.L, unicode.Nl, unicode.Other_Alphabetic, unicode.Nd, unicode.No)
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
	// SSH chains need no quoting, so the byte bound alone cannot bound their
	// recursive parsing. Keep nested shell scripts within the same budget.
	if nesting >= toolSummaryMaxNesting {
		return "…", true
	}
	root, ok := parseShell(source)
	if !ok || root.HasError() {
		return "", false
	}

	var commands []shellCommandToken
	var operators []shellOperatorToken
	var subshells []shellSubshell
	if !collectShellTokens(root, source, nesting, &commands, &operators, &subshells) {
		return "", false
	}
	// A script whose every command is named by an expansion says nothing the
	// tool's title does not.
	if !slices.ContainsFunc(commands, func(command shellCommandToken) bool {
		return command.summary != unnamedCommandSummary
	}) {
		return "", false
	}
	slices.SortStableFunc(commands, func(a, b shellCommandToken) int { return cmp.Compare(a.start, b.start) })
	slices.SortStableFunc(operators, func(a, b shellOperatorToken) int { return cmp.Compare(a.start, b.start) })
	slices.SortStableFunc(subshells, func(a, b shellSubshell) int { return cmp.Compare(a.start, b.start) })

	var tokens []shellToken
	for index := range commands {
		command := &commands[index]
		if index > 0 {
			separator := shellSeparatorBetween(&commands[index-1], command, operators)
			tokens = append(tokens, shellToken{start: separator.start, text: separator.kind, order: 1})
		}
		tokens = append(tokens, shellToken{start: command.start, text: command.summary, order: 2})
	}

	// Parentheses are meaningful only for subshells that contain a command
	// we retained. Other punctuation, such as case arms and group
	// delimiters, is structural and must not leak into the compact summary.
	for _, subshell := range subshells {
		if !slices.ContainsFunc(commands, func(command shellCommandToken) bool {
			return command.start >= subshell.start && command.end <= subshell.end
		}) {
			continue
		}
		closing := max(subshell.end-1, 0)
		tokens = append(tokens,
			shellToken{start: subshell.start, text: "(", order: 0},
			shellToken{start: closing, text: ")", order: 3})
	}

	slices.SortStableFunc(tokens, func(a, b shellToken) int {
		if a.start != b.start {
			return cmp.Compare(a.start, b.start)
		}
		return cmp.Compare(a.order, b.order)
	})
	texts := make([]string, len(tokens))
	for i, token := range tokens {
		texts[i] = token.text
	}
	return joinShellTokens(texts), true
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
	kind := node.Kind()
	if kind == "command_substitution" || kind == "process_substitution" {
		return true
	}
	if kind == "command" {
		summary, ok := summarizeCommandNode(node, source, nesting)
		if !ok {
			summary = unnamedCommandSummary
		}
		*commands = append(*commands, shellCommandToken{start: node.StartByte(), end: node.EndByte(), summary: summary})
		return true
	}
	if kind == "variable_assignment" {
		return collectAssignmentSubstitutions(node, source, nesting, commands, operators, subshells)
	}
	if isShellOperator(node) {
		*operators = append(*operators, shellOperatorToken{start: node.StartByte(), kind: kind})
		return true
	}

	if kind == "subshell" {
		*subshells = append(*subshells, shellSubshell{start: node.StartByte(), end: node.EndByte()})
	}

	for _, child := range node.Children() {
		if !collectShellTokens(child, source, nesting, commands, operators, subshells) {
			return false
		}
	}
	return true
}

// collectAssignmentSubstitutions: an assignment that is a statement of its
// own, such as `T=$(git grep -l x)`, runs the commands substituted into its
// value. An assignment before a command is part of that command and never
// gets here.
func collectAssignmentSubstitutions(node shellNode, source string, nesting int, commands *[]shellCommandToken, operators *[]shellOperatorToken, subshells *[]shellSubshell) bool {
	for _, child := range node.Children() {
		if child.Kind() == "command_substitution" {
			for _, grandchild := range child.Children() {
				if !collectShellTokens(grandchild, source, nesting, commands, operators, subshells) {
					return false
				}
			}
		} else if !collectAssignmentSubstitutions(child, source, nesting, commands, operators, subshells) {
			return false
		}
	}
	return true
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
	name, ok := node.ChildByFieldName("name")
	if !ok {
		return "", false
	}
	executable, ok := shellCommandName(name, source)
	if !ok {
		return "", false
	}
	argumentNodes := node.ChildrenByFieldName("argument")
	arguments := make([]invocationArgument, len(argumentNodes))
	for i, argument := range argumentNodes {
		if value, ok := shellArgumentValue(argument, source); ok {
			arguments[i] = invocationArgument{value: value, literal: true}
		} else {
			arguments[i] = invocationArgument{
				value:   stripMatchingQuotes(strings.TrimSpace(nodeText(argument, source))),
				literal: false,
			}
		}
	}
	return summarizeInvocationWithLiterals(executable, arguments, nesting)
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
	executable, ok := normalizeCommandName(executable)
	if !ok {
		return "", false
	}
	var words []string
	// Reuse the invocation rules at each wrapper, consuming arguments rather
	// than growing the call stack for long chains of nested commands.
invocations:
	for {
		basename := executable
		if index := strings.LastIndexAny(executable, `/\`); index >= 0 {
			basename = executable[index+1:]
		}
		if isShellInterpreter(basename) {
			words = append(words, summarizeShellInvocation(basename, arguments, nesting))
			break invocations
		}
		words = append(words, executable)
		if !isSummaryExecutable(basename) {
			break invocations
		}
		index := 0

		if (basename == "cargo" || basename == "mbx") && len(arguments) > 0 &&
			arguments[0].literal && strings.HasPrefix(arguments[0].value, "+") && len(arguments[0].value) > 1 {
			index++
		}

		var firstVerb string
		for {
			if index >= len(arguments) {
				break invocations
			}
			argument := arguments[index]
			if !argument.literal {
				break invocations
			}
			if argument.value == "--" || strings.HasPrefix(argument.value, "-") {
				if consumed, ok := knownLeadingOptionArguments(basename, arguments, index); ok {
					index += consumed
					continue
				}
				break invocations
			}
			firstVerb = argument.value
			break
		}

		if basename == "ssh" {
			words = append(words, firstVerb)
			index++
			// OpenSSH also accepts options after the destination. The rest
			// is joined with spaces and interpreted by the remote shell.
			for index < len(arguments) {
				argument := arguments[index]
				if !argument.literal || !strings.HasPrefix(argument.value, "-") {
					break
				}
				consumed, ok := knownLeadingOptionArguments("ssh", arguments, index)
				if !ok {
					break invocations
				}
				index += consumed
				if argument.value == "--" {
					break
				}
			}
			values := make([]string, 0, len(arguments)-index)
			for _, argument := range arguments[index:] {
				values = append(values, argument.value)
			}
			script := strings.Join(values, " ")
			if script != "" {
				if inner, ok := summarizeShell(script, nesting+1); ok {
					words = append(words, inner)
				}
			}
			break invocations
		}

		if basename == "timeout" {
			duration := firstVerb
			if last := len(duration) - 1; last >= 0 && strings.IndexByte("smhd", duration[last]) >= 0 {
				duration = duration[:last]
			}
			if parsesAsRustF64(duration) {
				index++
				if index >= len(arguments) || !arguments[index].literal {
					break invocations
				}
				firstVerb = arguments[index].value
			}
		}

		if basename == "nice" || basename == "timeout" {
			inner, ok := normalizeCommandName(firstVerb)
			if !ok {
				break invocations
			}
			executable = inner
			arguments = arguments[index+1:]
			continue
		}
		words = append(words, firstVerb)

		if allowsSecondVerb(basename, firstVerb) && index+1 < len(arguments) {
			argument := arguments[index+1]
			if argument.literal && !strings.HasPrefix(argument.value, "-") && argument.value != "--" {
				// `uv run` is followed by a program, which may be given as a
				// path.
				if basename == "uv" && firstVerb == "run" {
					name := argument.value
					if slash := strings.LastIndexByte(name, '/'); slash >= 0 {
						name = name[slash+1:]
					}
					inner, ok := normalizeCommandName(name)
					if !ok {
						break invocations
					}
					executable = inner
					arguments = arguments[index+2:]
					continue
				}
				words = append(words, argument.value)
			}
		}
		break invocations
	}
	return strings.Join(words, " "), true
}

// parsesAsRustF64 reports whether Rust's `str::parse::<f64>` accepts text:
// an optionally signed decimal number, `inf`, `infinity` or `nan` in any
// case. strconv.ParseFloat also reads Go's hexadecimal floats and digit
// separators, which Rust does not.
func parsesAsRustF64(text string) bool {
	body := text
	if body != "" && (body[0] == '+' || body[0] == '-') {
		body = body[1:]
	}
	switch strings.ToLower(body) {
	case "inf", "infinity", "nan":
		return true
	}
	if body == "" || strings.ContainsRune(body, '_') {
		return false
	}
	for _, c := range body {
		if !(c >= '0' && c <= '9' || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-') {
			return false
		}
	}
	_, err := strconv.ParseFloat(body, 64)
	return err == nil || errors.Is(err, strconv.ErrRange)
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
	if index >= len(arguments) {
		return 0, false
	}
	option := arguments[index].value
	// nextLiteral is Rust's `arguments.get(index + 1).filter(literal).map(|_| 2)`.
	nextLiteral := func() (int, bool) {
		if index+1 < len(arguments) && arguments[index+1].literal {
			return 2, true
		}
		return 0, false
	}
	if option == "--" {
		switch basename {
		case "nice", "timeout", "ssh":
			return 1, true
		}
		return 0, false
	}
	if basename == "ssh" {
		// Short flags can be bundled; a value-taking option consumes the
		// remainder of its bundle, or the next argument when there is none.
		flags, ok := strings.CutPrefix(option, "-")
		if !ok {
			return 0, false
		}
		for offset, flag := range flags {
			if strings.ContainsRune("BbcDEeFIiJLlmOoPpQRSWw", flag) {
				if offset+utf8.RuneLen(flag) < len(flags) {
					return 1, true
				}
				return nextLiteral()
			}
			if !strings.ContainsRune("46AaCfGgKkMNnqsTtVvXxYy", flag) {
				return 0, false
			}
		}
		if flags == "" {
			return 0, false
		}
		return 1, true
	}
	optionName, attachedValue := option, false
	if name, _, ok := strings.Cut(option, "="); ok {
		optionName, attachedValue = name, true
	}
	if basename == "nice" && strings.HasPrefix(option, "-") && len(option) > 1 && parsesAsRustI32(option[1:]) {
		return 1, true
	}
	if basename == "nice" {
		if value, ok := strings.CutPrefix(option, "-n"); ok && value != "" && parsesAsRustI32(value) {
			return 1, true
		}
	}
	hasAnyPrefix := func(prefixes ...string) bool {
		for _, prefix := range prefixes {
			if strings.HasPrefix(option, prefix) {
				return true
			}
		}
		return false
	}
	isOneOf := func(names ...string) bool {
		return slices.Contains(names, optionName)
	}
	attachedShortValue := false
	switch basename {
	case "git":
		attachedShortValue = hasAnyPrefix("-C", "-c")
	case "gh":
		attachedShortValue = hasAnyPrefix("-R")
	case "docker", "podman":
		attachedShortValue = hasAnyPrefix("-H")
	case "mj":
		attachedShortValue = hasAnyPrefix("-i")
	case "timeout":
		attachedShortValue = hasAnyPrefix("-s", "-k")
	case "tmux":
		attachedShortValue = hasAnyPrefix("-L", "-S", "-f", "-c", "-T")
	}
	attachedShortValue = attachedShortValue && len(option) > 2
	takesValue := false
	switch basename {
	case "git":
		takesValue = isOneOf("-C", "-c", "--config-env", "--exec-path", "--git-dir", "--namespace",
			"--super-prefix", "--work-tree")
	case "gh":
		takesValue = isOneOf("-R", "--hostname", "--repo", "--jq", "--template")
	case "cargo":
		takesValue = isOneOf("--manifest-path", "--target-dir", "--config", "--color")
	case "npm", "pnpm", "yarn", "bun":
		takesValue = isOneOf("--cwd", "--dir", "--prefix", "--registry", "--userconfig")
	case "uv":
		takesValue = isOneOf("--directory", "--project", "--python")
	case "rustup":
		takesValue = isOneOf("--toolchain")
	case "mj":
		takesValue = isOneOf("-i", "--instance", "--workspace")
	case "tmux":
		takesValue = isOneOf("-L", "-S", "-f", "-c", "-T")
	case "nice":
		takesValue = isOneOf("-n", "--adjustment")
	case "timeout":
		takesValue = isOneOf("-s", "--signal", "-k", "--kill-after")
	case "docker", "podman":
		takesValue = isOneOf("-H", "--config", "--connection", "--context", "--host", "--log-level")
	}
	if attachedShortValue {
		return 1, true
	}
	if attachedValue {
		if takesValue {
			return 1, true
		}
		return 0, false
	}
	if takesValue {
		return nextLiteral()
	}
	knownFlag := false
	switch basename {
	case "git":
		knownFlag = isOneOf("-p", "--paginate", "-P", "--no-pager", "--bare", "--literal-pathspecs",
			"--glob-pathspecs", "--noglob-pathspecs", "--icase-pathspecs", "--no-optional-locks",
			"--no-advice")
	case "cargo":
		knownFlag = isOneOf("-q", "--quiet", "-v", "--verbose", "--locked", "--offline", "--frozen")
	case "npm", "pnpm", "yarn", "bun":
		knownFlag = isOneOf("-g", "--global", "--silent")
	case "uv":
		knownFlag = isOneOf("-q", "--quiet", "-v", "--verbose", "--offline")
	case "rustup":
		knownFlag = isOneOf("-q", "--quiet", "-v", "--verbose")
	case "docker", "podman":
		knownFlag = isOneOf("-D", "--debug", "--tls")
	case "tmux":
		knownFlag = isOneOf("-2", "-C", "-CC", "-D", "-l", "-N", "-u", "-v")
	case "timeout":
		knownFlag = isOneOf("-f", "--foreground", "-p", "--preserve-status", "-v", "--verbose")
	}
	if knownFlag {
		return 1, true
	}
	return 0, false
}

// parsesAsRustI32 reports whether Rust's `str::parse::<i32>` accepts text:
// an optional sign and decimal digits within range.
func parsesAsRustI32(text string) bool {
	_, err := strconv.ParseInt(text, 10, 32)
	return err == nil
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
	value, err := jsonvalue.Parse(input)
	if err != nil {
		return core.SanitizeTerminalText(string(input))
	}
	var text string
	if object, ok := value.(jsonvalue.Object); ok && len(object) != 0 {
		parts := make([]string, len(object))
		for i, member := range object {
			parts[i] = member.Key + ":\n" + jsonValueText(member.Value)
		}
		text = strings.Join(parts, "\n\n")
	} else {
		text = jsonValueText(value)
	}
	return core.SanitizeTerminalText(text)
}

// jsonValueText is a string's own text, or any other value as
// serde_json::to_string_pretty writes it.
func jsonValueText(value any) string {
	if text, ok := value.(string); ok {
		return text
	}
	return prettyJSON(value)
}

// prettyJSON is serde_json::to_string_pretty of a jsonvalue value: two-space
// indentation, `": "` between a key and its value, and `[]` and `{}` for
// empty arrays and objects.
func prettyJSON(value any) string {
	compact, err := jsonvalue.Marshal(value)
	if err != nil {
		panic(fmt.Sprintf("JSON values are serializable: %v", err))
	}
	var pretty bytes.Buffer
	if err := json.Indent(&pretty, compact, "", "  "); err != nil {
		panic(fmt.Sprintf("JSON values are serializable: %v", err))
	}
	return pretty.String()
}

// ToolContentDetails is the readable detail of a tool call's content: its
// text blocks and the output of the terminals it ran.
func ToolContentDetails(content []acp.ToolCallContent, terminalOutputs []core.TerminalOutputRecord, rawOutput json.RawMessage, status core.ToolStatus) []string {
	terminalFinished := func(record *core.TerminalOutputRecord) bool {
		return !(status == core.ToolStatusPending || status == core.ToolStatusRunning) ||
			record.ExitCode != nil || record.Signal != nil
	}
	terminalDetail := func(record *core.TerminalOutputRecord) string {
		if !terminalFinished(record) {
			if record.Truncated {
				return record.Output.Text() + "\noutput truncated"
			}
			return record.Output.Text()
		}
		return TerminalOutputDetail(record)
	}
	details := []string{}
	var referenced []string
	for _, item := range content {
		var detail string
		var ok bool
		switch {
		case item.Content != nil:
			detail, ok = core.ContentBlockText(&item.Content.Content)
		case item.Diff != nil:
		// Kimi-style agents send a terminal reference and no textual copy
		// of the output, so the record hel captured is the only thing a
		// reader ever sees. Until the terminal is reaped there is none.
		case item.Terminal != nil:
			terminalID := string(item.Terminal.TerminalID)
			referenced = append(referenced, terminalID)
			ok = true
			found := false
			for i := range terminalOutputs {
				if terminalOutputs[i].TerminalID == terminalID {
					detail, found = terminalDetail(&terminalOutputs[i]), true
					break
				}
			}
			if !found && rawOutput != nil {
				detail, found = rawOutputTerminalDetail(rawOutput)
			}
			if !found {
				detail = "terminal " + terminalID
			}
		}
		if ok {
			details = append(details, core.SanitizeTerminalText(detail))
		}
	}
	// Grok-style agents name the terminal on a mid-flight update and then
	// replace `content` wholesale without it, so the output hel captured has
	// nothing in the final call pointing at it. Show it rather than lose it.
	for i := range terminalOutputs {
		record := &terminalOutputs[i]
		if slices.Contains(referenced, record.TerminalID) {
			continue
		}
		output := core.SanitizeTerminalText(record.Output.Text())
		if output != "" && slices.Contains(details, output) {
			// Kimi sends the captured stdout as ordinary tool content and in
			// its raw result. Keep the exit summary without printing those
			// same bytes a second time in Raw mode.
			if terminalFinished(record) {
				details = append(details, terminalExitSummary(record))
			}
		} else {
			details = append(details, core.SanitizeTerminalText(terminalDetail(record)))
		}
	}
	return details
}

// ToolErrorDetail reads a textual diagnostic from an ACP provider-specific
// `rawOutput.error`. Other raw output is deliberately not exposed here:
// successful result payloads can contain arbitrary data and belong to the
// tool-specific view. nil is none.
func ToolErrorDetail(rawOutput json.RawMessage) *string {
	if rawOutput == nil {
		return nil
	}
	value, err := jsonvalue.Parse(rawOutput)
	if err != nil {
		return nil
	}
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil
	}
	diagnostic, ok := object.Get("error")
	if !ok || diagnostic == nil {
		return nil
	}
	text := jsonValueText(diagnostic)
	if text == "" {
		return nil
	}
	sanitized := core.SanitizeTerminalText(text)
	return &sanitized
}

// rawOutputTerminalDetail is the output codex reports for a terminal it ran
// itself. Codex names its own server-side terminal, which hel never opened
// and has no record for, and puts the text in `rawOutput`; reading it here
// keeps such a call from rendering as a bare terminal id.
func rawOutputTerminalDetail(rawOutput json.RawMessage) (string, bool) {
	value, err := jsonvalue.Parse(rawOutput)
	if err != nil {
		return "", false
	}
	output, ok := transcriptGetString(value, "formatted_output")
	if !ok {
		return "", false
	}
	exitCode, ok := jsonAsI64(jsonMember(value, "exit_code"))
	if !ok {
		return output, true
	}
	summary := fmt.Sprintf("exited %d", exitCode)
	if output == "" {
		return summary, true
	}
	return output + "\n" + summary, true
}

// jsonMember is serde_json's value.get(key) on a jsonvalue value: the
// member of an object, nil (absent) for anything else.
func jsonMember(value any, key string) any {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil
	}
	member, _ := object.Get(key)
	return member
}

// jsonAsI64 is serde_json's Value::as_i64: an integer that fits i64.
func jsonAsI64(value any) (int64, bool) {
	switch number := value.(type) {
	case int64:
		return number, true
	case uint64:
		if number <= math.MaxInt64 {
			return int64(number), true
		}
	}
	return 0, false
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
	paths := []string{}
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
	text = core.SanitizeTerminalText(text)
	if count := len(*entries); count > 0 {
		last := &(*entries)[count-1]
		if last.Role == role && (role == core.ChatRoleThought || optionalStringEqual(last.MessageID, messageID)) {
			last.Touch(seq)
			if role == core.ChatRoleThought && !optionalStringEqual(last.MessageID, messageID) &&
				last.Text != "" && text != "" {
				last.Text = strings.TrimRight(last.Text, "\n") + "\n" + strings.TrimLeft(text, "\n")
			} else {
				last.Text += text
			}
			return
		}
	}
	entry := core.ChatEntryPlain(seq, role, text).WithRecordedAt(recordedAtMs)
	entry.MessageID = messageID
	*entries = append(*entries, entry)
}

// optionalStringEqual is Option<String> equality.
func optionalStringEqual(a, b *string) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
}

// chunkMessageID is a chunk's message ID as a string, nil when absent.
func chunkMessageID(id *acp.MessageId) *string {
	if id == nil {
		return nil
	}
	text := string(*id)
	return &text
}

// normalizedRaw is a raw JSON value as serde_json::Value holds and writes
// it: compact, members in order. nil stays nil.
func normalizedRaw(raw json.RawMessage) json.RawMessage {
	if raw == nil {
		return nil
	}
	value, err := jsonvalue.Parse(raw)
	if err != nil {
		return raw
	}
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		return raw
	}
	return encoded
}

// ApplySessionUpdateToEntries applies the transcript-visible part of one ACP
// session update. Returns the update again (ok true) when it changes the
// session surface rather than the transcript, so the chat view handles those
// without decoding twice.
func ApplySessionUpdateToEntries(entries *[]core.ChatEntry, seq uint64, recordedAtMs *int64, update acp.SessionUpdate) (acp.SessionUpdate, bool) {
	switch {
	case update.AgentMessageChunk != nil:
		chunk := update.AgentMessageChunk
		if text, ok := core.ContentBlockText(&chunk.Content); ok {
			pushStreamedEntry(entries, seq, recordedAtMs, core.ChatRoleAgent, chunkMessageID(chunk.MessageID), text)
		}
	case update.AgentThoughtChunk != nil:
		chunk := update.AgentThoughtChunk
		if text, ok := core.ContentBlockText(&chunk.Content); ok {
			pushStreamedEntry(entries, seq, recordedAtMs, core.ChatRoleThought, chunkMessageID(chunk.MessageID), text)
		}
	// PromptAccepted is the canonical local user-message event. ACP user
	// chunks would duplicate it during replay.
	case update.UserMessageChunk != nil:
	case update.ToolCall != nil:
		call := update.ToolCall
		presentation := ToolCallPresentation(call)
		status := core.ToolStatusOf(toolCallStatus(call))
		toolCallID := string(call.ToolCallID)
		entry := core.ChatEntryTool(seq, call.Title, &toolCallID, status).WithRecordedAt(recordedAtMs)
		markToolEnd(&entry, recordedAtMs)
		summary := presentation.Summary
		entry.ToolSummary = &summary
		entry.ToolPresentation = &presentation
		entry.ToolInput = normalizedRaw(call.RawInput)
		entry.ToolError = ToolErrorDetail(call.RawOutput)
		entry.ToolContent = ToolContentDetails(call.Content, nil, call.RawOutput, status)
		entry.ToolDiffstats = ToolDiffPaths(call.Content)
		entry.ToolLocations = ToolLocationDetails(call.Locations)
		*entries = append(*entries, entry)
	case update.ToolCallUpdate != nil:
		fields := update.ToolCallUpdate
		toolCallID := string(fields.ToolCallID)
		var entry *core.ChatEntry
		for i := len(*entries) - 1; i >= 0; i-- {
			candidate := &(*entries)[i]
			if candidate.Role == core.ChatRoleTool && candidate.ToolCallID != nil && *candidate.ToolCallID == toolCallID {
				entry = candidate
				break
			}
		}
		if entry == nil {
			return acp.SessionUpdate{}, false
		}
		entry.Touch(seq)
		rawInput, rawOutput := fields.RawInput, fields.RawOutput
		if rawInput != nil {
			entry.ToolInput = normalizedRaw(rawInput)
		}
		if rawOutput != nil {
			entry.ToolError = ToolErrorDetail(rawOutput)
		}
		if fields.Title != nil {
			entry.Text = core.SanitizeTerminalText(*fields.Title)
		}
		if fields.Status != nil {
			status := core.ToolStatusOf(*fields.Status)
			entry.ToolStatus = &status
			markToolEnd(entry, recordedAtMs)
		}
		if fields.Content != nil {
			status := core.ToolStatusPending
			if entry.ToolStatus != nil {
				status = *entry.ToolStatus
			}
			entry.ToolContent = ToolContentDetails(fields.Content, nil, rawOutput, status)
			entry.ToolDiffstats = ToolDiffPaths(fields.Content)
		}
		if fields.Locations != nil {
			entry.ToolLocations = ToolLocationDetails(fields.Locations)
		}
		presentation := UpdateToolCallPresentation(entry.ToolPresentation, entry.Text, fields.Kind, rawInput, rawOutput)
		summary := presentation.Summary
		entry.ToolSummary = &summary
		entry.ToolPresentation = &presentation
	case update.Plan != nil:
		lines := make([]core.PlanLine, len(update.Plan.Entries))
		for i, entry := range update.Plan.Entries {
			lines[i] = core.PlanLine{Text: core.SanitizeTerminalText(entry.Content), Status: core.PlanStatusOf(entry.Status)}
		}
		// Turn order, not authorship: a plan made after any prompt,
		// including one Mjolnir submitted, belongs below that prompt.
		var latestUserSeq uint64
		for i := len(*entries) - 1; i >= 0; i-- {
			if (*entries)[i].Role == core.ChatRoleUser {
				latestUserSeq = (*entries)[i].Seq
				break
			}
		}
		for i := len(*entries) - 1; i >= 0; i-- {
			entry := &(*entries)[i]
			if entry.Role == core.ChatRolePlan && entry.Seq > latestUserSeq {
				entry.Touch(seq)
				entry.Plan = lines
				return acp.SessionUpdate{}, false
			}
		}
		*entries = append(*entries, core.ChatEntryPlan(seq, lines))
	default:
		return update, true
	}
	return acp.SessionUpdate{}, false
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
	switch event := runtime.(type) {
	case core.RuntimeEventSessionUpdate:
		// Rust decodes the stable v1 SessionUpdate, so an update only the
		// unstable schema knows is invalid here as it is there.
		var stable schema.SessionUpdate
		if err := json.Unmarshal(event.Update, &stable); err != nil {
			slog.Debug("ignoring invalid ACP session update", "error", err)
			return nil
		}
		var parsed acp.SessionUpdate
		if err := json.Unmarshal(event.Update, &parsed); err != nil {
			slog.Debug("ignoring invalid ACP session update", "error", err)
			return nil
		}
		if _, ok := ApplySessionUpdateToEntries(entries, seq, recordedAtMs, parsed); ok {
			return event
		}
		return nil
	case core.RuntimeEventWarning:
		*entries = append(*entries, core.ChatEntryPlain(seq, core.ChatRoleSystem, "warning: "+event.Message))
		return nil
	case core.RuntimeEventSessionFault:
		*entries = append(*entries, core.ChatEntryPlain(seq, core.ChatRoleSystem, "warning: "+event.Message))
		return nil
	case core.RuntimeEventConfigApplied:
		*entries = append(*entries, core.ChatEntryPlain(seq, core.ChatRoleSystem, event.Key+" set to "+event.Value))
		return nil
	case core.RuntimeEventSessionStarted:
		if !event.Resumed {
			*entries = append(*entries, core.ChatEntryPlain(seq, core.ChatRoleSystem, "harness session started"))
		}
		return nil
	case core.RuntimeEventElicitationResolved:
		if event.Action == core.OperatorAwayAction {
			return nil
		}
		if event.Reply != nil {
			entry := core.ChatEntryPlain(seq, core.ChatRoleUser, *event.Reply).WithRecordedAt(recordedAtMs)
			messageID := fmt.Sprintf("%s%d", core.ElicitationReplyItemPrefix, seq)
			entry.MessageID = &messageID
			*entries = append(*entries, entry)
			return nil
		}
	}
	return runtime
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
	switch body := item.Body.(type) {
	case core.TranscriptBodyUser:
		return core.SanitizeTerminalText(core.MaterializedContentText(body.Content))
	case core.TranscriptBodyMessage:
		text, ok := body.Event.MessageText()
		if !ok {
			panic("message entry metadata")
		}
		return body.Event.MessageLabel() + "\n\n" + core.SanitizeTerminalText(text)
	case core.TranscriptBodyAgent:
		return core.MaterializedChunksText(body.Chunks)
	case core.TranscriptBodyThought:
		return core.MaterializedChunksText(body.Chunks)
	case core.TranscriptBodyTool:
		call, err := body.Call.Decode()
		if err != nil {
			return "[invalid tool call]"
		}
		text := MaterializedToolCallPresentation(body.Presentation, &call).Summary
		if strings.TrimSpace(text) == "" {
			text = call.Title
		}
		for _, record := range outputs.ForTool(item.StableID) {
			text += "\n" + TerminalOutputDetail(&record)
		}
		return core.SanitizeTerminalText(text)
	case core.TranscriptBodyTerminalOutput:
		if outputs.ShownUnderTool(body.Record.TerminalID) {
			return ""
		}
		return core.SanitizeTerminalText(TerminalOutputDetail(&body.Record))
	case core.TranscriptBodyPlan:
		var plan acp.Plan
		if err := json.Unmarshal(body.Plan, &plan); err != nil {
			return ""
		}
		lines := make([]string, len(plan.Entries))
		for i, entry := range plan.Entries {
			var status string
			switch core.PlanStatusOf(entry.Status) {
			case core.PlanStatusPending:
				status = "pending"
			case core.PlanStatusRunning:
				status = "running"
			case core.PlanStatusCompleted:
				status = "completed"
			}
			lines[i] = fmt.Sprintf("[%s] %s", status, core.SanitizeTerminalText(entry.Content))
		}
		return strings.Join(lines, "\n")
	case core.TranscriptBodyPlanProposal:
		return core.SanitizeTerminalText(body.Plan)
	case core.TranscriptBodySystem:
		return core.SanitizeTerminalText(body.Text)
	}
	panic(fmt.Sprintf("unknown transcript body %T", item.Body))
}

func computeToolDiffstats(content []acp.ToolCallContent) []string {
	diffstats := []string{}
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
	body, ok := item.Body.(core.TranscriptBodyTool)
	if !ok {
		return nil, false
	}
	call, err := body.Call.Decode()
	if err != nil {
		slog.Warn("could not decode a stored tool call while reading diff summary",
			"stable_id", item.StableID, "error", err)
		return nil, false
	}
	switch core.ToolStatusOf(toolCallStatus(&call)) {
	case core.ToolStatusCompleted, core.ToolStatusFailed:
	default:
		return nil, false
	}
	diffstats := computeToolDiffstats(call.Content)
	if len(diffstats) == 0 {
		return nil, false
	}
	return diffstats, true
}

// formatDiffstat: a diff recorded since `diff` landed already carries its
// counts, so this is a lookup. An older record still holds both file copies
// and is diffed here on demand. core.PatchOf takes the stable schema's Diff.
func formatDiffstat(diff *acp.Diff) string {
	patch := core.PatchOf(&schema.Diff{Path: diff.Path, OldText: diff.OldText, NewText: diff.NewText, Meta: diff.Meta})
	return fmt.Sprintf("%s  +%d −%d", diff.Path, patch.Insertions, patch.Deletions)
}
