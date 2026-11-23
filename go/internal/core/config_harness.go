// Port of mj-core/src/config/harness.rs.
//
// Harness identity, credentials layout and execution policy.

package core

import (
	"crypto/sha256"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"runtime"
	"strings"

	acpschema "github.com/BrokkAi/acp-go/schema"
	"github.com/tailscale/hujson"
)

// HarnessKind is a harness Mjolnir runs (serde kebab-case; note OpenCode is
// `open-code` on the wire while its ID is `opencode`).
type HarnessKind string

const (
	HarnessKindCodex       HarnessKind = "codex"
	HarnessKindClaude      HarnessKind = "claude"
	HarnessKindKimi        HarnessKind = "kimi"
	HarnessKindGrok        HarnessKind = "grok"
	HarnessKindMuse        HarnessKind = "muse"
	HarnessKindOpenCode    HarnessKind = "open-code"
	HarnessKindDsh         HarnessKind = "dsh"
	HarnessKindAntigravity HarnessKind = "antigravity"
)

// HarnessKindAll is every harness, in declaration order (Rust's Ord order;
// compare with HarnessKindOrdinal, not as strings).
var HarnessKindAll = [8]HarnessKind{
	HarnessKindCodex, HarnessKindClaude, HarnessKindKimi, HarnessKindGrok,
	HarnessKindMuse, HarnessKindOpenCode, HarnessKindDsh, HarnessKindAntigravity,
}

// HarnessKindOrdinal is the position of kind in HarnessKindAll, which is the
// order Rust's derived Ord gives.
func HarnessKindOrdinal(kind HarnessKind) int {
	for i, candidate := range HarnessKindAll {
		if candidate == kind {
			return i
		}
	}
	return len(HarnessKindAll)
}

func (k *HarnessKind) UnmarshalText(text []byte) error {
	for _, kind := range HarnessKindAll {
		if string(kind) == string(text) {
			*k = kind
			return nil
		}
	}
	return serdeUnknownVariant(string(text), "codex", "claude", "kimi", "grok", "muse", "open-code", "dsh", "antigravity")
}

// AntigravityQuotaPool is one of the independent subscription pools returned
// by Antigravity's `/usage`.
type AntigravityQuotaPool int

const (
	AntigravityQuotaPoolGemini AntigravityQuotaPool = iota
	AntigravityQuotaPoolClaude
	// AntigravityQuotaPoolArgon is Gemini 4 Argon's weekly allowance. Pro
	// plans report it as its own group, and an Argon turn draws on it alone;
	// agy says Ultra plans count Argon in the Gemini pool instead.
	AntigravityQuotaPoolArgon
)

var AntigravityQuotaPoolAll = [3]AntigravityQuotaPool{
	AntigravityQuotaPoolGemini, AntigravityQuotaPoolClaude, AntigravityQuotaPoolArgon,
}

func (p AntigravityQuotaPool) Label() string {
	switch p {
	case AntigravityQuotaPoolGemini:
		return "Ge"
	case AntigravityQuotaPoolClaude:
		return "Cl"
	}
	return "Ar"
}

// Name is the pool's name where there is room for one, as on a quota bar.
func (p AntigravityQuotaPool) Name() string {
	switch p {
	case AntigravityQuotaPoolGemini:
		return "Gemini"
	case AntigravityQuotaPoolClaude:
		return "Claude & GPT"
	}
	return "Argon"
}

// BucketPrefix is the start of this pool's bucket ids. Argon's
// (`gemini-argon-`) also starts with Gemini's, so a bucket belongs to the
// longest match.
func (p AntigravityQuotaPool) BucketPrefix() string {
	switch p {
	case AntigravityQuotaPoolGemini:
		return "gemini-"
	case AntigravityQuotaPoolClaude:
		return "3p-"
	}
	return "gemini-argon-"
}

// ModelScope is the pool's own scope, which also names it in a profile's
// settings.
func (p AntigravityQuotaPool) ModelScope() string {
	switch p {
	case AntigravityQuotaPoolGemini:
		return "gemini"
	case AntigravityQuotaPoolClaude:
		return "claude,gpt"
	}
	// Orchestrator (Argon) is `gemini-pro-orchestrator-g1-pro`. Its turns
	// draw on the Argon bucket, and it hands implementation to Gemini 4
	// Hydrogen, so Gemini's scope keeps it as well.
	return "argon,g1-pro"
}

// ReportedScope is the scope a reading of this pool carries. When the account
// reports a separate Argon pool, Gemini 4 Argon draws only on that pool, so
// Gemini's allowance leaves it out.
func (p AntigravityQuotaPool) ReportedScope(separateArgon bool) string {
	if p == AntigravityQuotaPoolGemini && separateArgon {
		return "gemini,!argon"
	}
	return p.ModelScope()
}

// AntigravityQuotaPoolOfScope is the pool whose reading carries scope.
func AntigravityQuotaPoolOfScope(scope string) (AntigravityQuotaPool, bool) {
	for _, pool := range AntigravityQuotaPoolAll {
		if scope == pool.ReportedScope(false) || scope == pool.ReportedScope(true) {
			return pool, true
		}
	}
	return 0, false
}

// ModelMatchesScope is true when model contains one of the comma-separated
// scope fragments and none of the fragments written with a leading `!`.
func ModelMatchesScope(model, scope string) bool {
	model = strings.ToLower(model)
	contains := func(fragment string) bool { return strings.Contains(model, strings.ToLower(fragment)) }
	included := false
	for _, fragment := range strings.Split(scope, ",") {
		fragment = strings.TrimSpace(fragment)
		if fragment == "" {
			continue
		}
		if excluded, ok := strings.CutPrefix(fragment, "!"); ok {
			if excluded != "" && contains(excluded) {
				return false
			}
			continue
		}
		included = included || contains(fragment)
	}
	return included
}

// ModelOffer is the models Mjolnir offers from a profile: its `models`
// allow-list (empty offers everything) minus models in scopes the profile
// turned off. Built by HarnessProfile.ModelOffer.
type ModelOffer struct {
	Models         []string
	ExcludedScopes []string
}

// IsUnrestricted reports whether this offer leaves every advertised model in
// place.
func (m ModelOffer) IsUnrestricted() bool { return len(m.Models) == 0 && len(m.ExcludedScopes) == 0 }

// Allows reports whether model is offered, before the current-model
// exemption.
func (m ModelOffer) Allows(model string) bool {
	listed := len(m.Models) == 0
	for _, candidate := range m.Models {
		if candidate == model {
			listed = true
			break
		}
	}
	return listed && !m.Excludes(model)
}

// Excludes reports whether model is in a scope the profile turned off. Only
// the current-model exemption in ProfileConfig.RestrictedTo still offers such
// a model: a session already on it, or a harness whose own default is in the
// scope, keeps it until someone switches.
func (m ModelOffer) Excludes(model string) bool {
	for _, scope := range m.ExcludedScopes {
		if ModelMatchesScope(model, scope) {
			return true
		}
	}
	return false
}

// Refusal says why a profile named profile refuses model, naming the setting.
func (m ModelOffer) Refusal(profile, model string) string {
	if m.Excludes(model) {
		return fmt.Sprintf("model %s uses profile %s's third-party quota pool, which "+
			"`third_party_models = false` turns off", debugString(model), debugString(profile))
	}
	return fmt.Sprintf("model %s is hidden by profile %s's models setting", debugString(model), debugString(profile))
}

// HarnessHost is the operating system of the machine a harness's own CLI runs
// on, as far as where that harness keeps its login is concerned. See
// HarnessKind.KeepsLoginInHome.
type HarnessHost int

const (
	HarnessHostMacOs HarnessHost = iota
	HarnessHostOther
)

// HarnessHostCurrent is the machine this process runs on.
func HarnessHostCurrent() HarnessHost {
	if runtime.GOOS == "darwin" {
		return HarnessHostMacOs
	}
	return HarnessHostOther
}

// HarnessLoginStore is where a profile's login lives on the machine that runs
// its harness's CLI. See HarnessKind.LoginStore.
type HarnessLoginStore interface {
	isHarnessLoginStore()
	// LockMarker is the credential file whose directory carries the locks
	// every writer of this login takes. For a file that is the file itself.
	// For the Keychain it is `~/.claude/.credentials.json`: Claude Code
	// derives its lock directories from its config directory, which is
	// `~/.claude` while the variable is unset, so Mjolnir's renewals
	// serialize with Claude Code's own.
	LockMarker() (string, error)
}

// HarnessLoginStoreFile: the login is the file at Marker.
type HarnessLoginStoreFile struct{ Marker string }

// HarnessLoginStoreMacKeychain: the login is the macOS Keychain item
// ClaudeKeychainService, which every Claude profile there shares because its
// commands run with CLAUDE_CONFIG_DIR unset, and nothing else: a
// `.credentials.json` on a Mac (a profile home's, left by earlier releases,
// or Claude Code's own plaintext fallback when its Keychain write fails) is
// not a login. Claude Code takes its refresh locks under `~/.claude`; see
// HarnessLoginStore.LockMarker.
type HarnessLoginStoreMacKeychain struct{}

func (HarnessLoginStoreFile) isHarnessLoginStore()        {}
func (HarnessLoginStoreMacKeychain) isHarnessLoginStore() {}

func (s HarnessLoginStoreFile) LockMarker() (string, error) { return s.Marker, nil }

func (HarnessLoginStoreMacKeychain) LockMarker() (string, error) {
	home, ok := UserHome()
	if !ok {
		return "", errors.New("the user's home directory is unknown")
	}
	return HarnessAuthenticationMarker(HarnessKindClaude, filepath.Join(home, HarnessKindClaude.DefaultHomeLeaf())), nil
}

// ClaudeKeychainService is the Keychain item Claude Code stores its login in
// when CLAUDE_CONFIG_DIR is unset (CC 2.1.321: a set variable adds
// `-<sha256(dir)[:8]>`).
const ClaudeKeychainService = "Claude Code-credentials"

// claudeKeychainFallbackAccount is the Keychain account Claude Code uses when
// $USER is missing or holds characters outside [a-zA-Z0-9._-].
const claudeKeychainFallbackAccount = "claude-code-user"

// ClaudeKeychainAccount is the account of Claude Code's Keychain item, by
// Claude Code's own rule (2.1.321): $USER when it is non-empty and made only
// of [a-zA-Z0-9._-], otherwise a fixed name. Reading and writing with the
// same account addresses the item Claude Code uses instead of adding a second
// one.
func ClaudeKeychainAccount(user *string) string {
	if user == nil || *user == "" {
		return claudeKeychainFallbackAccount
	}
	for i := 0; i < len(*user); i++ {
		b := (*user)[i]
		if !(b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z' || b >= '0' && b <= '9' || b == '.' || b == '_' || b == '-') {
			return claudeKeychainFallbackAccount
		}
	}
	return *user
}

// ExecutionPolicy is the target-level execution policy Hel applies
// independently of the selected harness. Raw targets may preserve configured
// approvals; isolated targets force full access because their boundary
// contains the blast radius.
type ExecutionPolicy string

const (
	// ExecutionPolicyConfiguredApprovals preserves the harness and profile's
	// configured approval behavior.
	ExecutionPolicyConfiguredApprovals ExecutionPolicy = "configured_approvals"
	// ExecutionPolicyUnconstrained runs without sandboxing. Most harnesses
	// also skip approval checks, Muse included.
	ExecutionPolicyUnconstrained ExecutionPolicy = "unconstrained"
)

func (p *ExecutionPolicy) UnmarshalText(text []byte) error {
	switch ExecutionPolicy(text) {
	case ExecutionPolicyConfiguredApprovals, ExecutionPolicyUnconstrained:
		*p = ExecutionPolicy(text)
		return nil
	}
	return serdeUnknownVariant(string(text), "configured_approvals", "unconstrained")
}

// PermissionMode is the approval behavior of a target that runs the harness
// directly on a machine: chosen for a named SSH machine, always Guardian on
// localhost.
type PermissionMode string

const (
	// PermissionModeGuardian preserves the selected harness profile's
	// approval behavior.
	PermissionModeGuardian PermissionMode = "guardian"
	// PermissionModeYolo runs without sandboxing. Most harnesses also skip
	// approval checks, Muse included.
	PermissionModeYolo PermissionMode = "yolo"
)

func (m PermissionMode) ExecutionPolicy() ExecutionPolicy {
	if m == PermissionModeYolo {
		return ExecutionPolicyUnconstrained
	}
	return ExecutionPolicyConfiguredApprovals
}

func (m *PermissionMode) UnmarshalText(text []byte) error {
	switch PermissionMode(text) {
	case PermissionModeGuardian, PermissionModeYolo:
		*m = PermissionMode(text)
		return nil
	}
	return serdeUnknownVariant(string(text), "guardian", "yolo")
}

// GuardianReviewer is who answers a harness's tool approvals on a target that
// keeps configured approvals. See HarnessKind.Guardian.
type GuardianReviewer int

const (
	// GuardianReviewerHarness is the harness's own reviewer (Codex's
	// guardian, Claude's Auto mode, ...).
	GuardianReviewerHarness GuardianReviewer = iota
	// GuardianReviewerMjolnir: the Mjolnir worker reviews each permission
	// request the harness sends.
	GuardianReviewerMjolnir
)

// ClaudePermissionMode is the Claude Code permission mode a Claude profile's
// sessions run in on targets that keep configured approvals (localhost,
// guardian SSH). Unconstrained targets always run Claude in
// bypassPermissions.
type ClaudePermissionMode string

const (
	// ClaudePermissionModeAuto: Claude Code's own classifier approves or
	// denies each action. The default.
	ClaudePermissionModeAuto ClaudePermissionMode = "auto"
	// ClaudePermissionModeAcceptEdits: file edits run without asking; other
	// actions ask.
	ClaudePermissionModeAcceptEdits ClaudePermissionMode = "accept-edits"
	// ClaudePermissionModeManual: every action that needs permission asks.
	ClaudePermissionModeManual ClaudePermissionMode = "manual"
)

// ACPMode is the claude-agent-acp mode id that realizes this choice.
func (m ClaudePermissionMode) ACPMode() string {
	switch m {
	case ClaudePermissionModeAcceptEdits:
		return "acceptEdits"
	case ClaudePermissionModeManual:
		return "default"
	}
	return "auto"
}

func (m ClaudePermissionMode) IsAuto() bool { return m == ClaudePermissionModeAuto }

func (m *ClaudePermissionMode) UnmarshalText(text []byte) error {
	switch ClaudePermissionMode(text) {
	case ClaudePermissionModeAuto, ClaudePermissionModeAcceptEdits, ClaudePermissionModeManual:
		*m = ClaudePermissionMode(text)
		return nil
	}
	return serdeUnknownVariant(string(text), "auto", "accept-edits", "manual")
}

// ApprovalChoices is a profile's own choices about how its harness realizes
// an execution policy. The controller reads them from the profile and the
// worker from its launch config, and both resolve the enforcement through
// HarnessKind.ResolveExecutionEnforcement and the reviewer through
// HarnessKind.MjolnirGuardianReviews. Build one with DefaultApprovalChoices:
// the zero value turns the Mjolnir guardian off.
type ApprovalChoices struct {
	// ClaudePermissionMode: see HarnessProfile.ClaudePermissionMode.
	ClaudePermissionMode ClaudePermissionMode
	// MjolnirGuardian: see HarnessProfile.MjolnirGuardian.
	MjolnirGuardian bool
}

