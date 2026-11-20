// Port of mj-core/src/harness_runtime.rs.

// Exact harness versions used by Mjolnir-managed installations.
//
// Installation and process ownership live in `brokk-mj-worker`; this module
// holds shared pin metadata and managed-install paths/manifests so the
// controller and worker cannot silently disagree about an installation.

package core

import (
	"bytes"
	"encoding/json"
	"fmt"
	"strconv"
	"strings"
)

const (
	ManifestFile        = "mj-harness.json"
	LeaseFile           = ".lease"
	CodexCLIEntrypoint  = "node_modules/.bin/codex"
	managedRuntimesDir  = "mjolnir"
	managedHarnessesDir = "harnesses"
	managedNodeDir      = "node"
)

// NodeVersion is the exact Node.js release Mjolnir installs when the target
// has no compatible runtime. Keep this in lockstep with
// `assets/node/runtime.json`.
const NodeVersion = "24.22.1"

// MiseVersion: keep in lockstep with `assets/mise/runtime.json`. Static musl
// binary hashes:
// https://github.com/jdx/mise/releases/download/v2026.11.9/SHASUMS256.txt
const MiseVersion = "2026.11.9"

// ManagedHarnessManifest is the install receipt written by the worker and
// checked by managed clients. Serialized with deny_unknown_fields.
type ManagedHarnessManifest struct {
	schema    uint32
	harness   HarnessKind
	installID string
}

type managedHarnessManifestWire struct {
	Schema    *uint32      `json:"schema"`
	Harness   *HarnessKind `json:"harness"`
	InstallID *string      `json:"install_id"`
}

func (m ManagedHarnessManifest) MarshalJSON() ([]byte, error) {
	return json.Marshal(managedHarnessManifestWire{Schema: &m.schema, Harness: &m.harness, InstallID: &m.installID})
}

func (m *ManagedHarnessManifest) UnmarshalJSON(data []byte) error {
	var wire managedHarnessManifestWire
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&wire); err != nil {
		return err
	}
	if wire.Schema == nil || wire.Harness == nil || wire.InstallID == nil {
		return fmt.Errorf("missing field in managed harness manifest")
	}
	*m = ManagedHarnessManifest{schema: *wire.Schema, harness: *wire.Harness, installID: *wire.InstallID}
	return nil
}

func ManagedHarnessManifestForHarness(harness HarnessKind) ManagedHarnessManifest {
	return ManagedHarnessManifestForInstall(harness, Pin(harness).InstallID)
}

func ManagedHarnessManifestForInstall(harness HarnessKind, installID string) ManagedHarnessManifest {
	return ManagedHarnessManifest{schema: 1, harness: harness, installID: installID}
}

// ManagedHarnessCacheRoot resolves the shared managed-harness cache root from
// the target environment.
//
// A non-empty `XDG_CACHE_HOME` takes precedence over `HOME/.cache`; the
// resulting base must be absolute on every target. An empty argument is the
// same as an absent one.
func ManagedHarnessCacheRoot(xdgCacheHome, home string) (string, error) {
	panic("unported: mj-core/src/harness_runtime.rs managed_harness_cache_root")
}

// ManagedNodeCacheRoot resolves the cache directory that holds Mjolnir's
// pinned Node.js releases.
func ManagedNodeCacheRoot(xdgCacheHome, home string) (string, error) {
	panic("unported: mj-core/src/harness_runtime.rs managed_node_cache_root")
}

func managedCacheBase(xdgCacheHome, home string) (string, error) {
	panic("unported: mj-core/src/harness_runtime.rs managed_cache_base")
}

// ManagedHarnessInstallDir resolves the pinned install directory under a
// managed-harness cache root.
func ManagedHarnessInstallDir(cacheRoot string, harness HarnessKind) string {
	panic("unported: mj-core/src/harness_runtime.rs managed_harness_install_dir")
}

// ManagedHarnessManifestMatches checks that an installation carries the
// receipt for the current harness pin. Missing and malformed receipts are
// incomplete installs; other read failures are returned with their path for
// diagnostics.
func ManagedHarnessManifestMatches(path string, harness HarnessKind, installID string) (bool, error) {
	panic("unported: mj-core/src/harness_runtime.rs managed_harness_manifest_matches")
}

