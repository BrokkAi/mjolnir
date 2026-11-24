// Port of mj-import/src/import/adapter_transcript.rs.
//
// The Rust module parses native sessions with the SessionWiki adapters of
// the external crate brokk-sessionwiki (`sessionwiki::adapters::{ClaudeCode,
// Codex, GrokBuild, KimiCode, Muse}` and `sessionwiki::model::{Session,
// Role}`). No Go equivalent of that crate exists yet. sessionwikiAdapter,
// sessionwikiSession and sessionwikiMessage declare the part of its API this
// module uses, so the importer is written against it.
//
// Until brokk-sessionwiki has a Go port, importAdapter returns
// sessionwikiPendingAdapter for the harnesses Rust imports: it reads a
// session's native id exactly as the adapter's `native_id` does (a small,
// self-contained part of each adapter, ported from brokk-sessionwiki
// 0.34.28, the version the pinned Rust build locks), and refuses to parse a
// conversation with an error that says the adapters are missing. Replace it
// with the ported adapters when they exist.

package mjimport

import (
	"bufio"
	"crypto/sha256"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/klauspost/compress/zstd"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// sessionwikiAdapter is the part of `sessionwiki::adapters::Adapter` the
// importer uses.
type sessionwikiAdapter interface {
	// NativeID is the literal harness identity, when it can be resolved from
	// the key or source.
	NativeID(key string) (string, bool)
	// Parse parses one session file.
	Parse(path string) (sessionwikiSession, error)
}

// sessionwikiRole is `sessionwiki::model::Role`.
type sessionwikiRole int

const (
	sessionwikiRoleUser sessionwikiRole = iota
	sessionwikiRoleAssistant
	sessionwikiRoleTool
)

// sessionwikiMessage is the part of `sessionwiki::model::Message` the
// projection reads.
type sessionwikiMessage struct {
	Role sessionwikiRole
	Text string
	// Ts is nil when the message has no timestamp.
	Ts *time.Time
	// IsThought is true for assistant reasoning summaries.
	IsThought bool
}

// sessionwikiSession is the part of `sessionwiki::model::Session` the
// projection reads.
type sessionwikiSession struct {
	// RawCwd is the literal native working directory; nil when absent.
	RawCwd *string
	// OperationalEditedPaths are literal paths backed by the adapter's native
	// write evidence.
	OperationalEditedPaths []string
	// CompactedPrefix: the source begins with compacted history before its
	// first real prompt.
	CompactedPrefix bool
	// Ended is nil when unknown.
	Ended    *time.Time
	Messages []sessionwikiMessage
}

// importAdapter is the SessionWiki adapter that reads a harness's native
// sessions; false for the harnesses Mjolnir cannot import.
func importAdapter(harness core.HarnessKind) (sessionwikiAdapter, bool) {
	switch harness {
	case core.HarnessKindClaude, core.HarnessKindCodex, core.HarnessKindKimi,
		core.HarnessKindGrok, core.HarnessKindMuse:
		return sessionwikiPendingAdapter{harness: harness}, true
	case core.HarnessKindAntigravity, core.HarnessKindDsh, core.HarnessKindOpenCode:
		return nil, false
	}
	panic(fmt.Sprintf("unknown harness kind %q", string(harness)))
}

// sessionwikiPendingAdapter stands in for one of brokk-sessionwiki's
// adapters (ClaudeCode, Codex, KimiCode, GrokBuild, Muse) until that crate
// has a Go port. See the file comment.
type sessionwikiPendingAdapter struct {
	harness core.HarnessKind
}

// NativeID is the adapter's `native_id`.
func (a sessionwikiPendingAdapter) NativeID(key string) (string, bool) {
	switch a.harness {
	case core.HarnessKindClaude:
		// ClaudeCode: `filename_stem`.
		stem, ok := importPathFileStem(key)
		return stem, ok && utf8.ValidString(stem)
	case core.HarnessKindKimi, core.HarnessKindGrok:
		// KimiCode and GrokBuild: `directory_name`.
		name, ok := importPathFileName(key)
		return name, ok && utf8.ValidString(name)
	case core.HarnessKindCodex:
		if id, ok := sessionwikiFindInJSONLHeader(key, func(value any) (string, bool) {
			if recordType, ok := importJSONIndex(value, "type").(string); !ok || recordType != "session_meta" {
				return "", false
			}
			return importNonblank(importJSONIndex(importJSONIndex(value, "payload"), "id"), true)
		}); ok {
			return id, true
		}
		return sessionwikiRolloutSuffix(key)
	case core.HarnessKindMuse:
		parentPath, ok := importPathParent(key)
		if !ok {
			return "", false
		}
		parent, hasParent := importPathFileName(parentPath)
		hasParent = hasParent && utf8.ValidString(parent)
		if metadata, err := os.Lstat(key); err == nil && metadata.Mode().IsRegular() {
			if id, ok, err := sessionwikiMuseFirstStream(key); err == nil && ok {
				return id, true
			}
		}
		return parent, hasParent
	}
	return "", false
}

// Parse is the adapter's `parse`, which needs the ported adapter.
func (a sessionwikiPendingAdapter) Parse(path string) (sessionwikiSession, error) {
	return sessionwikiSession{}, fmt.Errorf(
		"the Go build cannot read %s conversations yet: the SessionWiki session adapters (brokk-sessionwiki) are not ported",
		a.harness.DisplayName())
}

// sessionwikiHeaderBytes is how much decoded text
// `util::find_in_jsonl_header` reads.
const sessionwikiHeaderBytes = 256 * 1024

// sessionwikiLineReader is `util::line_reader`: the file, decompressed when
// its extension is `zst` or `zstd`, every concatenated frame in turn.
func sessionwikiLineReader(path string) (io.ReadCloser, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("open %s: %w", path, importIOError(err))
	}
	if extension, ok := importPathExtension(path); ok && (extension == "zst" || extension == "zstd") {
		decoder, err := zstd.NewReader(bufio.NewReader(file))
		if err != nil {
			file.Close()
			return nil, err
		}
		return sessionwikiDecoder{decoder: decoder, file: file}, nil
	}
	return file, nil
}