// DefaultApprovalChoices is ApprovalChoices::default(): the default Claude
// mode, with the Mjolnir guardian on.
func DefaultApprovalChoices() ApprovalChoices {
	return ApprovalChoices{ClaudePermissionMode: ClaudePermissionModeAuto, MjolnirGuardian: true}
}

func (p ExecutionPolicy) IsUnconstrained() bool { return p == ExecutionPolicyUnconstrained }

// ExecutionEnforcement is the harness-specific controls that collectively
// realize a target-level execution policy. A harness may need more than one
// launch-time mechanism in addition to an ACP mode.
type ExecutionEnforcement struct {
	label   string
	acpMode *string
	// acpModeSelector is the config option that carries acpMode when the
	// harness keeps it apart from its Mode selector.
	acpModeSelector *string
	// acpSetting is a further ACP config option and value the policy
	// requires.
	acpSetting        *[2]string
	launchFlag        *string
	launchEnvironment *[2]string
	// launchArgument is a word the policy requires present in, or absent
	// from, a whitespace-separated argv held in an env var.
	launchArgument *LaunchArgument
	// sessionSandbox is the value for the ACP session/new
	// _meta.sandbox.enabled field.
	sessionSandbox *bool
	// stagedSetting is a value written into a staged profile file before
	// upload.
	stagedSetting *StagedSetting
}

// LaunchArgument is one word of the argv a harness reads from an environment
// variable. Each policy states whether the word is present, so a map that an
// earlier policy or the user's own configuration left with it converges to
// this policy.
type LaunchArgument struct {
	Variable string
	Word     string
	Present  bool
}

// GhApproval is where Mjolnir records its approval of the GitHub CLI (`gh`)
// for one harness. An agent's issue, pull-request and review work is ordinary
// session work, so every launch trusts `gh` in the harness's own
// configuration rather than interrupting the person once per invocation.
type GhApproval interface {
	isGhApproval()
}

// GhApprovalStagedFile is a file the worker edits inside the session's staged
// profile. The profile staging allowlist must carry the file (or its parent
// directory) from the profile home, so a harness that names a new file here
// also adds it there.
type GhApprovalStagedFile struct{ Value string }

// GhApprovalLaunchFile is a file the launch writes into the session home
// itself, because what it grants depends on the servers and commands that
// launch offers.
type GhApprovalLaunchFile struct{ Value string }

// GhApprovalSessionOptions is the session-open request's own options. Claude
// Code accepts `Bash(gh:*)` in `allowedTools` on every new, load and resume
// request, so the grant is scoped to the session and no file is edited --
// including a home Mjolnir does not own.
type GhApprovalSessionOptions struct{}

// GhApprovalSessionConfigDocument is a configuration document the launch
// supplies in this environment variable, for a harness whose last-read
// document must carry the rule. OpenCode takes the last matching permission
// rule across its documents, so a rule in the staged file alone is shadowed
// by a project or profile document that follows it.
type GhApprovalSessionConfigDocument struct{ Value string }

// GhApprovalAdapterEnvironment: the adapter that serves the harness reads
// this variable from its environment; the harness itself has no settings key
// for a command.
type GhApprovalAdapterEnvironment struct{ Value string }

// GhApprovalAcpRequest: the harness asks for approval over ACP and reads no
// configuration of its own; the worker answers the request.
type GhApprovalAcpRequest struct{}

func (GhApprovalStagedFile) isGhApproval()            {}
func (GhApprovalLaunchFile) isGhApproval()            {}
func (GhApprovalSessionOptions) isGhApproval()        {}
func (GhApprovalSessionConfigDocument) isGhApproval() {}
func (GhApprovalAdapterEnvironment) isGhApproval()    {}
func (GhApprovalAcpRequest) isGhApproval()            {}

// StagedSetting is a setting the controller writes into a staged harness
// profile because the harness reads it from disk and no launch-time channel
// can carry it.
type StagedSetting struct {
	// File is the file inside the staged profile, for example
	// `settings.json`.
	File string
	// Path is the object keys walked from the document root to the value's
	// key.
	Path  []string
	Value string
	// ObjectVersion is the key inserted when absent on the root and on every
	// object created or traversed along Path. Muse requires
	// `schema_version: 1` on both.
	ObjectVersion *StagedObjectVersion
}

// StagedObjectVersion is StagedSetting's (key, version) pair.
type StagedObjectVersion struct {
	Key     string
	Version uint64
}

// Apply writes this setting into a staged document's root object, creating
// the objects along Path and stamping ObjectVersion where it is missing.
func (s StagedSetting) Apply(root map[string]any) error {
	if len(s.Path) == 0 {
		return errors.New("staged setting path must name a key")
	}
	key, parents := s.Path[len(s.Path)-1], s.Path[:len(s.Path)-1]
	object := root
	for _, parent := range parents {
		s.stampVersion(object)
		child, ok := object[parent]
		if !ok {
			child = map[string]any{}
			object[parent] = child
		}
		next, ok := child.(map[string]any)
		if !ok {
			return fmt.Errorf("%s must be a JSON object", parent)
		}
		object = next
	}
	s.stampVersion(object)
	object[key] = s.Value
	return nil
}

func (s StagedSetting) stampVersion(object map[string]any) {
	if s.ObjectVersion == nil {
		return
	}
	if _, ok := object[s.ObjectVersion.Key]; !ok {
		object[s.ObjectVersion.Key] = s.ObjectVersion.Version
	}
}

// OwnedConfigKeys is the ACP selectors reserved for the target's execution
// policy: the selector the policy mode lives on, and the policy setting. The
// mode selector is the policy's named one when the bridge has it, else its
// Mode selector (Muse images before muse-acp 0.10 carry approvals on `mode`).
// It stays owned when the bridge does not offer the policy's mode right now,
// such as Auto on a model without it, so no other mode can be chosen then.
func (e ExecutionEnforcement) OwnedConfigKeys(options []acpschema.SessionConfigOption) []string {
	// `mode` resolves the way a SetConfig for it does: the option with that
	// id, else the Mode-category one.
	keys := []string{}
	if e.acpMode != nil {
		var mode *acpschema.SessionConfigOption
		if e.acpModeSelector != nil {
			for i := range options {
				if string(options[i].ID) == *e.acpModeSelector {
					mode = &options[i]
					break
				}
			}
		}
		if mode == nil {
			mode = FindSessionConfigOption(options, "mode")
		}
		if mode != nil {
			keys = append(keys, string(mode.ID))
		}
	}
	if e.acpSetting != nil {
		keys = append(keys, e.acpSetting[0])
	}
	return keys
}

// Label is the name reported to the UI for the mode this session runs in.
func (e ExecutionEnforcement) Label() string { return e.label }

// ACPMode is the ACP mode to select after the session opens, when there is
// one.
func (e ExecutionEnforcement) ACPMode() (string, bool) { return derefString(e.acpMode) }

// ACPModeSelector is the config option to select ACPMode on, when the bridge
// offers it. Without it, the mode goes to the bridge's Mode selector.
func (e ExecutionEnforcement) ACPModeSelector() (string, bool) { return derefString(e.acpModeSelector) }

// ACPSetting is a further config option and value to select after the mode,
// when the policy needs one.
func (e ExecutionEnforcement) ACPSetting() (key, value string, ok bool) {
	if e.acpSetting == nil {
		return "", "", false
	}
	return e.acpSetting[0], e.acpSetting[1], true
}

// LaunchFlag is the launch flag to add to the bridge command line, when there
// is one.
func (e ExecutionEnforcement) LaunchFlag() (string, bool) { return derefString(e.launchFlag) }

func (e ExecutionEnforcement) LaunchEnvironment() (key, value string, ok bool) {
	if e.launchEnvironment == nil {
		return "", "", false
	}
	return e.launchEnvironment[0], e.launchEnvironment[1], true
}

// LaunchArgument is an argv word the policy adds to, or removes from, the
// environment variable that carries the harness's own command line.
func (e ExecutionEnforcement) LaunchArgument() (LaunchArgument, bool) {
	if e.launchArgument == nil {
		return LaunchArgument{}, false
	}
	return *e.launchArgument, true
}

// SessionSandbox is what the ACP session/new request asks for the harness's
// own sandbox.
func (e ExecutionEnforcement) SessionSandbox() (bool, bool) {
	if e.sessionSandbox == nil {
		return false, false
	}
	return *e.sessionSandbox, true
}

// StagedSetting is a setting the controller writes into the staged profile
// before upload.
func (e ExecutionEnforcement) StagedSetting() (StagedSetting, bool) {
	if e.stagedSetting == nil {
		return StagedSetting{}, false
	}
	return *e.stagedSetting, true
}

func derefString(value *string) (string, bool) {
	if value == nil {
		return "", false
	}
	return *value, true
}

// absoluteEnvironmentPath is an XDG base directory variable's value, when it
// names an absolute path. XDG requires a relative value to be ignored.
func absoluteEnvironmentPath(name string) (string, bool) {
	value, ok := os.LookupEnv(name)
	if !ok || !filepath.IsAbs(value) {
		return "", false
	}
	return value, true
}

// HarnessAuthenticationMarker is the file inside a harness home that proves
// the harness is logged in.
//
// Setup, quota checks, credential sync, and the target-side worker all read
// the same path, so it is decided here beside HarnessKind rather than in any
// one of them.
func HarnessAuthenticationMarker(kind HarnessKind, home string) string {
	return filepath.Join(home, kind.CredentialFileName())
}

// ConfigureHomeEnvironment points this harness at home through its home
// variable. Muse and OpenCode resolve their configuration from
// $XDG_CONFIG_HOME/<name>, so both are given the parent of their home, with
// session data under `.data` beside it.
//
// Every session and every configuration probe runs from a home Mjolnir staged
// for it, on every target and every operating system, so this always sets the
// variable.
func (k HarnessKind) ConfigureHomeEnvironment(home string, environment map[string]string) {
	configRoot := home
	if k.NestedHome() {
		environment["XDG_DATA_HOME"] = Text(Join(home, ".data"))
		if parent, ok := pathParent(home); ok {
			configRoot = parent
		}
	}
	environment[k.HomeEnv()] = Text(configRoot)
	if k == HarnessKindAntigravity {
		// The managed pin owns updates, including during login/discovery.
		environment["AGY_CLI_DISABLE_AUTO_UPDATE"] = "true"
	}
	if k == HarnessKindKimi {
		// Kimi treats a session lock recorded under another hostname, or
		// under a live process with the recorded PID, as held forever. A home
		// survives container replacement (a new hostname) and a killed turn
		// leaves its lock behind, so the lock would refuse every later
		// prompt. One harness owns each staged home, so the lock has no
		// second writer to guard against.
		environment["KIMI_CODE_SESSION_LOCK"] = "0"
	}
}

// ConfigureStagedHomeEnvironment pins global instructions to a session-owned
// staged copy.
func (k HarnessKind) ConfigureStagedHomeEnvironment(home string, environment map[string]string) {
	k.ConfigureHomeEnvironment(home, environment)
	if k == HarnessKindOpenCode {
		// OpenCode replaces its XDG global config root with this override.
		// Both variables must name the session-owned staged profile.
		environment["OPENCODE_CONFIG_DIR"] = Text(home)
	}
}

// ConfigureProfileHomeEnvironment points this harness's own CLI at a
// profile's configured home, for the commands that act on the person's own
// login rather than on a session: `mj login`, `claude setup-token`, and quota
// probes.
//
// host is the machine that runs the command. Where the login does not live in
// the home (HarnessKind.KeepsLoginInHome), the variable is left unset, so the
// command finds the login and the settings the person signed in with. Setting
// it there could only move the command off those settings. The same holds for
// the person's own XDG home (HarnessKind.IsOwnNestedHome): the harness already
// resolves it with its data directory beside it, and the `<home>/.data`
// layout of a staged home would move the login out of the person's own data.
func (k HarnessKind) ConfigureProfileHomeEnvironment(home string, host HarnessHost, environment map[string]string) {
	if k.KeepsLoginInHome(host) && !k.IsOwnNestedHome(home) {
		k.ConfigureHomeEnvironment(home, environment)
	}
}

// IsDefaultHome reports whether home is the conventional home
// HarnessKind.DefaultHomeLeaf names beneath the user's home.
func (k HarnessKind) IsDefaultHome(home string) bool {
	user, ok := UserHome()
	return ok && samePath(home, filepath.Join(user, k.DefaultHomeLeaf()))
}

// ManagedProfileHome is the profile home Mjolnir creates during managed
// onboarding. Keeping it below the configuration directory identifies
// Mjolnir, rather than the harness's own installation, as the owner of the
// login and native data.
func (k HarnessKind) ManagedProfileHome(profileID string) string {
	return k.ManagedProfileHomeIn(ConfigDir(), profileID)
}

// ManagedProfileHomeIn is ManagedProfileHome with an explicit configuration
// root for tests and callers that are already operating on another instance.
func (k HarnessKind) ManagedProfileHomeIn(configRoot, profileID string) string {
	return filepath.Join(configRoot, "profiles", profileID, k.ID())
}

// IsOwnNestedHome reports whether home is the person's own home for a harness
// with an XDG home (HarnessKind.NestedHome): $XDG_CONFIG_HOME/<id>, or
// ~/.config/<id> when that variable is unset or relative, as XDG requires.
func (k HarnessKind) IsOwnNestedHome(home string) bool {
	if !k.NestedHome() {
		return false
	}
	if config, ok := absoluteEnvironmentPath("XDG_CONFIG_HOME"); ok {
		return samePath(k.HomeFromEnvironment(config), home)
	}
	user, ok := UserHome()
	return ok && samePath(filepath.Join(user, k.DefaultHomeLeaf()), home)
}

// NestedDataDir is where a harness with an XDG home (HarnessKind.NestedHome)
// keeps its data for the home home. The person's own home pairs with the
// person's own data directory, $XDG_DATA_HOME/<id> or ~/.local/share/<id>;
// any other home is one Mjolnir runs the harness against, with its data
// under <home>/.data/<id> (HarnessKind.ConfigureHomeEnvironment). False for
// a harness whose data lives in its home.
func (k HarnessKind) NestedDataDir(home string) (string, bool) {
	if !k.NestedHome() {
		return "", false
	}
	staged := filepath.Join(home, ".data", k.ID())
	if !k.IsOwnNestedHome(home) {
		return staged, true
	}
	if data, ok := absoluteEnvironmentPath("XDG_DATA_HOME"); ok {
		return filepath.Join(data, k.ID()), true
	}
	if user, ok := UserHome(); ok {
		return filepath.Join(user, ".local/share", k.ID()), true
	}
	return staged, true
}

// KeepsLoginInHome reports whether this harness keeps its login inside its
// home on host, so that pointing HarnessKind.HomeEnv at a home selects that
// home's login.
//
// Claude Code on macOS keeps the login in the Keychain. With
// CLAUDE_CONFIG_DIR unset the item is `Claude Code-credentials`, so every
// profile whose commands run that way shares one item; with the variable set
// it looks for an item named after a hash of the directory, which Mjolnir
// never creates. A Claude home there holds settings, history and MCP servers,
// but not the host's login. A session still runs from its own staged home
// there, with a copy of the login staged as a file; only the commands that
// act on the person's own home leave the variable unset
// (HarnessKind.ConfigureProfileHomeEnvironment).
//
// HarnessKind.LoginStore is the one place that turns this into a location;
// ask it, not this, where a login is.
func (k HarnessKind) KeepsLoginInHome(host HarnessHost) bool {
	return !(k == HarnessKindClaude && host == HarnessHostMacOs)
}