const (
	CodexACPPackage    = "@brokkai/codex-acp"
	CodexACPVersion    = "1.15.0"
	CodexCLIVersion    = "0.170.0"
	ClaudeACPVersion   = "0.108.0"
	ClaudeCLIVersion   = "2.1.326"
	KimiVersion        = "2.6.3"
	GrokVersion        = "1.0.89"
	MuseACPPackage     = "@brokkai/muse-acp"
	MuseACPVersion     = "0.25.0"
	MuseVersion        = "1.4.23-R8977.1"
	OpencodeVersion    = "2.2.7"
	DshACPVersion      = "0.7.0"
	DshACPPackage      = "@brokkai/dsh-acp"
	AntigravityVersion = "1.5.11"
)

// OpencodeACPScript is the shell commands that start OpenCode's ACP server
// with the executable named by `$opencode`, for the managed install and the
// container fallback alike.
//
// OpenCode 2 keeps its logins in its database, which a session home starts
// without, so the commands first import the logins staging wrote into the
// home (HarnessKind.CredentialFileName). `auth import` adds only the entries
// the database lacks, so a relaunch imports nothing again, and `--standalone`
// keeps it off any OpenCode background service. Its report goes to standard
// error because standard output is the ACP stream, and a failed import fails
// the launch rather than starting a session without the logins.
func OpencodeACPScript() string {
	kind := HarnessKindOpenCode
	return fmt.Sprintf(
		"logins=\"${%s:?}/%s/%s\"; if [ -f \"$logins\" ]; then \"$opencode\" auth import --standalone \"$logins\" >&2 || exit; fi; exec \"$opencode\" %s",
		kind.HomeEnv(), kind.ID(), kind.CredentialFileName(),
		strings.Join(kind.BridgeArgs(ExecutionPolicyConfiguredApprovals), " "),
	)
}

// DshACPInstructionsLimit is the most bytes dsh-acp reads from one
// instruction file.
const DshACPInstructionsLimit = 1024 * 1024

// UndeliverableDshInstructions is why dsh-acp cannot load an instruction
// file. The controller refuses a session for any of these before it launches
// anything (HarnessProfile.EnsureReady).
type UndeliverableDshInstructions interface {
	isUndeliverableDshInstructions()
}

// UndeliverableDshInstructionsNotRegularFile: the path exists but is not a
// regular file.
type UndeliverableDshInstructionsNotRegularFile struct{}

// UndeliverableDshInstructionsTooLong: the file has more bytes than
// DshACPInstructionsLimit.
type UndeliverableDshInstructionsTooLong struct {
	Bytes uint64
}

// UndeliverableDshInstructionsNotUtf8: the file is not UTF-8, and dsh-acp
// refuses instruction files that are not. Line is the line with the first
// byte that is not.
type UndeliverableDshInstructionsNotUtf8 struct {
	Line int
}

func (UndeliverableDshInstructionsNotRegularFile) isUndeliverableDshInstructions() {}
func (UndeliverableDshInstructionsTooLong) isUndeliverableDshInstructions()        {}
func (UndeliverableDshInstructionsNotUtf8) isUndeliverableDshInstructions()        {}

// FindUndeliverableDshInstructions reports whether dsh-acp can load the
// instruction file at path (Rust `undeliverable_dsh_instructions`, renamed
// because the type has the same Go name). A nil result means it can. Like
// dsh-acp, this reads only a regular file.
func FindUndeliverableDshInstructions(path string) (UndeliverableDshInstructions, error) {
	panic("unported: mj-core/src/harness_runtime.rs undeliverable_dsh_instructions")
}

// DshACPScript is the shell commands that start dsh-acp, which the positional
// arguments name (`"$@"`), for bare and container targets alike.
//
// dsh-acp reads HarnessKind.AgentInstructionsFile from its data directory,
// the session working directory and each additional workspace root,
// rebuilding that prompt whenever a session is opened. The command clears
// `DSH_ACP_SYSTEM_PROMPT` so a value from the target's login shell cannot add
// a second prompt beside the staged file. Without `DSH_ACP_DATA_DIR` dsh-acp
// would keep its sessions in a `.dsh-acp` directory inside the repository, so
// the command refuses to start.
func DshACPScript() string {
	data := HarnessKindDsh.HomeEnv()
	return fmt.Sprintf(
		"if [ -z \"${%s:-}\" ]; then echo 'Mjolnir requires %s to keep dsh-acp sessions outside the workspace' >&2; exit 1; fi; DSH_ACP_SYSTEM_PROMPT=; export DSH_ACP_SYSTEM_PROMPT; exec \"$@\"",
		data, data,
	)
}

func dshACPRequiredMajorMinor() (uint64, uint64) {
	major, rest, ok := strings.Cut(DshACPVersion, ".")
	minor, _, _ := strings.Cut(rest, ".")
	majorValue, majorErr := strconv.ParseUint(major, 10, 64)
	minorValue, minorErr := strconv.ParseUint(minor, 10, 64)
	if !ok || majorErr != nil || minorErr != nil {
		panic("DSH_ACP_VERSION starts with numeric major and minor versions")
	}
	return majorValue, minorValue
}

