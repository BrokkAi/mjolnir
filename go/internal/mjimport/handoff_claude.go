// Port of mj-import/src/handoff/claude.rs.

// Claude's native compaction records from a verified checkpoint.

package mjimport

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"path/filepath"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

const maxNativeTranscriptBytes uint64 = 256 * 1024 * 1024

// nativeCompaction is one compact-boundary/summary pair of a Claude
// transcript; beforeRecord is how many records precede it.
type nativeCompaction struct {
	summary      string
	boundaryAtMs int64
	summaryAtMs  int64
	beforeRecord int
}

type nativeTranscript struct {
	records     []nativeRecord
	compactions []nativeCompaction
}

type nativeRecordKind int

const (
	nativeRecordUser nativeRecordKind = iota
	nativeRecordToolUse
	nativeRecordToolResult
)

// nativeRecord is Rust's NativeRecord::User { text }, ToolUse { id } or
// ToolResult { id }, told apart by kind.
type nativeRecord struct {
	kind nativeRecordKind
	// text is a user record's text.
	text string
	// id is a tool use's or tool result's tool call id.
	id string
}

// readNativeCompactions reads only the primary session JSONL. Subagent
// files and project memory are separate native artifacts and cannot
// establish the parent's compaction. nil means the checkpoint has none.
func readNativeCompactions(ctx context.Context, archivePath string, manifest *core.ArchiveManifest) (*nativeTranscript, error) {
	if err := checkCancel(ctx); err != nil {
		return nil, err
	}
	if manifest.Session.HarnessKind != core.HarnessKindClaude {
		return nil, nil
	}
	name := manifest.Session.NativeSessionID + ".jsonl"
	var descriptor *core.PayloadDescriptor
	for index := range manifest.Payloads {
		role := &manifest.Payloads[index].Role
		if role.Kind != core.PayloadRoleNativeArtifact {
			continue
		}
		components := nativePathComponents(role.RelativePath)
		fileName, ok := nativePathFileName(components)
		if ok && fileName == name && !nativePathHasComponent(components, "subagents") {
			descriptor = &manifest.Payloads[index]
			break
		}
	}
	if descriptor == nil {
		return nil, nil
	}
	if err := checkCancel(ctx); err != nil {
		return nil, err
	}
	data, err := checkpoint.ReadVerifiedCheckpointPayload(archivePath, descriptor, maxNativeTranscriptBytes)
	if err != nil {
		return nil, err
	}
	if err := checkCancel(ctx); err != nil {
		return nil, err
	}
	return parseNativeCompactions(ctx, data)
}

// nativePathComponents is a relative path's normal components as Rust's
// Path::components yields them: separators collapse and `.` disappears.
func nativePathComponents(path string) []string {
	parts := strings.FieldsFunc(path, func(character rune) bool {
		return character == '/' || character == filepath.Separator
	})
	components := parts[:0]
	for _, part := range parts {
		if part != "." {
			components = append(components, part)
		}
	}
	return components
}

// nativePathFileName is Rust's Path::file_name: the last component, unless
// it is `..`.
func nativePathFileName(components []string) (string, bool) {
	if len(components) == 0 || components[len(components)-1] == ".." {
		return "", false
	}
	return components[len(components)-1], true
}

func nativePathHasComponent(components []string, name string) bool {
	for _, component := range components {
		if component == name {
			return true
		}
	}
	return false
}

