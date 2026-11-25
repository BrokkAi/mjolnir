// Port of mj-import/src/import/kimi.rs.

package mjimport

import (
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// LocateKimiSession locates a Kimi session directory. Its on-disk
// `session_<uuid>` name is the native identifier required by Kimi ACP's
// `session/load`.
func LocateKimiSession(home string, selection KimiSessionSelection) (LocatedKimiSession, error) {
	candidates, err := ListKimiSessions(home)
	if err != nil {
		return LocatedKimiSession{}, err
	}
	switch selection := selection.(type) {
	case ClaudeSessionSelectionNativeSessionID:
		for _, candidate := range candidates {
			if candidate.NativeSessionID == string(selection) {
				return candidate, nil
			}
		}
		return locateNamedKimiSession(home, string(selection))
	case ClaudeSessionSelectionLatest:
		if len(candidates) == 0 {
			return LocatedKimiSession{}, errors.New("no Kimi session directories were found")
		}
		return candidates[0], nil
	}
	panic(fmt.Sprintf("unknown session selection %T", selection))
}

// locateNamedKimiSession finds the session directory a named id points at,
// for the ids the listing does not show. The listing follows Kimi's own
// index: a session it never indexed, one marked deleted there, and one the
// user archived are all left out. None of that applies once the user names a
// session, so this lookup reads the directory on disk whatever the index
// says about it.
//
// A directory reached through a symlink is still refused, because the
// archive step stores only files that sit directly inside the harness home.
// That refusal is reported, so the caller can say why the named path was
// rejected instead of reporting the session as missing.
func locateNamedKimiSession(home string, nativeSessionID string) (LocatedKimiSession, error) {
	if err := core.ValidateID("Kimi session", nativeSessionID); err != nil {
		return LocatedKimiSession{}, err
	}
	sessions := filepath.Join(home, "sessions")
	var indexedWorkDir *string
	if workDir, ok := kimiIndexedWorkDir(home, nativeSessionID); ok {
		indexedWorkDir = &workDir
	}
	rejected := []string{}
	matches := []LocatedKimiSession{}
	paths, err := kimiNamedSessionPaths(sessions, nativeSessionID, &rejected)
	if err != nil {
		return LocatedKimiSession{}, err
	}
	for _, sessionPath := range paths {
		var metadata fs.FileInfo
		switch entry := kimiStore.Directory(sessionPath).(type) {
		case namedEntryAbsent:
			continue
		case namedEntryRejected:
			rejected = append(rejected, string(entry))
			continue
		case namedEntryImportable:
			metadata = entry.metadata
		}
		summary, err := kimiSessionSummaryFor(sessionPath, indexedWorkDir)
		if err != nil {
			return LocatedKimiSession{}, err
		}
		// The archive is collected relative to the directory the session ran
		// in, so a session that records none cannot be imported.
		if summary.Cwd == nil {
			rejected = append(rejected, kimiStore.NoCwd(sessionPath))
			continue
		}
		cwd := *summary.Cwd
		sizeBytes, err := DirectorySize(sessionPath)
		if err != nil {
			return LocatedKimiSession{}, err
		}
		matches = append(matches, LocatedKimiSession{
			Title:           summary.Title,
			NativeSessionID: nativeSessionID,
			ModifiedAt:      kimiSessionModifiedAt(sessionPath, metadata),
			GitBranch:       gitBranchOrHead(cwd),
			SizeBytes:       sizeBytes,
			Cwd:             cwd,
			SessionPath:     sessionPath,
		})
	}
	switch {
	case len(matches) == 0 && len(rejected) > 0:
		return LocatedKimiSession{}, kimiStore.CannotImport(nativeSessionID, rejected)
	case len(matches) == 0:
		return LocatedKimiSession{}, fmt.Errorf("Kimi session %s was not found under %s",
			core.RustDebugString(nativeSessionID), sessions)
	case len(matches) == 1:
		return matches[0], nil
	}
	return LocatedKimiSession{}, fmt.Errorf("Kimi session %s occurs in multiple workspace directories",
		core.RustDebugString(nativeSessionID))
}

// kimiNamedSessionPaths is where a named Kimi session directory can sit:
// directly under `sessions`, or inside one workspace directory there.
func kimiNamedSessionPaths(sessions string, nativeSessionID string, rejected *[]string) ([]string, error) {
	paths := []string{filepath.Join(sessions, nativeSessionID)}
	entries, err := os.ReadDir(sessions)
	if err != nil {
		if importIsNotFound(err) {
			return paths, nil
		}
		return nil, fmt.Errorf("read Kimi sessions directory %s: %w", sessions, importIOError(err))
	}
	for _, entry := range entries {
		workspace := filepath.Join(sessions, entry.Name())
		path := filepath.Join(workspace, nativeSessionID)
		switch entry := kimiStore.Container(workspace, "workspace directory").(type) {
		case namedEntryAbsent:
			continue
		case namedEntryRejected:
			// A workspace that cannot be archived is worth reporting only
			// when the named session is inside it.
			if importExists(path) {
				*rejected = append(*rejected, string(entry))
			}
		case namedEntryImportable:
			paths = append(paths, path)
		}
	}
	return paths, nil
}

// kimiIndexedWorkDir is the working directory Kimi's own index recorded for
// one session, whatever else that index says about it.
func kimiIndexedWorkDir(home string, nativeSessionID string) (string, bool) {
	file, err := os.Open(filepath.Join(home, "session_index.jsonl"))
	if err != nil {
		return "", false
	}
	defer file.Close()
	var workDir *string
	lines := newImportLineReader(file)
	for {
		line, ok, err := lines.next()
		if err != nil {
			return "", false
		}
		if !ok {
			break
		}
		record, err := jsonvalue.Parse([]byte(line))
		if err != nil {
			continue
		}
		if id, ok := importJSONStr(importJSONGet(record, "sessionId")); !ok || id != nativeSessionID {
			continue
		}
		if recorded, ok := importJSONStr(importJSONGet(record, "workDir")); ok {
			workDir = &recorded
		}
	}
	if workDir == nil {
		return "", false
	}
	return *workDir, true
}

// ListKimiSessions lists native Kimi sessions newest first.
func ListKimiSessions(home string) ([]LocatedKimiSession, error) {
	sessions := []LocatedKimiSession{}
	err := ScanKimiSessions(home, func(progress SessionScanProgress[LocatedKimiSession]) {
		if progress.Session != nil {
			sessions = append(sessions, *progress.Session)
		}
	})
	if err != nil {
		return nil, err
	}
	return sessions, nil
}

// ScanKimiSessions scans native Kimi sessions newest first, reporting after
// every candidate directory.
func ScanKimiSessions(home string, report func(SessionScanProgress[LocatedKimiSession])) error {
	sessions := filepath.Join(home, "sessions")
	if !importIsDir(sessions) {
		return fmt.Errorf("Kimi sessions directory is missing: %s", sessions)
	}
	candidates, err := kimiIndexedCandidates(home, sessions)
	if err != nil {
		return err
	}
	sortKimiCandidatesNewestFirst(candidates)
	total := len(candidates)
	report(SessionScanProgress[LocatedKimiSession]{Scanned: 0, Total: total})
	for index, candidate := range candidates {
		sizeBytes, err := DirectorySize(candidate.sessionPath)
		if err != nil {
			return err
		}
		report(SessionScanProgress[LocatedKimiSession]{
			Scanned: index + 1,
			Total:   total,
			Session: &LocatedKimiSession{
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

// sortKimiCandidatesNewestFirst orders session directories newest first,
// then by path, both descending.
func sortKimiCandidatesNewestFirst(candidates []kimiScanCandidate) {
	slices.SortStableFunc(candidates, func(left, right kimiScanCandidate) int {
		if order := right.modifiedAt.Compare(left.modifiedAt); order != 0 {
			return order
		}
		return importComparePaths(right.sessionPath, left.sessionPath)
	})
}

func kimiIndexedCandidates(home string, sessions string) ([]kimiScanCandidate, error) {
	indexPath := filepath.Join(home, "session_index.jsonl")
	if !importIsFile(indexPath) {
		return []kimiScanCandidate{}, nil
	}

	type indexedSession struct {
		sessionPath string
		workDir     string
	}
	indexed := map[string]indexedSession{}
	file, err := os.Open(indexPath)
	if err != nil {
		return nil, importIOError(err)
	}
	defer file.Close()
	lines := newImportLineReader(file)
	for {
		line, ok, err := lines.next()
		if err != nil {
			return nil, err
		}
		if !ok {
			break
		}
		record, err := jsonvalue.Parse([]byte(line))
		if err != nil {
			continue
		}
		sessionID, ok := importJSONStr(importJSONGet(record, "sessionId"))
		if !ok || sessionID == "" {
			continue
		}
		if deleted, ok := importJSONBool(importJSONGet(record, "deleted")); ok && deleted {
			delete(indexed, sessionID)
			continue
		}
		sessionPath, hasSessionPath := importJSONStr(importJSONGet(record, "sessionDir"))
		workDir, hasWorkDir := importJSONStr(importJSONGet(record, "workDir"))
		if !hasSessionPath || !hasWorkDir {
			continue
		}
		if name, ok := importPathFileName(sessionPath); core.ValidateID("Kimi session", sessionID) != nil ||
			!filepath.IsAbs(sessionPath) || !ok || name != sessionID {
			continue
		}
		indexed[sessionID] = indexedSession{sessionPath: sessionPath, workDir: workDir}
	}

	sessions, err = importCanonicalize(sessions)
	if err != nil {
		return nil, err
	}
	candidates := []kimiScanCandidate{}
	for _, nativeSessionID := range slices.Sorted(maps.Keys(indexed)) {
		session := indexed[nativeSessionID]
		metadata, err := os.Lstat(session.sessionPath)
		if err != nil {
			continue
		}
		if metadata.Mode()&fs.ModeSymlink != 0 || !metadata.IsDir() {
			continue
		}
		canonicalSessionPath, err := importCanonicalize(session.sessionPath)
		if err != nil {
			continue
		}
		if !importPathStartsWith(canonicalSessionPath, sessions) {
			continue
		}
		title, cwd, archived, ok := kimiStateListingMetadata(canonicalSessionPath, session.workDir)
		if !ok || archived {
			continue
		}
		candidates = append(candidates, kimiScanCandidate{
			nativeSessionID: nativeSessionID,
			modifiedAt:      kimiSessionModifiedAt(canonicalSessionPath, metadata),
			sessionPath:     canonicalSessionPath,
			title:           title,
			cwd:             cwd,
		})
	}
	return candidates, nil
}

// kimiSessionSummary is what one Kimi session's `state.json` says about it.
type kimiSessionSummary struct {
	// Archived is the listing's verdict: the user archived this session in
	// Kimi. Importing a session the user named by id ignores this.
	Archived bool
	Title    string
	// Cwd is nil when neither `state.json` nor Kimi's own index records an
	// absolute working directory.
	Cwd *string
}

// kimiStateListingMetadata is the listing's view of one session directory:
// its title, cwd ("" when none) and archived flag; ok is false when its
// `state.json` cannot be read or parsed, so one damaged session does not
// fail the scan.
func kimiStateListingMetadata(sessionPath string, indexedWorkDir string) (title string, cwd string, archived bool, ok bool) {
	summary, err := kimiSessionSummaryFor(sessionPath, &indexedWorkDir)
	if err != nil {
		return "", "", false, false
	}
	if summary.Cwd != nil {
		cwd = *summary.Cwd
	}
	return summary.Title, cwd, summary.Archived, true
}

// kimiSessionSummaryFor is Rust's `kimi_session_summary` (renamed: the type
// kimiSessionSummary keeps its name). indexedWorkDir is nil when Kimi's
// index records none.
func kimiSessionSummaryFor(sessionPath string, indexedWorkDir *string) (kimiSessionSummary, error) {
	statePath := filepath.Join(sessionPath, "state.json")
	var state any = jsonvalue.Object{}
	if importIsFile(statePath) {
		data, err := os.ReadFile(statePath)
		if err != nil {
			return kimiSessionSummary{}, importIOError(err)
		}
		state, err = jsonvalue.Parse(data)
		if err != nil {
			return kimiSessionSummary{}, fmt.Errorf("parse Kimi session state %s: %w", statePath, err)
		}
	}
	text := func(key string) (string, bool) {
		return importNonblank(importJSONGet(state, key))
	}
	var rawTitle string
	var hasTitle bool
	if flag, _ := importJSONGet(state, "isCustomTitle"); isBoolean(flag) {
		rawTitle, hasTitle = text("title")
	} else {
		rawTitle, hasTitle = kimiFirstText(text, "customTitle", "title")
	}
	title := ""
	normalized := false
	if hasTitle {
		title, normalized = core.NormalizeSessionTitle(rawTitle)
	}
	var cwd *string
	if recorded, ok := kimiFirstText(text, "workDir", "cwd"); ok && filepath.IsAbs(recorded) {
		cwd = &recorded
	} else if indexedWorkDir != nil && filepath.IsAbs(*indexedWorkDir) {
		workDir := *indexedWorkDir
		cwd = &workDir
	}
	if !normalized {
		title = "Untitled session"
		if name, ok := importPathFileName(sessionPath); ok && utf8.ValidString(name) {
			title = name
		}
	}
	archived, _ := importJSONBool(importJSONGet(state, "archived"))
	return kimiSessionSummary{Archived: archived, Title: title, Cwd: cwd}, nil
}

// isBoolean is `Value::is_boolean`.
func isBoolean(value any) bool {
	_, ok := value.(bool)
	return ok
}

// kimiFirstText is the first of keys whose text is set.
func kimiFirstText(text func(string) (string, bool), keys ...string) (string, bool) {
	for _, key := range keys {
		if value, ok := text(key); ok {
			return value, true
		}
	}
	return "", false
}

func kimiSessionModifiedAt(sessionPath string, metadata fs.FileInfo) time.Time {
	modifiedAt := metadata.ModTime()
	consider := func(path string) {
		if info, err := os.Stat(path); err == nil && info.ModTime().After(modifiedAt) {
			modifiedAt = info.ModTime()
		}
	}
	consider(filepath.Join(sessionPath, "state.json"))
	consider(filepath.Join(sessionPath, "wire.jsonl"))
	agents := filepath.Join(sessionPath, "agents")
	if entries, err := os.ReadDir(agents); err == nil {
		for _, entry := range entries {
			consider(filepath.Join(agents, entry.Name(), "wire.jsonl"))
		}
	}
	return modifiedAt
}

func selectJSONLSession(
	candidates []LocatedCodexSession,
	selection CodexSessionSelection,
	harness string,
) (LocatedCodexSession, error) {
	// Rust's `max_by` keeps the last of equal maxima.
	newest := func(sessions []LocatedCodexSession) (LocatedCodexSession, bool) {
		var best *LocatedCodexSession
		for index := range sessions {
			candidate := &sessions[index]
			if best == nil {
				best = candidate
				continue
			}
			order := candidate.ModifiedAt.Compare(best.ModifiedAt)
			if order == 0 {
				order = importComparePaths(candidate.JSONLPath, best.JSONLPath)
			}
			if order >= 0 {
				best = candidate
			}
		}
		if best == nil {
			return LocatedCodexSession{}, false
		}
		return *best, true
	}
	switch selection := selection.(type) {
	case ClaudeSessionSelectionNativeSessionID:
		nativeSessionID := string(selection)
		if err := core.ValidateID(harness+" session", nativeSessionID); err != nil {
			return LocatedCodexSession{}, err
		}
		matching := []LocatedCodexSession{}
		for _, candidate := range candidates {
			if candidate.NativeSessionID == nativeSessionID {
				matching = append(matching, candidate)
			}
		}
		if session, ok := newest(matching); ok {
			return session, nil
		}
		return LocatedCodexSession{}, fmt.Errorf("%s session %s was not found", harness, core.RustDebugString(nativeSessionID))
	case ClaudeSessionSelectionLatest:
		if session, ok := newest(candidates); ok {
			return session, nil
		}
		return LocatedCodexSession{}, errors.New("no session JSONL files were found")
	}
	panic(fmt.Sprintf("unknown session selection %T", selection))
}