// DshACPVersionSupportsNativeInstructions reports whether `dsh-acp --version`
// output names a release that loads staged instructions from its data
// directory.
func DshACPVersionSupportsNativeInstructions(versionOutput string) bool {
	version, ok := strings.CutPrefix(versionOutput, "dsh-acp ")
	if !ok {
		return false
	}
	parts := strings.Split(strings.TrimSpace(version), ".")
	if len(parts) < 2 {
		return false
	}
	major, err := strconv.ParseUint(parts[0], 10, 64)
	if err != nil {
		return false
	}
	minor, err := strconv.ParseUint(parts[1], 10, 64)
	if err != nil {
		return false
	}
	requiredMajor, requiredMinor := dshACPRequiredMajorMinor()
	return major > requiredMajor || (major == requiredMajor && minor >= requiredMinor)
}

// DshACPVersionSupportedScript is a POSIX shell command that succeeds when
// the `dsh-acp` on `PATH` can load staged instructions from its data
// directory. The caller checks that the command exists first.
func DshACPVersionSupportedScript() string {
	major, minor := dshACPRequiredMajorMinor()
	return fmt.Sprintf(
		"{ reported=\"$(dsh-acp --version 2>/dev/null)\" || reported=; case \"$reported\" in 'dsh-acp '*) reported=\"${reported#dsh-acp }\" ;; *) reported= ;; esac; major=\"${reported%%%%.*}\"; rest=\"${reported#*.}\"; minor=\"${rest%%%%.*}\"; case \"$major\" in ''|*[!0-9]*) major= ;; esac; case \"$minor\" in ''|*[!0-9]*) minor= ;; esac; [ -n \"$major\" ] && [ -n \"$minor\" ] && { [ \"$major\" -gt %d ] || { [ \"$major\" -eq %d ] && [ \"$minor\" -ge %d ]; }; }; }",
		major, major, minor,
	)
}

// DshACPLaunchScript is the shell commands that start dsh-acp on any target:
// the target's own `dsh-acp` when its version supports native instruction
// loading, otherwise the pinned npm package through `npx`, running
// DshACPScript with the resolved command as its positional arguments. DSH has
// no managed installation — the npm package bundles the same release binaries
// — so this one script is the whole launch path for bare and container
// targets alike, and for the model discovery probe.
func DshACPLaunchScript() string {
	return "if command -v dsh-acp >/dev/null 2>&1; then if " + DshACPVersionSupportedScript() + "; then " +
		"set -- dsh-acp; " +
		"elif command -v node >/dev/null 2>&1 && command -v npm >/dev/null 2>&1 && command -v npx >/dev/null 2>&1; then " +
		"set -- npx -y " + DshACPPackage + "@" + DshACPVersion + "; " +
		"else echo 'Mjolnir requires dsh-acp " + DshACPVersion + " or newer to load AGENTS.md; upgrade it or install Node.js 22+, npm, and npx to use the pinned release' >&2; exit 127; fi; " +
		"elif command -v node >/dev/null 2>&1 && command -v npm >/dev/null 2>&1 && command -v npx >/dev/null 2>&1; then " +
		"set -- npx -y " + DshACPPackage + "@" + DshACPVersion + "; " +
		"else echo 'Mjolnir needs dsh-acp or Node.js, npm, and npx on PATH; install one in the target environment' >&2; exit 127; fi; " +
		DshACPScript()
}

// FallbackLaunch is the shell launcher used by container targets when
// Mjolnir's worker has not replaced it with a managed installation. Workers
// also use this value to distinguish the built-in fallback from a custom
// launch command.
func FallbackLaunch(harness HarnessKind, policy ExecutionPolicy) (string, []string) {
	panic("unported: mj-core/src/harness_runtime.rs fallback_launch")
}

// IsFallbackLaunch reports whether a persisted launch description is the one
// Mjolnir generated for this harness, rather than a command supplied by a
// caller.
func IsFallbackLaunch(harness HarnessKind, policy ExecutionPolicy, command string, args []string) bool {
	expectedCommand, expectedArgs := FallbackLaunch(harness, policy)
	if command != expectedCommand || len(args) != len(expectedArgs) {
		return false
	}
	for i := range args {
		if args[i] != expectedArgs[i] {
			return false
		}
	}
	return true
}

// NpmBridge is the built-in npm launcher, selected on the worker before ACP
// startup.
type NpmBridge struct {
	Command string
	Package string
	Version string
}