// LoginStore is where the login of the profile with home home lives on host.
// This is the one owner of that fact: credential sync, the grant refresher,
// staging and the authentication check all ask it.
func (k HarnessKind) LoginStore(host HarnessHost, home string) HarnessLoginStore {
	if k.KeepsLoginInHome(host) {
		return HarnessLoginStoreFile{Marker: HarnessAuthenticationMarker(k, home)}
	}
	return HarnessLoginStoreMacKeychain{}
}

// NestedHome reports whether this harness's home is a named subdirectory of
// the directory its home variable points at. The XDG layout fixes the leaf
// name (`muse`, `opencode`), which is also HarnessKind.ID.
func (k HarnessKind) NestedHome() bool { return k == HarnessKindMuse || k == HarnessKindOpenCode }

func (k HarnessKind) HomeFromEnvironment(value string) string {
	if k.NestedHome() {
		return filepath.Join(value, k.ID())
	}
	return value
}

// HomeCLIArgs is the extra CLI arguments required to point this harness's ACP
// or login command at home, in addition to ConfigureHomeEnvironment.
func (k HarnessKind) HomeCLIArgs(home string) []string {
	if k == HarnessKindAntigravity {
		return []string{"--gemini-dir", Text(home)}
	}
	return []string{}
}

// HomeCLIArgsFromEnvironment is the extra CLI arguments for k derived from
// environment, failing if a harness that requires a CLI home argument is
// missing its home variable.
func (k HarnessKind) HomeCLIArgsFromEnvironment(environment map[string]string) ([]string, error) {
	args := []string{}
	if value, ok := environment[k.HomeEnv()]; ok {
		args = k.HomeCLIArgs(k.HomeFromEnvironment(value))
	}
	if k == HarnessKindAntigravity && len(args) == 0 {
		return nil, errors.New("Antigravity staged home is missing")
	}
	return args, nil
}

// HomeEnv is the environment variable used to isolate this harness's
// configuration.
func (k HarnessKind) HomeEnv() string {
	switch k {
	case HarnessKindCodex:
		return "CODEX_HOME"
	case HarnessKindClaude:
		return "CLAUDE_CONFIG_DIR"
	case HarnessKindKimi:
		return "KIMI_CODE_HOME"
	case HarnessKindGrok:
		return "GROK_HOME"
	case HarnessKindMuse, HarnessKindOpenCode:
		return "XDG_CONFIG_HOME"
	case HarnessKindDsh:
		// dsh-acp has no home of its own; its data directory holds its
		// sessions and would otherwise default to `.dsh-acp` inside the
		// directory it runs in.
		return "DSH_ACP_DATA_DIR"
	case HarnessKindAntigravity:
		return "MJ_ANTIGRAVITY_HOME"
	}
	panic(fmt.Sprintf("unknown harness kind %q", string(k)))
}

// DefaultHomeLeaf is the directory beneath the user's home the harness uses
// when HomeEnv is unset. The single source for both setup discovery and
// import.
func (k HarnessKind) DefaultHomeLeaf() string {
	switch k {
	case HarnessKindCodex:
		return ".codex"
	case HarnessKindClaude:
		return ".claude"
	case HarnessKindKimi:
		return ".kimi-code"
	case HarnessKindGrok:
		return ".grok"
	case HarnessKindMuse:
		return ".config/muse"
	case HarnessKindOpenCode:
		return ".config/opencode"
	case HarnessKindDsh:
		// DeepSeek's own CLI, `dsh`, keeps its home here. Detection keys off
		// the user's DeepSeek installation; the dsh-acp bridge that serves a
		// session is installed on demand and proves nothing about one.
		return ".dsh"
	case HarnessKindAntigravity:
		return ".gemini"
	}
	panic(fmt.Sprintf("unknown harness kind %q", string(k)))
}

// CredentialFileName is the harness-home-relative file that proves the
// harness is logged in. Join it onto a home with
// HarnessAuthenticationMarker.
//
// OpenCode 2 keeps its logins in its database rather than a file
// (AuthSchemeNativeLoginStore); this names the file a staged home carries
// them in, the JSON array `opencode auth export` prints, which every launch
// imports (OpencodeACPScript).
func (k HarnessKind) CredentialFileName() string {
	switch k {
	case HarnessKindCodex, HarnessKindGrok, HarnessKindMuse:
		return "auth.json"
	case HarnessKindClaude:
		return ".credentials.json"
	case HarnessKindKimi:
		return "credentials/kimi-code.json"
	case HarnessKindOpenCode:
		return ".data/opencode/mjolnir-credentials.json"
	case HarnessKindDsh:
		// dsh-acp keeps no login: it reads its key from its environment
		// (HarnessKind.APIKeyVariable), so every DSH profile authenticates
		// with an API key and nothing that decides a login reads this. No
		// dsh-acp release writes the file.
		return "auth.json"
	case HarnessKindAntigravity:
		return "antigravity-cli/antigravity-oauth-token"
	}
	panic(fmt.Sprintf("unknown harness kind %q", string(k)))
}

// HarnessKindClaudeGhRule is the Claude Code allow rule for the GitHub CLI
// (Rust's HarnessKind::CLAUDE_GH_RULE).
const HarnessKindClaudeGhRule = "Bash(gh:*)"

// GhApproval is where this harness accepts Mjolnir's `gh` approval. See
// GhApproval for how each destination is delivered to a session.
func (k HarnessKind) GhApproval() GhApproval {
	switch k {
	case HarnessKindCodex:
		// Codex loads every `rules/*.rules` file from its home.
		return GhApprovalStagedFile{Value: "rules/mj-gh.rules"}
	case HarnessKindClaude:
		return GhApprovalSessionOptions{}
	case HarnessKindKimi, HarnessKindGrok:
		return GhApprovalStagedFile{Value: "config.toml"}
	case HarnessKindOpenCode:
		// OpenCode merges its configuration documents and takes the last
		// matching permission rule, so the rule is asserted in the document
		// Mjolnir supplies last.
		return GhApprovalSessionConfigDocument{Value: "OPENCODE_CONFIG_CONTENT"}
	case HarnessKindMuse:
		// Muse keeps command prefixes in its adapter, not its settings.
		return GhApprovalAdapterEnvironment{Value: "MUSE_ACP_ALLOW_COMMAND_PREFIXES"}
	case HarnessKindAntigravity:
		// agy's Auto mode answers commands from its project's grant file,
		// which the launch stages for the servers it registers.
		return GhApprovalLaunchFile{Value: "config/projects/default-cli-project.json"}
	case HarnessKindDsh:
		// dsh-acp has no configuration of its own; its permission mode is
		// all-or-nothing, so the worker answers its `gh` requests.
		return GhApprovalAcpRequest{}
	}
	panic(fmt.Sprintf("unknown harness kind %q", string(k)))
}

// KeepsLoginsInDatabase reports whether this harness keeps its logins in its
// own database rather than a file Mjolnir can copy (OpenCode 2). Staging
// exports them into the staged home, the credential sync pushes a changed
// export to live sessions, and each launch imports it.
func (k HarnessKind) KeepsLoginsInDatabase() bool { return k == HarnessKindOpenCode }

// APIKeyVariable is the variable this harness reads its API key from, for a
// harness that has no login of its own. Every profile of such a harness
// authenticates with AuthSchemeApiKey.
func (k HarnessKind) APIKeyVariable() (string, bool) {
	if k == HarnessKindDsh {
		return "DEEPSEEK_API_KEY", true
	}
	return "", false
}

// LaunchOwnedEnvironment is the variables Mjolnir sets on every launch of
// this harness, each with where its value comes from instead. A profile's
// `environment` cannot set them, because the launch would silently replace
// the value.
func (k HarnessKind) LaunchOwnedEnvironment() [][2]string {
	if k == HarnessKindDsh {
		return [][2]string{
			{"DSH_ACP_SYSTEM_PROMPT", "put DSH's instructions in AGENTS.md in the profile home"},
			{"DSH_ACP_PERMISSION_MODE", "the target's permission mode decides it"},
		}
	}
	return nil
}

// SharesLocalLogin reports whether sessions on this machine can share the
// profile's own login file through a link instead of each holding a copy.
//
// A login that rotates its refresh token on every refresh has one valid copy
// at a time, so copies refreshed independently make one another stale. Codex
// is built for one file shared by several processes: it reloads the file
// before it refreshes and uses a token another process already saved, and it
// rewrites the file in place, which keeps a link intact. Other harnesses are
// unverified and keep their copies.
func (k HarnessKind) SharesLocalLogin() bool { return k == HarnessKindCodex }

// CLIBinaryName is the harness's own command-line program, as found on PATH.
func (k HarnessKind) CLIBinaryName() string {
	switch k {
	case HarnessKindOpenCode:
		return "opencode"
	case HarnessKindDsh:
		// DeepSeek's `dsh`, not the dsh-acp bridge Mjolnir installs to serve
		// sessions: a machine carrying the bridge but no DeepSeek
		// installation has nothing to detect.
		return "dsh"
	case HarnessKindAntigravity:
		return "agy"
	}
	return string(k)
}

// AgentInstructionsFile is the project instruction file this harness reads.
func (k HarnessKind) AgentInstructionsFile() string {
	if k == HarnessKindClaude {
		return "CLAUDE.md"
	}
	// dsh-acp reads AGENTS.md from its data directory and workspace roots
	// (DshACPScript).
	return "AGENTS.md"
}

// SyncedSkillDirs is the harness-home-relative directories Hel keeps in sync
// for a profile.
//
// Every harness resolves user skills from a `skills/` directory under its
// home, matching the provisioning allowlist the controller stages. dsh-acp
// reads no skills; its tree is staged like every other harness's and stays
// inert.
func (k HarnessKind) SyncedSkillDirs() []string { return []string{"skills"} }

// HarnessOwnedSkillPaths is the home-relative paths inside a synced skills
// directory that the harness writes and maintains itself. Skills sync
// neither reads, copies, nor replaces them.
//
// Claude Code provisions `skills/synced/<org>_<user>/` from the user's
// claude.ai account and re-syncs it on its own; `skills/.trash/` is where it
// moves skills it removed. The Codex CLI writes its built-in skills into
// `skills/.system/`, with a `.codex-system-skills.marker` file there.
//
// Kimi, Grok, Muse and OpenCode keep none inside `skills/`. Kimi registers
// its built-in skills in memory, Grok caches its bundled skills under
// `bundled/skills/`, Muse writes its own under its data directory
// (`.data/muse/skills/` in a Mjolnir home), and OpenCode embeds its built-in
// skill. DSH reads no skills at all.
func (k HarnessKind) HarnessOwnedSkillPaths() []string {
	switch k {
	case HarnessKindClaude:
		return []string{"skills/synced", "skills/.trash"}
	case HarnessKindCodex:
		return []string{"skills/.system"}
	}
	return nil
}

// ID is the lowercase stable identifier used in config, storage, and the
// HTTP API.
func (k HarnessKind) ID() string {
	if k == HarnessKindOpenCode {
		return "opencode"
	}
	return string(k)
}

// DisplayName is the product name shown to people.
func (k HarnessKind) DisplayName() string {
	switch k {
	case HarnessKindCodex:
		return "Codex"
	case HarnessKindClaude:
		return "Claude Code"
	case HarnessKindKimi:
		return "Kimi Code"
	case HarnessKindGrok:
		return "Grok Build"
	case HarnessKindMuse:
		return "Muse Code"
	case HarnessKindOpenCode:
		return "OpenCode"
	case HarnessKindDsh:
		return "DSH"
	case HarnessKindAntigravity:
		return "Antigravity"
	}
	panic(fmt.Sprintf("unknown harness kind %q", string(k)))
}

// InstallAdvice is how to install this harness's own CLI, as the start of a
// sentence: "Install it with `npm install -g @openai/codex` (Node.js 22 or
// newer)". `mj login` and launch preflight say it when the CLI is missing.
func (k HarnessKind) InstallAdvice() string {
	switch k {
	case HarnessKindCodex:
		return "Install it with `npm install -g @openai/codex` (Node.js 22 or newer)"
	case HarnessKindClaude:
		return "Install it with `npm install -g @anthropic-ai/claude-code`"
	case HarnessKindDsh:
		return "Install it with `npm install -g @brokkai/dsh-acp` (Node.js 22 or newer), or from a release archive at https://github.com/BrokkAi/dsh-acp/releases"
	}
	return fmt.Sprintf("Install the %s CLI", k.DisplayName())
}

// HarnessKindEveryDisplayNameOr is every harness by product name, in
// HarnessKindAll order, as a list that ends with "or": "Codex, Claude Code,
// Kimi Code, Grok Build, Muse Code, or OpenCode". Messages that say no agent
// was found use it, so each one names every agent Mjolnir looks for.
func HarnessKindEveryDisplayNameOr() string {
	names := make([]string, len(HarnessKindAll))
	for i, kind := range HarnessKindAll {
		names[i] = kind.DisplayName()
	}
	return strings.Join(names[:len(names)-1], ", ") + ", or " + names[len(names)-1]
}