type sessionwikiDecoder struct {
	decoder *zstd.Decoder
	file    *os.File
}

func (d sessionwikiDecoder) Read(buffer []byte) (int, error) { return d.decoder.Read(buffer) }

func (d sessionwikiDecoder) Close() error {
	d.decoder.Close()
	return d.file.Close()
}

// sessionwikiFindInJSONLHeader is `util::find_in_jsonl_header`: inspect
// complete JSONL records within a bounded decoded header.
func sessionwikiFindInJSONLHeader(path string, find func(value any) (string, bool)) (string, bool) {
	source, err := sessionwikiLineReader(path)
	if err != nil {
		return "", false
	}
	defer source.Close()
	reader := bufio.NewReader(io.LimitReader(source, sessionwikiHeaderBytes))
	for {
		line, err := reader.ReadBytes('\n')
		if err != nil && !errors.Is(err, io.EOF) {
			return "", false
		}
		if len(line) == 0 {
			return "", false
		}
		if value, parseErr := jsonvalue.Parse(line); parseErr == nil {
			if found, ok := find(value); ok {
				return found, true
			}
		}
		if err != nil {
			return "", false
		}
	}
}

// sessionwikiRolloutSuffix is the Codex adapter's `rollout_suffix`: the UUID
// a `rollout-...-<uuid>.jsonl[.zst]` file name ends in.
func sessionwikiRolloutSuffix(path string) (string, bool) {
	name, ok := importPathFileName(path)
	if !ok || !utf8.ValidString(name) {
		return "", false
	}
	stem, ok := strings.CutSuffix(name, ".jsonl.zst")
	if !ok {
		stem, ok = strings.CutSuffix(name, ".jsonl")
	}
	if !ok {
		return "", false
	}
	rollout, ok := strings.CutPrefix(stem, "rollout-")
	if !ok || len(rollout) < 36 || !utf8.RuneStart(rollout[len(rollout)-36]) {
		return "", false
	}
	prefix, id := rollout[:len(rollout)-36], rollout[len(rollout)-36:]
	if !strings.HasSuffix(prefix, "-") {
		return "", false
	}
	for index := 0; index < len(id); index++ {
		b := id[index]
		switch index {
		case 8, 13, 18, 23:
			if b != '-' {
				return "", false
			}
		default:
			if !('0' <= b && b <= '9' || 'a' <= b && b <= 'f' || 'A' <= b && b <= 'F') {
				return "", false
			}
		}
	}
	return id, true
}

