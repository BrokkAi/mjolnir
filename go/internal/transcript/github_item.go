// Port of mj-transcript/src/github_item.rs.
//
// Compact GitHub relevance evidence from the materialized transcript.

package transcript

import (
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// prefix is the longest prefix of text within bytes, cut at a character
// boundary.
func prefix(text string, bytes int) string {
	end := min(bytes, len(text))
	for end > 0 && end < len(text) && !utf8.RuneStart(text[end]) {
		end--
	}
	return text[:end]
}

const mentionElision = "…"

// mentionLeadBytes is the bytes kept before the reference, so the subject
// that relates the session to it ("reviewing", "blocked on") usually
// survives.
const mentionLeadBytes = 96

// mentionWindow is the part of line around the reference parsed at byte
// offset at, at most core.MentionBytes. Agent paragraphs are often one long
// line, so a plain prefix would drop the reference the mention was chosen
// for. The parser's offset, not a text search, places the window: `#12` must
// not land on `#123`, and each occurrence keeps its own context.
func mentionWindow(line string, at, referenceLen int) string {
	panic("unported: mj-transcript/src/github_item.rs mention_window")
}

const messageTruncationMarker = "\n[Message truncated.]"

// distinct keeps the first count distinct values, in order.
func distinct(values []string, count int) []string {
	var result []string
	for _, value := range values {
		seen := false
		for _, kept := range result {
			if kept == value {
				seen = true
				break
			}
		}
		if !seen {
			result = append(result, value)
		}
		if len(result) == count {
			break
		}
	}
	return result
}

func SessionDigest(session *core.MaterializedSession) core.SessionContext {
	panic("unported: mj-transcript/src/github_item.rs session_digest")
}