// ExecutionEnforcement is how this harness realizes a target-level execution
// policy. Configured approvals preserve harness configuration, except that
// Codex, Claude, Muse and Antigravity select their guardian mode explicitly.
// Unconstrained targets run every harness without approval checks; only the
// sandbox is disabled for Muse, whose profile may opt back into auto-review.
func (k HarnessKind) ExecutionEnforcement(policy ExecutionPolicy) (ExecutionEnforcement, bool) {
	unconstrained := policy.IsUnconstrained()
	switch {
	// Antigravity's guardian is its Auto mode: agy approves workspace edits,
	// runs commands in its terminal sandbox, and has a separate reviewer
	// agent clear or ask about commands that would leave it. Unconstrained
	// targets use Default mode, which has no reviewer, and the flag that
	// approves every request except agy's own system command denylist.
	case k == HarnessKindAntigravity:
		enforcement := ExecutionEnforcement{label: "auto / guardian", acpMode: staticString("auto")}
		if unconstrained {
			enforcement.label = "default / skip-permissions"
			enforcement.acpMode = staticString("default")
			enforcement.launchFlag = staticString("--dangerously-skip-permissions")
		}
		return enforcement, true
	case k == HarnessKindMuse && unconstrained:
		// Isolated targets run Muse without permission checks, like
		// OpenCode's `permission = "allow"`: `allowAll` emits no permission
		// requests, so the adapter's client-side auto-review would have
		// nothing to decide, and it stays off. The container remains the
		// isolation boundary, so only the sandbox is off.
		//
		// muse-acp 0.10 puts Default, Read-only, and Plan on `mode` and the
		// approval policy on `approval_mode`. Container images built before
		// it keep the approval policy on `mode`.
		return ExecutionEnforcement{
			label:             "allowAll / no-review / sandbox-off / :ask-me",
			acpMode:           staticString("allowAll"),
			acpModeSelector:   staticString("approval_mode"),
			acpSetting:        &[2]string{"auto_review", "off"},
			launchEnvironment: &[2]string{"MUSE_APPROVAL_MODE", "allowAll"},
			launchArgument:    &LaunchArgument{Variable: "MUSE_SERVE_ARGS", Word: "--disable-sandbox", Present: true},
			stagedSetting:     museStagedSetting(),
		}, true
	// Muse's guardian is muse-acp's auto-review: a read-only Muse reviewer
	// answers each approval, and a failed review denies. `muse serve`
	// refuses Muse's own `:auto-review` profile, so the staged profile asks
	// and the adapter's reviewer answers. The sandbox stays on even when a
	// profile's MUSE_SERVE_ARGS disables it.
	case k == HarnessKindMuse:
		return ExecutionEnforcement{
			label:             "promptUnmatched / auto-review / :ask-me",
			acpMode:           staticString("promptUnmatched"),
			acpModeSelector:   staticString("approval_mode"),
			acpSetting:        &[2]string{"auto_review", "on"},
			launchEnvironment: &[2]string{"MUSE_APPROVAL_MODE", "promptUnmatched"},
			launchArgument:    &LaunchArgument{Variable: "MUSE_SERVE_ARGS", Word: "--disable-sandbox", Present: false},
			stagedSetting:     museStagedSetting(),
		}, true
	case k == HarnessKindCodex && !unconstrained:
		return ExecutionEnforcement{
			label:             "agent / guardian",
			acpMode:           staticString("agent"),
			launchEnvironment: &[2]string{"INITIAL_AGENT_MODE", "agent"},
		}, true
	case k == HarnessKindCodex:
		return ExecutionEnforcement{
			label:             "agent-full-access",
			acpMode:           staticString("agent-full-access"),
			launchEnvironment: &[2]string{"INITIAL_AGENT_MODE", "agent-full-access"},
		}, true
	// Claude's guardian defaults to its Auto mode; a profile's
	// `claude_permission_mode` picks another.
	case k == HarnessKindClaude && !unconstrained:
		return HarnessKindClaudeGuardianEnforcement(ClaudePermissionModeAuto), true
	// dsh-acp records each session's permission policy and keeps it across
	// `session/load`, so the policy is selected on every open: a resumed
	// session, or one moved from a container to a raw target, must not keep
	// an earlier one. `ask` sends every write, command and MCP call to Hel as
	// a permission request.
	case k == HarnessKindDsh && !unconstrained:
		return ExecutionEnforcement{label: "permission-ask", acpSetting: &[2]string{"permission_mode", "ask"}}, true
	// Every remaining harness keeps the configuration its user wrote.
	case !unconstrained:
		return ExecutionEnforcement{}, false
	case k == HarnessKindClaude:
		return ExecutionEnforcement{
			label:          "bypassPermissions / sandbox-off",
			acpMode:        staticString("bypassPermissions"),
			sessionSandbox: new(bool),
		}, true
	case k == HarnessKindKimi:
		return ExecutionEnforcement{label: "auto", acpMode: staticString("auto")}, true
	case k == HarnessKindGrok:
		return ExecutionEnforcement{
			label:             "always-approve / sandbox-off",
			launchFlag:        staticString("--always-approve"),
			launchEnvironment: &[2]string{"GROK_SANDBOX", "off"},
		}, true
	// OpenCode reads permission rules from its config file; `allow` is its
	// shorthand for allowing every tool without a prompt.
	case k == HarnessKindOpenCode:
		return ExecutionEnforcement{
			label:         "permission-allow",
			stagedSetting: &StagedSetting{File: "opencode.json", Path: []string{"permission"}, Value: "allow"},
		}, true
	// dsh-acp has no sandbox to disable; `allow` stops it asking.
	case k == HarnessKindDsh:
		return ExecutionEnforcement{label: "permission-allow", acpSetting: &[2]string{"permission_mode", "allow"}}, true
	}
	panic(fmt.Sprintf("unknown harness kind %q", string(k)))
}

// staticString is a pointer to a copy of value, for the optional fields of
// ExecutionEnforcement.
func staticString(value string) *string { return &value }

// museStagedSetting is the profile both Muse policies stage.
func museStagedSetting() *StagedSetting {
	return &StagedSetting{
		File:          "settings.json",
		Path:          []string{"permissions", "default_profile"},
		Value:         ":ask-me",
		ObjectVersion: &StagedObjectVersion{Key: "schema_version", Version: 1},
	}
}

// HarnessKindClaudeGuardianEnforcement is Claude under configured approvals,
// in the profile's chosen mode.
func HarnessKindClaudeGuardianEnforcement(mode ClaudePermissionMode) ExecutionEnforcement {
	label := "auto / guardian"
	switch mode {
	case ClaudePermissionModeAcceptEdits:
		label = "acceptEdits / guardian"
	case ClaudePermissionModeManual:
		label = "default / guardian"
	}
	return ExecutionEnforcement{label: label, acpMode: staticString(mode.ACPMode())}
}

// ResolveExecutionEnforcement is execution enforcement for policy, honoring
// a Claude profile's claude_permission_mode choice.
func (k HarnessKind) ResolveExecutionEnforcement(policy ExecutionPolicy, choices ApprovalChoices) (ExecutionEnforcement, bool) {
	if k == HarnessKindClaude && policy == ExecutionPolicyConfiguredApprovals {
		return HarnessKindClaudeGuardianEnforcement(choices.ClaudePermissionMode), true
	}
	return k.ExecutionEnforcement(policy)
}

// ConfigureExecutionEnvironment applies the launch environment required to
// realize policy. The controller writes this into new launch configs, and the
// worker repeats it so persisted configs from older Hel versions acquire the
// same enforcement after an upgrade.
func (k HarnessKind) ConfigureExecutionEnvironment(policy ExecutionPolicy, environment map[string]string) error {
	enforcement, ok := k.ExecutionEnforcement(policy)
	if !ok {
		return nil
	}
	applyExecutionEnforcementEnvironment(enforcement, environment)
	return nil
}

// applyExecutionEnforcementEnvironment is the shared body of
// HarnessKind.ConfigureExecutionEnvironment and
// HarnessProfile.ConfigureExecutionEnvironment: the launch argument, then the
// launch environment.
func applyExecutionEnforcementEnvironment(enforcement ExecutionEnforcement, environment map[string]string) {
	if argument, ok := enforcement.LaunchArgument(); ok {
		args := environment[argument.Variable]
		words := strings.Fields(args)
		hasWord := false
		for _, word := range words {
			if word == argument.Word {
				hasWord = true
				break
			}
		}
		if argument.Present && !hasWord {
			if args != "" {
				args += " "
			}
			environment[argument.Variable] = args + argument.Word
		} else if !argument.Present && hasWord {
			// The harness splits this variable on whitespace without quoting,
			// so rejoining the other words keeps their meaning.
			rest := make([]string, 0, len(words))
			for _, word := range words {
				if word != argument.Word {
					rest = append(rest, word)
				}
			}
			if len(rest) == 0 {
				delete(environment, argument.Variable)
			} else {
				environment[argument.Variable] = strings.Join(rest, " ")
			}
		}
	}
	if key, value, ok := enforcement.LaunchEnvironment(); ok {
		environment[key] = value
	}
}

// Guardian is whether the harness ships a native reviewer under configured
// approvals. Kimi Code, OpenCode and DSH rely on Mjolnir's reviewer.
func (k HarnessKind) Guardian() GuardianReviewer {
	switch k {
	case HarnessKindKimi, HarnessKindOpenCode, HarnessKindDsh:
		return GuardianReviewerMjolnir
	}
	return GuardianReviewerHarness
}

// MjolnirGuardianReviews reports whether requests that reach Mjolnir are
// eligible for its guardian.
func (k HarnessKind) MjolnirGuardianReviews(policy ExecutionPolicy, choices ApprovalChoices) bool {
	return policy == ExecutionPolicyConfiguredApprovals && choices.MjolnirGuardian
}

// LaunchFlagFor is the launch flag the bridge command line carries, if any.
func (k HarnessKind) LaunchFlagFor(policy ExecutionPolicy) (string, bool) {
	enforcement, ok := k.ExecutionEnforcement(policy)
	if !ok {
		return "", false
	}
	return enforcement.LaunchFlag()
}

// MCPConfigFile is the file in a staged harness home that holds its MCP
// server list, for the harnesses that read that list from disk rather than
// over ACP.
func (k HarnessKind) MCPConfigFile() (string, bool) {
	switch k {
	case HarnessKindClaude:
		return ".claude.json", true
	case HarnessKindKimi:
		return "mcp.json", true
	}
	return "", false
}

// SupportsDelegationTools reports whether this harness receives Mjolnir's
// delegation tools: a parent's `spawn` and related tools, and a child's
// `handback`. Such a harness gets the subagent choice in the wizards. Claude
// reads the tools from its staged profile; Codex, DSH and OpenCode take them
// over ACP, and DSH and OpenCode read the delegation policy from their staged
// AGENTS.md.
func (k HarnessKind) SupportsDelegationTools() bool {
	switch k {
	case HarnessKindClaude, HarnessKindCodex, HarnessKindDsh, HarnessKindOpenCode, HarnessKindAntigravity:
		return true
	}
	return false
}

// SupportsReviewGate reports whether a session on this harness is offered the
// review tools, and a primary one can be held to push review: the `mj-review`
// server, the push hook and MJ_REVIEW_GATE. Separate from
// SupportsDelegationTools, which OpenCode satisfies without having the gate.
func (k HarnessKind) SupportsReviewGate() bool {
	switch k {
	case HarnessKindClaude, HarnessKindCodex, HarnessKindDsh, HarnessKindKimi, HarnessKindAntigravity:
		return true
	}
	return false
}

// HasNativeSubagents reports whether the harness has sub-agents of its own,
// which the Native delegation choice keeps. dsh-acp has none, so Native would
// do nothing for DSH and is not offered there.
func (k HarnessKind) HasNativeSubagents() bool {
	return k != HarnessKindDsh && k != HarnessKindAntigravity
}

// NativeSessionDirs is the harness-home-relative directories that hold its
// native session files, scanned when a checkpoint captures or restores native
// state.
func (k HarnessKind) NativeSessionDirs() []string {
	switch k {
	case HarnessKindCodex:
		return []string{"sessions", "archived_sessions"}
	case HarnessKindClaude:
		return []string{"projects", "session-env", "file-history"}
	case HarnessKindKimi, HarnessKindGrok, HarnessKindDsh:
		return []string{"sessions"}
	case HarnessKindMuse:
		return []string{".data/muse/sessions"}
	case HarnessKindOpenCode:
		return []string{".data/opencode"}
	case HarnessKindAntigravity:
		// agy's conversation database and, beside it, the files each
		// conversation owns, such as the full output of a long command.
		return []string{"antigravity-cli/conversations", "antigravity-cli/brain"}
	}
	panic(fmt.Sprintf("unknown harness kind %q", string(k)))
}

// MCPServersNeedAdvertisedHTTP reports whether this harness's adapter passes
// on the MCP servers a session offers only when its `initialize` advertises
// HTTP MCP. muse-acp forwards them to Muse only under the host's session MCP
// grant, advertises HTTP MCP exactly then, and otherwise drops every server
// with a log line.
func (k HarnessKind) MCPServersNeedAdvertisedHTTP() bool { return k == HarnessKindMuse }

// ExtraManagedEntrypoint is an executable a managed install must create
// beside its pinned entrypoint, relative to the install root.
func (k HarnessKind) ExtraManagedEntrypoint() (string, bool) {
	switch k {
	case HarnessKindGrok:
		return "bin/agent", true
	case HarnessKindClaude:
		return "node_modules/.bin/claude", true
	}
	return "", false
}

// BridgeArgs is the harness-specific arguments that start its ACP stdio
// server.
func (k HarnessKind) BridgeArgs(policy ExecutionPolicy) []string {
	flag, hasFlag := k.LaunchFlagFor(policy)
	switch k {
	case HarnessKindKimi, HarnessKindOpenCode:
		return []string{"acp"}
	case HarnessKindGrok:
		args := []string{"agent"}
		if hasFlag {
			args = append(args, flag)
		}
		return append(args, "stdio")
	case HarnessKindAntigravity:
		args := []string{"acp", "--disable-telemetry"}
		if hasFlag {
			args = append(args, flag)
		}
		return args
	}
	// dsh-acp is an ACP server and nothing else.
	return []string{}
}

// ParseHarnessKind is FromStr for HarnessKind: it reads an ID.
func ParseHarnessKind(value string) (HarnessKind, error) {
	for _, kind := range HarnessKindAll {
		if kind.ID() == value {
			return kind, nil
		}
	}
	return "", fmt.Errorf("unknown harness kind %s", debugString(value))
}

// QuotaProvider is the company that meters a profile's allowance,
// independent of its harness.
type QuotaProvider interface {
	isQuotaProvider()
	DisplayName() string
}

type QuotaProviderAnthropic struct{}
type QuotaProviderOpenAI struct{}
type QuotaProviderGoogle struct{}
type QuotaProviderXAI struct{}
type QuotaProviderMoonshot struct{}
type QuotaProviderDeepSeek struct{}
type QuotaProviderZai struct{}
type QuotaProviderMeta struct{}
type QuotaProviderOther struct{ Value string }

func (QuotaProviderAnthropic) isQuotaProvider() {}
func (QuotaProviderOpenAI) isQuotaProvider()    {}
func (QuotaProviderGoogle) isQuotaProvider()    {}
func (QuotaProviderXAI) isQuotaProvider()       {}
func (QuotaProviderMoonshot) isQuotaProvider()  {}
func (QuotaProviderDeepSeek) isQuotaProvider()  {}
func (QuotaProviderZai) isQuotaProvider()       {}
func (QuotaProviderMeta) isQuotaProvider()      {}
func (QuotaProviderOther) isQuotaProvider()     {}

func (QuotaProviderAnthropic) DisplayName() string { return "Anthropic" }
func (QuotaProviderOpenAI) DisplayName() string    { return "OpenAI" }
func (QuotaProviderGoogle) DisplayName() string    { return "Google" }
func (QuotaProviderXAI) DisplayName() string       { return "xAI" }
func (QuotaProviderMoonshot) DisplayName() string  { return "Moonshot" }
func (QuotaProviderDeepSeek) DisplayName() string  { return "DeepSeek" }
func (QuotaProviderZai) DisplayName() string       { return "Z.ai" }
func (QuotaProviderMeta) DisplayName() string      { return "Meta" }
func (p QuotaProviderOther) DisplayName() string   { return p.Value }

// QuotaProvider serializes as serde's externally tagged enum: a unit variant
// is its name and Other is `{"Other": name}`.
func (QuotaProviderAnthropic) MarshalJSON() ([]byte, error) { return []byte(`"Anthropic"`), nil }
func (QuotaProviderOpenAI) MarshalJSON() ([]byte, error)    { return []byte(`"OpenAI"`), nil }
func (QuotaProviderGoogle) MarshalJSON() ([]byte, error)    { return []byte(`"Google"`), nil }
func (QuotaProviderXAI) MarshalJSON() ([]byte, error)       { return []byte(`"XAI"`), nil }
func (QuotaProviderMoonshot) MarshalJSON() ([]byte, error)  { return []byte(`"Moonshot"`), nil }
func (QuotaProviderDeepSeek) MarshalJSON() ([]byte, error)  { return []byte(`"DeepSeek"`), nil }
func (QuotaProviderZai) MarshalJSON() ([]byte, error)       { return []byte(`"Zai"`), nil }
func (QuotaProviderMeta) MarshalJSON() ([]byte, error)      { return []byte(`"Meta"`), nil }
func (p QuotaProviderOther) MarshalJSON() ([]byte, error) {
	return json.Marshal(map[string]string{"Other": p.Value})
}