// sessionwikiMuseFirstStream is the Muse adapter's `first_stream`: the
// stream id of the first supported record.
func sessionwikiMuseFirstStream(path string) (string, bool, error) {
	source, err := sessionwikiLineReader(path)
	if err != nil {
		return "", false, err
	}
	defer source.Close()
	reader := bufio.NewReader(source)
	for {
		line, err := reader.ReadBytes('\n')
		if err != nil && !errors.Is(err, io.EOF) {
			return "", false, importIOError(err)
		}
		if len(line) == 0 {
			return "", false, nil
		}
		if value, parseErr := jsonvalue.Parse(line); parseErr == nil {
			if id, ok := sessionwikiMuseStreamID(value); ok {
				return id, true, nil
			}
		}
		if err != nil {
			return "", false, nil
		}
	}
}

// sessionwikiMuseStreamID is the Muse adapter's `stream_id`.
func sessionwikiMuseStreamID(value any) (string, bool) {
	if _, retained := importJSONGet(value, "retained_frame"); retained || !sessionwikiMuseSupportedRecord(value) {
		return "", false
	}
	return importNonblank(importJSONPointer(value, "stream", "id"))
}

// sessionwikiMuseSupportedRecord is the Muse adapter's `supported_record`.
func sessionwikiMuseSupportedRecord(value any) bool {
	isOne := func(number any) bool {
		switch number := number.(type) {
		case int64:
			return number == 1
		case uint64:
			return number == 1
		}
		return false
	}
	if !isOne(importJSONIndex(value, "schema_version")) {
		return false
	}
	if kind, ok := importJSONStr(importJSONPointer(value, "stream", "kind")); !ok || kind != "session" {
		return false
	}
	if _, ok := importJSONStr(importJSONPointer(value, "stream", "id")); !ok {
		return false
	}
	if _, ok := importJSONIndex(value, "payload").(jsonvalue.Object); !ok {
		return false
	}
	payloadType, _ := importJSONIndex(value, "payload_type").(string)
	version := importJSONIndex(value, "payload_schema_version")
	switch payloadType {
	case "runtime.session":
		switch version := version.(type) {
		case int64:
			return version == 1 || version == 2
		case uint64:
			return version == 1 || version == 2
		}
		return false
	case "runtime.session.metadata", "runtime.user_intent.accepted",
		"runtime.user_intent.materialized", "session.name.changed":
		return isOne(version)
	}
	return false
}

func ReadClaudeTranscript(path string) (ClaudeTranscript, error) {
	return readProjection(core.HarnessKindClaude, path, path)
}

func ReadCodexTranscript(path string) (CodexTranscript, error) {
	summary, err := codexSessionSummaryFor(path)
	if err != nil {
		return ClaudeTranscript{}, err
	}
	if issue, ok := summary.HistoryMode.ImportIssue(); ok {
		return ClaudeTranscript{}, errors.New(issue)
	}
	return readProjection(core.HarnessKindCodex, path, path)
}

func ReadKimiTranscript(path string) (KimiTranscript, error) {
	source := filepath.Join(path, "agents", "main", "wire.jsonl")
	if !importIsFile(source) {
		source = filepath.Join(path, "wire.jsonl")
	}
	return readProjection(core.HarnessKindKimi, path, source)
}