// NpmBridgeFor is Rust `npm_bridge` (renamed because the type has the same Go
// name).
func NpmBridgeFor(kind HarnessKind) (NpmBridge, bool) {
	switch kind {
	case HarnessKindCodex:
		return NpmBridge{Command: "codex-acp", Package: CodexACPPackage, Version: CodexACPVersion}, true
	case HarnessKindClaude:
		return NpmBridge{Command: "claude-agent-acp", Package: "@agentclientprotocol/claude-agent-acp", Version: ClaudeACPVersion}, true
	case HarnessKindMuse:
		// The npm package carries the adapter only; the native Muse server
		// keeps its own verified download.
		return NpmBridge{Command: "muse-acp", Package: MuseACPPackage, Version: MuseACPVersion}, true
	}
	return NpmBridge{}, false
}

// HarnessUsesNodeRuntime reports whether preparing this harness may need
// Node.js, npm, and npx.
//
// Kimi, Grok, and OpenCode install and launch native executables. Codex,
// Claude, and Muse install npm packages or adapters. DSH itself is not
// managed, but its fallback fetches the pinned package through npx.
func HarnessUsesNodeRuntime(kind HarnessKind) bool {
	switch kind {
	case HarnessKindCodex, HarnessKindClaude, HarnessKindMuse, HarnessKindDsh:
		return true
	}
	return false
}

func (b NpmBridge) MatchesLauncher(command string, args []string) bool {
	return (command == b.Command && len(args) == 0) || b.IsLegacyLauncher(command, args)
}

// BootstrapScript keeps launch descriptions usable by older workers,
// including reviewers on a busy worker that cannot upgrade yet. New workers
// resolve this before ACP.
func (b NpmBridge) BootstrapScript() string {
	return b.scriptForVersion(b.Version)
}

// IsLegacyLauncher recognizes persisted launch descriptions from before
// worker-side selection. Match the entire generated script; arbitrary shell
// wrappers stay custom.
func (b NpmBridge) IsLegacyLauncher(command string, args []string) bool {
	if command != "sh" || len(args) != 2 || args[0] != "-c" {
		return false
	}
	marker := "exec npx -y " + b.Package + "@"
	index := strings.LastIndex(args[1], marker)
	if index < 0 {
		return false
	}
	version := args[1][index+len(marker):]
	if version == "" {
		return false
	}
	for _, c := range []byte(version) {
		if !(harnessRuntimeIsASCIIAlphanumeric(c) || strings.IndexByte(".-+", c) >= 0) {
			return false
		}
	}
	return args[1] == b.scriptForVersion(version)
}

func harnessRuntimeIsASCIIAlphanumeric(c byte) bool {
	return ('0' <= c && c <= '9') || ('a' <= c && c <= 'z') || ('A' <= c && c <= 'Z')
}

func (b NpmBridge) scriptForVersion(version string) string {
	check := ""
	if b.Package == CodexACPPackage {
		check = fmt.Sprintf(" && [ \"$(codex-acp --version 2>/dev/null)\" = \"%s %s\" ]", b.Package, version)
	}
	nodeCheck := "if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1 || ! command -v npx >/dev/null 2>&1; then echo 'Mjolnir needs Node.js, npm, and npx on PATH; install Node in the target environment' >&2; exit 127; fi"
	return fmt.Sprintf("if command -v %s >/dev/null 2>&1%s; then exec %s; fi; %s; exec npx -y %s@%s",
		b.Command, check, b.Command, nodeCheck, b.Package, version)
}

type HarnessPin struct {
	// InstallID is the identity of the installed content, and the directory
	// name under the harness cache. Change it whenever the installed bytes
	// change, including a change to the harness asset `package.json`, so
	// caches written by an earlier packaging are replaced instead of reused.
	InstallID      string
	DisplayVersion string
	Entrypoint     string
	CLIEntrypoint  string
}