// UnmarshalQuotaProviderJSON reads serde's externally tagged QuotaProvider.
// Fields holding a QuotaProvider decode through it.
func UnmarshalQuotaProviderJSON(data []byte) (QuotaProvider, error) {
	var name string
	if err := json.Unmarshal(data, &name); err == nil {
		switch name {
		case "Anthropic":
			return QuotaProviderAnthropic{}, nil
		case "OpenAI":
			return QuotaProviderOpenAI{}, nil
		case "Google":
			return QuotaProviderGoogle{}, nil
		case "XAI":
			return QuotaProviderXAI{}, nil
		case "Moonshot":
			return QuotaProviderMoonshot{}, nil
		case "DeepSeek":
			return QuotaProviderDeepSeek{}, nil
		case "Zai":
			return QuotaProviderZai{}, nil
		case "Meta":
			return QuotaProviderMeta{}, nil
		case "Other":
			return nil, errors.New("invalid type: unit variant, expected newtype variant")
		}
		return nil, serdeUnknownVariant(name, "Anthropic", "OpenAI", "Google", "XAI", "Moonshot", "DeepSeek", "Zai", "Meta", "Other")
	}
	var tagged map[string]json.RawMessage
	if err := json.Unmarshal(data, &tagged); err != nil || len(tagged) != 1 {
		return nil, fmt.Errorf("invalid type: %s, expected enum QuotaProvider", serdeUnexpected(data))
	}
	for variant, content := range tagged {
		if variant != "Other" {
			return nil, serdeUnknownVariant(variant, "Anthropic", "OpenAI", "Google", "XAI", "Moonshot", "DeepSeek", "Zai", "Meta", "Other")
		}
		var value string
		if err := json.Unmarshal(content, &value); err != nil {
			return nil, err
		}
		return QuotaProviderOther{Value: value}, nil
	}
	panic("unreachable")
}

// QuotaProviderOrdinal is the position of p's variant in declaration order,
// which with QuotaProviderOther's name is Rust's derived Ord.
func QuotaProviderOrdinal(p QuotaProvider) int {
	switch p.(type) {
	case QuotaProviderAnthropic:
		return 0
	case QuotaProviderOpenAI:
		return 1
	case QuotaProviderGoogle:
		return 2
	case QuotaProviderXAI:
		return 3
	case QuotaProviderMoonshot:
		return 4
	case QuotaProviderDeepSeek:
		return 5
	case QuotaProviderZai:
		return 6
	case QuotaProviderMeta:
		return 7
	}
	return 8
}

// quotaProviderUnitNames is each unit variant's serde name, by ordinal.
var quotaProviderUnitNames = []string{"Anthropic", "OpenAI", "Google", "XAI", "Moonshot", "DeepSeek", "Zai", "Meta"}

// QuotaProvider serializes externally tagged, as serde derives it: a unit
// variant as its name, Other as {"Other":"<name>"}.
func (p QuotaProviderAnthropic) MarshalJSON() ([]byte, error) { return quotaProviderUnitJSON(p) }
func (p QuotaProviderOpenAI) MarshalJSON() ([]byte, error)    { return quotaProviderUnitJSON(p) }
func (p QuotaProviderGoogle) MarshalJSON() ([]byte, error)    { return quotaProviderUnitJSON(p) }
func (p QuotaProviderXAI) MarshalJSON() ([]byte, error)       { return quotaProviderUnitJSON(p) }
func (p QuotaProviderMoonshot) MarshalJSON() ([]byte, error)  { return quotaProviderUnitJSON(p) }
func (p QuotaProviderDeepSeek) MarshalJSON() ([]byte, error)  { return quotaProviderUnitJSON(p) }
func (p QuotaProviderZai) MarshalJSON() ([]byte, error)       { return quotaProviderUnitJSON(p) }
func (p QuotaProviderMeta) MarshalJSON() ([]byte, error)      { return quotaProviderUnitJSON(p) }

func (p QuotaProviderOther) MarshalJSON() ([]byte, error) {
	return json.Marshal(map[string]string{"Other": p.Value})
}

func quotaProviderUnitJSON(p QuotaProvider) ([]byte, error) {
	return json.Marshal(quotaProviderUnitNames[QuotaProviderOrdinal(p)])
}

// UnmarshalQuotaProviderJSON decodes serde's externally tagged QuotaProvider.
func UnmarshalQuotaProviderJSON(data []byte) (QuotaProvider, error) {
	var name string
	if err := json.Unmarshal(data, &name); err == nil {
		units := []QuotaProvider{QuotaProviderAnthropic{}, QuotaProviderOpenAI{}, QuotaProviderGoogle{},
			QuotaProviderXAI{}, QuotaProviderMoonshot{}, QuotaProviderDeepSeek{}, QuotaProviderZai{}, QuotaProviderMeta{}}
		for i, unit := range quotaProviderUnitNames {
			if unit == name {
				return units[i], nil
			}
		}
		if name == "Other" {
			return nil, errors.New("invalid type: unit variant, expected newtype variant")
		}
		return nil, fmt.Errorf("unknown variant `%s`, expected one of `Anthropic`, `OpenAI`, `Google`, `XAI`, `Moonshot`, `DeepSeek`, `Zai`, `Meta`, `Other`", name)
	}
	var other struct {
		Other *string `json:"Other"`
	}
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return nil, err
	}
	if err := json.Unmarshal(data, &other); err != nil {
		return nil, err
	}
	if len(members) != 1 || other.Other == nil {
		return nil, errors.New("invalid type: map, expected a QuotaProvider variant")
	}
	return QuotaProviderOther{Value: *other.Other}, nil
}

func quotaProviderFromEndpoint(id string, endpoint *string) QuotaProvider {
	// The endpoint decides when there is one: an unrecognised host (a proxy,
	// OpenRouter, a local server) is its own provider, never the table id's
	// default vendor. Without an endpoint the provider id decides.
	if endpoint != nil {
		if host, ok := urlHost(*endpoint); ok {
			if provider, ok := quotaProviderRecognise(host); ok {
				return provider
			}
			return QuotaProviderOther{Value: host}
		}
	}
	if provider, ok := quotaProviderRecognise(id); ok {
		return provider
	}
	return QuotaProviderOther{Value: id}
}

// urlHost is the host of an absolute URL, as Rust's url crate writes it:
// lowercase, without a port.
func urlHost(text string) (string, bool) {
	parsed, err := url.Parse(text)
	if err != nil || parsed.Scheme == "" || parsed.Hostname() == "" {
		return "", false
	}
	return strings.ToLower(parsed.Hostname()), true
}

func quotaProviderRecognise(value string) (QuotaProvider, bool) {
	switch CodexProviderKindFromHost(value) {
	case CodexProviderKindZai:
		return QuotaProviderZai{}, true
	case CodexProviderKindDeepSeek:
		return QuotaProviderDeepSeek{}, true
	}
	value = strings.ToLower(value)
	domain := func(suffix string) bool { return value == suffix || strings.HasSuffix(value, "."+suffix) }
	switch {
	case value == "deepseek":
		return QuotaProviderDeepSeek{}, true
	case domain("anthropic.com") || value == "anthropic":
		return QuotaProviderAnthropic{}, true
	case domain("openai.com") || value == "openai":
		return QuotaProviderOpenAI{}, true
	case domain("googleapis.com") || value == "google" || value == "gemini" || value == "google-vertex":
		return QuotaProviderGoogle{}, true
	case domain("meta.ai") || domain("meta.com") || value == "meta":
		return QuotaProviderMeta{}, true
	case domain("x.ai") || value == "xai" || value == "grok":
		return QuotaProviderXAI{}, true
	case domain("moonshot.ai") || domain("moonshot.cn") || value == "moonshot" || value == "kimi" || value == "kimi-for-coding":
		return QuotaProviderMoonshot{}, true
	}
	return nil, false
}

// CodexQuotaSource is where a Codex profile's quota comes from, decided once
// from its provider configuration. Only Codex's own OpenAI login has ChatGPT
// rate-limit windows; every other provider must never be probed through that
// login, or it reports the ChatGPT account's quota as its own.
type CodexQuotaSource interface {
	isCodexQuotaSource()
}

// CodexQuotaSourceChatGptLogin: no `model_provider`, or the built-in `openai`
// provider.
type CodexQuotaSourceChatGptLogin struct{}

// CodexQuotaSourceProvider is a custom provider whose API key and host
// Mjolnir can read; its quota comes from the provider's own endpoint when it
// publishes one.
type CodexQuotaSourceProvider struct{ Value ProviderCredential }

// CodexQuotaSourceUsagePriced is any other provider: a local or Bedrock
// built-in, or a custom provider whose key Mjolnir cannot read. It has no
// allowance to report.
type CodexQuotaSourceUsagePriced struct{ Provider string }

// CodexQuotaSourceUnreadable: the profile's Codex configuration could not be
// read.
type CodexQuotaSourceUnreadable struct{ Value string }

func (CodexQuotaSourceChatGptLogin) isCodexQuotaSource() {}
func (CodexQuotaSourceProvider) isCodexQuotaSource()     {}
func (CodexQuotaSourceUsagePriced) isCodexQuotaSource()  {}
func (CodexQuotaSourceUnreadable) isCodexQuotaSource()   {}

// CodexQuotaSourceOfProfile is the source for one configured profile.
// `mj doctor` reads the same answer to say where each profile's quota comes
// from.
func CodexQuotaSourceOfProfile(profile HarnessProfile) CodexQuotaSource {
	source, _ := codexQuotaSourceResolve(profile)
	return source
}

// codexQuotaSourceResolve decides collection and attribution together from
// the same configuration.
func codexQuotaSourceResolve(profile HarnessProfile) (CodexQuotaSource, QuotaProvider) {
	provider, err := profile.CodexProvider()
	if err != nil {
		slog.Warn("read quota provider", "home", profile.Home, "error", err.Error())
		return CodexQuotaSourceUnreadable{Value: err.Error()}, QuotaProviderOther{Value: "Codex (provider unavailable)"}
	}
	if provider == nil {
		return CodexQuotaSourceChatGptLogin{}, QuotaProviderOpenAI{}
	}
	if provider.UsesCodexLogin() {
		// The quota collector reads ChatGPT's allowance, even for an API-key
		// login whose launch environment redirects model requests elsewhere.
		return CodexQuotaSourceChatGptLogin{}, QuotaProviderOpenAI{}
	}
	var endpoint *string
	if custom := provider.Custom(); custom != nil {
		endpoint = &custom.BaseURL
	}
	attribution := quotaProviderFromEndpoint(provider.ID(), endpoint)
	if provider.Custom() != nil {
		apiKey, hasKey := profile.codexProviderAPIKeyFrom(*provider)
		host, hasHost := provider.Host()
		if hasKey && hasHost {
			return CodexQuotaSourceProvider{Value: ProviderCredential{ID: provider.ID(), Host: host, APIKey: apiKey}}, attribution
		}
	}
	return CodexQuotaSourceUsagePriced{Provider: provider.ID()}, attribution
}

// ProviderCredential is where a profile's quota lives when the profile
// authenticates with an API key against a provider named in its harness
// configuration.
type ProviderCredential struct {
	// ID is the provider id from the harness configuration, for error
	// messages.
	ID string
	// Host is the host of the provider's base URL, for example `api.z.ai`.
	Host   string
	APIKey string
}

// MuseQuotaEndpoints is where Muse reads its allowance and refreshes its
// OAuth login.
type MuseQuotaEndpoints struct {
	Mint string
	Auth string
}

// MuseQuotaEndpointsOf: Muse's own TBH overrides take the profile environment
// first, then the daemon environment, exactly as the quota adapter does.
func MuseQuotaEndpointsOf(environment map[string]string) MuseQuotaEndpoints {
	configured := func(name, fallback string) string {
		if value, ok := environment[name]; ok {
			return value
		}
		if value, ok := os.LookupEnv(name); ok {
			return value
		}
		return fallback
	}
	return MuseQuotaEndpoints{
		Mint: configured("TBH_MINT_BASE_URL", "https://api.meta.ai"),
		Auth: configured("TBH_AUTH_BASE_URL", "https://auth.meta.com"),
	}
}

type HarnessProfile struct {
	// Enabled is whether Mjolnir may select this profile for new work or
	// probe it.
	Enabled bool        `json:"enabled" toml:"enabled"`
	Kind    HarnessKind `json:"kind" toml:"kind"`
	// Home is the controller-side source home. A fresh copy is made for each
	// target.
	Home string `json:"home" toml:"home"`
	// Environment is passed to the harness and its bridge. An entry may name
	// a secret instead of holding it; see Environment.
	Environment Environment `json:"environment" toml:"environment"`
	// ContextWindowBytes is the conservative byte budget for cross-harness
	// transcript compaction. Bytes avoid pretending Hel has an accurate
	// tokenizer for every model.
	ContextWindowBytes *int `json:"context_window_bytes,omitempty" toml:"context_window_bytes,omitempty"`
	// GuardianReviewModel is which model reviews this profile's escalated
	// actions in Codex's Guardian mode: "newest-flash" (the default when
	// absent) picks the newest flash model the provider's catalog lists,
	// "session" leaves reviews to the session's own model, and any other
	// value names a catalog slug. Only meaningful for a Codex profile with a
	// custom model provider, because Mjolnir generates the catalog only for
	// those.
	GuardianReviewModel *string `json:"guardian_review_model,omitempty" toml:"guardian_review_model,omitempty"`
	// LoadBalancingMayConsumeCredits is whether load balancing may pick this
	// subscription profile once its quota for the requested model is gone
	// and only paid credits remain. Off by default: a profile out of
	// subscription quota is not offered until the quota resets.
	// Usage-priced profiles are unaffected.
	LoadBalancingMayConsumeCredits bool `json:"load_balancing_may_consume_credits,omitempty" toml:"load_balancing_may_consume_credits,omitempty"`
	// CodexProfile is a Codex profile overlay, `<home>/<name>.config.toml`,
	// layered over the home's `config.toml` as `codex --profile <name>`
	// would layer it. See CodexEffectiveConfig.
	CodexProfile *string `json:"codex_profile,omitempty" toml:"codex_profile,omitempty"`
	// LegacyMuseAutoReview is the legacy Muse review toggle. It is consumed
	// and ignored because Muse now follows its runtime's policy like every
	// harness. Never written.
	LegacyMuseAutoReview json.RawMessage `json:"muse_auto_review" toml:"muse_auto_review"`
	// ClaudePermissionMode is the Claude Code permission mode this Claude
	// profile's sessions run in where the target keeps configured approvals
	// (localhost, guardian SSH): "auto" (the default when absent) lets
	// Claude's classifier decide each action, "accept-edits" runs file edits
	// and asks for the rest, and "manual" asks for every action that needs
	// permission. Unconstrained targets always bypass permissions. Only valid
	// on a Claude profile.
	ClaudePermissionMode *ClaudePermissionMode `json:"claude_permission_mode,omitempty" toml:"claude_permission_mode,omitempty"`
	// MjolnirGuardian is whether the Mjolnir guardian answers this profile's
	// tool approvals where the target keeps configured approvals (localhost,
	// guardian SSH). On by default. Turn it off to send pending harness
	// approval requests directly to the person.
	MjolnirGuardian bool `json:"mjolnir_guardian" toml:"mjolnir_guardian"`
	// AWS is the IAM role Mjolnir assumes for each session of a profile that
	// authenticates with the AWS credential chain (Bedrock), so the session
	// gets short-lived credentials limited to that role instead of a copy of
	// the controller host's credentials. See AwsRoleConfig.
	AWS *AwsRoleConfig `json:"aws,omitempty" toml:"aws,omitempty"`
	// Models is the models Mjolnir offers from this profile, as exact values
	// from the profile's advertised catalogue
	// (`openrouter/deepseek/deepseek-v4.1-flash-1017`). Empty offers
	// everything the harness advertises. Some harnesses, such as OpenCode on
	// OpenRouter, advertise hundreds of models and ignore their own
	// allow-lists. The profile's current model is always offered.
	Models []string `json:"models,omitempty" toml:"models,omitempty"`
	// ThirdPartyModels is whether an Antigravity profile uses its third-party
	// quota pool (the `3p-` buckets in `agy /usage`), which serves Claude and
	// GPT models. False offers only Gemini models and hides that pool's
	// quota. Only valid on an Antigravity profile.
	ThirdPartyModels bool `json:"third_party_models" toml:"third_party_models"`
	// LegacySubagents consumes legacy per-profile choices without migrating
	// them to global settings. Never written.
	LegacySubagents json.RawMessage `json:"subagents" toml:"subagents"`
}

