// Port of mj-checkpoint/src/checkpoint/native_scan.rs.

package checkpoint

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"io/fs"
	"math"
	"os"
	"slices"
	"sort"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

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
	for _, path := range wire.NotOurs {
		c.NotOurs[path] = struct{}{}
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
	panic("unported: mj-checkpoint/src/checkpoint/native_scan.rs collect_native_artifacts_cached")
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
	panic("unported: mj-checkpoint/src/checkpoint/native_scan.rs collect_import_native_artifacts")
}

// collectMuseImportArtifacts: external Muse storage has a separate XDG
// root. Normalize the selected subtree into the same private layout used by
// ordinary worker checkpoints.
func collectMuseImportArtifacts(home string, sessionID string, sourcePath string) ([]core.NativeArtifact, error) {
	panic("unported: mj-checkpoint/src/checkpoint/native_scan.rs collect_muse_import_artifacts")
}

func collectKimiRegistryArtifacts(home string, sessionID string, output *[]core.NativeArtifact) error {
	panic("unported: mj-checkpoint/src/checkpoint/native_scan.rs collect_kimi_registry_artifacts")
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
	panic("unported: mj-checkpoint/src/checkpoint/native_scan.rs collect_claude_memory_tree")
}

func collectNativeTree(harness core.HarnessKind, home string, path string, sessionID string, insideSession bool, probe *codexProbeContext, output *[]core.NativeArtifact) error {
	panic("unported: mj-checkpoint/src/checkpoint/native_scan.rs collect_native_tree")
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
	panic("unported: mj-checkpoint/src/checkpoint/native_scan.rs sqlite_native_database_snapshot")
}

// sqliteSnapshot is a consistent copy of a database a harness may be
// writing, write-ahead log included, with edit applied to the copy. SQLite's
// backup API reads every page in one transaction, so the copy holds exactly
// what a reader of the live database sees; copying the file alone would
// leave out whatever is still in the log.
func sqliteSnapshot(path string, label string, edit func(*sql.Conn) error) ([]byte, error) {
	panic("unported: mj-checkpoint/src/checkpoint/native_scan.rs sqlite_snapshot")
}

// sqliteSnapshotFile is the same consistent copy, left in a private
// temporary file. The caller closes and removes it (Rust NamedTempFile
// removes it on drop).
func sqliteSnapshotFile(path string, label string, edit func(*sql.Conn) error) (*os.File, error) {
	panic("unported: mj-checkpoint/src/checkpoint/native_scan.rs sqlite_snapshot_file")
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
	panic("unported: mj-checkpoint/src/checkpoint/native_scan.rs codex_probe_selects")
}

func codexRolloutHasThreadID(path string, threadID string) bool {
	panic("unported: mj-checkpoint/src/checkpoint/native_scan.rs codex_rollout_has_thread_id")
}
