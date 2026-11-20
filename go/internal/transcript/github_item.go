// Port of mj-transcript/src/github_item.rs.
//
// Compact GitHub relevance evidence from the materialized transcript.

package transcript

import (
	"slices"
	"strings"
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
	if len(line) <= core.MentionBytes {
		return line
	}
	// Shrink the lead so the whole reference fits: a URL with long owner and
	// repository names can take most of the window by itself.
	leadBytes := min(mentionLeadBytes, max(core.MentionBytes-(referenceLen+2*len(mentionElision)), 0))
	start := floorCharBoundary(line, max(at-leadBytes, 0))
	lead := ""
	if start > 0 {
		lead = mentionElision
	}
	rest := line[start:]
	room := core.MentionBytes - len(lead)
	if len(rest) <= room {
		return lead + rest
	}
	end := floorCharBoundary(rest, room-len(mentionElision))
	return lead + rest[:end] + mentionElision
}

// floorCharBoundary is Rust's str::floor_char_boundary: the largest
// character boundary at or below index.
func floorCharBoundary(text string, index int) int {
	if index >= len(text) {
		return len(text)
	}
	for index > 0 && !utf8.RuneStart(text[index]) {
		index--
	}
	return index
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
	boundary := 0
	for i := len(session.Transcript) - 1; i >= 0; i-- {
		if core.IsContextBoundary(session.Transcript[i].StableID) {
			boundary = i + 1
			break
		}
	}
	items := session.Transcript[boundary:]
	start := 0
	users := 0
	for i := len(items) - 1; i >= 0; i-- {
		if _, ok := items[i].Body.(core.TranscriptBodyUser); ok {
			users++
			if users == core.SessionTurns {
				start = i
				break
			}
		}
	}
	items = items[start:]
	digest := core.SessionContext{}
	type message struct{ role, text string }
	var messages []message
	var files []string
	var refs []string
	for _, item := range items {
		var text string
		switch body := item.Body.(type) {
		case core.TranscriptBodyUser:
			text = core.MaterializedContentText(body.Content)
			if _, _, isCommand := core.ContextCommandText(text); strings.TrimSpace(text) != core.Prompt &&
				!isCommand && strings.TrimSpace(text) != "" {
				messages = append(messages, message{"User: ", text})
			}
		case core.TranscriptBodyMessage:
			if _, ok := body.Event.MessageText(); ok {
				messages = append(messages, message{"User: ", core.RenderMailboxEvent(body.Event)})
				text = core.RenderMailboxEvent(body.Event)
			}
		case core.TranscriptBodyAgent:
			text = core.MaterializedChunksText(body.Chunks)
			if strings.TrimSpace(text) != "" {
				messages = append(messages, message{"Agent: ", text})
			}
		case core.TranscriptBodyTool:
			if call, err := body.Call.Decode(); err == nil {
				for _, location := range call.Locations {
					files = append(files, prefix(location.Path, core.FileBytes))
				}
				text = call.Title
			}
		}
		// Index full lines before conversation truncation or the
		// reference-list cap.
		for _, line := range rustLines(text) {
			line = strings.TrimSpace(line)
			if line == "" {
				continue
			}
			for _, reference := range core.GithubReferences(line) {
				window := mentionWindow(line, reference.Start, len(reference.Text))
				digest.MentionCandidates = append(digest.MentionCandidates, core.GithubMentionCandidate{Reference: reference, Line: window})
			}
		}
		for _, reference := range core.GithubReferences(text) {
			refs = append(refs, prefix(reference.Text, core.ReferenceBytes))
		}
	}
	// Reserve newest messages first, then restore their chronological
	// order. Labels, newlines and truncation markers all count against the
	// budget.
	remaining := core.ConversationBytes
	var selected []string
	for i := len(messages) - 1; i >= 0; i-- {
		role, text := messages[i].role, messages[i].text
		if remaining < len(role)+1 {
			break
		}
		available := remaining - (len(role) + 1)
		limit := min(core.MessageBytes, available)
		rendered := text
		if len(text) > limit {
			if limit < len(messageTruncationMarker) {
				break
			}
			rendered = prefix(text, limit-len(messageTruncationMarker)) + messageTruncationMarker
		}
		rendered = role + rendered + "\n"
		remaining -= len(rendered)
		selected = append(selected, rendered)
	}
	slices.Reverse(selected)
	digest.Conversation = strings.Join(selected, "")
	slices.Reverse(files)
	digest.Files = distinct(files, core.MaxFiles)
	slices.Reverse(refs)
	digest.References = distinct(refs, core.MaxReferences)
	return digest
}

// rustLines is Rust's str::lines: lines split at `\n`, a `\r` before the
// `\n` removed, and no final empty line after a trailing newline.
func rustLines(text string) []string {
	var lines []string
	for text != "" {
		line, rest, terminated := strings.Cut(text, "\n")
		if terminated {
			line = strings.TrimSuffix(line, "\r")
		}
		lines = append(lines, line)
		text = rest
	}
	return lines
}