func (h HarnessProfile) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(isTrue(h.Enabled), "enabled", h.Enabled)
	object.field("kind", h.Kind)
	object.field("home", h.Home)
	object.fieldUnless(h.Environment.IsEmpty(), "environment", h.Environment)
	object.fieldUnless(h.ContextWindowBytes == nil, "context_window_bytes", h.ContextWindowBytes)
	object.fieldUnless(h.GuardianReviewModel == nil, "guardian_review_model", h.GuardianReviewModel)
	object.fieldUnless(isFalse(h.LoadBalancingMayConsumeCredits), "load_balancing_may_consume_credits", h.LoadBalancingMayConsumeCredits)
	object.fieldUnless(h.CodexProfile == nil, "codex_profile", h.CodexProfile)
	object.fieldUnless(h.ClaudePermissionMode == nil, "claude_permission_mode", h.ClaudePermissionMode)
	object.fieldUnless(isTrue(h.MjolnirGuardian), "mjolnir_guardian", h.MjolnirGuardian)
	object.fieldUnless(h.AWS == nil, "aws", h.AWS)
	object.fieldUnless(len(h.Models) == 0, "models", h.Models)
	object.fieldUnless(isTrue(h.ThirdPartyModels), "third_party_models", h.ThirdPartyModels)
	return object.finish()
}

func (h *HarnessProfile) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "kind", "home"); err != nil {
		return err
	}
	type plain HarnessProfile
	value := plain{Enabled: true, ThirdPartyModels: true, MjolnirGuardian: true}
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*h = HarnessProfile(value)
	return nil
}

// ProfileDiscoveryInputs is the inputs that change advertised models and
// efforts. Session defaults and UI preferences are deliberately absent. Use
// this projection in every cache.
type ProfileDiscoveryInputs struct {
	Kind        HarnessKind                 `json:"kind"`
	Home        string                      `json:"home"`
	Environment map[string]EnvironmentValue `json:"environment"`
	// CodexProfile is the Codex profile overlay, which can change the
	// provider and model. Absent from the serialized inputs when unset, so
	// the cache keys of profiles without one are unchanged.
	CodexProfile *string `json:"codex_profile,omitempty"`
	// ManagedInstall is the managed installation a probe actually runs. A
	// harness upgrade changes what it advertises, so a cached catalogue from
	// an older pin must never answer for this one.
	ManagedInstall string `json:"managed_install"`
	// InheritedEnvironment is the provider keys the profile takes from the
	// environment Mjolnir runs in, by name: which ones are set decides which
	// providers' models OpenCode offers. Absent when there are none, so the
	// cache keys of other profiles are unchanged.
	InheritedEnvironment []string `json:"inherited_environment,omitempty"`
}

// CapabilitiesKey keys one configured profile's entry in every capability
// cache.
func (p ProfileDiscoveryInputs) CapabilitiesKey(id string, enabled bool) string {
	inputs, err := serdeJSON([]any{"profile-capabilities-v2", id, enabled, p})
	if err != nil {
		panic(fmt.Sprintf("profile capability inputs serialize: %v", err))
	}
	digest := sha256.Sum256(inputs)
	return LowerHex(digest[:])
}

// DefaultContextBytes is the transcript budget a profile without an explicit
// context_window_bytes runs under. It lives beside the setting so compaction
// and the settings screen read one number.
const DefaultContextBytes = 256 * 1024

// GuardianReviewNewestFlash is the guardian_review_model value that picks the
// newest flash model.
const GuardianReviewNewestFlash = "newest-flash"

// GuardianReviewSession is the guardian_review_model value that leaves
// reviews to the session model.
const GuardianReviewSession = "session"

// installationHome compares discovery identities without rewriting the
// user's configured path.
func installationHome(path string) string {
	expanded, err := ExpandLocal(path)
	if err != nil {
		expanded = path
	}
	if canonical, err := filepath.EvalSymlinks(expanded); err == nil {
		if absolute, err := filepath.Abs(canonical); err == nil {
			return absolute
		}
	}
	// Homes that are not present yet still have a stable lexical identity.
	separator := string(filepath.Separator)
	rooted := filepath.IsAbs(expanded) || strings.HasPrefix(expanded, separator)
	volume := filepath.VolumeName(expanded)
	var components []string
	for _, component := range strings.FieldsFunc(expanded[len(volume):], func(r rune) bool {
		return r == '/' || os.IsPathSeparator(uint8(r)) && r < 0x80
	}) {
		switch component {
		case ".":
		case "..":
			if len(components) > 0 && components[len(components)-1] != ".." {
				components = components[:len(components)-1]
			} else if !rooted {
				components = append(components, component)
			}
		default:
			components = append(components, component)
		}
	}
	normalized := strings.Join(components, separator)
	if rooted {
		return volume + separator + normalized
	}
	return volume + normalized
}

// QuotaProvider reads the harness's own provider configuration. Call on a
// background goroutine, never from a UI event or render loop.
func (h HarnessProfile) QuotaProvider() QuotaProvider {
	switch h.Kind {
	// Claude's collector reads Anthropic's fixed OAuth usage endpoint,
	// regardless of the model-request base URL. Bedrock is usage priced and
	// has no subscription allowance to attribute elsewhere.
	case HarnessKindClaude:
		return QuotaProviderAnthropic{}
	case HarnessKindAntigravity:
		return QuotaProviderGoogle{}
	case HarnessKindGrok:
		return QuotaProviderXAI{}
	case HarnessKindKimi:
		return QuotaProviderMoonshot{}
	case HarnessKindDsh:
		return QuotaProviderDeepSeek{}
	case HarnessKindMuse:
		endpoints := MuseQuotaEndpointsOf(h.Environment.Resolved())
		return quotaProviderFromEndpoint("meta", &endpoints.Mint)
	case HarnessKindCodex:
		_, provider := codexQuotaSourceResolve(h)
		return provider
	}
	// OpenCode.
	configuration := map[string]any{}
	for _, name := range []string{"opencode.json", "opencode.jsonc"} {
		text, err := os.ReadFile(filepath.Join(h.Home, name))
		if err != nil {
			continue
		}
		value, ok := opencodeConfiguration(string(text))
		if !ok {
			continue
		}
		if object, ok := value.(map[string]any); ok {
			for key, member := range object {
				configuration[key] = member
			}
		}
	}
	model, _ := configuration["model"].(string)
	id, _, found := strings.Cut(model, "/")
	if !found {
		return QuotaProviderOther{Value: "OpenCode"}
	}
	var endpoint *string
	if providers, ok := configuration["provider"].(map[string]any); ok {
		if provider, ok := providers[id].(map[string]any); ok {
			if options, ok := provider["options"].(map[string]any); ok {
				if baseURL, ok := options["baseURL"].(string); ok {
					endpoint = &baseURL
				}
			}
		}
	}
	return quotaProviderFromEndpoint(id, endpoint)
}

// NewHarnessProfile creates an enabled profile with all optional settings
// unset.
func NewHarnessProfile(kind HarnessKind, home string) HarnessProfile {
	return HarnessProfile{Enabled: true, ThirdPartyModels: true, MjolnirGuardian: true, Kind: kind, Home: home}
}

func (h HarnessProfile) DiscoveryInputs() ProfileDiscoveryInputs {
	environment := make(map[string]EnvironmentValue, len(h.Environment.Sources()))
	for key, value := range h.Environment.Sources() {
		environment[key] = value
	}
	var inherited []string
	for name := range h.Environment.InheritedNames() {
		inherited = append(inherited, name)
	}
	return ProfileDiscoveryInputs{
		Kind:                 h.Kind,
		Home:                 h.Home,
		Environment:          environment,
		CodexProfile:         cloneStringPointer(h.CodexProfile),
		ManagedInstall:       Pin(h.Kind).InstallID,
		InheritedEnvironment: inherited,
	}
}

// CapabilitiesKey is the stable identity of configured capabilities,
// independent of defaults, refreshed credentials and files the harness writes
// inside its home. The managed pin is part of the identity because upgrading
// it changes the models and efforts a probe reports.
func (h HarnessProfile) CapabilitiesKey(id string) string {
	return h.DiscoveryInputs().CapabilitiesKey(id, h.Enabled)
}

// SameInstallation: discovery identifies an installation independently of
// user settings. Each Codex profile overlay in a home is an installation of
// its own.
func (h HarnessProfile) SameInstallation(other HarnessProfile) bool {
	return h.Kind == other.Kind && installationHome(h.Home) == installationHome(other.Home) &&
		equalStringPointers(h.CodexProfile, other.CodexProfile)
}

func equalStringPointers(a, b *string) bool {
	return a == nil && b == nil || a != nil && b != nil && *a == *b
}

func (h HarnessProfile) HomeEnv() string { return h.Kind.HomeEnv() }

func (h HarnessProfile) ExecutionEnforcement(policy ExecutionPolicy) (ExecutionEnforcement, bool) {
	return h.Kind.ResolveExecutionEnforcement(policy, h.ApprovalChoices())
}

// ApprovalChoices is this profile's choices about how its harness realizes a
// policy.
func (h HarnessProfile) ApprovalChoices() ApprovalChoices {
	mode := ClaudePermissionModeAuto
	if h.ClaudePermissionMode != nil {
		mode = *h.ClaudePermissionMode
	}
	return ApprovalChoices{ClaudePermissionMode: mode, MjolnirGuardian: h.MjolnirGuardian}
}

// ConfigureExecutionEnvironment applies the launch environment for policy.
// The controller writes this into new launch configs, and the worker repeats
// it so persisted configs from older releases acquire the same enforcement
// after an upgrade.
func (h HarnessProfile) ConfigureExecutionEnvironment(policy ExecutionPolicy, environment map[string]string) error {
	enforcement, ok := h.ExecutionEnforcement(policy)
	if !ok {
		return nil
	}
	applyExecutionEnforcementEnvironment(enforcement, environment)
	return nil
}

// CodexProvider is the provider named in this profile's Codex configuration,
// if any. Always nil for another harness, or when Codex uses its default.
func (h HarnessProfile) CodexProvider() (*CodexProvider, error) {
	if h.Kind != HarnessKindCodex {
		return nil, nil
	}
	return ReadCodexProvider(h.Home, h.CodexProfile)
}

// ModelService is the service this profile's harness sends model requests
// to.
func (h HarnessProfile) ModelService() (ModelService, error) {
	if service, ok := ModelServiceOfHarness(h.Kind); ok {
		return service, nil
	}
	provider, err := h.CodexProvider()
	if err != nil {
		return nil, err
	}
	if provider == nil {
		return ModelServiceCodexBuiltIn{Value: BuiltInCodexProviderOpenAi}, nil
	}
	switch definition := provider.Definition.(type) {
	case CodexProviderDefinitionBuiltIn:
		return ModelServiceCodexBuiltIn{Value: definition.Provider}, nil
	case CodexProviderDefinitionCustom:
		return ModelServiceCodexCustom{Value: normalizedBaseURL(definition.Provider.BaseURL)}, nil
	}
	panic("codex provider without a definition")
}

// CodexConfig is the Codex configuration this profile runs with, as staged
// into every session home. Nil for another harness, or when there is none.
func (h HarnessProfile) CodexConfig() (*string, error) {
	if h.Kind != HarnessKindCodex {
		return nil, nil
	}
	return EffectiveConfig(h.Home, h.CodexProfile)
}

// DescribeCodexConfig is how this profile's Codex configuration is named in
// messages.
func (h HarnessProfile) DescribeCodexConfig() string {
	return DescribeConfig(h.Home, h.CodexProfile)
}

// AuthScheme is how this profile proves it may talk to its service.
//
// ApiKey means the key is supplied through the named environment variable
// from the profile's `environment` map, so there is no interactive login and
// no credential file to sync or expire. Bedrock uses the AWS credential
// chain; local built-in providers need no login. InlineApiKey is a custom
// provider that writes its key into its own `config.toml`; that file is
// copied into the staged home, so the profile is just as set up as an
// `env_key` one and needs no interactive login.
func (h HarnessProfile) AuthScheme() AuthScheme {
	if h.Kind.KeepsLoginsInDatabase() {
		return AuthSchemeNativeLoginStore{}
	}
	if envKey, ok := h.Kind.APIKeyVariable(); ok {
		return AuthSchemeApiKey{EnvKey: envKey}
	}
	if h.Kind == HarnessKindClaude {
		if auth, ok := ClaudeEnvironmentAuthFrom(h.Environment.Resolved()); ok && auth.Kind == ClaudeEnvironmentAuthBedrock {
			return AuthSchemeAwsCredentialChain{}
		}
	}
	provider, err := h.CodexProvider()
	// An unreadable or malformed home is reported where it is read
	// (EnsureReady, staging, doctor), not silently here.
	if err != nil || provider == nil {
		return AuthSchemeNativeLogin{}
	}
	switch {
	case provider.UsesAWSCredentials():
		return AuthSchemeAwsCredentialChain{}
	case provider.NeedsNoAuthentication():
		return AuthSchemeNoAuthentication{}
	}
	custom := provider.Custom()
	if custom == nil {
		// The built-in OpenAI provider uses Codex's own login.
		return AuthSchemeNativeLogin{}
	}
	// A custom provider names its key either in an environment variable
	// (`env_key`) or inline in its own `config.toml`
	// (`experimental_bearer_token`); either way there is no interactive login
	// and no credential file to sync or expire.
	switch key := custom.Key().(type) {
	case CodexProviderKeyEnvKey:
		return AuthSchemeApiKey{EnvKey: string(key)}
	case CodexProviderKeyInline:
		return AuthSchemeInlineApiKey{}
	}
	return AuthSchemeNativeLogin{}
}

