// Port of mj-checkpoint/src/checkpoint/native_scan.rs.

package checkpoint

import (
	"bufio"
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"math"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"modernc.org/sqlite"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// codexProbeFloorSlackMs is the clock-skew slack subtracted from a Codex
// session's own creation time before it is used as an mtime floor for
// content probes.
const codexProbeFloorSlackMs int64 = 48 * 3600 * 1000

const codexScanCacheFile = "codex-scan-cache.json"

// NoSessionArtifacts is how a checkpoint export fails when the harness saved
// nothing for a native session that received a prompt. The controller
// recognizes it in the export's stderr to tell the caller what happened.
const NoSessionArtifacts = "no session artifacts found"

// codexScanCache records rollouts whose `session_meta` header named a
// different resumable thread. Codex writes that header once, when it creates
// the file, so a negative verdict never turns positive and is safe to
// remember across exports.
//
// NotOurs is a set (Rust BTreeSet<PathBuf>); it serializes as a list in
// Rust path order.
type codexScanCache struct {
	SessionID string
	NotOurs   map[string]struct{}
}

type codexScanCacheWire struct {
	SessionID string   `json:"session_id"`
	NotOurs   []string `json:"not_ours"`
}

// MarshalJSON writes NotOurs as a list sorted as Rust orders paths.
func (c codexScanCache) MarshalJSON() ([]byte, error) {
	notOurs := make([]string, 0, len(c.NotOurs))
	for path := range c.NotOurs {
		notOurs = append(notOurs, path)
	}
	sort.Slice(notOurs, func(i, j int) bool { return checkpointPathCompare(notOurs[i], notOurs[j]) < 0 })
	return checkpointMarshal(codexScanCacheWire{SessionID: c.SessionID, NotOurs: notOurs})
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (c *codexScanCache) UnmarshalJSON(data []byte) error {
	var wire codexScanCacheWire
	if err := checkpointStrictDecode(data, &wire, true); err != nil {
		return err
	}
	*c = codexScanCacheEmpty(wire.SessionID)
	// Keys are compared as Rust compares paths, by component.
	for _, path := range wire.NotOurs {
		c.NotOurs[checkpointNormalize(path)] = struct{}{}
	}
	return nil
}

func codexScanCacheEmpty(sessionID string) codexScanCache {
	return codexScanCache{SessionID: sessionID, NotOurs: map[string]struct{}{}}
}

// codexProbeContext is everything that lets the Codex walk skip a content
// probe.
type codexProbeContext struct {
	// floorMs: rollouts modified before this unix-ms instant cannot belong
	// to the session, so they are never opened. nil disables the gate.
	floorMs *int64
	cache   *codexScanCache
}

// loadCodexScanCache: a missing, unreadable, corrupt, or foreign-session
// cache is not an error: the cache only ever saves work, so falling back to
// an empty one is correct.
func loadCodexScanCache(relayRoot string, sessionID string) codexScanCache {
	body, err := os.ReadFile(core.Join(relayRoot, codexScanCacheFile))
	if err == nil {
		var cache codexScanCache
		if json.Unmarshal(body, &cache) == nil && cache.SessionID == sessionID {
			return cache
		}
	}
	return codexScanCacheEmpty(sessionID)
}

func saveCodexScanCache(relayRoot string, cache *codexScanCache) error {
	body, err := checkpointMarshal(cache)
	if err != nil {
		return err
	}
	if err := writePrivateFile(relayRoot, codexScanCacheFile, body, 0o600); err != nil {
		return fmt.Errorf("write Codex scan cache %s: %w", core.Join(relayRoot, codexScanCacheFile), err)
	}
	return nil
}

// uuidV7TimestampMs: Codex native session IDs are UUIDv7, whose leading 48
// bits hold the session's creation time in unix milliseconds.
func uuidV7TimestampMs(id string) (int64, bool) {
	groups := strings.Split(id, "-")
	widths := []int{8, 4, 4, 4, 12}
	if len(groups) != len(widths) {
		return 0, false
	}
	for i, group := range groups {
		if len(group) != widths[i] || strings.TrimLeft(group, "0123456789abcdef") != "" {
			return 0, false
		}
	}
	if !strings.HasPrefix(groups[2], "7") {
		return 0, false
	}
	millis, err := strconv.ParseInt(groups[0]+groups[1], 16, 64)
	return millis, err == nil
}

// unixMillis is a time as signed unix milliseconds, truncated toward zero
// as Rust's Duration::as_millis is on either side of the epoch.
func unixMillis(t time.Time) (int64, bool) {
	seconds, nanos := t.Unix(), int64(t.Nanosecond())
	if seconds < 0 && nanos > 0 {
		seconds++
		nanos -= int64(time.Second)
	}
	if seconds > math.MaxInt64/1000 || seconds < math.MinInt64/1000 {
		return 0, false
	}
	return seconds*1000 + nanos/int64(time.Millisecond), true
}

func CollectNativeArtifacts(harness core.HarnessKind, home string, sessionID string, allowEmpty bool) ([]core.NativeArtifact, error) {
	return collectNativeArtifactsCached(harness, home, sessionID, allowEmpty, nil)
}

func collectNativeArtifactsCached(harness core.HarnessKind, home string, sessionID string, allowEmpty bool, cache *codexScanCache) ([]core.NativeArtifact, error) {
	if err := ValidateComponent(sessionID, "native session ID"); err != nil {
		return nil, err
	}
	roots := harness.NativeSessionDirs()
	var probe codexProbeContext
	if harness == core.HarnessKindCodex {
		if createdMs, ok := uuidV7TimestampMs(sessionID); ok {
			floor := createdMs - codexProbeFloorSlackMs
			probe.floorMs = &floor
		}
		probe.cache = cache
	}
	output := []core.NativeArtifact{}
	for _, relative := range roots {
		root := core.Join(home, relative)
		if checkpointIsDir(root) {
			if err := collectNativeTree(harness, home, root, sessionID, false, &probe, &output); err != nil {
				return nil, err
			}
		}
	}
	if harness == core.HarnessKindKimi && len(output) > 0 {
		if err := collectKimiRegistryArtifacts(home, sessionID, &output); err != nil {
			return nil, err
		}
	}
	if harness == core.HarnessKindClaude {
		if err := collectClaudeMemoryArtifacts(home, sessionID, &output); err != nil {
			return nil, err
		}
	}
	checkpointSortArtifacts(output)
	if !allowEmpty && !NativeArtifactsHoldConversation(harness, nativeArtifactPaths(output)) {
		return nil, errors.New(NoSessionArtifacts)
	}
	return output, nil
}

// nativeArtifactPaths are the artifacts' relative paths, in order.
func nativeArtifactPaths(artifacts []core.NativeArtifact) []string {
	paths := make([]string, len(artifacts))
	for i := range artifacts {
		paths[i] = artifacts[i].RelativePath
	}
	return paths
}

// NativeArtifactsHoldConversation is whether native artifacts, named by
// their paths in the harness home, hold a conversation the harness can load.
// The one rule for both sides: a capture refuses a prompted session without
// one, and a restore asks the harness to load the archived session only when
// its archive has one.
//
// Only the harness's own session files count. A checkpoint also carries
// Mjolnir's attachments, goals and sub-agent reports, and project memory,
// none of which is a conversation.
func NativeArtifactsHoldConversation(harness core.HarnessKind, relativePaths []string) bool {
	roots := harness.NativeSessionDirs()
	for _, path := range relativePaths {
		if isProjectMemoryArtifact(path) || !slices.ContainsFunc(roots, func(root string) bool { return checkpointPathStartsWith(path, root) }) {
			continue
		}
		// agy's `brain` files only support the conversation, which lives in
		// its database; agy loads a conversation without one and fails at
		// the next prompt, so the files alone are no history.
		if harness != core.HarnessKindAntigravity || isSqliteNativeDatabase(harness, path) {
			return true
		}
	}
	return false
}

// ArchiveHoldsNativeConversation is whether the archive holds the
// conversation of the native session it names. A session that never
// received a prompt can have none: Codex writes a thread's rollout only at
// its first turn, so its archive names a thread that no harness can load.
func ArchiveHoldsNativeConversation(manifest *core.ArchiveManifest) bool {
	var paths []string
	for i := range manifest.Payloads {
		if role := &manifest.Payloads[i].Role; role.Kind == core.PayloadRoleNativeArtifact {
			paths = append(paths, role.RelativePath)
		}
	}
	return NativeArtifactsHoldConversation(manifest.Session.HarnessKind, paths)
}

// isProjectMemoryArtifact: `projects/<slug>/memory/...`: Claude's project
// memory, and Mjolnir's project-memory replica in every harness home, both
// live there. Claude keeps its transcripts under `projects` too, so the
// memory has to be told apart from them.
func isProjectMemoryArtifact(relativePath string) bool {
	components := checkpointComponents(relativePath)
	return len(components) >= 3 &&
		components[0] == checkpointComponent{checkpointComponentNormal, "projects"} &&
		components[2] == checkpointComponent{checkpointComponentNormal, "memory"}
}

// CollectImportNativeArtifacts collects native artifacts for an import whose
// locator already resolved the exact source artifact. Codex rollouts are
// standalone JSONL files, so this avoids probing every historical rollout
// (and any unrelated corrupt one).
func CollectImportNativeArtifacts(harness core.HarnessKind, home string, sessionID string, sourcePath string) ([]core.NativeArtifact, error) {
	if harness == core.HarnessKindMuse {
		return collectMuseImportArtifacts(home, sessionID, sourcePath)
	}
	if harness != core.HarnessKindCodex {
		return CollectNativeArtifacts(harness, home, sessionID, false)
	}
	if err := ValidateComponent(sessionID, "native session ID"); err != nil {
		return nil, err
	}
	relative, ok := checkpointStripPrefix(sourcePath, home)
	if !ok {
		return nil, fmt.Errorf("Codex rollout %s is outside %s: %w", sourcePath, home, errCheckpointPrefixNotFound)
	}
	if err := validateRelativePath(relative); err != nil {
		return nil, err
	}
	components := checkpointComponents(relative)
	if len(components) == 0 || components[0].kind != checkpointComponentNormal ||
		(components[0].text != "sessions" && components[0].text != "archived_sessions") {
		return nil, fmt.Errorf("Codex rollout '%s' is outside a session root", sourcePath)
	}
	name, ok := checkpointFileName(sourcePath)
	if !ok || !utf8.ValidString(name) {
		name = ""
	}
	if !strings.HasSuffix(name, ".jsonl") && !strings.HasSuffix(name, ".jsonl.zst") {
		return nil, fmt.Errorf("Codex rollout '%s' is not a JSONL artifact", sourcePath)
	}
	info, err := os.Lstat(sourcePath)
	if err != nil {
		return nil, fmt.Errorf("stat Codex rollout %s: %w", sourcePath, err)
	}
	if checkpointIsSymlink(info) || !info.Mode().IsRegular() {
		return nil, fmt.Errorf("Codex rollout '%s' is not a regular file", sourcePath)
	}
	return []core.NativeArtifact{{
		RelativePath: relative,
		Data:         core.NativeArtifactDataFile(sourcePath),
		Mode:         fileMode(info),
	}}, nil
}

// collectMuseImportArtifacts: external Muse storage has a separate XDG
// root. Normalize the selected subtree into the same private layout used by
// ordinary worker checkpoints.
func collectMuseImportArtifacts(home string, sessionID string, sourcePath string) ([]core.NativeArtifact, error) {
	if err := ValidateComponent(sessionID, "Muse native session ID"); err != nil {
		return nil, err
	}
	sessionsRoot, err := MuseSessionsRoot(home)
	if err != nil {
		return nil, err
	}
	relative, ok := checkpointStripPrefix(sourcePath, sessionsRoot)
	if !ok {
		return nil, fmt.Errorf("Muse session log is outside its native session root: %w", errCheckpointPrefixNotFound)
	}
	if err := validateRelativePath(relative); err != nil {
		return nil, err
	}
	if err := ensureNoSymlinkAncestors(sessionsRoot, relative); err != nil {
		return nil, err
	}
	directory, ok := checkpointParent(sourcePath)
	if !ok {
		return nil, errors.New("Muse session has no directory")
	}
	name, hasName := checkpointFileName(sourcePath)
	directoryName, hasDirectoryName := checkpointFileName(directory)
	if !hasName || name != "session.jsonl" || !hasDirectoryName || directoryName != sessionID {
		return nil, errors.New("Muse native session ID does not match its source path")
	}
	output := []core.NativeArtifact{}
	if err := collectNativeTree(core.HarnessKindMuse, sessionsRoot, directory, sessionID, false, &codexProbeContext{}, &output); err != nil {
		return nil, err
	}
	if len(output) == 0 {
		return nil, errors.New("Muse native session contains no durable artifacts")
	}
	for i := range output {
		output[i].RelativePath = core.Join(".data/muse/sessions", output[i].RelativePath)
	}
	checkpointSortArtifacts(output)
	return output, nil
}

func collectKimiRegistryArtifacts(home string, sessionID string, output *[]core.NativeArtifact) error {
	sourceWorkspace, found := "", false
	for i := range *output {
		if workspace, ok := kimiSourceWorkspace((*output)[i].RelativePath, sessionID); ok {
			sourceWorkspace, found = workspace, true
			break
		}
	}
	if !found {
		return errors.New("Kimi native session state artifact is missing")
	}
	workspacesPath := core.Join(home, "workspaces.json")
	info, err := os.Lstat(workspacesPath)
	if err != nil {
		return fmt.Errorf("read Kimi workspace registry %s: %w", workspacesPath, err)
	}
	if checkpointIsSymlink(info) || !info.Mode().IsRegular() {
		return errors.New("Kimi workspace registry is not a regular file")
	}
	body, err := os.ReadFile(workspacesPath)
	if err != nil {
		return err
	}
	workspaces, err := jsonvalue.Parse(body)
	if err != nil {
		return fmt.Errorf("parse Kimi workspace registry: %w", err)
	}
	workspace, ok := checkpointJSONPointer(workspaces, "/workspaces/"+sourceWorkspace)
	if !ok {
		return fmt.Errorf("Kimi workspace %s is missing from registry", checkpointDebugString(sourceWorkspace))
	}
	registry, _ := workspaces.(jsonvalue.Object)
	version, _ := registry.Get("version")
	selected, err := jsonvalue.Marshal(jsonvalue.Object{
		{Key: "version", Value: version},
		{Key: "deleted_workspace_ids", Value: []any{}},
		{Key: "workspaces", Value: jsonvalue.Object{{Key: sourceWorkspace, Value: checkpointCloneJSON(workspace)}}},
	})
	if err != nil {
		return err
	}
	*output = append(*output, core.NativeArtifact{
		RelativePath: "workspaces.json",
		Data:         core.NativeArtifactDataBytes(selected),
		Mode:         fileMode(info),
	})

	indexPath := core.Join(home, "session_index.jsonl")
	info, err = os.Lstat(indexPath)
	if err != nil {
		return fmt.Errorf("read Kimi session index %s: %w", indexPath, err)
	}
	if checkpointIsSymlink(info) || !info.Mode().IsRegular() {
		return errors.New("Kimi session index is not a regular file")
	}
	index, err := os.Open(indexPath)
	if err != nil {
		return err
	}
	defer index.Close()
	var entries []byte
	reader := bufio.NewReader(index)
	for lineNumber := 0; ; lineNumber++ {
		line, more, err := checkpointReadLine(reader)
		if err != nil {
			return err
		}
		if !more {
			break
		}
		if strings.TrimSpace(line) == "" {
			continue
		}
		entry, err := jsonvalue.Parse([]byte(line))
		if err != nil {
			return fmt.Errorf("parse Kimi session index line %d: %w", lineNumber+1, err)
		}
		object, _ := entry.(jsonvalue.Object)
		if id, _ := object.Get("sessionId"); id == any(sessionID) {
			if entries, err = jsonvalue.Append(entries, entry); err != nil {
				return err
			}
			entries = append(entries, '\n')
		}
	}
	if len(entries) == 0 {
		return fmt.Errorf("Kimi session index does not contain native session %s", checkpointDebugString(sessionID))
	}
	*output = append(*output, core.NativeArtifact{
		RelativePath: "session_index.jsonl",
		Data:         core.NativeArtifactDataBytes(entries),
		Mode:         fileMode(info),
	})
	return nil
}

// checkpointReadLine is one step of Rust's BufRead::lines: the next line
// without its "\n" or "\r\n", false at the end of the input, and an error for
// a line that is not UTF-8.
func checkpointReadLine(reader *bufio.Reader) (string, bool, error) {
	line, err := reader.ReadString('\n')
	if err != nil && !errors.Is(err, io.EOF) {
		return "", false, err
	}
	if line == "" && err != nil {
		return "", false, nil
	}
	if !utf8.ValidString(line) {
		return "", false, errors.New("stream did not contain valid UTF-8")
	}
	if strings.HasSuffix(line, "\n") {
		line = strings.TrimSuffix(strings.TrimSuffix(line, "\n"), "\r")
	}
	return line, true, nil
}

// checkpointJSONPointer is serde_json's Value::pointer.
func checkpointJSONPointer(value any, pointer string) (any, bool) {
	if pointer == "" {
		return value, true
	}
	if !strings.HasPrefix(pointer, "/") {
		return nil, false
	}
	target := value
	for _, token := range strings.Split(pointer, "/")[1:] {
		token = strings.ReplaceAll(strings.ReplaceAll(token, "~1", "/"), "~0", "~")
		switch current := target.(type) {
		case jsonvalue.Object:
			next, ok := current.Get(token)
			if !ok {
				return nil, false
			}
			target = next
		case []any:
			if strings.HasPrefix(token, "+") || (strings.HasPrefix(token, "0") && len(token) != 1) {
				return nil, false
			}
			index, err := strconv.ParseUint(token, 10, 64)
			if err != nil || index >= uint64(len(current)) {
				return nil, false
			}
			target = current[index]
		default:
			return nil, false
		}
	}
	return target, true
}

// checkpointCloneJSON is Value::clone for a decoded JSON value.
func checkpointCloneJSON(value any) any {
	switch value := value.(type) {
	case jsonvalue.Object:
		return value.Clone()
	case []any:
		cloned := make([]any, len(value))
		for i := range value {
			cloned[i] = checkpointCloneJSON(value[i])
		}
		return cloned
	}
	return value
}

func kimiSourceWorkspace(relativePath string, sessionID string) (string, bool) {
	components := checkpointComponents(relativePath)
	if len(components) != 4 || components[0] != (checkpointComponent{checkpointComponentNormal, "sessions"}) {
		return "", false
	}
	workspace, session, file := components[1].text, components[2].text, components[3].text
	if !utf8.ValidString(workspace) || !utf8.ValidString(session) || !utf8.ValidString(file) {
		return "", false
	}
	if file == "state.json" && (session == sessionID || session == "session_"+sessionID) {
		return workspace, true
	}
	return "", false
}

// collectClaudeMemoryArtifacts: Claude keeps per-project memory next to the
// transcripts, outside the session-id subtree the main pass walks, so
// capture it in a post-pass.
//
// The memory directory is scoped to the slug that owns this session's
// transcript. On a LocalBare target the harness home is the user's real
// `~/.claude`, which holds memory for every unrelated project; only the
// session's own project memory may leave the machine.
func collectClaudeMemoryArtifacts(home string, sessionID string, output *[]core.NativeArtifact) error {
	var slugs []string
	for i := range *output {
		if slug, ok := claudeSessionProjectSlug((*output)[i].RelativePath, sessionID); ok {
			slugs = append(slugs, slug)
		}
	}
	sort.Strings(slugs)
	// An unprompted session exported with `allow_empty` has no transcript,
	// so there is no project to scope memory to.
	for _, slug := range slices.Compact(slugs) {
		root := core.Join(core.Join(core.Join(home, "projects"), slug), "memory")
		if info, err := os.Stat(root); err == nil && info.IsDir() {
			if err := collectClaudeMemoryTree(home, root, output); err != nil {
				return err
			}
		}
	}
	return nil
}

// claudeSessionProjectSlug returns the project slug when relativePath is
// this session's transcript (`projects/<slug>/<session_id>.jsonl`) or lives
// in its session subtree (`projects/<slug>/<session_id>/...`).
func claudeSessionProjectSlug(relativePath string, sessionID string) (string, bool) {
	components := checkpointComponents(relativePath)
	if len(components) < 3 || components[0] != (checkpointComponent{checkpointComponentNormal, "projects"}) ||
		!utf8.ValidString(components[1].text) || !utf8.ValidString(components[2].text) {
		return "", false
	}
	entry := components[2].text
	isTranscript := entry == sessionID+".jsonl" && len(components) == 3
	if isTranscript || entry == sessionID {
		return components[1].text, true
	}
	return "", false
}

func collectClaudeMemoryTree(home string, path string, output *[]core.NativeArtifact) error {
	info, err := os.Lstat(path)
	if err != nil {
		return err
	}
	if checkpointIsSymlink(info) {
		return nil
	}
	if info.IsDir() {
		entries, err := os.ReadDir(path)
		if err != nil {
			return err
		}
		for _, entry := range entries {
			if err := collectClaudeMemoryTree(home, core.Join(path, entry.Name()), output); err != nil {
				return err
			}
		}
		return nil
	}
	if !info.Mode().IsRegular() {
		return nil
	}
	relative, ok := checkpointStripPrefix(path, home)
	if !ok {
		return errCheckpointPrefixNotFound
	}
	if err := validateRelativePath(relative); err != nil {
		return err
	}
	*output = append(*output, core.NativeArtifact{
		RelativePath: relative,
		Data:         core.NativeArtifactDataFile(path),
		Mode:         fileMode(info),
	})
	return nil
}

func collectNativeTree(harness core.HarnessKind, home string, path string, sessionID string, insideSession bool, probe *codexProbeContext, output *[]core.NativeArtifact) error {
	info, err := os.Lstat(path)
	if err != nil {
		return err
	}
	if checkpointIsSymlink(info) {
		return nil
	}
	fileName, hasName := checkpointFileName(path)
	inside := insideSession || (hasName && (fileName == sessionID ||
		(harness == core.HarnessKindKimi && fileName == "session_"+sessionID)))
	if info.IsDir() {
		entries, err := os.ReadDir(path)
		if err != nil {
			return err
		}
		for _, entry := range entries {
			if err := collectNativeTree(harness, home, core.Join(path, entry.Name()), sessionID, inside, probe, output); err != nil {
				return err
			}
		}
		return nil
	}
	if !info.Mode().IsRegular() {
		return errors.New("native artifact is not a regular file")
	}
	name := fileName
	if !utf8.ValidString(name) {
		name = ""
	}
	relative, ok := checkpointStripPrefix(path, home)
	if !ok {
		return errCheckpointPrefixNotFound
	}
	var selected bool
	switch harness {
	case core.HarnessKindCodex:
		selected = (strings.Contains(name, sessionID) &&
			(strings.HasSuffix(name, ".jsonl") || strings.HasSuffix(name, ".jsonl.zst"))) ||
			(strings.HasSuffix(name, ".jsonl") && codexProbeSelects(probe, path, relative, info, sessionID))
	case core.HarnessKindClaude:
		selected = inside || name == sessionID+".jsonl"
	case core.HarnessKindKimi:
		selected = inside && kimiSessionArtifact(relative, sessionID)
	case core.HarnessKindGrok:
		selected = inside && grokSessionArtifact(relative, sessionID)
	case core.HarnessKindMuse:
		selected = inside && name == "session.jsonl"
	case core.HarnessKindOpenCode:
		selected = isOpencodeNativeArtifact(relative)
	case core.HarnessKindAntigravity:
		selected = isAntigravityNativeArtifact(relative, sessionID)
	case core.HarnessKindDsh:
		selected = isDshSessionRecord(relative, sessionID)
	default:
		return fmt.Errorf("unknown harness kind %q", string(harness))
	}
	if !selected {
		return nil
	}
	if err := validateRelativePath(relative); err != nil {
		return err
	}
	// SQLite snapshots use the backup API; their database representation
	// stays in memory.
	var data core.NativeArtifactData = core.NativeArtifactDataFile(path)
	if isSqliteNativeDatabase(harness, relative) {
		snapshot, err := sqliteNativeDatabaseSnapshot(harness, path)
		if err != nil {
			return err
		}
		data = core.NativeArtifactDataBytes(snapshot)
	}
	*output = append(*output, core.NativeArtifact{
		RelativePath: relative,
		Data:         data,
		Mode:         fileMode(info),
	})
	return nil
}

// isDshSessionRecord: dsh-acp writes each session to `sessions/<id>.json`
// by renaming a `.<id>.<suffix>.tmp` file over it, so the record itself is
// complete and an interrupted write's temporary file is never captured.
func isDshSessionRecord(relativePath string, sessionID string) bool {
	return checkpointPathEqual(relativePath, core.Join("sessions", sessionID+".json"))
}

// opencodeDatabase is OpenCode's database within a session home.
const opencodeDatabase = ".data/opencode/opencode.db"

// isOpencodeNativeArtifact: OpenCode keeps every session of a data directory
// in one SQLite database, opencodeDatabase. A session home has exactly one
// data directory, so the database is the session's native state.
func isOpencodeNativeArtifact(relativePath string) bool {
	return checkpointPathEqual(relativePath, opencodeDatabase)
}

// antigravityConversations is where agy keeps one SQLite database per
// conversation, named by the ACP session ID, and antigravityBrain the
// directory per conversation beside it.
const (
	antigravityConversations = "antigravity-cli/conversations"
	antigravityBrain         = "antigravity-cli/brain"
)

// isAntigravityNativeArtifact: a native session's own agy files: its
// conversation database, and everything under its `brain` directory (the
// full text of long tool output, which the conversation points the model
// at, and agy's artifacts). Never the login, the logs, the indexes shared by
// all conversations, or another conversation's files. The database's `-wal`
// and `-shm` files are not selected: the snapshot taken of the database
// already holds their pages.
func isAntigravityNativeArtifact(relativePath string, sessionID string) bool {
	return checkpointPathEqual(relativePath, core.Join(antigravityConversations, sessionID+".db")) ||
		checkpointPathStartsWith(relativePath, core.Join(antigravityBrain, sessionID))
}

// isSqliteNativeDatabase is whether relativePath is a SQLite database a
// checkpoint carries as a snapshot, which is also the one rule for what a
// restore must not leave beside the database it replaces (sqliteSidecars).
func isSqliteNativeDatabase(harness core.HarnessKind, relativePath string) bool {
	switch harness {
	case core.HarnessKindOpenCode:
		return isOpencodeNativeArtifact(relativePath)
	case core.HarnessKindAntigravity:
		parent, hasParent := checkpointParent(relativePath)
		extension, hasExtension := checkpointExtension(relativePath)
		return hasParent && checkpointPathEqual(parent, antigravityConversations) && hasExtension && extension == "db"
	}
	return false
}

// sqliteSidecars are the files SQLite keeps beside a database while it is
// open: the write-ahead log, whose pages SQLite replays over the database
// when it next opens it, and the shared-memory index of that log. A
// snapshot already holds the log's pages, so a restore removes any it finds
// beside the database it writes; an old log replayed over the restored pages
// would corrupt them.
func sqliteSidecars(database string) [2]string {
	return [2]string{database + "-wal", database + "-shm"}
}

// opencodeLoginTables are the tables in which OpenCode keeps logins:
// provider and connector credentials, and the tokens of OpenCode's own
// accounts.
var opencodeLoginTables = [3]string{"credential", "account", "control_account"}

// sqliteNativeDatabaseSnapshot is a database as a checkpoint carries it.
// OpenCode 2 keeps its logins in the same file as its sessions, and every
// launch imports the profile's logins into the staged home afresh, so a
// checkpoint needs none of them and must not carry their secrets. agy's
// logins are in another file, so its conversation database needs nothing
// removed.
func sqliteNativeDatabaseSnapshot(harness core.HarnessKind, path string) ([]byte, error) {
	if harness == core.HarnessKindOpenCode {
		return sqliteSnapshot(path, "OpenCode", func(copy *sql.Conn) error {
			ctx := context.Background()
			for _, table := range opencodeLoginTables {
				var exists bool
				if err := copy.QueryRowContext(ctx,
					"SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
					table).Scan(&exists); err != nil {
					return err
				}
				if exists {
					if _, err := copy.ExecContext(ctx, fmt.Sprintf(`DELETE FROM "%s"`, table)); err != nil {
						return err
					}
				}
			}
			// VACUUM rebuilds the file, so no freed page keeps a deleted
			// login.
			_, err := copy.ExecContext(ctx, "VACUUM;")
			return err
		})
	}
	return sqliteSnapshot(path, harness.DisplayName(), func(*sql.Conn) error { return nil })
}

// sqliteSnapshot is a consistent copy of a database a harness may be
// writing, write-ahead log included, with edit applied to the copy. SQLite's
// backup API reads every page in one transaction, so the copy holds exactly
// what a reader of the live database sees; copying the file alone would
// leave out whatever is still in the log.
func sqliteSnapshot(path string, label string, edit func(*sql.Conn) error) ([]byte, error) {
	snapshot, err := sqliteSnapshotFile(path, label, edit)
	if err != nil {
		return nil, err
	}
	defer removeSqliteSnapshotFile(snapshot)
	data, err := os.ReadFile(snapshot.Name())
	if err != nil {
		return nil, fmt.Errorf("read %s database snapshot %s: %w", label, snapshot.Name(), err)
	}
	return data, nil
}

// sqliteSnapshotFile is the same consistent copy, left in a private
// temporary file. The caller closes and removes it (Rust NamedTempFile
// removes it on drop), for example with removeSqliteSnapshotFile.
func sqliteSnapshotFile(path string, label string, edit func(*sql.Conn) error) (*os.File, error) {
	scratch, err := os.CreateTemp("", "snapshot*")
	if err != nil {
		return nil, fmt.Errorf("create %s snapshot file: %w", label, err)
	}
	if err := sqliteCopyInto(path, scratch.Name(), label, edit); err != nil {
		removeSqliteSnapshotFile(scratch)
		return nil, fmt.Errorf("snapshot %s database %s: %w", label, path, err)
	}
	return scratch, nil
}

// removeSqliteSnapshotFile closes and removes a sqliteSnapshotFile copy.
func removeSqliteSnapshotFile(snapshot *os.File) {
	snapshot.Close()
	os.Remove(snapshot.Name())
}

// sqliteBusy and sqliteLocked are SQLite's primary result codes for a
// database another connection holds.
const (
	sqliteBusy   = 5
	sqliteLocked = 6
)

// sqliteCopyInto copies every page of the database at source into the
// database file at destination in one backup step, then applies edit to the
// copy and moves any log the copy keeps into its file.
func sqliteCopyInto(source string, destination string, label string, edit func(*sql.Conn) error) error {
	ctx := context.Background()
	sourceDB, err := sql.Open("sqlite", sqliteURI(source, "mode=ro"))
	if err != nil {
		return err
	}
	defer sourceDB.Close()
	sourceConn, err := sourceDB.Conn(ctx)
	if err != nil {
		return err
	}
	defer sourceConn.Close()
	// The running harness may hold a brief write lock.
	if _, err := sourceConn.ExecContext(ctx, "PRAGMA busy_timeout = 5000"); err != nil {
		return err
	}
	// SQLite's backup takes the destination as a path, whatever its
	// encoding. Copying every page in one step reads them all in one
	// transaction, so the copy is consistent, write-ahead log included.
	err = sourceConn.Raw(func(driverConn any) error {
		backuper, ok := driverConn.(interface {
			NewBackup(string) (*sqlite.Backup, error)
		})
		if !ok {
			return errors.New("the SQLite driver cannot back up a database")
		}
		backup, err := backuper.NewBackup(sqliteURI(destination, ""))
		if err != nil {
			return err
		}
		deadline := time.Now().Add(5 * time.Second)
		for {
			more, err := backup.Step(-1)
			if err == nil && !more {
				break
			}
			state := "More"
			if err != nil {
				var failure *sqlite.Error
				if !errors.As(err, &failure) {
					backup.Finish()
					return err
				}
				switch failure.Code() & 0xff {
				case sqliteBusy:
					state = "Busy"
				case sqliteLocked:
					state = "Locked"
				default:
					backup.Finish()
					return err
				}
			}
			// A writer held the database; wait it out, briefly.
			if time.Now().Before(deadline) {
				time.Sleep(50 * time.Millisecond)
				continue
			}
			backup.Finish()
			return fmt.Errorf("%s's database stayed busy (%s)", label, state)
		}
		return backup.Finish()
	})
	if err != nil {
		return err
	}
	sourceConn.Close()
	sourceDB.Close()

	copyDB, err := sql.Open("sqlite", sqliteURI(destination, ""))
	if err != nil {
		return err
	}
	defer copyDB.Close()
	copyConn, err := copyDB.Conn(ctx)
	if err != nil {
		return err
	}
	defer copyConn.Close()
	if err := edit(copyConn); err != nil {
		return err
	}
	// A copy in WAL mode keeps its pages in a log until a checkpoint moves
	// them into the file itself, which is all the checkpoint carries.
	if _, err := copyConn.ExecContext(ctx, "PRAGMA wal_checkpoint(TRUNCATE);"); err != nil {
		return err
	}
	if err := copyConn.Close(); err != nil {
		return err
	}
	return copyDB.Close()
}

// sqliteURI names a database file as the SQLite URI the driver opens, so
// that no character of the path is read as a query or as a driver option.
func sqliteURI(path string, query string) string {
	slashed := filepath.ToSlash(path)
	if filepath.VolumeName(path) != "" && !strings.HasPrefix(slashed, "/") {
		slashed = "/" + slashed
	}
	var uri strings.Builder
	uri.WriteString("file:")
	for i := 0; i < len(slashed); i++ {
		b := slashed[i]
		switch {
		case 'A' <= b && b <= 'Z', 'a' <= b && b <= 'z', '0' <= b && b <= '9',
			b == '-', b == '_', b == '.', b == '~', b == '/', b == ':':
			uri.WriteByte(b)
		default:
			fmt.Fprintf(&uri, "%%%02X", b)
		}
	}
	if query != "" {
		uri.WriteByte('?')
		uri.WriteString(query)
	}
	return uri.String()
}

func kimiSessionArtifact(relative string, sessionID string) bool {
	var components []string
	for _, component := range checkpointComponents(relative) {
		if component.kind == checkpointComponentNormal && utf8.ValidString(component.text) {
			components = append(components, component.text)
		}
	}
	index := slices.IndexFunc(components, func(component string) bool {
		return component == sessionID || component == "session_"+sessionID
	})
	if index < 0 {
		return false
	}
	tail := components[index+1:]
	return (len(tail) == 1 && tail[0] == "state.json") ||
		(len(tail) == 3 && tail[0] == "agents" && tail[2] == "wire.jsonl")
}

// codexProbeSelects is the content-probe fallback for a root rollout whose
// filename no longer carries its resumable thread ID, for example after
// Codex archives or renames it. Opening every rollout costs gigabytes of
// reads on a busy `~/.codex`, so two gates come first.
//
// The mtime floor is filesystem truth: rollout filenames encode ambiguous
// local time, historical rollouts are never rewritten, and hel's own restore
// rewrites the files it installs with a fresh mtime. A rollout last modified
// before the session was created cannot mention that session.
func codexProbeSelects(probe *codexProbeContext, path string, relative string, info fs.FileInfo, sessionID string) bool {
	// An unreadable mtime or a non-UUIDv7 session ID fails open into the
	// probe.
	if probe.floorMs != nil {
		if modifiedMs, ok := unixMillis(info.ModTime()); ok && modifiedMs < *probe.floorMs {
			return false
		}
	}
	key := checkpointNormalize(relative)
	if probe.cache != nil {
		if _, ok := probe.cache.NotOurs[key]; ok {
			return false
		}
	}
	if codexRolloutHasThreadID(path, sessionID) {
		return true
	}
	if probe.cache != nil {
		probe.cache.NotOurs[key] = struct{}{}
	}
	return false
}

func codexRolloutHasThreadID(path string, threadID string) bool {
	file, err := os.Open(path)
	if err != nil {
		return false
	}
	defer file.Close()
	reader := bufio.NewReader(file)
	for range 8 {
		line, err := reader.ReadString('\n')
		if err != nil && !errors.Is(err, io.EOF) {
			return false
		}
		if !utf8.ValidString(line) {
			return false
		}
		if line == "" {
			break
		}
		value, parseErr := jsonvalue.Parse([]byte(line))
		if parseErr != nil {
			return false
		}
		record, _ := value.(jsonvalue.Object)
		// A rollout carries exactly one `session_meta` header, so the first
		// one settles the question without parsing the rest of the file.
		if kind, _ := record.Get("type"); kind == any("session_meta") {
			payloadValue, ok := record.Get("payload")
			if !ok {
				return false
			}
			payload, _ := payloadValue.(jsonvalue.Object)
			// Modern Codex stores the resumable thread ID in `id` and a
			// shared session-tree ID in `session_id`. Child agents therefore
			// have the same `session_id` as the root but must not be
			// archived as roots. Only old records with no `id` field use the
			// legacy fallback.
			if id, ok := payload.Get("id"); ok {
				return id == any(threadID)
			}
			sessionID, _ := payload.Get("session_id")
			return sessionID == any(threadID)
		}
		if err != nil {
			break
		}
	}
	return false
}
