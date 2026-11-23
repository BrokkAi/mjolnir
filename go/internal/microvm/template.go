//go:build linux

// Port of mj-microvm/src/template.rs.
//
// The small subset of Go templates that `--format` needs.
//
// The controller's Podman scripts use `{{.State.Running}}`, `{{.Id}}`,
// `{{json .Config.Labels}}` and `{{index .Labels "key"}}`. Anything else is
// refused by name rather than ignored. (Go's text/template is not used: it
// would accept, and render differently, what Rust refuses.)
//
// Values are JSON values as github.com/BrokkAi/acp-go/jsonvalue holds them
// (serde_json::Value with preserve_order): nil, bool, string, int64,
// uint64, float64, []any or jsonvalue.Object.

package microvm

import (
	"errors"
	"fmt"
	"strings"
	"unicode"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// MicrovmTemplateRender renders template against value, followed by a
// newline like Podman.
func MicrovmTemplateRender(template string, value any) (string, error) {
	var output strings.Builder
	rest := template
	for {
		start := strings.Index(rest, "{{")
		if start < 0 {
			break
		}
		output.WriteString(rest[:start])
		after := rest[start+2:]
		end := strings.Index(after, "}}")
		if end < 0 {
			return "", fmt.Errorf("unsupported --format template %s: unterminated action", microvmDebugString(template))
		}
		rendered, err := microvmTemplateAction(strings.TrimSpace(after[:end]), value)
		if err != nil {
			return "", fmt.Errorf("unsupported --format template %s: %s", microvmDebugString(template), err.Error())
		}
		output.WriteString(rendered)
		rest = after[end+2:]
	}
	output.WriteString(rest)
	output.WriteByte('\n')
	return output.String(), nil
}

func microvmTemplateAction(text string, root any) (string, error) {
	words, err := microvmTemplateSplitWords(text)
	if err != nil {
		return "", err
	}
	switch {
	case len(words) == 1 && strings.HasPrefix(words[0], "."):
		value, err := microvmTemplateLookup(root, words[0])
		if err != nil {
			return "", err
		}
		return microvmTemplateScalar(value)
	case len(words) == 2 && words[0] == "json" && strings.HasPrefix(words[1], "."):
		value, err := microvmTemplateLookup(root, words[1])
		if err != nil {
			return "", err
		}
		encoded, err := jsonvalue.Marshal(value)
		if err != nil {
			return "", err
		}
		return string(encoded), nil
	case len(words) == 3 && words[0] == "index" && strings.HasPrefix(words[1], "."):
		path, key := words[1], words[2]
		table, err := microvmTemplateLookup(root, path)
		if err != nil {
			return "", err
		}
		object, isObject := table.(jsonvalue.Object)
		if isObject {
			if value, ok := object.Get(key); ok {
				return microvmTemplateScalar(value)
			}
		}
		// A map of strings yields its zero value for a missing key.
		if isObject || table == nil {
			return "", nil
		}
		return "", fmt.Errorf("%s is not a map", path)
	}
	return "", fmt.Errorf("action {{%s}} is not supported", text)
}

func microvmTemplateSplitWords(text string) ([]string, error) {
	var words []string
	chars := []rune(text)
	for i := 0; i < len(chars); {
		c := chars[i]
		switch {
		case unicode.IsSpace(c):
			i++
		case c == '"':
			i++
			var word strings.Builder
			for {
				if i >= len(chars) {
					return nil, errors.New("unterminated string")
				}
				next := chars[i]
				i++
				if next == '"' {
					break
				}
				if next == '\\' {
					if i >= len(chars) {
						return nil, errors.New("unterminated string")
					}
					word.WriteRune(chars[i])
					i++
					continue
				}
				word.WriteRune(next)
			}
			words = append(words, word.String())
		default:
			var word strings.Builder
			for i < len(chars) && !unicode.IsSpace(chars[i]) {
				word.WriteRune(chars[i])
				i++
			}
			words = append(words, word.String())
		}
	}
	return words, nil
}

func microvmTemplateLookup(root any, path string) (any, error) {
	current := root
	for _, part := range strings.Split(path, ".") {
		if part == "" {
			continue
		}
		object, ok := current.(jsonvalue.Object)
		var next any
		if ok {
			next, ok = object.Get(part)
		}
		if !ok {
			return nil, fmt.Errorf("%s names a field that does not exist (%s)", path, part)
		}
		current = next
	}
	return current, nil
}

func microvmTemplateScalar(value any) (string, error) {
	switch v := value.(type) {
	case string:
		return v, nil
	case bool:
		if v {
			return "true", nil
		}
		return "false", nil
	case nil:
		return "<nil>", nil
	case []any, jsonvalue.Object:
		return "", errors.New("the value is not a scalar; use json")
	}
	// A number, written as serde_json's Number Display writes it.
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		return "", err
	}
	return string(encoded), nil
}