// CodexProviderAPIKey is the API key this Codex profile's custom provider
// authenticates with, from the environment variable it names (`env_key`) or
// the token it writes into its own `config.toml`
// (`experimental_bearer_token`).
//
// False when the profile is not a Codex custom-provider profile, or when it
// names an `env_key` whose variable has no value. EnsureReady reports the
// missing variable before anything stages the profile.
func (h HarnessProfile) CodexProviderAPIKey() (string, bool) {
	provider, err := h.CodexProvider()
	if err != nil || provider == nil {
		return "", false
	}
	return h.codexProviderAPIKeyFrom(*provider)
}

func (h HarnessProfile) codexProviderAPIKeyFrom(provider CodexProvider) (string, bool) {
	custom := provider.Custom()
	if custom == nil {
		return "", false
	}
	switch key := custom.Key().(type) {
	case CodexProviderKeyEnvKey:
		return h.Environment.Get(string(key))
	case CodexProviderKeyInline:
		return string(key), true
	}
	return "", false
}

// AuthenticationMarker is the file inside this profile's home that proves it
// is authenticated. An API-key profile is proven by its Codex `config.toml`,
// whether the key lives in the profile environment or inline in that file. A
// login kept in the harness's database (AuthSchemeNativeLoginStore) has no
// such file in the profile home; this names where a staged home carries it.
func (h HarnessProfile) AuthenticationMarker() string {
	switch h.AuthScheme().(type) {
	case AuthSchemeApiKey:
		// A harness that reads its key from its environment keeps no file
		// that proves it; its kind's marker names where one would be.
		if _, ok := h.Kind.APIKeyVariable(); ok {
			return HarnessAuthenticationMarker(h.Kind, h.Home)
		}
		return filepath.Join(h.Home, "config.toml")
	case AuthSchemeInlineApiKey, AuthSchemeAwsCredentialChain, AuthSchemeNoAuthentication:
		return filepath.Join(h.Home, "config.toml")
	}
	return HarnessAuthenticationMarker(h.Kind, h.Home)
}

// CredentialFreshness: see CredentialFreshness. An API key does not expire or
// refresh, so it orders no copies.
func (h HarnessProfile) CredentialFreshness(bytes []byte) (int64, bool) {
	if _, ok := h.AuthScheme().(AuthSchemeNativeLogin); !ok {
		return 0, false
	}
	return CredentialFreshness(h.Kind, bytes)
}

// CredentialExpiry: see CredentialExpiry. An API key has no expiry.
func (h HarnessProfile) CredentialExpiry(bytes []byte) (int64, bool) {
	if _, ok := h.AuthScheme().(AuthSchemeNativeLogin); !ok {
		return 0, false
	}
	return CredentialExpiry(h.Kind, bytes)
}

func (h HarnessProfile) validate(id string) error {
	if err := ValidateID("profile", id); err != nil {
		return err
	}
	if h.Kind.NestedHome() {
		leaf := h.Kind.ID()
		name := h.Kind.DisplayName()
		if name, _ := pathFileName(h.Home); name != leaf {
			return fmt.Errorf("%s profile %s home must end in /%s (its XDG configuration directory)", name, debugString(id), leaf)
		}
		if h.Environment.ContainsKey("XDG_DATA_HOME") {
			return fmt.Errorf("%s profile %s must not override its managed XDG_DATA_HOME", name, debugString(id))
		}
	}
	if h.Home == "" {
		return fmt.Errorf("profile %s has an empty home path", debugString(id))
	}
	// The entries as written, so one whose reference did not resolve is
	// checked too.
	environment := h.Environment.Sources()
	for key := range environment {
		if strings.TrimSpace(key) == "" || strings.Contains(key, "=") {
			return fmt.Errorf("profile %s contains an invalid environment variable name", debugString(id))
		}
	}
	if _, ok := environment[h.Kind.HomeEnv()]; ok {
		return fmt.Errorf("profile %s must use `home`, not override %s in `environment`", debugString(id), h.Kind.HomeEnv())
	}
	for _, owned := range h.Kind.LaunchOwnedEnvironment() {
		if _, ok := environment[owned[0]]; ok {
			return fmt.Errorf("profile %s must not set %s in `environment`; %s", debugString(id), owned[0], owned[1])
		}
	}
	if h.ContextWindowBytes != nil && *h.ContextWindowBytes < 32*1024 {
		return fmt.Errorf("profile %s: `context_window_bytes` must be at least 32768", debugString(id))
	}
	if name := h.CodexProfile; name != nil {
		if h.Kind != HarnessKindCodex {
			return fmt.Errorf("profile %s: `codex_profile` applies only to a Codex profile", debugString(id))
		}
		if !IsOverlayName(*name) {
			return fmt.Errorf("profile %s: `codex_profile` %s must be a plain name of ASCII letters, digits, '-' or '_', naming <home>/<name>%s",
				debugString(id), debugString(*name), OverlaySuffix)
		}
	}
	if reviewer := h.GuardianReviewModel; reviewer != nil {
		if strings.TrimSpace(*reviewer) == "" {
			return fmt.Errorf("profile %s: `guardian_review_model` must be %s, %s, or a model slug from the provider's catalog",
				debugString(id), debugString(GuardianReviewNewestFlash), debugString(GuardianReviewSession))
		}
		// Whether a Codex profile names a custom provider is up to its home,
		// which EnsureReady reads; another harness never does.
		if h.Kind != HarnessKindCodex {
			return fmt.Errorf("profile %s: `guardian_review_model` applies only to a Codex profile whose config.toml names a custom model provider, because Mjolnir generates the model catalog only for those", debugString(id))
		}
	}
	if h.ClaudePermissionMode != nil && h.Kind != HarnessKindClaude {
		return fmt.Errorf("profile %s: `claude_permission_mode` applies only to a Claude profile", debugString(id))
	}
	if !h.ThirdPartyModels && h.Kind != HarnessKindAntigravity {
		return fmt.Errorf("profile %s: `third_party_models` applies only to an Antigravity profile", debugString(id))
	}
	if h.AWS != nil {
		if err := h.AWS.Validate(id); err != nil {
			return err
		}
	}
	return nil
}

// AntigravityPools is the Antigravity quota pools this profile uses; empty
// for other harnesses.
func (h HarnessProfile) AntigravityPools() []AntigravityQuotaPool {
	if h.Kind != HarnessKindAntigravity {
		return nil
	}
	pools := []AntigravityQuotaPool{}
	for _, pool := range AntigravityQuotaPoolAll {
		if h.ThirdPartyModels || pool != AntigravityQuotaPoolClaude {
			pools = append(pools, pool)
		}
	}
	return pools
}

// ModelOffer is the single interpretation of `models` and
// `third_party_models` that catalogues, pickers and model refusals share.
func (h HarnessProfile) ModelOffer() ModelOffer {
	offer := ModelOffer{Models: append([]string(nil), h.Models...)}
	if h.Kind == HarnessKindAntigravity && !h.ThirdPartyModels {
		offer.ExcludedScopes = []string{AntigravityQuotaPoolClaude.ModelScope()}
	}
	return offer
}

// InheritProviderKey supplies the provider keys this profile's harness reads
// from its environment from the environment Mjolnir runs in, as the
// standalone harness reads them from its own, unless `environment` sets
// them. Workers start harnesses with a cleared environment, so without this
// an exported key never reaches the harness. Done once as the configuration
// is read, exactly when `{ from_env = ... }` entries resolve.
//
// For Codex that is the variable its custom provider names (`env_key`), and
// for a harness without a login its own key variable
// (HarnessKind.APIKeyVariable); EnsureReady reports either when neither
// supplies it. For OpenCode it is every provider key variable OpenCode reads
// and every variable its configuration substitutes as `{env:NAME}`.
func (h *HarnessProfile) InheritProviderKey() {
	// A projection reads no files, and a malformed home is reported by
	// EnsureReady.
	if !resolvesValues() {
		return
	}
	if h.Kind == HarnessKindDsh {
		h.resolveDshKey()
		return
	}
	if envKey, ok := h.Kind.APIKeyVariable(); ok {
		h.Environment.inherit(envKey)
		return
	}
	if h.Kind == HarnessKindOpenCode {
		environment := openCodeEnvironmentRead(h.Home)
		names := make(map[string]struct{}, len(environment.keys)+len(environment.references))
		for name := range environment.keys {
			names[name] = struct{}{}
		}
		for name := range environment.references {
			names[name] = struct{}{}
		}
		for _, name := range sortedKeys(names) {
			h.Environment.inherit(name)
		}
		return
	}
	if provider, err := h.CodexProvider(); err == nil && provider != nil {
		if custom := provider.Custom(); custom != nil && custom.EnvKey != nil {
			h.Environment.inherit(*custom.EnvKey)
		}
	}
}

// resolveDshKey decides the key a DSH profile authenticates with, first
// found wins: the key saved in DSH itself (its home's `.credentials.yaml`),
// the one in the environment Mjolnir started with, the one in the DSH home's
// `.env`, and last the profile's own `environment` entry, such as a secret
// from `secrets.toml`. The first three are DSH's own order, so a session uses
// the key the person's `dsh` uses.
func (h *HarnessProfile) resolveDshKey() {
	envKey, ok := h.Kind.APIKeyVariable()
	if !ok {
		return
	}
	home := h.Home
	// A DSH file Mjolnir cannot use keeps a lower source usable: the session
	// sends only the key, so DSH's own refusal of the file does not stop it.
	// It is the reason when no source has a key.
	var problems []string
	usable := func(key *string, err error) *string {
		if err != nil {
			problems = append(problems, err.Error())
			return nil
		}
		if key == nil || strings.TrimSpace(*key) == "" {
			return nil
		}
		return key
	}
	stored, storedErr := dshStored(home, envKey)
	accountSignedIn := storedErr == nil && stored.AccountSignedIn
	key := usable(stored.APIKey, storedErr)
	if key == nil {
		if value, found := environmentFromProcess(envKey); found {
			key = usable(&value, nil)
		}
	}
	if key == nil {
		key = usable(dshDotenv(home, envKey))
	}
	if len(problems) > 0 {
		slog.Warn("DSH credentials: "+strings.Join(problems, "; "), "home", home)
	}
	if key != nil {
		h.Environment.supply(envKey, *key)
		return
	}
	if h.SuppliesAPIKey(envKey) {
		return
	}
	if accountSignedIn {
		problems = append(problems, fmt.Sprintf(
			"DSH in %s is signed in to a DeepSeek account but holds no API key; dsh-acp authenticates only with an API key, so save one in DSH's Models settings, or export %s and run `mj daemon restart`",
			home, envKey))
	}
	if len(problems) > 0 {
		h.Environment.refuse(envKey, strings.Join(problems, "; "))
	}
}

// OpencodeEnvironmentLogins is the provider keys this OpenCode profile's
// environment supplies, by variable name. OpenCode signs in to a provider
// with any of them without a stored login, and prefers a stored login when
// it has both. Empty for another harness.
func (h HarnessProfile) OpencodeEnvironmentLogins() []string {
	logins := []string{}
	if h.Kind != HarnessKindOpenCode {
		return logins
	}
	// OpenCode tests each variable for truth, so an empty one is unset.
	for _, name := range sortedKeys(openCodeEnvironmentRead(h.Home).keys) {
		if value, ok := h.Environment.Get(name); ok && value != "" {
			logins = append(logins, name)
		}
	}
	return logins
}

// SuppliesAPIKey reports whether this profile's environment gives the API key
// variable envKey a value. A blank value is no key.
func (h HarnessProfile) SuppliesAPIKey(envKey string) bool {
	value, ok := h.Environment.Get(envKey)
	return ok && strings.TrimSpace(value) != ""
}

// EnsureReady reports whether a harness can start from this profile: every
// environment reference resolved, the harness home's own configuration
// readable, the credential that configuration names supplied, and the home's
// instruction file one the harness can receive.
//
// These depend on files and variables outside Mjolnir's config.toml, so they
// are not part of Config.Validate: a profile that fails here is unusable,
// while the configuration, the daemon and every other profile keep working.
// Everything that starts a harness from a profile asks this first, and
// `mj doctor` reports it.
//
// The failure is a precondition refusal: the person fixes it, so it travels
// to the client as the reason rather than as an internal error.
func (h HarnessProfile) EnsureReady(id string) error {
	if err := h.ready(id); err != nil {
		return profileRefusal(id, err)
	}
	return nil
}

func (h HarnessProfile) ready(id string) error {
	if err := h.Environment.EnsureResolved(); err != nil {
		return err
	}
	provider, err := h.CodexProvider()
	if err != nil {
		return err
	}
	var source, envKey string
	needsKey := false
	if key, ok := h.Kind.APIKeyVariable(); ok {
		source, envKey, needsKey = h.Kind.DisplayName(), key, true
	} else if provider != nil {
		if custom := provider.Custom(); custom != nil && custom.EnvKey != nil {
			source, envKey, needsKey = h.DescribeCodexConfig(), *custom.EnvKey, true
		}
	}
	if needsKey && !h.SuppliesAPIKey(envKey) {
		if h.Kind == HarnessKindDsh {
			return fmt.Errorf(
				"DSH authenticates with %[1]s, which is found neither in DSH's own credentials (%[2]s), the environment Mjolnir started with, %[3]s, nor under [profiles.%[4]s.environment]; save a key in DSH, export %[1]s and run `mj daemon restart`, or put it in secrets.toml and add `%[1]s = { from_secret = \"%[1]s\" }` under [profiles.%[4]s.environment]",
				envKey, filepath.Join(h.Home, dshCredentialsFile), filepath.Join(h.Home, dshDotenvFile), id)
		}
		return fmt.Errorf(
			"%[1]s authenticates with %[2]s, which is set neither in the environment Mjolnir started with nor under [profiles.%[3]s.environment]; export %[2]s and run `mj daemon restart`, or put it in secrets.toml and add `%[2]s = { from_secret = \"%[2]s\" }` under [profiles.%[3]s.environment]",
			source, envKey, id)
	}
	if h.GuardianReviewModel != nil && (provider == nil || provider.Custom() == nil) {
		return fmt.Errorf(
			"`guardian_review_model` applies only to a Codex profile whose %s names a custom model provider, because Mjolnir generates the model catalog only for those",
			h.DescribeCodexConfig())
	}
	return h.ensureInstructionsDeliverable(nil)
}

// EnsureStagedInstructionsDeliverable refuses a session whose harness cannot
// receive the instruction file staged for it in staged. That file is the
// profile's own file followed by any notes Mjolnir adds for the session, such
// as the one describing a disposable container, so it can be refused when
// the profile's file alone, which EnsureReady checks, is not. The refusal
// names the profile's file, the one the person edits, and is the same kind
// of refusal.
func (h HarnessProfile) EnsureStagedInstructionsDeliverable(id, staged string) error {
	if err := h.ensureInstructionsDeliverable(&staged); err != nil {
		return profileRefusal(id, err)
	}
	return nil
}

