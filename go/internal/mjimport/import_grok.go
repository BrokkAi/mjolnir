// Port of mj-import/src/import/grok.rs.

package mjimport

import (
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// grokCwdDirectory is how a refusal names the directory a Grok Build
// session sits in. Grok Build names that directory after the working
// directory the session ran in, and the listing and the by-id lookup both
// refuse it in these words.
const grokCwdDirectory = "working directory"

// LocateGrokSession locates a Grok Build session directory. Its name is the
// session UUID, which is the native identifier `session/load` takes.
func LocateGrokSession(home string, selection GrokSessionSelection) (LocatedGrokSession, error) {
	candidates, err := ListGrokSessions(home)
	if err != nil {
		return LocatedGrokSession{}, err
	}
	switch selection := selection.(type) {
	case ClaudeSessionSelectionNativeSessionID:
		for _, candidate := range candidates {
			if candidate.NativeSessionID == string(selection) {
				return candidate, nil
			}
		}
		return locateNamedGrokSession(home, string(selection))
	case ClaudeSessionSelectionLatest:
		if len(candidates) == 0 {
			return LocatedGrokSession{}, errors.New("no Grok Build session directories were found")
		}
		return candidates[0], nil
	}
	panic(fmt.Sprintf("unknown session selection %T", selection))
}

// locateNamedGrokSession finds the session directory a named id points at,
// for the ids the listing leaves out. The listing skips whatever it cannot
// make a row from, including a session whose working directory it cannot
// recover. Naming a session must say why it was skipped rather than report
// it as missing.
//
// A directory reached through a symlink is still refused, because the
// archive step stores only files that sit directly inside the harness home.
func locateNamedGrokSession(home string, nativeSessionID string) (LocatedGrokSession, error) {
	if err := core.ValidateID("Grok Build session", nativeSessionID); err != nil {
		return LocatedGrokSession{}, err
	}
	sessions := filepath.Join(home, "sessions")
	rejected := []string{}
	matches := []LocatedGrokSession{}
	entries, err := os.ReadDir(sessions)
	if err != nil {
		return LocatedGrokSession{}, fmt.Errorf("read Grok Build sessions directory %s: %w", sessions, importIOError(err))
	}
	for _, entry := range entries {
		cwdDirectory := filepath.Join(sessions, entry.Name())
		sessionPath := filepath.Join(cwdDirectory, nativeSessionID)
		switch container := grokStore.Container(cwdDirectory, grokCwdDirectory).(type) {
		case namedEntryAbsent:
			continue
		case namedEntryRejected:
			// A working directory that cannot be archived is worth reporting
			// only when the named session is inside it; an unrelated symlink
			// in the sessions root says nothing about it.
			if importExists(sessionPath) {
				rejected = append(rejected, string(container))
			}
			continue
		}
		var metadata fs.FileInfo
		switch session := grokStore.Directory(sessionPath).(type) {
		case namedEntryAbsent:
			continue
		case namedEntryRejected:
			rejected = append(rejected, string(session))
			continue
		case namedEntryImportable:
			metadata = session.metadata
		}
		title, summaryCwd := grokListingMetadata(sessionPath)
		// Grok Build records the working directory in the session summary
		// and again in the name of the directory holding it. The archive is
		// collected relative to that directory, so neither may be missing.
		cwd := summaryCwd
		if cwd == nil {
			if decoded, ok := grokDecodeCwdDirname(cwdDirectory); ok {
				cwd = &decoded
			}
		}
		if cwd == nil {
			rejected = append(rejected, grokStore.NoCwd(sessionPath))
			continue
		}
		listedTitle := nativeSessionID
		if title != nil {
			listedTitle = *title
		}
		sizeBytes, err := DirectorySize(sessionPath)
		if err != nil {
			return LocatedGrokSession{}, err
		}
		matches = append(matches, LocatedGrokSession{
			Title:           listedTitle,
			NativeSessionID: nativeSessionID,
			ModifiedAt:      grokSessionModifiedAt(sessionPath, metadata),
			GitBranch:       gitBranchOrHead(*cwd),
			SizeBytes:       sizeBytes,
			Cwd:             *cwd,
			SessionPath:     sessionPath,
		})
	}
	switch {
	case len(matches) == 0 && len(rejected) > 0:
		return LocatedGrokSession{}, grokStore.CannotImport(nativeSessionID, rejected)
	case len(matches) == 0:
		return LocatedGrokSession{}, fmt.Errorf("Grok Build session %s was not found under %s",
			core.RustDebugString(nativeSessionID), sessions)
	case len(matches) == 1:
		return matches[0], nil
	}
	return LocatedGrokSession{}, fmt.Errorf("Grok Build session %s occurs in multiple working directories",
		core.RustDebugString(nativeSessionID))
}

// ListGrokSessions lists native Grok Build sessions newest first.
func ListGrokSessions(home string) ([]LocatedGrokSession, error) {
	sessions := []LocatedGrokSession{}
	err := ScanGrokSessions(home, func(progress SessionScanProgress[LocatedGrokSession]) {
		if progress.Session != nil {
			sessions = append(sessions, *progress.Session)
		}
	})
	if err != nil {
		return nil, err
	}
	return sessions, nil
}

// ScanGrokSessions scans native Grok Build sessions newest first, reporting
// after every candidate directory.
func ScanGrokSessions(home string, report func(SessionScanProgress[LocatedGrokSession])) error {
	sessions := filepath.Join(home, "sessions")
	if !importIsDir(sessions) {
		return fmt.Errorf("Grok Build sessions directory is missing: %s", sessions)
	}
	candidates, err := grokCandidates(sessions)
	if err != nil {
		return err
	}
	sortKimiCandidatesNewestFirst(candidates)
	total := len(candidates)
	report(SessionScanProgress[LocatedGrokSession]{Scanned: 0, Total: total})
	for index, candidate := range candidates {
		sizeBytes, err := DirectorySize(candidate.sessionPath)
		if err != nil {
			return err
		}
		report(SessionScanProgress[LocatedGrokSession]{
			Scanned: index + 1,
			Total:   total,
			Session: &LocatedGrokSession{
				Title:           candidate.title,
				NativeSessionID: candidate.nativeSessionID,
				SessionPath:     candidate.sessionPath,
				ModifiedAt:      candidate.modifiedAt,
				GitBranch:       gitBranchOrHead(candidate.cwd),
				SizeBytes:       sizeBytes,
				Cwd:             candidate.cwd,
			},
		})
	}
	return nil
}

// grokCandidates walks `sessions/<encoded-cwd>/<session-uuid>`. The sessions
// root also holds the shared search index and lock files, which are not
// sessions.
//
// A working directory the import could not archive is left out, so the
// listing offers only sessions that can actually be imported and the by-id
// lookup is left to report why one is missing from it.
func grokCandidates(sessions string) ([]kimiScanCandidate, error) {
	candidates := []kimiScanCandidate{}
	entries, err := os.ReadDir(sessions)
	if err != nil {
		return nil, importIOError(err)
	}
	for _, cwdEntry := range entries {
		cwdDirectory := filepath.Join(sessions, cwdEntry.Name())
		switch container := grokStore.Container(cwdDirectory, grokCwdDirectory).(type) {
		case namedEntryAbsent:
			continue
		case namedEntryRejected:
			slog.Debug("skipped a Grok Build working directory", "reason", string(container))
			continue
		}
		decodedCwd, hasDecodedCwd := grokDecodeCwdDirname(cwdDirectory)
		sessionEntries, err := os.ReadDir(cwdDirectory)
		if err != nil {
			return nil, importIOError(err)
		}
		for _, sessionEntry := range sessionEntries {
			sessionPath := filepath.Join(cwdDirectory, sessionEntry.Name())
			metadata, err := os.Lstat(sessionPath)
			if err != nil {
				return nil, importIOError(err)
			}
			if metadata.Mode()&fs.ModeSymlink != 0 || !metadata.IsDir() {
				continue
			}
			nativeSessionID := sessionEntry.Name()
			if !utf8.ValidString(nativeSessionID) || core.ValidateID("Grok Build session", nativeSessionID) != nil {
				continue
			}
			title, summaryCwd := grokListingMetadata(sessionPath)
			var cwd string
			switch {
			case summaryCwd != nil:
				cwd = *summaryCwd
			case hasDecodedCwd:
				cwd = decodedCwd
			default:
				continue
			}
			listedTitle := nativeSessionID
			if title != nil {
				listedTitle = *title
			}
			candidates = append(candidates, kimiScanCandidate{
				nativeSessionID: nativeSessionID,
				modifiedAt:      grokSessionModifiedAt(sessionPath, metadata),
				title:           listedTitle,
				cwd:             cwd,
				sessionPath:     sessionPath,
			})
		}
	}
	return candidates, nil
}

// grokDecodeCwdDirname decodes Grok Build's session-directory name: the
// URL-encoded working directory, or a `{slug}-{hash}` form for a long path
// that keeps the real value in a `.cwd` file. Mirrors
// `decode_cwd_from_dirname` in grok-build.
func grokDecodeCwdDirname(directory string) (string, bool) {
	name, ok := importPathFileName(directory)
	if !ok || !utf8.ValidString(name) {
		return "", false
	}
	if decoded, ok := urlDecode(name); ok && strings.HasPrefix(decoded, "/") {
		return decoded, true
	}
	recorded, err := os.ReadFile(filepath.Join(directory, ".cwd"))
	if err != nil || !utf8.Valid(recorded) {
		return "", false
	}
	trimmed := strings.TrimSpace(string(recorded))
	if !strings.HasPrefix(trimmed, "/") {
		return "", false
	}
	return trimmed, true
}

// urlDecode is percent-decoding for the session-directory name. false when
// the text is not valid percent-encoded UTF-8, which means it is not a cwd
// key.
func urlDecode(value string) (string, bool) {
	decoded := make([]byte, 0, len(value))
	for index := 0; index < len(value); {
		if value[index] != '%' {
			decoded = append(decoded, value[index])
			index++
			continue
		}
		if index+3 > len(value) {
			return "", false
		}
		hex := value[index+1 : index+3]
		// Rust's `u8::from_str_radix` accepts a leading `+`.
		digits := strings.TrimPrefix(hex, "+")
		if digits == "" || strings.HasPrefix(digits, "+") || strings.HasPrefix(digits, "-") {
			return "", false
		}
		parsed, err := strconv.ParseUint(digits, 16, 8)
		if err != nil {
			return "", false
		}
		decoded = append(decoded, byte(parsed))
		index += 3
	}
	if !utf8.Valid(decoded) {
		return "", false
	}
	return string(decoded), true
}

// grokListingMetadata is the listing title and cwd from `summary.json`, each
// nil when absent. Never fails the whole scan: an unreadable session is
// listed with what could be recovered.
func grokListingMetadata(sessionPath string) (title *string, cwd *string) {
	var summary any
	if data, err := os.ReadFile(filepath.Join(sessionPath, "summary.json")); err == nil {
		if parsed, err := jsonvalue.Parse(data); err == nil {
			summary = parsed
		}
	}
	if text, ok := importJSONStr(importJSONGet(summary, "session_summary")); ok {
		if normalized, ok := core.NormalizeSessionTitle(text); ok {
			title = &normalized
		}
	}
	if text, ok := importJSONStr(importJSONPointer(summary, "info", "cwd")); ok && filepath.IsAbs(text) {
		cwd = &text
	}
	return title, cwd
}

func grokSessionModifiedAt(sessionPath string, metadata fs.FileInfo) time.Time {
	modifiedAt := metadata.ModTime()
	for _, name := range []string{chatHistory, "events.jsonl", "summary.json"} {
		if info, err := os.Stat(filepath.Join(sessionPath, name)); err == nil && info.ModTime().After(modifiedAt) {
			modifiedAt = info.ModTime()
		}
	}
	return modifiedAt
}