func ReadGrokTranscript(path string) (GrokTranscript, error) {
	source := filepath.Join(path, chatHistory)
	if !importIsFile(source) {
		source = filepath.Join(path, "history.jsonl")
	}
	return readProjection(core.HarnessKindGrok, path, source)
}

func readMuseTranscript(path string, originalKey string) (ClaudeTranscript, error) {
	cwd, err := museValidateTranscript(path, originalKey)
	if err != nil {
		return ClaudeTranscript{}, err
	}
	transcript, err := readProjection(core.HarnessKindMuse, path, path)
	if err != nil {
		return ClaudeTranscript{}, err
	}
	if transcript.Cwd != cwd {
		return ClaudeTranscript{}, errors.New("Muse session workspace changed while it was being read")
	}
	return transcript, nil
}

func readProjection(harness core.HarnessKind, key string, source string) (ClaudeTranscript, error) {
	adapter, ok := importAdapter(harness)
	if !ok {
		return ClaudeTranscript{}, errors.New("native harness does not support import")
	}
	session, err := adapter.Parse(key)
	if err != nil {
		return ClaudeTranscript{}, fmt.Errorf("read %s session %s: %w", harness.DisplayName(), key, err)
	}
	return projectSession(session, source)
}

// projectSession projects a parsed native session into the canonical
// transcript. source supplies a clock for older streams with no timed
// dialogue.
func projectSession(session sessionwikiSession, source string) (ClaudeTranscript, error) {
	if session.RawCwd == nil || strings.TrimSpace(*session.RawCwd) == "" {
		return ClaudeTranscript{}, errors.New("native session has no recorded working directory")
	}
	cwd := *session.RawCwd
	if !filepath.IsAbs(cwd) {
		return ClaudeTranscript{}, errors.New("native session working directory is not absolute")
	}

	type dialogueEntry struct {
		body core.TranscriptBody
		// time is nil when the native record had no timestamp.
		time *int64
	}
	dialogue := []dialogueEntry{}
	if session.CompactedPrefix {
		dialogue = append(dialogue, dialogueEntry{
			body: core.TranscriptBodySystem{Text: "Earlier turns were compacted by the native harness."},
		})
	}
	sawPrompt := false
	for _, message := range session.Messages {
		var body core.TranscriptBody
		switch message.Role {
		case sessionwikiRoleTool:
			continue
		case sessionwikiRoleUser:
			text := core.StripHiddenPromptContext(message.Text)
			if strings.TrimSpace(text) == "" {
				continue
			}
			sawPrompt = true
			content, err := jsonvalue.Marshal(jsonvalue.Object{
				{Key: "type", Value: "text"},
				{Key: "text", Value: text},
			})
			if err != nil {
				return ClaudeTranscript{}, err
			}
			body = core.TranscriptBodyUser{Content: []json.RawMessage{content}}
		case sessionwikiRoleAssistant:
			if strings.TrimSpace(message.Text) == "" {
				continue
			}
			chunk, err := jsonvalue.Marshal(jsonvalue.Object{
				{Key: "content", Value: jsonvalue.Object{
					{Key: "type", Value: "text"},
					{Key: "text", Value: message.Text},
				}},
			})
			if err != nil {
				return ClaudeTranscript{}, err
			}
			chunks := []json.RawMessage{chunk}
			if message.IsThought {
				body = core.TranscriptBodyThought{Chunks: chunks, Streaming: false}
			} else {
				body = core.TranscriptBodyAgent{Chunks: chunks, Streaming: false}
			}
		default:
			panic(fmt.Sprintf("unknown SessionWiki role %d", message.Role))
		}
		var at *int64
		if message.Ts != nil {
			millis := message.Ts.UnixMilli()
			at = &millis
		}
		dialogue = append(dialogue, dialogueEntry{body: body, time: at})
	}
	if !sawPrompt && !session.CompactedPrefix {
		return ClaudeTranscript{}, errors.New("native session contains no importable user prompt")
	}

	// Untimed leading items inherit the first visible native timestamp. The
	// source artifact supplies a clock for older streams with no timed
	// dialogue.
	var clock int64
	timed := false
	for _, entry := range dialogue {
		if entry.time != nil {
			clock, timed = *entry.time, true
			break
		}
	}
	if !timed {
		metadata, err := os.Stat(source)
		if err != nil {
			return ClaudeTranscript{}, importIOError(err)
		}
		clock = metadata.ModTime().UnixMilli()
	}
	items := make([]*core.TranscriptItem, 0, len(dialogue))
	for index, entry := range dialogue {
		if entry.time != nil {
			clock = max(clock, *entry.time)
		}
		position := uint64(index) + 1
		stableID := fmt.Sprintf("import-%d", position)
		if _, user := entry.body.(core.TranscriptBodyUser); user {
			stableID = fmt.Sprintf("user-import-%d", position)
		}
		var latest *uint64
		if _, agent := entry.body.(core.TranscriptBodyAgent); agent {
			ordinal := position
			latest = &ordinal
		}
		items = append(items, &core.TranscriptItem{
			StableID:                  stableID,
			Position:                  position,
			LatestContentEventOrdinal: latest,
			CreatedAtMs:               clock,
			LastChangedAtMs:           clock,
			Body:                      entry.body,
		})
	}
	editedPaths := make([]string, len(session.OperationalEditedPaths))
	copy(editedPaths, session.OperationalEditedPaths)
	lastActivity := clock
	if session.Ended != nil {
		lastActivity = max(clock, session.Ended.UnixMilli())
	}
	return ClaudeTranscript{
		Cwd:              cwd,
		EditedPaths:      editedPaths,
		items:            items,
		lastActivityAtMs: &lastActivity,
	}, nil
}