// priorCompaction: the native summary is usable only when its surrounding
// native conversation can be aligned to this checkpoint's canonical
// conversation by identity. nil means no usable summary.
func priorCompaction(ctx context.Context, archivePath string, manifest *core.ArchiveManifest, snapshot *core.CanonicalSessionSnapshot) (*PriorCompaction, error) {
	if err := checkCancel(ctx); err != nil {
		return nil, err
	}
	transcript, err := readNativeCompactions(ctx, archivePath, manifest)
	if err != nil || transcript == nil {
		return nil, err
	}
	for index := len(transcript.compactions) - 1; index >= 0; index-- {
		native := &transcript.compactions[index]
		if err := checkCancel(ctx); err != nil {
			return nil, err
		}
		if prior := mapCompaction(native, transcript.records[:native.beforeRecord], transcript.records[native.beforeRecord:], snapshot); prior != nil {
			if err := checkCancel(ctx); err != nil {
				return nil, err
			}
			return prior, nil
		}
	}
	if err := checkCancel(ctx); err != nil {
		return nil, err
	}
	return nil, nil
}

func checkCancel(ctx context.Context) error {
	if ctx.Err() != nil {
		return errors.New("Claude native compaction preparation cancelled")
	}
	return nil
}

// canonicalUser is one canonical user prompt's position and text.
type canonicalUser struct {
	position uint64
	text     string
}

// toolCallID is a canonical tool call's `toolCallId`, when it is a string.
func toolCallID(call []byte) (string, bool) {
	value, err := jsonvalue.Parse(call)
	if err != nil {
		return "", false
	}
	id, ok := utilityJSONField(value, "toolCallId").(string)
	return id, ok
}

func mapCompaction(native *nativeCompaction, preceding, following []nativeRecord, snapshot *core.CanonicalSessionSnapshot) *PriorCompaction {
	if native.summaryAtMs < native.boundaryAtMs {
		return nil
	}
	contextStart := snapshot.CurrentContextStart()
	lineageStart := contextStart
	if state, ok := transcript.ReusableHandoff(snapshot); ok {
		lineageStart = max(contextStart, state.ThroughPosition)
	}
	var canonicalUsers []canonicalUser
	for index := range snapshot.Transcript {
		item := &snapshot.Transcript[index]
		if item.Position <= lineageStart {
			continue
		}
		if body, ok := item.Body.(core.CanonicalTranscriptBodyUser); ok {
			text := core.MaterializedContentText(body.Content)
			canonicalUsers = append(canonicalUsers, canonicalUser{
				position: item.Position,
				text:     strings.TrimSpace(core.StripHiddenPromptContext(text)),
			})
		}
	}
	var nativeUsers []string
	for _, record := range preceding {
		if record.kind == nativeRecordUser {
			nativeUsers = append(nativeUsers, record.text)
		}
	}
	if len(nativeUsers) == 0 {
		return nil
	}
	anchorText := nativeUsers[len(nativeUsers)-1]
	var matchingAnchors []int
	for index, user := range canonicalUsers {
		if user.text == anchorText {
			matchingAnchors = append(matchingAnchors, index)
		}
	}
	if len(matchingAnchors) != 1 {
		return nil
	}
	anchorIndex := matchingAnchors[0]
	expected := canonicalUsers[:anchorIndex+1]
	if len(nativeUsers) < len(expected) {
		return nil
	}
	for index, native := range nativeUsers[len(nativeUsers)-len(expected):] {
		if native != expected[index].text {
			return nil
		}
	}

	type occurrences struct{ count, index int }
	uses := map[string]occurrences{}
	results := map[string]occurrences{}
	for index, record := range preceding {
		switch record.kind {
		case nativeRecordToolUse:
			entry := uses[record.id]
			uses[record.id] = occurrences{entry.count + 1, index}
		case nativeRecordToolResult:
			entry := results[record.id]
			results[record.id] = occurrences{entry.count + 1, index}
		}
	}
	completed := map[string]bool{}
	for id, use := range uses {
		result, ok := results[id]
		if ok && use.count == 1 && result.count == 1 && use.index < result.index {
			completed[id] = true
		}
	}
	postIDs := map[string]bool{}
	for _, record := range following {
		if record.kind == nativeRecordToolUse || record.kind == nativeRecordToolResult {
			postIDs[record.id] = true
		}
	}
	anchorPosition := canonicalUsers[anchorIndex].position
	var toolPosition uint64
	found := false
	for index := range snapshot.Transcript {
		item := &snapshot.Transcript[index]
		if item.Position <= anchorPosition {
			continue
		}
		body, ok := item.Body.(core.CanonicalTranscriptBodyTool)
		if !ok {
			continue
		}
		id, ok := toolCallID(body.Call)
		if !ok || !completed[id] || postIDs[id] {
			continue
		}
		if !found || item.Position >= toolPosition {
			toolPosition = item.Position
			found = true
		}
	}
	if !found || toolPosition == 0 {
		return nil
	}

	cutoff := toolPosition - 1
	if cutoff <= lineageStart {
		return nil
	}
	representedUsers := map[uint64]bool{}
	for _, user := range expected {
		representedUsers[user.position] = true
	}
	outputs := core.TerminalOutputsOfCanonical(snapshot.Transcript)
	var retainedPositions []uint64
	for index := range snapshot.Transcript {
		item := &snapshot.Transcript[index]
		if item.Position <= lineageStart || item.Position >= toolPosition {
			continue
		}
		barrier := false
		switch body := item.Body.(type) {
		case core.CanonicalTranscriptBodyUser:
			barrier = !representedUsers[item.Position]
			delete(representedUsers, item.Position)
		case core.CanonicalTranscriptBodyTool:
			id, ok := toolCallID(body.Call)
			barrier = !(ok && completed[id] && !postIDs[id]) ||
				len(body.TerminalRefs) != 0 ||
				len(outputs.ForTool(item.StableID)) != 0
		case core.CanonicalTranscriptBodyMessage, core.CanonicalTranscriptBodyPlan:
			barrier = true
		case core.CanonicalTranscriptBodyAgent:
			barrier = item.LatestContentEventOrdinal == nil || *item.LatestContentEventOrdinal >= toolPosition
		}
		if barrier {
			retainedPositions = append(retainedPositions, item.Position)
		}
	}
	return &PriorCompaction{
		ThroughPosition:   cutoff,
		Summary:           native.summary,
		RetainedPositions: retainedPositions,
	}
}

