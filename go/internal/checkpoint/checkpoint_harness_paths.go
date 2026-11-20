// Port of mj-checkpoint/src/checkpoint/harness_paths.rs.

package checkpoint

import (
	"crypto/sha256"
	"errors"
	"fmt"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"lukechampine.com/blake3"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func rewriteKimiWorkspaceRegistry(data []byte, targetCwd string) ([]byte, error) {
	value, err := jsonvalue.Parse(data)
	if err != nil {
		return nil, fmt.Errorf("parse Kimi workspace registry: %w", err)
	}
	registry, _ := value.(jsonvalue.Object)
	listed, _ := registry.Get("workspaces")
	workspaces, ok := listed.(jsonvalue.Object)
	if !ok {
		return nil, errors.New("Kimi workspace registry has no workspaces object")
	}
	keys := workspaces.Keys()
	if len(keys) != 1 {
		return nil, errors.New("Kimi workspace registry must contain one imported workspace")
	}
	entry, _ := workspaces.Get(keys[0])
	workspace, ok := entry.(jsonvalue.Object)
	if !ok {
		return nil, errors.New("Kimi workspace registry entry is not an object")
	}
	workspace = workspace.Clone()
	workspace.Set("root", checkpointLossy([]byte(targetCwd)))
	if name, ok := checkpointFileName(targetCwd); ok && utf8.ValidString(name) {
		workspace.Set("name", name)
	}
	registry.Set("workspaces", jsonvalue.Object{{Key: kimiWorkspaceKey(targetCwd), Value: workspace}})
	return jsonvalue.Marshal(registry)
}

func rewriteKimiSessionIndex(data []byte, targetCwd string, harnessHome string) ([]byte, error) {
	text, err := gitDecodeUTF8(data)
	if err != nil {
		return nil, fmt.Errorf("decode Kimi session index: %w", err)
	}
	var rewritten []byte
	targetWorkspace := kimiWorkspaceKey(targetCwd)
	for lineNumber, line := range checkpointLines(text) {
		if strings.TrimSpace(line) == "" {
			continue
		}
		value, err := jsonvalue.Parse([]byte(line))
		if err != nil {
			return nil, fmt.Errorf("parse Kimi session index line %d: %w", lineNumber+1, err)
		}
		entry, isObject := value.(jsonvalue.Object)
		sessionValue, _ := entry.Get("sessionId")
		sessionID, ok := sessionValue.(string)
		if !ok {
			return nil, errors.New("Kimi session index entry lacks sessionId")
		}
		if !isObject {
			return nil, errors.New("Kimi session index entry is not an object")
		}
		entry.Set("workDir", checkpointLossy([]byte(targetCwd)))
		sessionDir := core.Join(core.Join(core.Join(harnessHome, "sessions"), targetWorkspace), sessionID)
		entry.Set("sessionDir", checkpointLossy([]byte(sessionDir)))
		if rewritten, err = jsonvalue.Append(rewritten, entry); err != nil {
			return nil, err
		}
		rewritten = append(rewritten, '\n')
	}
	if len(rewritten) == 0 {
		return nil, errors.New("Kimi session index has no imported sessions")
	}
	return rewritten, nil
}

// checkpointLines splits text as Rust's `str::lines` does: on "\n", dropping
// one "\r" before it, with no empty last line for a trailing newline.
func checkpointLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.SplitAfter(text, "\n")
	if lines[len(lines)-1] == "" {
		lines = lines[:len(lines)-1]
	}
	for i, line := range lines {
		line = strings.TrimSuffix(line, "\n")
		lines[i] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

// harnessStorageCwd is the working directory a harness keys its native
// session storage by, given the directory the worker launches it in. Grok
// Build encodes the cwd text it receives, so `<dir>/` and `<dir>` are
// different storage keys to it; Claude Code and Kimi Code key by the
// directory itself (Kimi hashes it without the trailing separator even
// though its registry records the text), and Muse only records it.
func harnessStorageCwd(harness core.HarnessKind, launchCwd string) string {
	if harness == core.HarnessKindGrok {
		return launchCwd
	}
	return checkpointNormalize(launchCwd)
}

func isKimiSessionState(relativePath string) bool {
	components := checkpointComponents(relativePath)
	if len(components) != 4 {
		return false
	}
	for _, component := range components {
		if component.kind != checkpointComponentNormal {
			return false
		}
	}
	return components[0].text == "sessions" &&
		strings.HasPrefix(checkpointLossy([]byte(components[2].text)), "session_") &&
		components[3].text == "state.json"
}

func isGrokSessionSummary(relativePath string) bool {
	components, ok := grokSessionComponents(relativePath)
	return ok && components.file == "summary.json"
}

// rewriteGrokSessionSummary: Grok Build records the session's working
// directory and home in `summary.json`; both must follow the restored
// session to its new workspace.
func rewriteGrokSessionSummary(data []byte, targetCwd string, harnessHome string) ([]byte, error) {
	value, err := jsonvalue.Parse(data)
	if err != nil {
		return nil, fmt.Errorf("parse Grok Build session summary: %w", err)
	}
	summary, ok := value.(jsonvalue.Object)
	if !ok {
		return nil, errors.New("Grok Build session summary is not a JSON object")
	}
	if _, ok := summary.Get("grok_home"); ok {
		summary.Set("grok_home", checkpointLossy([]byte(harnessHome)))
	}
	if value, ok := summary.Get("info"); ok {
		if info, ok := value.(jsonvalue.Object); ok {
			if _, ok := info.Get("cwd"); ok {
				info.Set("cwd", checkpointLossy([]byte(targetCwd)))
				summary.Set("info", info)
			}
		}
	}
	return jsonvalue.Marshal(summary)
}

type grokSessionPath struct {
	session string
	file    string
}

// grokSessionComponents splits `sessions/<cwd-key>/<session-uuid>/<file>`
// into the parts Hel needs. Anything with a different shape is not a Grok
// Build session artifact.
func grokSessionComponents(relative string) (grokSessionPath, bool) {
	components := checkpointComponents(relative)
	if len(components) != 4 {
		return grokSessionPath{}, false
	}
	for _, component := range components {
		if component.kind != checkpointComponentNormal || !utf8.ValidString(component.text) {
			return grokSessionPath{}, false
		}
	}
	if components[0].text != "sessions" {
		return grokSessionPath{}, false
	}
	return grokSessionPath{session: components[2].text, file: components[3].text}, true
}

// grokSessionArtifact excludes runtime state that must not travel in a
// checkpoint: advisory lock files and the sessions-wide search index.
func grokSessionArtifact(relative string, sessionID string) bool {
	components, ok := grokSessionComponents(relative)
	return ok && components.session == sessionID &&
		!strings.HasSuffix(components.file, ".lock") &&
		!strings.HasPrefix(components.file, "session_search.sqlite")
}

// grokCwdKey is Grok Build's on-disk cwd-key algorithm, replicated from
// grok-build `xai-grok-config::paths::encode_cwd_dirname`: URL-encode the
// working directory, or fall back to `{slug}-{blake3-hex-16}` when that
// would exceed one filesystem name.
func grokCwdKey(cwd string) string {
	// macOS APFS, Linux ext4, and NTFS all cap a name at 255 bytes.
	const maxDirnameBytes = 255
	const maxSlugChars = 40

	text := checkpointLossy([]byte(cwd))
	encoded := urlEncode(text)
	if len(encoded) <= maxDirnameBytes {
		return encoded
	}
	digest := blake3.Sum256([]byte(text))
	leaf, ok := checkpointFileName(text)
	if !ok || !utf8.ValidString(leaf) {
		leaf = "workspace"
	}
	slug := grokSlug(leaf, maxSlugChars)
	if slug == "" {
		slug = "workspace"
	}
	return fmt.Sprintf("%s-%s", slug, core.LowerHex(digest[:])[:16])
}

// urlEncode percent-encodes every byte outside the RFC 3986 unreserved set,
// matching the `urlencoding` crate Grok Build uses.
func urlEncode(value string) string {
	var encoded strings.Builder
	encoded.Grow(len(value))
	for i := 0; i < len(value); i++ {
		b := value[i]
		switch {
		case 'A' <= b && b <= 'Z', 'a' <= b && b <= 'z', '0' <= b && b <= '9', b == '-', b == '_', b == '.', b == '~':
			encoded.WriteByte(b)
		default:
			fmt.Fprintf(&encoded, "%%%02X", b)
		}
	}
	return encoded.String()
}

// grokSlug is Grok Build's `slugify`: lowercase, non-alphanumerics collapse
// to a single dash, trim dashes, truncate to maxChars.
func grokSlug(input string, maxChars int) string {
	var slug strings.Builder
	previousDash := false
	for _, character := range grokLowercase(input) {
		if character < 0x80 && (('a' <= character && character <= 'z') || ('A' <= character && character <= 'Z') || ('0' <= character && character <= '9')) {
			slug.WriteRune(character)
			previousDash = false
		} else if !previousDash {
			slug.WriteByte('-')
			previousDash = true
		}
	}
	trimmed := strings.Trim(slug.String(), "-")
	if len(trimmed) > maxChars {
		trimmed = trimmed[:maxChars]
	}
	return trimmed
}

// grokLowercase is Rust's str::to_lowercase as far as it can reach an ASCII
// letter: U+0130 lowercases to "i̇" there, where Go's ToLower gives a
// bare "i". Every other difference stays outside ASCII, which the slug turns
// into a dash either way.
func grokLowercase(input string) string {
	return strings.ToLower(strings.ReplaceAll(input, "İ", "i̇"))
}

// ClaudeProjectSlug is Claude Code's on-disk project-key algorithm, captured
// from local rollouts: every non-ASCII-alphanumeric cwd character becomes a
// hyphen.
func ClaudeProjectSlug(cwd string) string {
	var slug strings.Builder
	for _, character := range checkpointLossy([]byte(cwd)) {
		if character < 0x80 && (('a' <= character && character <= 'z') || ('A' <= character && character <= 'Z') || ('0' <= character && character <= '9')) {
			slug.WriteRune(character)
		} else {
			slug.WriteByte('-')
		}
	}
	return slug.String()
}

// kimiWorkspaceKey: Kimi Code keys session directories by the final cwd
// component and the first 12 hexadecimal digits of SHA-256(cwd), captured
// from real rollouts.
func kimiWorkspaceKey(cwd string) string {
	basename, ok := checkpointFileName(cwd)
	if !ok || basename == "" || !utf8.ValidString(basename) {
		basename = "workspace"
	}
	digest := sha256.Sum256([]byte(checkpointLossy([]byte(cwd))))
	return fmt.Sprintf("wd_%s_%s", basename, core.LowerHex(digest[:])[:12])
}
