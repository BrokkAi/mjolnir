// Port of mj-import/src/handoff/claude.rs.

// Claude's native compaction records from a verified checkpoint.

package mjimport

import (
	"bytes"
	"context"
	"errors"
	"io"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	panic("unported: mj-import/src/handoff/claude.rs read_native_compactions")
}

// priorCompaction: the native summary is usable only when its surrounding
// native conversation can be aligned to this checkpoint's canonical
// conversation by identity. nil means no usable summary.
func priorCompaction(ctx context.Context, archivePath string, manifest *core.ArchiveManifest, snapshot *core.CanonicalSessionSnapshot) (*PriorCompaction, error) {
	panic("unported: mj-import/src/handoff/claude.rs prior_compaction")
}

func checkCancel(ctx context.Context) error {
	if ctx.Err() != nil {
		return errors.New("Claude native compaction preparation cancelled")
	}
	return nil
}

func mapCompaction(native *nativeCompaction, preceding, following []nativeRecord, snapshot *core.CanonicalSessionSnapshot) *PriorCompaction {
	panic("unported: mj-import/src/handoff/claude.rs map_compaction")
}

func parseNativeCompactions(ctx context.Context, data []byte) (*nativeTranscript, error) {
	return parseNativeCompactionsFrom(ctx, bytes.NewReader(data))
}

// parseNativeCompactionsFrom checks ctx before every line it reads (Rust
// reads a BufRead; the Go reader is buffered here).
func parseNativeCompactionsFrom(ctx context.Context, reader io.Reader) (*nativeTranscript, error) {
	panic("unported: mj-import/src/handoff/claude.rs parse_native_compactions_from")
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
