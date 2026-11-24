// Port of brokk-anvil-client 0.28.6 src/tool_arguments.rs (the part Mjolnir uses).

package anvilclient

import (
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"strings"
	"unicode"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

type NormalizedToolArguments struct {
	// Value is a JSON value.
	Value     any
	Arguments string
	Repaired  bool
}

type ToolArgumentErrorKind int

const (
	ToolArgumentErrorKindInvalid ToolArgumentErrorKind = iota
	ToolArgumentErrorKindIncomplete
)

type ToolArgumentParseError struct {
	kind    ToolArgumentErrorKind
	message string
}

func (e *ToolArgumentParseError) Kind() ToolArgumentErrorKind { return e.kind }
func (e *ToolArgumentParseError) Error() string               { return e.message }

// parseJSONValue is serde_json::from_str::<Value>: the value, and whether a
// failure was the input ending early (serde's Category::Eof).
func parseJSONValue(text string) (any, bool, error) {
	value, err := jsonvalue.Parse([]byte(text))
	if err == nil {
		return value, false, nil
	}
	var probe any
	decodeErr := json.Unmarshal([]byte(text), &probe)
	var syntax *json.SyntaxError
	eof := errors.As(decodeErr, &syntax) && syntax.Error() == "unexpected end of JSON input"
	if decodeErr == nil {
		decodeErr = err
	}
	return nil, eof, decodeErr
}

func NormalizeToolArguments(raw string) (NormalizedToolArguments, error) {
	value, firstEOF, firstErr := parseJSONValue(raw)
	if firstErr == nil {
		return NormalizedToolArguments{Value: value, Arguments: raw, Repaired: false}, nil
	}
	rawTrimmed := strings.TrimSpace(raw)
	sawEOF := firstEOF && rawTrimmed != ""
	var candidates []string
	pushCandidate(&candidates, rawTrimmed)
	if fenced, ok := stripJSONCodeFence(rawTrimmed); ok {
		pushCandidate(&candidates, fenced)
	}

	// Only repair JSON that is structurally complete but syntactically
	// sloppy (unquoted keys, single quotes, trailing commas, code fences).
	// Deliberately do NOT fabricate missing structure: a buffer that is
	// still open at finish is a truncated/length-stopped tool-call and must
	// be classified Incomplete so the stream is retried.
	bases := append([]string{}, candidates...)
	for _, base := range bases {
		repaired := quoteUnquotedObjectKeys(base)
		repaired = convertSingleQuotedStrings(repaired)
		repaired = removeTrailingCommas(repaired)
		pushCandidate(&candidates, repaired)
	}

	for _, candidate := range candidates {
		value, eof, err := parseJSONValue(candidate)
		if err == nil {
			encoded, err := jsonvalue.Marshal(value)
			if err != nil {
				return NormalizedToolArguments{}, err
			}
			return NormalizedToolArguments{Arguments: string(encoded), Value: value, Repaired: true}, nil
		}
		sawEOF = sawEOF || eof
	}

	kind := ToolArgumentErrorKindInvalid
	if sawEOF {
		kind = ToolArgumentErrorKindIncomplete
	}
	return NormalizedToolArguments{}, &ToolArgumentParseError{
		kind:    kind,
		message: fmt.Sprintf("parse tool arguments as JSON: %v", firstErr),
	}
}

// normalizeRequestToolArguments repairs an assistant tool-call's stored
// arguments before replaying it into a Responses-API request. Repairable
// JSON is rewritten; anything else passes through unchanged so the provider
// still receives the model's original text.
func normalizeRequestToolArguments(raw, toolName string) string {
	normalized, err := NormalizeToolArguments(raw)
	if err != nil {
		slog.Debug("leaving unrepaired assistant tool-call arguments in Responses request", "tool_name", toolName, "error", err)
		return raw
	}
	if normalized.Repaired {
		slog.Warn("repaired malformed assistant tool-call arguments for Responses request", "tool_name", toolName)
		return normalized.Arguments
	}
	return raw
}

// normalizeStreamedToolArguments repairs a streamed tool-call's assembled
// arguments. Repairable JSON is rewritten; an Incomplete (truncated) buffer
// is surfaced as a retryable IncompleteStreamError so the turn is replayed;
// any other malformed input is passed through for the dispatch layer to
// reject with a tool error. protocol names the wire shape for diagnostics.
func normalizeStreamedToolArguments(toolCallID, toolName, arguments, protocol string) (string, error) {
	normalized, err := NormalizeToolArguments(arguments)
	if err == nil {
		if normalized.Repaired {
			slog.Warn("repaired malformed streamed tool-call arguments", "tool_call_id", toolCallID, "tool_name", toolName)
			return normalized.Arguments, nil
		}
		return arguments, nil
	}
	var parseErr *ToolArgumentParseError
	if errors.As(err, &parseErr) && parseErr.Kind() == ToolArgumentErrorKindIncomplete {
		return "", withContext(NewIncompleteStreamError(protocol, "complete tool-call arguments"),
			"incomplete tool-call arguments for "+toolName)
	}
	slog.Debug("leaving unrepaired malformed streamed tool-call arguments",
		"tool_call_id", toolCallID, "tool_name", toolName, "error", err)
	return arguments, nil
}

func pushCandidate(candidates *[]string, candidate string) {
	trimmed := strings.TrimSpace(candidate)
	if trimmed == "" {
		return
	}
	for _, existing := range *candidates {
		if existing == trimmed {
			return
		}
	}
	*candidates = append(*candidates, trimmed)
}

func stripJSONCodeFence(input string) (string, bool) {
	rest, ok := strings.CutPrefix(input, "```")
	if !ok {
		return "", false
	}
	_, body, ok := strings.Cut(rest, "\n")
	if !ok {
		return "", false
	}
	body, ok = strings.CutSuffix(strings.TrimSpace(body), "```")
	if !ok {
		return "", false
	}
	return strings.TrimSpace(body), true
}

func convertSingleQuotedStrings(input string) string {
	var out strings.Builder
	chars := []rune(input)
	inDouble := false
	doubleEscaped := false
	for index := 0; index < len(chars); index++ {
		ch := chars[index]
		if inDouble {
			out.WriteRune(ch)
			switch {
			case doubleEscaped:
				doubleEscaped = false
			case ch == '\\':
				doubleEscaped = true
			case ch == '"':
				inDouble = false
			}
			continue
		}
		switch ch {
		case '"':
			inDouble = true
			out.WriteRune(ch)
		case '\'':
			out.WriteByte('"')
			escaped := false
			for index+1 < len(chars) {
				index++
				next := chars[index]
				if escaped {
					switch next {
					case '\'':
						out.WriteByte('\'')
					case '"':
						out.WriteString(`\"`)
					case '\\':
						out.WriteString(`\\`)
					case 'n':
						out.WriteString(`\n`)
					case 'r':
						out.WriteString(`\r`)
					case 't':
						out.WriteString(`\t`)
					default:
						out.WriteByte('\\')
						out.WriteRune(next)
					}
					escaped = false
				} else if next == '\\' {
					escaped = true
				} else if next == '\'' {
					out.WriteByte('"')
					break
				} else if next == '"' {
					out.WriteString(`\"`)
				} else if next == '\n' {
					out.WriteString(`\n`)
				} else if next == '\r' {
					out.WriteString(`\r`)
				} else if next == '\t' {
					out.WriteString(`\t`)
				} else {
					out.WriteRune(next)
				}
			}
		default:
			out.WriteRune(ch)
		}
	}
	return out.String()
}

type jsonContainer int

const (
	containerObjectExpectKey jsonContainer = iota
	containerObjectExpectValue
	containerArray
)

func quoteUnquotedObjectKeys(input string) string {
	chars := []rune(input)
	var out strings.Builder
	var stack []jsonContainer
	top := func() (jsonContainer, bool) {
		if len(stack) == 0 {
			return 0, false
		}
		return stack[len(stack)-1], true
	}
	index := 0
	for index < len(chars) {
		ch := chars[index]
		if ch == '"' || ch == '\'' {
			index = copyStringLiteral(chars, index, &out)
			continue
		}
		if container, ok := top(); ok && container == containerObjectExpectKey && isKeyStart(ch) {
			keyStart := index
			index++
			for index < len(chars) && isKeyChar(chars[index]) {
				index++
			}
			keyEnd := index
			colon := index
			for colon < len(chars) && unicode.IsSpace(chars[colon]) {
				colon++
			}
			if colon < len(chars) && chars[colon] == ':' {
				out.WriteByte('"')
				out.WriteString(string(chars[keyStart:keyEnd]))
				out.WriteByte('"')
				out.WriteString(string(chars[keyEnd:colon]))
				out.WriteByte(':')
				stack[len(stack)-1] = containerObjectExpectValue
				index = colon + 1
				continue
			}
			out.WriteString(string(chars[keyStart:keyEnd]))
			continue
		}
		container, ok := top()
		switch ch {
		case '{':
			stack = append(stack, containerObjectExpectKey)
		case '[':
			stack = append(stack, containerArray)
		case '}':
			if ok && (container == containerObjectExpectKey || container == containerObjectExpectValue) {
				stack = stack[:len(stack)-1]
			}
		case ']':
			if ok && container == containerArray {
				stack = stack[:len(stack)-1]
			}
		case ':':
			if ok && container == containerObjectExpectKey {
				stack[len(stack)-1] = containerObjectExpectValue
			}
		case ',':
			if ok && container == containerObjectExpectValue {
				stack[len(stack)-1] = containerObjectExpectKey
			}
		}
		out.WriteRune(ch)
		index++
	}
	return out.String()
}

// copyStringLiteral emits the opening quote, then scans the body. Starting
// the loop at the opening quote would let it match itself as the closing
// quote and bail after a single character.
func copyStringLiteral(chars []rune, start int, out *strings.Builder) int {
	quote := chars[start]
	out.WriteRune(quote)
	index := start + 1
	escaped := false
	for index < len(chars) {
		ch := chars[index]
		out.WriteRune(ch)
		index++
		if escaped {
			escaped = false
		} else if ch == '\\' {
			escaped = true
		} else if ch == quote {
			break
		}
	}
	return index
}

func isKeyStart(ch rune) bool {
	return ch == '_' || ch == '$' || ch >= 'a' && ch <= 'z' || ch >= 'A' && ch <= 'Z'
}

func isKeyChar(ch rune) bool {
	return ch == '_' || ch == '$' || ch == '-' || ch >= 'a' && ch <= 'z' || ch >= 'A' && ch <= 'Z' || ch >= '0' && ch <= '9'
}

func removeTrailingCommas(input string) string {
	chars := []rune(input)
	var out strings.Builder
	index := 0
	for index < len(chars) {
		ch := chars[index]
		if ch == '"' || ch == '\'' {
			index = copyStringLiteral(chars, index, &out)
			continue
		}
		if ch == ',' {
			next := index + 1
			for next < len(chars) && unicode.IsSpace(chars[next]) {
				next++
			}
			if next < len(chars) && (chars[next] == '}' || chars[next] == ']') {
				index++
				continue
			}
		}
		out.WriteRune(ch)
		index++
	}
	return out.String()
}