// importedTranscriptFrontierDomain separates the digest of an imported
// transcript's frontier from every other digest.
const importedTranscriptFrontierDomain = "hel-imported-transcript-frontier-v1\x00"

// materialize is the imported transcript as a materialized session with id
// sessionID.
func (t *ClaudeTranscript) materialize(sessionID string) core.MaterializedSession {
	appliedEventOrdinal := uint64(len(t.items))
	appliedEventDigest := core.RelayEventGenesisDigest
	if appliedEventOrdinal != 0 {
		digest := sha256.New()
		digest.Write([]byte(importedTranscriptFrontierDomain))
		digest.Write([]byte(sessionID))
		digest.Write(binary.LittleEndian.AppendUint64(nil, appliedEventOrdinal))
		appliedEventDigest = core.LowerHex(digest.Sum(nil))
	}
	var lastActivity *int64
	if t.lastActivityAtMs != nil {
		value := *t.lastActivityAtMs
		lastActivity = &value
	}
	return core.MaterializedSession{
		SessionID:           sessionID,
		AppliedEventOrdinal: appliedEventOrdinal,
		AppliedEventDigest:  appliedEventDigest,
		LastActivityAtMs:    lastActivity,
		Execution:           core.MaterializedExecutionStateIdle{},
		SessionTitle:        nil,
		Configuration:       core.SessionConfiguration{},
		Transcript:          append([]*core.TranscriptItem(nil), t.items...),
		QueuedPrompts:       []core.MaterializedQueuedPrompt{},
		PendingElicitations: []core.ElicitationRequest{},
		ActiveTurn:          nil,
		LastTurnOutcome:     nil,
	}
}

// transcriptItemsEqual is Rust's `Vec<Arc<TranscriptItem>> == ...`: equal
// items in the same order. Items are compared by their serialized form;
// both sides come from the same projection, so their JSON members are in
// the same order.
func transcriptItemsEqual(left, right []*core.TranscriptItem) bool {
	if len(left) != len(right) {
		return false
	}
	for index := range left {
		leftJSON, leftErr := json.Marshal(left[index])
		rightJSON, rightErr := json.Marshal(right[index])
		if leftErr != nil || rightErr != nil || string(leftJSON) != string(rightJSON) {
			return false
		}
	}
	return true
}
