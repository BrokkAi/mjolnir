// Port of mj-import/src/import/muse.rs.
//
// Rust calls these functions by their module path (`muse::scan`); the Go
// names carry the `muse` prefix instead.

package mjimport

import (
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// sortMuseCandidatesNewestFirst orders sessions newest first, then by path,
// both descending.
func sortMuseCandidatesNewestFirst(candidates []museCandidate) {
	slices.SortStableFunc(candidates, func(left, right museCandidate) int {
		if order := right.modifiedAt.Compare(left.modifiedAt); order != 0 {
			return order
		}
		return importComparePaths(right.sessionPath, left.sessionPath)
	})
}

// museParentName is the name of the directory a session log sits in, as
// `path.parent().and_then(Path::file_name).and_then(OsStr::to_str)`.
func museParentName(path string) (string, bool) {
	parent, ok := importPathParent(path)
	if !ok {
		return "", false
	}
	name, ok := importPathFileName(parent)
	if !ok || !utf8.ValidString(name) {
		return "", false
	}
	return name, true
}

const museActiveReason = "Muse session has an active or incomplete turn"
const museUnavailableReason = "Muse session is corrupt or uses an unsupported format"
const museDuplicateReason = "Muse session id occurs in multiple session directories"
const maxMuseSessionFileBytes uint64 = 1024 * 1024 * 1024

type museCandidate struct {
	nativeSessionID string
	sessionPath     string
	modifiedAt      time.Time
	sizeBytes       uint64
}

type museSummary struct {
	nativeSessionID string
	cwd             string
	title           string
	active          bool
}

// museLocate is Rust's `muse::locate`: a Muse session under its native
// date/session directory layout.
func museLocate(sessionsRoot string, selection ClaudeSessionSelection) (LocatedNativeSession, error) {
	candidates, err := museCandidates(sessionsRoot)
	if err != nil {
		return LocatedNativeSession{}, err
	}
	sortMuseCandidatesNewestFirst(candidates)
	var candidate museCandidate
	switch selection := selection.(type) {
	case ClaudeSessionSelectionNativeSessionID:
		nativeSessionID := string(selection)
		if err := core.ValidateID("Muse session", nativeSessionID); err != nil {
			return LocatedNativeSession{}, err
		}
		matches := []museCandidate{}
		for _, candidate := range candidates {
			if candidate.nativeSessionID == nativeSessionID {
				matches = append(matches, candidate)
			}
		}
		switch len(matches) {
		case 0:
			return LocatedNativeSession{}, fmt.Errorf("Muse session %s was not found under %s",
				core.RustDebugString(nativeSessionID), sessionsRoot)
		case 1:
			candidate = matches[0]
		default:
			return LocatedNativeSession{}, fmt.Errorf("Muse session %s occurs in multiple session directories under %s",
				core.RustDebugString(nativeSessionID), sessionsRoot)
		}
	case ClaudeSessionSelectionLatest:
		if len(candidates) == 0 {
			return LocatedNativeSession{}, errors.New("no Muse session JSONL files were found")
		}
		candidate = candidates[0]
	default:
		panic(fmt.Sprintf("unknown session selection %T", selection))
	}
	summary, err := museSummaryFor(candidate.sessionPath)
	if err != nil {
		return LocatedNativeSession{}, fmt.Errorf("read Muse session %s: %w", candidate.sessionPath, err)
	}
	if summary.nativeSessionID != candidate.nativeSessionID {
		return LocatedNativeSession{}, errors.New("Muse session stream id does not match its directory")
	}
	if summary.active {
		return LocatedNativeSession{}, fmt.Errorf("Muse session %s has an active or incomplete turn; wait for its native terminal event before importing",
			core.RustDebugString(summary.nativeSessionID))
	}
	return LocatedNativeSession{NativeSessionID: candidate.nativeSessionID, SourcePath: candidate.sessionPath}, nil
}

// museScan is Rust's `muse::scan`. It scans only
// `YYYY/MM/DD/<session-id>/session.jsonl` files directly below the sessions
// root. `subagent/` descendants are native child streams and are captured
// with their parent rather than listed as independent sessions.
func museScan(sessionsRoot string, report func(SessionScanProgress[NativeSessionListing])) error {
	candidates, err := museCandidates(sessionsRoot)
	if err != nil {
		return err
	}
	sortMuseCandidatesNewestFirst(candidates)
	total := len(candidates)
	report(SessionScanProgress[NativeSessionListing]{Scanned: 0, Total: total})
	occurrences := map[string]int{}
	for _, candidate := range candidates {
		occurrences[candidate.nativeSessionID]++
	}

	for index, candidate := range candidates {
		duplicate := occurrences[candidate.nativeSessionID] > 1
		var session NativeSessionListing
		summary, err := museSummaryFor(candidate.sessionPath)
		if err == nil {
			var reason *string
			switch {
			case summary.nativeSessionID != candidate.nativeSessionID:
				reason = museReason(museUnavailableReason)
			case duplicate:
				reason = museReason(museDuplicateReason)
			case summary.active:
				reason = museReason(museActiveReason)
			}
			session = NativeSessionListing{
				NativeSessionID:   summary.nativeSessionID,
				Title:             summary.title,
				ModifiedAt:        candidate.modifiedAt,
				GitBranch:         gitBranchOrHead(summary.cwd),
				SizeBytes:         candidate.sizeBytes,
				Cwd:               summary.cwd,
				UnavailableReason: reason,
				NativelyArchived:  false,
			}
		} else {
			slog.Debug("Muse session is unavailable during discovery",
				"path", candidate.sessionPath, "error", err.Error())
			title, ok := museParentName(candidate.sessionPath)
			if !ok {
				title = "Muse session"
			}
			session = NativeSessionListing{
				NativeSessionID:   candidate.nativeSessionID,
				Title:             title,
				ModifiedAt:        candidate.modifiedAt,
				GitBranch:         "HEAD",
				SizeBytes:         candidate.sizeBytes,
				Cwd:               "",
				UnavailableReason: museReason(museUnavailableReason),
				NativelyArchived:  false,
			}
		}
		report(SessionScanProgress[NativeSessionListing]{Scanned: index + 1, Total: total, Session: &session})
	}
	return nil
}

// museReason is a listing's unavailable reason.
func museReason(reason string) *string {
	return &reason
}

// museValidateTranscript is Rust's `muse::validate_transcript`: it validates
// the selected stream's identity and lifecycle before importing it, and
// returns the session's workspace root.
func museValidateTranscript(path string, originalKey string) (string, error) {
	summary, err := museSummaryFor(path)
	if err != nil {
		return "", err
	}
	if name, ok := museParentName(originalKey); !ok || name != summary.nativeSessionID {
		return "", errors.New("Muse session stream id does not match its directory")
	}
	if summary.active {
		return "", fmt.Errorf("Muse session %s has an active or incomplete turn; import requires a terminal lifecycle record",
			core.RustDebugString(summary.nativeSessionID))
	}
	return summary.cwd, nil
}

func museCandidates(sessionsRoot string) ([]museCandidate, error) {
	if !importIsDir(sessionsRoot) {
		return nil, fmt.Errorf("Muse sessions directory is missing: %s", sessionsRoot)
	}
	candidates := []museCandidate{}
	years, err := readDirectories(sessionsRoot)
	if err != nil {
		return nil, err
	}
	for _, year := range years {
		if !isDateComponent(year, 4) {
			continue
		}
		months, err := readDirectories(year)
		if err != nil {
			return nil, err
		}
		for _, month := range months {
			if !isDateComponent(month, 2) {
				continue
			}
			days, err := readDirectories(month)
			if err != nil {
				return nil, err
			}
			for _, day := range days {
				if !isDateComponent(day, 2) {
					continue
				}
				sessionDirectories, err := readDirectories(day)
				if err != nil {
					return nil, err
				}
				for _, sessionDirectory := range sessionDirectories {
					nativeSessionID, ok := importPathFileName(sessionDirectory)
					if !ok || core.ValidateID("Muse session", nativeSessionID) != nil {
						continue
					}
					sessionPath := filepath.Join(sessionDirectory, "session.jsonl")
					metadata, err := os.Lstat(sessionPath)
					if err != nil || !metadata.Mode().IsRegular() {
						continue
					}
					candidates = append(candidates, museCandidate{
						nativeSessionID: nativeSessionID,
						sessionPath:     sessionPath,
						modifiedAt:      metadata.ModTime(),
						sizeBytes:       uint64(metadata.Size()),
					})
				}
			}
		}
	}
	return candidates, nil
}

// readDirectories lists the real (not symlinked) directories in root.
func readDirectories(root string) ([]string, error) {
	entries, err := os.ReadDir(root)
	if err != nil {
		return nil, fmt.Errorf("read Muse directory %s: %w", root, importIOError(err))
	}
	directories := []string{}
	for _, entry := range entries {
		path := filepath.Join(root, entry.Name())
		metadata, err := os.Lstat(path)
		if err != nil {
			return nil, importIOError(err)
		}
		if metadata.IsDir() && metadata.Mode()&fs.ModeSymlink == 0 {
			directories = append(directories, path)
		}
	}
	return directories, nil
}

func isDateComponent(path string, width int) bool {
	name, ok := importPathFileName(path)
	if !ok || len(name) != width {
		return false
	}
	for i := 0; i < len(name); i++ {
		if name[i] < '0' || name[i] > '9' {
			return false
		}
	}
	return true
}

// museSummaryFor is Rust's `muse_summary` (renamed: the type museSummary
// keeps its name).
func museSummaryFor(path string) (museSummary, error) {
	data, err := readMuseSessionFile(path)
	if err != nil {
		return museSummary{}, err
	}
	records, err := checkpoint.ReadRecords(data)
	if err != nil {
		return museSummary{}, fmt.Errorf("parse Muse session %s: %w", path, err)
	}
	return museSummaryFromRecords(path, records)
}

func readMuseSessionFile(path string) ([]byte, error) {
	metadata, err := os.Lstat(path)
	if err != nil {
		return nil, fmt.Errorf("stat Muse session %s: %w", path, importIOError(err))
	}
	if !metadata.Mode().IsRegular() {
		return nil, fmt.Errorf("Muse session is not a regular file: %s", path)
	}
	if uint64(metadata.Size()) > maxMuseSessionFileBytes {
		return nil, fmt.Errorf("Muse session is too large: %d bytes (maximum is %d bytes)",
			metadata.Size(), maxMuseSessionFileBytes)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read Muse session %s: %w", path, importIOError(err))
	}
	if int64(len(data)) != metadata.Size() {
		return nil, fmt.Errorf("Muse session changed while it was being read: %s", path)
	}
	return data, nil
}

// museStreamID is a record's `/stream/id` text.
func museStreamID(record *checkpoint.MuseRecord) (string, bool) {
	return importJSONStr(importJSONPointer(record.Value, "stream", "id"))
}

// musePayloadType is a record's `payload_type` text.
func musePayloadType(record *checkpoint.MuseRecord) (string, bool) {
	return importJSONStr(importJSONGet(record.Value, "payload_type"))
}

func museSummaryFromRecords(path string, records []checkpoint.MuseRecord) (museSummary, error) {
	nativeSessionID := ""
	found := false
	for index := range records {
		if records[index].RetainedFrame {
			continue
		}
		if id, ok := museStreamID(&records[index]); ok {
			nativeSessionID, found = id, true
			break
		}
	}
	if !found {
		nativeSessionID, found = museParentName(path)
	}
	if !found {
		return museSummary{}, errors.New("Muse session does not declare a stream id")
	}

	cwd := ""
	hasCwd := false
	for index := range records {
		record := &records[index]
		if record.RetainedFrame {
			continue
		}
		if payloadType, ok := musePayloadType(record); !ok || payloadType != "runtime.session.metadata" {
			continue
		}
		if root, ok := importNonblank(importJSONPointer(record.Value, "payload", "record", "workspace_root")); ok {
			cwd, hasCwd = root, true
		}
	}
	if !hasCwd {
		return museSummary{}, errors.New("Muse session does not declare payload.record.workspace_root")
	}
	if err := core.ValidateID("Muse session", nativeSessionID); err != nil {
		return museSummary{}, err
	}
	if !filepath.IsAbs(cwd) {
		return museSummary{}, errors.New("Muse workspace root is not absolute")
	}
	for index := range records {
		if id, ok := museStreamID(&records[index]); !ok || id != nativeSessionID {
			return museSummary{}, errors.New("Muse session contains records from a different session stream")
		}
	}

	promptTitle := nativeSessionID
	for index := range records {
		record := &records[index]
		if payloadType, ok := musePayloadType(record); !ok || payloadType != "runtime.user_intent.accepted" {
			continue
		}
		text, ok := museIntentText(record.Value)
		if !ok {
			continue
		}
		if normalized, ok := core.NormalizeSessionTitle(text); ok {
			promptTitle = normalized
			break
		}
	}
	title := promptTitle
	for index := range records {
		record := &records[index]
		if payloadType, ok := musePayloadType(record); !ok || payloadType != "session.name.changed" {
			continue
		}
		newName, ok := importJSONStr(importJSONPointer(record.Value, "payload", "new_name"))
		if !ok {
			continue
		}
		if normalized, ok := core.NormalizeSessionTitle(newName); ok {
			title = normalized
		}
	}

	activeRuns := map[string]struct{}{}
	pendingIntents := map[string]struct{}{}
	for index := range records {
		value := records[index].Value
		payloadType, _ := musePayloadType(&records[index])
		switch payloadType {
		case "runtime.user_intent.accepted":
			if intentID, ok := importJSONStr(importJSONPointer(value, "payload", "intent_id")); ok {
				pendingIntents[intentID] = struct{}{}
			}
		case "runtime.user_intent.materialized":
			intentID, hasIntent := importJSONStr(importJSONPointer(value, "payload", "intent_id"))
			runID, hasRun := importJSONStr(importJSONPointer(value, "payload", "outcome", "run_id"))
			if hasIntent && hasRun {
				delete(pendingIntents, intentID)
				activeRuns[runID] = struct{}{}
			}
		case "runtime.session":
			if kind, ok := importJSONStr(importJSONPointer(value, "payload", "kind")); !ok || kind != "run" {
				continue
			}
			runID, hasRun := importJSONStr(importJSONPointer(value, "payload", "run_id"))
			eventKind, _ := importJSONStr(importJSONPointer(value, "payload", "event", "kind"))
			switch eventKind {
			case "started":
				if hasRun {
					activeRuns[runID] = struct{}{}
				}
			case "terminal":
				if hasRun {
					delete(activeRuns, runID)
					delete(pendingIntents, runID)
				} else if len(activeRuns) > 0 {
					// Rust's BTreeSet::pop_last: the greatest run id.
					last := slices.Max(slices.Collect(maps.Keys(activeRuns)))
					delete(activeRuns, last)
					delete(pendingIntents, last)
				}
			}
		}
	}

	return museSummary{
		nativeSessionID: nativeSessionID,
		cwd:             cwd,
		title:           title,
		active:          len(activeRuns) > 0 || len(pendingIntents) > 0,
	}, nil
}

// museIntentText is the prompt text of a `runtime.user_intent.accepted`
// record (a jsonvalue value); false when it has none.
func museIntentText(record any) (string, bool) {
	text := func(part any) (string, bool) {
		kind, present := importJSONGet(part, "kind")
		if !present {
			kind, present = importJSONGet(part, "type")
		}
		if kind, ok := importJSONStr(kind, present); !ok || kind != "text" {
			return "", false
		}
		return importJSONStr(importJSONGet(part, "text"))
	}
	collect := func(parts []any, into []string) []string {
		for _, part := range parts {
			if value, ok := text(part); ok && strings.TrimSpace(value) != "" {
				into = append(into, value)
			}
		}
		return into
	}
	parts := []string{}
	if messages, ok := importJSONArray(importJSONPointer(record, "payload", "model_messages")); ok {
		for _, message := range messages {
			if content, ok := importJSONArray(importJSONGet(message, "content")); ok {
				parts = collect(content, parts)
			}
		}
	}
	if len(parts) > 0 {
		return strings.Join(parts, "\n"), true
	}
	if blocks, ok := importJSONArray(importJSONPointer(record, "payload", "refill_blocks")); ok {
		parts = collect(blocks, parts)
	}
	if len(parts) > 0 {
		return strings.Join(parts, "\n"), true
	}
	prompt, present := importJSONPointer(record, "payload", "prompt")
	if !present {
		prompt, present = importJSONPointer(record, "payload", "text")
	}
	return importNonblank(prompt, present)
}

// importJSONArray is `Value::as_array` on an optional value.
func importJSONArray(value any, present bool) ([]any, bool) {
	array, ok := value.([]any)
	return array, present && ok
}

// museRecentDirectories is Rust's `muse::recent_directories`.
func museRecentDirectories(home string, limit int, executor core.CommandExecutor) (NativeProjectSeed, error) {
	root, err := checkpoint.MuseSessionsRoot(home)
	if err != nil {
		return NativeProjectSeed{}, err
	}
	candidates, err := museCandidates(root)
	if err != nil {
		return NativeProjectSeed{}, err
	}
	sortMuseCandidatesNewestFirst(candidates)
	seed := NativeProjectSeed{Directories: []string{}, Errors: [][2]string{}}
	for index, candidate := range candidates {
		if index >= limit {
			break
		}
		if executor.CancellationRequested() {
			return NativeProjectSeed{}, errors.New("project discovery cancelled")
		}
		directory, err := NativeProjectDirectory(candidate.sessionPath, core.HarnessKindMuse, executor)
		if err != nil {
			seed.Errors = append(seed.Errors, [2]string{candidate.sessionPath, fmt.Sprintf("%s: %s", candidate.sessionPath, err)})
			continue
		}
		seed.Directories = append(seed.Directories, directory)
	}
	return seed, nil
}
