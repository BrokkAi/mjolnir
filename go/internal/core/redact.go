// Port of mj-core/src/redact.rs.

// Secret redaction for text that leaves the process: provider diagnostics in
// logs and the evidence Mjolnir sends to the Jev classifier.
//
// JSON values here are the untyped values of
// github.com/BrokkAi/acp-go/jsonvalue (serde_json::Value's Go form): nil,
// bool, string, int64, uint64, float64, []any and jsonvalue.Object.

package core

import (
	"fmt"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// secretRunChars is the shortest run of token characters that is redacted.
// Shorter runs are left alone so that ordinary words, paths and numbers stay
// readable.
const secretRunChars = 24

// RedactSecrets replaces every long run of token characters. Access, refresh
// and id tokens, API keys and account ids are all long runs of base64url, hex
// or dotted JWT segments, and a diagnostic has no use for them.
func RedactSecrets(text string) string {
	return redactTokenRuns(text, func(string) string { return "[redacted]" })
}

// anonymousLabel names text by the order in which labels first saw it.
func anonymousLabel(text string, labels map[string]int) string {
	label, ok := labels[text]
	if !ok {
		label = len(labels)
		labels[text] = label
	}
	return fmt.Sprintf("[redacted token %d]", label)
}

// redactSecretsDistinct keeps punctuation outside labels so formatting cannot
// change identifier equality.
func redactSecretsDistinct(text string, labels map[string]int) string {
	return redactTokenRuns(text, func(token string) string {
		if !strings.ContainsFunc(token, redactASCIIAlphanumeric) {
			return anonymousLabel(token, labels)
		}
		var result strings.Builder
		start := 0
		for index, ch := range token {
			if !redactASCIIAlphanumeric(ch) {
				if start < index {
					result.WriteString(anonymousLabel(token[start:index], labels))
				}
				result.WriteRune(ch)
				start = index + utf8.RuneLen(ch)
			}
		}
		if start < len(token) {
			result.WriteString(anonymousLabel(token[start:], labels))
		}
		return result.String()
	})
}

func redactASCIIAlphanumeric(ch rune) bool {
	return ch >= '0' && ch <= '9' || ch >= 'a' && ch <= 'z' || ch >= 'A' && ch <= 'Z'
}

func redactTokenRuns(text string, replacement func(string) string) string {
	isTokenChar := func(ch rune) bool {
		return redactASCIIAlphanumeric(ch) || ch == '-' || ch == '_' || ch == '.' || ch == '+'
	}
	var redacted, run strings.Builder
	redacted.Grow(len(text))
	flush := func() {
		// Token characters are ASCII, so the byte length is the char count.
		if run.Len() >= secretRunChars {
			redacted.WriteString(replacement(run.String()))
		} else {
			redacted.WriteString(run.String())
		}
		run.Reset()
	}
	for _, ch := range text {
		if isTokenChar(ch) {
			run.WriteRune(ch)
		} else {
			flush()
			redacted.WriteRune(ch)
		}
	}
	flush()
	return redacted.String()
}

// RedactJSONStrings redacts every string value in serialized evidence, at any
// depth, so a field added later is covered without being named here. Values
// under `id` or a `*_id` key are Mjolnir's own identifiers, which the receiver
// matches against each other, so they are kept.
func RedactJSONStrings(value *any) {
	redactJSONStringsWith(value, true, RedactSecrets)
}

// RedactJSONStringsDistinct redacts string values before serialization so
// escapes cannot alter identity.
func RedactJSONStringsDistinct(value *any, labels map[string]int) {
	redactJSONStringsWith(value, false, func(text string) string {
		return redactSecretsDistinct(text, labels)
	})
}

// AnonymizeJSONStrings labels every string value: secret answer choices need
// anonymous labels even when they are short.
func AnonymizeJSONStrings(value *any, labels map[string]int) {
	redactJSONStringsWith(value, false, func(text string) string {
		return anonymousLabel(text, labels)
	})
}

func redactJSONStringsWith(value *any, keepIDs bool, redactor func(string) string) {
	switch typed := (*value).(type) {
	case string:
		*value = redactor(typed)
	case []any:
		for index := range typed {
			redactJSONStringsWith(&typed[index], keepIDs, redactor)
		}
	case jsonvalue.Object:
		for index := range typed {
			key := typed[index].Key
			if keepIDs && (key == "id" || strings.HasSuffix(key, "_id")) {
				continue
			}
			redactJSONStringsWith(&typed[index].Value, keepIDs, redactor)
		}
	}
}