// ensureInstructionsDeliverable: dsh-acp limits each instruction file it reads
// (UndeliverableDshInstructions); every other harness owns its
// instruction-file validation.
func (h HarnessProfile) ensureInstructionsDeliverable(staged *string) error {
	if h.Kind != HarnessKindDsh {
		return nil
	}
	name := h.Kind.AgentInstructionsFile()
	own := filepath.Join(h.Home, name)
	checked := own
	if staged != nil {
		checked = filepath.Join(*staged, name)
	}
	problem, err := FindUndeliverableDshInstructions(checked)
	if err != nil {
		return fmt.Errorf("read %s: %w", checked, err)
	}
	switch problem := problem.(type) {
	case nil:
		return nil
	case UndeliverableDshInstructionsNotRegularFile:
		return fmt.Errorf("%s is not a regular file, but dsh-acp accepts instructions only from a regular AGENTS.md. Replace it with a regular file.", own)
	case UndeliverableDshInstructionsNotUtf8:
		return fmt.Errorf("%s is not UTF-8 text (line %d has the first byte that is not), but dsh-acp accepts only UTF-8 instructions. Save the file as UTF-8.", own, problem.Line)
	case UndeliverableDshInstructionsTooLong:
		limit := uint64(DshACPInstructionsLimit)
		var ownBytes uint64
		if info, err := os.Stat(own); err == nil {
			ownBytes = uint64(info.Size())
		}
		size := fmt.Sprintf("%s is %d bytes", own, problem.Bytes)
		if problem.Bytes > ownBytes {
			size = fmt.Sprintf("%s is %d bytes, and with the %d bytes Mjolnir adds to describe this session's environment, DSH's instructions would be %d bytes",
				own, ownBytes, problem.Bytes-ownBytes, problem.Bytes)
		}
		excess := "1 byte"
		if problem.Bytes-limit != 1 {
			excess = fmt.Sprintf("%d bytes", problem.Bytes-limit)
		}
		return fmt.Errorf("%s, but dsh-acp loads at most %d bytes from each instruction file. Shorten the file by at least %s.", size, limit, excess)
	}
	panic(fmt.Sprintf("unknown undeliverable instructions %T", problem))
}

// profileRefusal is a profile that cannot start, as the reason its client is
// given.
func profileRefusal(id string, err error) error {
	return RefusalPrecondition(fmt.Sprintf("profile %s: %s", debugString(id), err.Error()))
}

// ModelService is the service a profile's harness sends model requests to,
// as far as Mjolnir tells services apart. Its variants are comparable with ==.
//
// A native conversation can continue under another profile only on the same
// service. A Codex thread recorded against OpenAI holds items that only
// OpenAI interprets, such as encrypted reasoning, and names models that
// another provider does not offer.
type ModelService interface {
	isModelService()
}

// ModelServiceHarness is the harness's own service. Mjolnir does not tell
// services apart within a harness other than Codex.
type ModelServiceHarness struct{ Value HarnessKind }

// ModelServiceCodexBuiltIn is one of Codex's built-in providers. A profile
// that names no `model_provider` uses OpenAI.
type ModelServiceCodexBuiltIn struct{ Value BuiltInCodexProvider }

// ModelServiceCodexCustom is a Codex custom provider, identified by its base
// URL. The table name is each profile's own choice, so two profiles can call
// one service by different names.
type ModelServiceCodexCustom struct{ Value string }

func (ModelServiceHarness) isModelService()      {}
func (ModelServiceCodexBuiltIn) isModelService() {}
func (ModelServiceCodexCustom) isModelService()  {}

// ModelServiceOfHarness is the service every profile of kind uses, when the
// harness alone decides it. False for Codex, whose profiles each choose a
// provider.
func ModelServiceOfHarness(kind HarnessKind) (ModelService, bool) {
	if kind == HarnessKindCodex {
		return nil, false
	}
	return ModelServiceHarness{Value: kind}, true
}

// normalizedBaseURL is a base URL in the form a URL parser writes it, without
// a trailing slash, so spelling differences in the host or a final `/` do not
// name a different service. Text that is not a URL is compared as written.
func normalizedBaseURL(baseURL string) string {
	text := baseURL
	if parsed, err := url.Parse(baseURL); err == nil && parsed.Scheme != "" {
		text = parsedURLText(parsed)
	}
	return strings.TrimRight(text, "/")
}

// parsedURLText is a URL in the form Rust's url crate serializes it: the
// scheme and host lowercase, and an empty path written as `/` for a URL with
// a host.
func parsedURLText(parsed *url.URL) string {
	normalized := *parsed
	normalized.Scheme = strings.ToLower(parsed.Scheme)
	normalized.Host = strings.ToLower(parsed.Host)
	if port := parsed.Port(); port == "443" && normalized.Scheme == "https" || port == "80" && normalized.Scheme == "http" {
		normalized.Host = strings.ToLower(parsed.Hostname())
	}
	if normalized.Host != "" && normalized.Path == "" && normalized.Opaque == "" {
		normalized.Path = "/"
	}
	return normalized.String()
}

// AuthScheme is how a profile proves it may talk to its service.
type AuthScheme interface {
	isAuthScheme()
	IsAPIKey() bool
	// UsesNativeLoginFile reports whether this profile uses the harness's own
	// login file.
	UsesNativeLoginFile() bool
	// SyncsLogin reports whether the credential sync exchanges this
	// profile's login with its sessions: a login file both ways, a login
	// database by push.
	SyncsLogin() bool
}

// AuthSchemeNativeLogin: the harness's own login writes a credential file
// into the profile home.
type AuthSchemeNativeLogin struct{}

// AuthSchemeNativeLoginStore: the harness's own login keeps its credentials
// in the harness's own database, beside the session history in the same file
// (HarnessKind.KeepsLoginsInDatabase). Staging exports them into each session
// home, where every launch imports them; the credential sync pushes a changed
// export, and the session reloads to import it. A session's copy is never
// pulled back.
type AuthSchemeNativeLoginStore struct{}

// AuthSchemeApiKey is a long-lived API key supplied through this environment
// variable.
type AuthSchemeApiKey struct{ EnvKey string }

// AuthSchemeInlineApiKey is a long-lived API key written into the profile's
// own harness configuration file, so the staged home carries it.
type AuthSchemeInlineApiKey struct{}

// AuthSchemeAwsCredentialChain: the harness obtains AWS credentials from its
// configured AWS provider chain.
type AuthSchemeAwsCredentialChain struct{}

// AuthSchemeNoAuthentication: a built-in local provider does not require
// credentials.
type AuthSchemeNoAuthentication struct{}

func (AuthSchemeNativeLogin) isAuthScheme()        {}
func (AuthSchemeNativeLoginStore) isAuthScheme()   {}
func (AuthSchemeApiKey) isAuthScheme()             {}
func (AuthSchemeInlineApiKey) isAuthScheme()       {}
func (AuthSchemeAwsCredentialChain) isAuthScheme() {}
func (AuthSchemeNoAuthentication) isAuthScheme()   {}

func (AuthSchemeNativeLogin) IsAPIKey() bool        { return false }
func (AuthSchemeNativeLoginStore) IsAPIKey() bool   { return false }
func (AuthSchemeApiKey) IsAPIKey() bool             { return true }
func (AuthSchemeInlineApiKey) IsAPIKey() bool       { return true }
func (AuthSchemeAwsCredentialChain) IsAPIKey() bool { return false }
func (AuthSchemeNoAuthentication) IsAPIKey() bool   { return false }

func (AuthSchemeNativeLogin) UsesNativeLoginFile() bool        { return true }
func (AuthSchemeNativeLoginStore) UsesNativeLoginFile() bool   { return false }
func (AuthSchemeApiKey) UsesNativeLoginFile() bool             { return false }
func (AuthSchemeInlineApiKey) UsesNativeLoginFile() bool       { return false }
func (AuthSchemeAwsCredentialChain) UsesNativeLoginFile() bool { return false }
func (AuthSchemeNoAuthentication) UsesNativeLoginFile() bool   { return false }

func (AuthSchemeNativeLogin) SyncsLogin() bool        { return true }
func (AuthSchemeNativeLoginStore) SyncsLogin() bool   { return true }
func (AuthSchemeApiKey) SyncsLogin() bool             { return false }
func (AuthSchemeInlineApiKey) SyncsLogin() bool       { return false }
func (AuthSchemeAwsCredentialChain) SyncsLogin() bool { return false }
func (AuthSchemeNoAuthentication) SyncsLogin() bool   { return false }

// opencodeProviderEnvironment is the provider key variables the pinned
// OpenCode reads from its environment: the `env` lists of its bundled
// models.dev catalog and the variables its built-in plugins register.
// `scripts/generate-opencode-provider-environment.py` generates it from
// OpenCode's source when the pin changes.
//
//go:embed assets/opencode-provider-environment.txt
var opencodeProviderEnvironment string

// openCodeEnvironment is what an OpenCode home reads from its environment, as
// OpenCode 2 does.
//
// OpenCode signs in to a provider when one of the provider's key variables is
// set: those the pinned OpenCode names, and those a provider in the home's
// own configuration lists under `env`. Its configuration also substitutes
// `{env:NAME}` anywhere in its text. It reads `opencode.json` and
// `opencode.jsonc` from its configuration directory and skips, with a
// warning, a file that is unreadable or not valid JSONC; so does this.
type openCodeEnvironment struct {
	// keys is the variables OpenCode accepts as a provider's key.
	keys map[string]struct{}
	// references is the variables the configuration substitutes.
	references map[string]struct{}
}

// opencodeConfiguration parses OpenCode's JSONC (comments and trailing
// commas, not JavaScript).
func opencodeConfiguration(text string) (any, bool) {
	standard, err := hujson.Standardize([]byte(text))
	if err != nil {
		return nil, false
	}
	var value any
	if err := json.Unmarshal(standard, &value); err != nil {
		return nil, false
	}
	return value, true
}

func openCodeEnvironmentRead(home string) openCodeEnvironment {
	environment := openCodeEnvironment{keys: map[string]struct{}{}, references: map[string]struct{}{}}
	for _, line := range strings.Split(opencodeProviderEnvironment, "\n") {
		line = strings.TrimSuffix(line, "\r")
		if line != "" && !strings.HasPrefix(line, "#") {
			environment.keys[line] = struct{}{}
		}
	}
	for _, name := range []string{"opencode.json", "opencode.jsonc"} {
		if text, err := os.ReadFile(filepath.Join(home, name)); err == nil {
			environment.addConfiguration(string(text))
		}
	}
	return environment
}

// openCodeEnvironmentReference matches an `{env:NAME}` substitution.
var openCodeEnvironmentReference = regexp.MustCompile(`\{env:([^}]+)\}`)

func (e *openCodeEnvironment) addConfiguration(text string) {
	configuration, ok := opencodeConfiguration(text)
	if !ok {
		return
	}
	for _, reference := range openCodeEnvironmentReference.FindAllStringSubmatch(text, -1) {
		e.references[reference[1]] = struct{}{}
	}
	object, _ := configuration.(map[string]any)
	providers, _ := object["provider"].(map[string]any)
	for _, provider := range providers {
		entry, _ := provider.(map[string]any)
		names, _ := entry["env"].([]any)
		for _, name := range names {
			if name, ok := name.(string); ok {
				e.keys[name] = struct{}{}
			}
		}
	}
}

// CodexCredentialEnvironment is the variables through which Codex takes a
// credential other than its own login file, or sends its requests somewhere
// other than OpenAI.
//
// Read from the versions Mjolnir pins (codex-acp 1.13.2 with codex 0.156.1):
// the bridge's API-key login reads CODEX_API_KEY and then OPENAI_API_KEY; the
// codex binary lists OPENAI_API_KEY, CODEX_API_KEY and CODEX_ACCESS_TOKEN as
// the variables it accepts in place of `auth.json`; and OPENAI_BASE_URL sets
// the address of Codex's built-in OpenAI provider.
var CodexCredentialEnvironment = [4]string{
	"CODEX_ACCESS_TOKEN",
	"CODEX_API_KEY",
	"OPENAI_API_KEY",
	"OPENAI_BASE_URL",
}

// CodexLogin is how a Codex profile that talks to OpenAI itself signs in.
type CodexLogin int

const (
	// CodexLoginChatGpt is a ChatGPT account: an OAuth grant in `auth.json`
	// that Codex refreshes.
	CodexLoginChatGpt CodexLogin = iota
	// CodexLoginApiKey is an OpenAI API key that
	// `codex login --with-api-key` stored in `auth.json`.
	CodexLoginApiKey
)

// CodexLogin is how this profile signs in to OpenAI. False for another
// harness and for providers that do not use Codex's own OpenAI login.
//
// Codex records the choice as `auth_mode` in `auth.json`: "apikey" after
// `codex login --with-api-key` and "chatgpt" after a ChatGPT login. A
// missing file leaves the target free to supply credentials. An unreadable
// or unrecognized file still protects a possible ChatGPT login from
// inherited API credentials.
func (h HarnessProfile) CodexLogin() (CodexLogin, bool) {
	if h.Kind != HarnessKindCodex {
		return 0, false
	}
	if provider, err := h.CodexProvider(); err == nil && provider != nil && !provider.UsesCodexLogin() {
		return 0, false
	}
	bytes, err := os.ReadFile(HarnessAuthenticationMarker(h.Kind, h.Home))
	if errors.Is(err, fs.ErrNotExist) {
		return 0, false
	}
	if err != nil {
		return CodexLoginChatGpt, true
	}
	authMode := ""
	var value any
	if json.Unmarshal(bytes, &value) == nil {
		if object, ok := value.(map[string]any); ok {
			authMode, _ = object["auth_mode"].(string)
		}
	}
	if authMode == "apikey" {
		return CodexLoginApiKey, true
	}
	return CodexLoginChatGpt, true
}

// ExcludeHarnessEnvironment removes from environment every variable this
// profile's harness must never see, and returns the names of all such
// variables, present or not.
//
// A Codex profile that signs in with ChatGPT, or selects a built-in provider
// that does not use OpenAI credentials, must not inherit an API key from its
// environment. The names travel in the launch description as well, because
// the worker adds the target's own login environment later and has to remove
// them from that too.
func (h HarnessProfile) ExcludeHarnessEnvironment(environment map[string]string) []string {
	builtInWithoutOpenAIAuth := false
	if provider, err := h.CodexProvider(); err == nil && provider != nil {
		if builtIn, ok := provider.BuiltIn(); ok && !builtIn.UsesCodexLogin() {
			builtInWithoutOpenAIAuth = true
		}
	}
	login, ok := h.CodexLogin()
	if !(ok && login == CodexLoginChatGpt) && !builtInWithoutOpenAIAuth {
		return []string{}
	}
	names := make([]string, 0, len(CodexCredentialEnvironment))
	for _, name := range CodexCredentialEnvironment {
		delete(environment, name)
		names = append(names, name)
	}
	return names
}

// MissingNativeLoginReason reports whether this profile's native login file
// is missing on disk when it is required to launch a session.
//
// Codex AuthSchemeNativeLogin profiles require `auth.json` at launch unless
// an accepted API-key/token environment variable is supplied.
func (h HarnessProfile) MissingNativeLoginReason(id string) (string, bool) {
	if h.Kind != HarnessKindCodex || !h.AuthScheme().UsesNativeLoginFile() {
		return "", false
	}
	for _, name := range [3]string{"CODEX_ACCESS_TOKEN", "CODEX_API_KEY", "OPENAI_API_KEY"} {
		if h.SuppliesAPIKey(name) {
			return "", false
		}
	}
	if info, err := os.Stat(h.AuthenticationMarker()); err == nil && info.Mode().IsRegular() {
		return "", false
	}
	return fmt.Sprintf("Codex login file auth.json is missing; run `mj login --profile %s`", id), true
}