func Pin(kind HarnessKind) HarnessPin {
	switch kind {
	case HarnessKindAntigravity:
		return HarnessPin{InstallID: "antigravity-1.5.11", DisplayVersion: AntigravityVersion, Entrypoint: "bin/agy", CLIEntrypoint: "bin/agy"}
	case HarnessKindMuse:
		return HarnessPin{InstallID: "muse-acp-0.25.0_muse-1.4.23-R8977.1", DisplayVersion: "muse-acp 0.25.0 + Muse Code 1.4.23-R8977.1", Entrypoint: "bin/muse", CLIEntrypoint: "bin/muse"}
	case HarnessKindCodex:
		return HarnessPin{InstallID: "brokkai-codex-acp-1.15.0_codex-0.170.0", DisplayVersion: "@brokkai/codex-acp 1.15.0 + codex 0.170.0", Entrypoint: "node_modules/.bin/codex-acp", CLIEntrypoint: CodexCLIEntrypoint}
	case HarnessKindClaude:
		// pkg2: the asset lists @anthropic-ai/claude-code in allowScripts.
		// Installs made before that change kept the package's native-binary
		// stub, so their identity must not match this pin.
		return HarnessPin{InstallID: "claude-agent-acp-0.108.0_claude-2.1.326_pkg2", DisplayVersion: "claude-agent-acp 0.108.0 + Claude Code 2.1.326", Entrypoint: "node_modules/.bin/claude-agent-acp", CLIEntrypoint: "node_modules/.bin/claude"}
	case HarnessKindKimi:
		return HarnessPin{InstallID: "kimi-2.6.3", DisplayVersion: "Kimi Code 2.6.3", Entrypoint: "bin/kimi", CLIEntrypoint: "bin/kimi"}
	case HarnessKindGrok:
		return HarnessPin{InstallID: "grok-1.0.89", DisplayVersion: "Grok 1.0.89", Entrypoint: "bin/grok", CLIEntrypoint: "bin/grok"}
	case HarnessKindOpenCode:
		return HarnessPin{InstallID: "opencode-2.2.7", DisplayVersion: "OpenCode 2.2.7", Entrypoint: "opencode", CLIEntrypoint: "opencode"}
	case HarnessKindDsh:
		return HarnessPin{InstallID: "dsh-acp-0.7.0", DisplayVersion: "dsh-acp 0.7.0", Entrypoint: "dsh-acp", CLIEntrypoint: "dsh-acp"}
	}
	panic(fmt.Sprintf("unknown harness kind %v", kind))
}

// RuntimeProvenance is a legacy receipt shape retained to decode journals and
// events from shipped releases.
type RuntimeProvenance string

const (
	RuntimeProvenanceManagedInstallation RuntimeProvenance = "managed_installation"
	RuntimeProvenanceTargetInstallation  RuntimeProvenance = "target_installation"
)

type RuntimeComponent struct {
	Name    string  `json:"name"`
	Version *string `json:"version"`
	Sha256  *string `json:"sha256"`
}

// RuntimeIdentity is the historical runtime identity; new workers no longer
// inspect installations.
type RuntimeIdentity struct {
	ID                *string            `json:"id"`
	Harness           HarnessKind        `json:"harness"`
	Platform          string             `json:"platform"`
	Provenance        RuntimeProvenance  `json:"provenance"`
	Components        []RuntimeComponent `json:"components"`
	UnavailableReason *string            `json:"unavailable_reason"`
}

// RuntimeReceipt is one immutable initialization, identified by its position
// in the worker journal. Identity is flattened into the receipt's object.
type RuntimeReceipt struct {
	Identity     RuntimeIdentity
	EventOrdinal uint64
	ObservedAtMs int64
}

func (r RuntimeReceipt) MarshalJSON() ([]byte, error) {
	identity := r.Identity
	if identity.Components == nil {
		identity.Components = []RuntimeComponent{}
	}
	body, err := json.Marshal(identity)
	if err != nil {
		return nil, err
	}
	tail, err := json.Marshal(struct {
		EventOrdinal uint64 `json:"event_ordinal"`
		ObservedAtMs int64  `json:"observed_at_ms"`
	}{r.EventOrdinal, r.ObservedAtMs})
	if err != nil {
		return nil, err
	}
	// Splice: identity fields first, then the receipt's own, as serde's
	// flatten writes them.
	out := append(body[:len(body)-1:len(body)-1], ',')
	return append(out, tail[1:]...), nil
}

func (r *RuntimeReceipt) UnmarshalJSON(data []byte) error {
	var identity RuntimeIdentity
	if err := json.Unmarshal(data, &identity); err != nil {
		return err
	}
	var own struct {
		EventOrdinal *uint64 `json:"event_ordinal"`
		ObservedAtMs *int64  `json:"observed_at_ms"`
	}
	if err := json.Unmarshal(data, &own); err != nil {
		return err
	}
	if own.EventOrdinal == nil {
		return fmt.Errorf("missing field `event_ordinal`")
	}
	if own.ObservedAtMs == nil {
		return fmt.Errorf("missing field `observed_at_ms`")
	}
	*r = RuntimeReceipt{Identity: identity, EventOrdinal: *own.EventOrdinal, ObservedAtMs: *own.ObservedAtMs}
	return nil
}