func parseNativeCompactions(ctx context.Context, data []byte) (*nativeTranscript, error) {
	return parseNativeCompactionsFrom(ctx, bytes.NewReader(data))
}

// isASCIIWhitespace is Rust's u8::is_ascii_whitespace.
func isASCIIWhitespace(character byte) bool {
	switch character {
	case ' ', '\t', '\n', '\x0C', '\r':
		return true
	}
	return false
}

// parseNativeCompactionsFrom checks ctx before every line it reads (Rust
// reads a BufRead; the Go reader is buffered here).
func parseNativeCompactionsFrom(ctx context.Context, reader io.Reader) (*nativeTranscript, error) {
	var records []nativeRecord
	var compactions []nativeCompaction
	var pendingBoundary *int64
	buffered := bufio.NewReader(reader)
	lineNumber := 0
	for {
		if err := checkCancel(ctx); err != nil {
			return nil, err
		}
		line, err := buffered.ReadBytes('\n')
		if err != nil && !errors.Is(err, io.EOF) {
			return nil, err
		}
		if len(line) == 0 && err != nil {
			break
		}
		lineNumber++
		line = bytes.TrimSuffix(line, []byte{'\n'})
		blank := true
		for _, character := range line {
			if !isASCIIWhitespace(character) {
				blank = false
				break
			}
		}
		if blank {
			continue
		}
		record, parseErr := jsonvalue.Parse(line)
		if parseErr != nil {
			return nil, fmt.Errorf("parse Claude native transcript line %d: %w", lineNumber, parseErr)
		}
		if err := checkCancel(ctx); err != nil {
			return nil, err
		}
		if utilityJSONField(record, "isMeta") == true || utilityJSONField(record, "isSidechain") == true {
			continue
		}
		kind, _ := utilityJSONField(record, "type").(string)
		subtype, _ := utilityJSONField(record, "subtype").(string)
		boundary := kind == "system" && (subtype == "compact_boundary" || subtype == "compaction")
		message := utilityJSONField(record, "message")
		summaryFlag, hasFlag := nativeJSONMember(record, "isCompactSummary")
		if !hasFlag {
			summaryFlag = utilityJSONField(message, "isCompactSummary")
		}
		summary := kind == "user" && summaryFlag == true
		if boundary {
			pendingBoundary = nil
			if at, ok := timestampMs(record); ok {
				pendingBoundary = &at
			}
			continue
		}
		content := utilityJSONField(message, "content")
		if summary {
			boundaryAtMs := pendingBoundary
			pendingBoundary = nil
			summaryAtMs, summaryOK := timestampMs(record)
			text, textOK := contentText(content)
			if boundaryAtMs != nil && summaryOK && textOK &&
				summaryAtMs >= *boundaryAtMs && strings.TrimSpace(text) != "" {
				compactions = append(compactions, nativeCompaction{
					summary:      text,
					boundaryAtMs: *boundaryAtMs,
					summaryAtMs:  summaryAtMs,
					beforeRecord: len(records),
				})
			}
			continue
		}
		// A boundary without its immediately following summary is not a
		// valid pair. Other record types may occur between the two in newer
		// Claude streams, but a real conversational record cannot.
		if kind == "user" || kind == "assistant" {
			pendingBoundary = nil
		}
		blocks, _ := content.([]any)
		switch kind {
		case "user":
			hasToolResult := false
			for _, block := range blocks {
				if utilityJSONField(block, "type") == "tool_result" {
					hasToolResult = true
					break
				}
			}
			if !hasToolResult {
				if text, ok := contentText(content); ok {
					text = strings.TrimSpace(core.StripHiddenPromptContext(text))
					if text != "" {
						records = append(records, nativeRecord{kind: nativeRecordUser, text: text})
					}
				}
			}
			for _, block := range blocks {
				if utilityJSONField(block, "type") != "tool_result" {
					continue
				}
				if id, ok := utilityJSONField(block, "tool_use_id").(string); ok {
					records = append(records, nativeRecord{kind: nativeRecordToolResult, id: id})
				}
			}
		case "assistant":
			for _, block := range blocks {
				if utilityJSONField(block, "type") != "tool_use" {
					continue
				}
				if id, ok := utilityJSONField(block, "id").(string); ok {
					records = append(records, nativeRecord{kind: nativeRecordToolUse, id: id})
				}
			}
		}
		if err != nil {
			break
		}
	}
	return &nativeTranscript{records: records, compactions: compactions}, nil
}

// nativeJSONMember is serde_json's `value.get(key)` keeping a present null
// apart from a missing member.
func nativeJSONMember(value any, key string) (any, bool) {
	if object, ok := value.(jsonvalue.Object); ok {
		return object.Get(key)
	}
	return nil, false
}

// timestampMs is a record's RFC 3339 `timestamp` in Unix milliseconds.
func timestampMs(record any) (int64, bool) {
	text, ok := utilityJSONField(record, "timestamp").(string)
	if !ok {
		return 0, false
	}
	parsed, ok := core.ParseChronoRFC3339(text)
	if !ok {
		return 0, false
	}
	return parsed.UnixMilli(), true
}

// contentText is a message content's text: the string itself, or its text
// blocks joined by newlines. false when there is none.
func contentText(content any) (string, bool) {
	if text, ok := content.(string); ok {
		return text, true
	}
	blocks, ok := content.([]any)
	if !ok {
		return "", false
	}
	var texts []string
	for _, block := range blocks {
		if text, ok := utilityJSONField(block, "text").(string); ok && utilityJSONField(block, "type") == "text" {
			texts = append(texts, text)
		}
	}
	text := strings.Join(texts, "\n")
	return text, text != ""
}
