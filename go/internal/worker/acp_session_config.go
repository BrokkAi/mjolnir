// Port of mj-worker/src/acp/session_config.rs.

// Session configuration selectors and the execution policy's modes.

package worker

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"unicode"
	"unicode/utf8"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// sessionConfigSelector is one selector and its value: a stored selector a
// startup had to drop, or one the operator chose (Rust `(&'static str,
// String)`).
type sessionConfigSelector struct {
	key   string
	value string
}

// selectorValueIsOffered reports whether the harness currently lists value
// for key.
func selectorValueIsOffered(options []schema.SessionConfigOption, key, value string) bool {
	option := core.FindSessionConfigOption(options, key)
	return option != nil && core.SelectContains(option, value)
}

func droppedSelectorWarning(dropped []sessionConfigSelector, options []schema.SessionConfigOption) string {
	panic("unported: mj-worker/src/acp/session_config.rs dropped_selector_warning")
}

// sessionConfigRecoveryRequest is the one form that asks the operator to
// replace every selector this startup had to drop, or nil when the harness
// offers no choice for any of them.
func sessionConfigRecoveryRequest(dropped []sessionConfigSelector, options []schema.SessionConfigOption) *core.ElicitationRequest {
	panic("unported: mj-worker/src/acp/session_config.rs session_config_recovery_request")
}

// resolveSessionConfigRecovery answers the recovery question Hel raised for
// itself.
//
// handled is false when the id belongs to the harness instead, which the
// caller forwards to the ACP responder waiting on it. Otherwise answer is
// what the caller reports back to whoever submitted the form: nil when the
// answer was taken, or the refusal (Rust `Option<Result<(), String>>`). err
// is a failure of the session itself.
func resolveSessionConfigRecovery(
	ctx context.Context,
	connection *acp.Connection,
	sessionID schema.SessionId,
	spec *LaunchSpec,
	events chan<- core.RuntimeEvent,
	configOptions *[]schema.SessionConfigOption,
	grokModels *core.GrokModelState,
	pending **CommandLoopQuestion,
	idle bool,
	elicitationID string,
	response *core.ElicitationResponse,
) (answer error, handled bool, err error) {
	panic("unported: mj-worker/src/acp/session_config.rs resolve_session_config_recovery")
}

// applySessionSelector applies one selector change and returns the value the
// session now uses. A nil grokModels is a session without Grok's model state.
func applySessionSelector(
	ctx context.Context,
	connection *acp.Connection,
	sessionID schema.SessionId,
	options *[]schema.SessionConfigOption,
	grokModels *core.GrokModelState,
	harness core.HarnessKind,
	key string,
	value string,
) (string, error) {
	panic("unported: mj-worker/src/acp/session_config.rs apply_session_selector")
}

// resolveSelectorValue is the value to send for a requested selector change;
// ok is false when the harness should resolve the request itself.
//
// A listed value is sent as is. A value that matches a listed value or its
// display name without regard to case selects that value, so `/model` takes
// what the picker shows. Claude's bridge also resolves full model ids such as
// `claude-opus-5-5` to the advertised value that runs them (its
// `resolveModelPreference`), and it advertises no ids of its own to match
// against, so an unlisted Claude model is left to the bridge, which refuses
// what it cannot resolve. Any other unlisted value is refused here with the
// values the harness does accept.
func resolveSelectorValue(options []schema.SessionConfigOption, harness core.HarnessKind, key, value string) (resolved string, ok bool, err error) {
	panic("unported: mj-worker/src/acp/session_config.rs resolve_selector_value")
}

func acceptedValuesSentence(options []schema.SessionConfigOption, key string) string {
	panic("unported: mj-worker/src/acp/session_config.rs accepted_values_sentence")
}

func museImplicitDefault(options []schema.SessionConfigOption, value string) bool {
	option := core.FindSessionConfigOption(options, "model")
	if option == nil || option.Select == nil {
		return false
	}
	choices := core.SessionConfigChoices(options, "model")
	return strings.TrimSpace(string(option.Select.CurrentValue)) == "" &&
		len(choices) > 0 && choices[0].Value == value
}

// setSessionConfig applies one selector change and returns the value the
// session now uses.
func setSessionConfig(
	ctx context.Context,
	connection *acp.Connection,
	sessionID schema.SessionId,
	options *[]schema.SessionConfigOption,
	harness core.HarnessKind,
	key string,
	value string,
) (string, error) {
	panic("unported: mj-worker/src/acp/session_config.rs set_session_config")
}

// defaultModelValue is the value claude-agent-acp advertises for "Default
// (recommended)".
const defaultModelValue = "default"

// landsOnTheDefaultModel reports whether the harness reports the default
// model after it was asked for sent, a value that is not the default. Only
// an unlisted Claude model reaches this check: it is the one kind of value
// left to the bridge.
func landsOnTheDefaultModel(options []schema.SessionConfigOption, optionID schema.SessionConfigId, sent string) bool {
	if strings.EqualFold(strings.TrimSpace(sent), defaultModelValue) {
		return false
	}
	for i := range options {
		if options[i].ID == optionID {
			current, ok := selectorCurrentValue(&options[i])
			return ok && current == defaultModelValue
		}
	}
	return false
}

// selectorCurrentValue is a select option's current value. The Rust
// signature takes the option's SessionConfigKind; acp-go keeps the kind on
// the option itself.
func selectorCurrentValue(option *schema.SessionConfigOption) (string, bool) {
	if option.Select == nil {
		return "", false
	}
	return string(option.Select.CurrentValue), true
}

// adoptRequestedValue records the value a successful set actually applied.
//
// Codex and Kimi answer a successful `session/set_config_option` with the
// configuration they held before the change, so the selector this call just
// set still reports its previous value until the harness refreshes it after
// the next turn. The request succeeded, so the requested value is the
// applied one and the answer has to say so.
//
// A harness that reports no value for the selector at all is corrected the
// same way, for the same reason: it has not named an effective value, and the
// request it just accepted did.
//
// Only those cases are corrected. A harness that reports any other value has
// normalized or replaced the request, so its answer stands and the mismatch
// stays visible. Every other selector keeps what the harness reported too,
// because a model change can legitimately reset the rest of the
// configuration. A nil previous is no previous value.
func adoptRequestedValue(options []schema.SessionConfigOption, optionID schema.SessionConfigId, previous *string, value string) {
	panic("unported: mj-worker/src/acp/session_config.rs adopt_requested_value")
}

// announcedModeSubstitute is the mode a harness puts a session in instead of
// desired when the model the session runs cannot use desired, and which it
// announces itself.
//
// claude-agent-acp 0.81.0 (`dist/session-mode.js`, `AUTO_MODE_FALLBACK`)
// answers a request for Auto on a model without Auto mode with Accept edits
// and its "Auto mode unavailable" notice. It does the same when a model
// change or a reloaded session's model rules Auto out, which Mjolnir already
// accepted; this lets a mode request that meets the same rule succeed too.
func announcedModeSubstitute(harness core.HarnessKind, desired string) (string, bool) {
	if harness == core.HarnessKindClaude && desired == "auto" {
		return "acceptEdits", true
	}
	return "", false
}

// enforceExecutionMode selects desired and returns the mode the harness
// confirmed. A nil selector is the policy's default selector; a nil
// legacyModes is a bridge without legacy session modes.
func enforceExecutionMode(
	ctx context.Context,
	connection *acp.Connection,
	sessionID schema.SessionId,
	harness core.HarnessKind,
	selector *string,
	desired string,
	configOptions *[]schema.SessionConfigOption,
	legacyModes *schema.SessionModeState,
) (string, error) {
	panic("unported: mj-worker/src/acp/session_config.rs enforce_execution_mode")
}

// sessionEnforcement is the ACP enforcement for this launch, based on
// harness and target policy; nil when there is none.
func sessionEnforcement(spec *LaunchSpec) *core.ExecutionEnforcement {
	enforcement, ok := spec.Harness.ResolveExecutionEnforcement(spec.ExecutionPolicy, spec.ApprovalChoices)
	if !ok {
		return nil
	}
	return &enforcement
}

// enforcePolicySetting selects a config option an execution policy requires
// besides its mode, such as Muse's auto-review. A bridge without it cannot
// realize the policy.
func enforcePolicySetting(
	ctx context.Context,
	connection *acp.Connection,
	sessionID schema.SessionId,
	harness core.HarnessKind,
	key string,
	value string,
	configOptions *[]schema.SessionConfigOption,
) error {
	panic("unported: mj-worker/src/acp/session_config.rs enforce_policy_setting")
}

// selectConfirmed selects desired on optionID and returns the value the
// harness then reports, refusing any other.
func selectConfirmed(
	ctx context.Context,
	connection *acp.Connection,
	sessionID schema.SessionId,
	harness core.HarnessKind,
	optionID schema.SessionConfigId,
	label string,
	desired string,
	configOptions *[]schema.SessionConfigOption,
) (string, error) {
	panic("unported: mj-worker/src/acp/session_config.rs select_confirmed")
}

// modeRefusal is the harness's own words when it answered a mode request
// with an error; ok is false when the failure was not an answer from the
// harness: a mode it does not list, or an acknowledged request that left
// another mode.
func modeRefusal(err error) (string, bool) {
	var refusal *acp.RPCError
	if !errors.As(err, &refusal) {
		return "", false
	}
	text := refusal.Message
	var data map[string]json.RawMessage
	if json.Unmarshal(refusal.Data, &data) == nil {
		member, ok := data["details"]
		if !ok {
			member, ok = data["message"]
		}
		var words string
		if ok && json.Unmarshal(member, &words) == nil && member[0] == '"' {
			text = words
		}
	}
	if text = strings.Join(strings.Fields(text), " "); text == "" {
		return refusal.Message, true
	}
	return text, true
}

// refusedModeWarning is the one line a session gets when its harness refused
// the policy's mode for a new session, naming the mode the session is left
// in. A nil selector is the policy's default selector; a nil modes is a
// bridge without legacy session modes.
func refusedModeWarning(
	harness core.HarnessKind,
	selector *string,
	desired string,
	refusal string,
	modes *schema.SessionModeState,
	configOptions []schema.SessionConfigOption,
) string {
	panic("unported: mj-worker/src/acp/session_config.rs refused_mode_warning")
}

// PolicySync is how reassertExecutionPolicy treats the cached mode.
type PolicySync int

const (
	// PolicySyncAlways requests the policy values even when the cache already
	// shows them: the cache can predate a permission response's mode change.
	PolicySyncAlways PolicySync = iota
	// PolicySyncIfDrifted requests them only when the cache shows another
	// value, leaving Plan on.
	PolicySyncIfDrifted
)

// ModeRefusal is a harness's answer refusing the policy mode, as modeRefusal
// reads it.
type ModeRefusal struct {
	error error
	text  string
	// unoffered: the bridge does not offer the mode at all, as opposed to
	// refusing a request for it. Only Claude has a safe mode to take instead.
	unoffered bool
}

// reassertExecutionPolicy reconciles the bridge with the launch policy at a
// command boundary. Plan controls may temporarily select Plan; leaving it
// always returns to policy. A refused policy mode is the answer, not an
// error, so the caller decides what the session does instead; the policy
// setting is selected either way, and any other failure, the setting's
// included, is an error. A nil enforcement is a launch without one; a nil
// result is no refusal.
func reassertExecutionPolicy(
	ctx context.Context,
	connection *acp.Connection,
	sessionID schema.SessionId,
	harness core.HarnessKind,
	enforcement *core.ExecutionEnforcement,
	configOptions *[]schema.SessionConfigOption,
	modes *schema.SessionModeState,
	sync PolicySync,
) (*ModeRefusal, error) {
	panic("unported: mj-worker/src/acp/session_config.rs reassert_execution_policy")
}

// refusedPolicyWarning is the warning a session gets when its harness
// refused the policy mode, naming the mode the session is left in.
func refusedPolicyWarning(
	harness core.HarnessKind,
	enforcement *core.ExecutionEnforcement,
	refusal *ModeRefusal,
	modes *schema.SessionModeState,
	configOptions []schema.SessionConfigOption,
) string {
	panic("unported: mj-worker/src/acp/session_config.rs refused_policy_warning")
}

// planExitDefaultMode is the mode Claude and Kimi leave Plan for when the
// policy sets no mode, and Claude's when it refuses the policy's: it asks
// before every change, so it is never looser than the policy, and Plan can
// always be left.
const planExitDefaultMode = "default"

// takeSafeMode answers a refused policy mode. Claude takes its default mode,
// which asks before every change, so whatever mode the session held (a
// restored bypassPermissions included) never outlives the refusal. Returns
// the warning to show, or ok false for a harness with no such safe mode,
// whose caller decides.
func takeSafeMode(
	ctx context.Context,
	connection *acp.Connection,
	sessionID schema.SessionId,
	harness core.HarnessKind,
	enforcement *core.ExecutionEnforcement,
	configOptions *[]schema.SessionConfigOption,
	modes *schema.SessionModeState,
	refusal *ModeRefusal,
) (warning string, ok bool, err error) {
	panic("unported: mj-worker/src/acp/session_config.rs take_safe_mode")
}

// leavePlanForPolicy leaves Plan for the policy mode, returning the warning
// to show when the harness refused it and the session took Claude's default
// mode instead. A policy that sets no mode leaves Plan for the harness
// default.
func leavePlanForPolicy(
	ctx context.Context,
	connection *acp.Connection,
	sessionID schema.SessionId,
	harness core.HarnessKind,
	enforcement *core.ExecutionEnforcement,
	configOptions *[]schema.SessionConfigOption,
	modes *schema.SessionModeState,
) (warning string, ok bool, err error) {
	panic("unported: mj-worker/src/acp/session_config.rs leave_plan_for_policy")
}

// reconcileExecutionPolicy reasserts the policy after the session's model
// moved. A harness that refuses the policy mode stays usable with a warning
// instead of ending: Claude takes its default mode, another harness keeps
// its own.
//
// A model change can move the bridge's mode, and its answer need not say so
// (a legacy mode list, or an empty catalogue the worker keeps its own copy
// over), so the policy is requested outright unless Plan is on.
func reconcileExecutionPolicy(
	ctx context.Context,
	connection *acp.Connection,
	sessionID schema.SessionId,
	harness core.HarnessKind,
	enforcement *core.ExecutionEnforcement,
	configOptions *[]schema.SessionConfigOption,
	modes *schema.SessionModeState,
	events chan<- core.RuntimeEvent,
) error {
	panic("unported: mj-worker/src/acp/session_config.rs reconcile_execution_policy")
}

// sessionModel is the model the session reports, which decides whether the
// bridge allows the policy mode (Claude's Auto).
func sessionModel(options []schema.SessionConfigOption) (string, bool) {
	for i := range options {
		category := options[i].Category
		if category != nil && *category == schema.SessionConfigOptionCategoryModel {
			if current := core.ConfigCurrentValue(options, string(options[i].ID)); current != nil {
				return *current, true
			}
			break
		}
	}
	if current := core.ConfigCurrentValue(options, "model"); current != nil {
		return *current, true
	}
	return "", false
}

// policyPermits reports whether a mode change from outside the worker may
// select value on the selector key (nil for the legacy session-mode list).
// The policy owns its selectors: only Plan and the policy's own values pass,
// so no caller, an older controller included, can move a session off its
// policy.
func policyPermits(enforcement *core.ExecutionEnforcement, options []schema.SessionConfigOption, key *string, value string) bool {
	panic("unported: mj-worker/src/acp/session_config.rs policy_permits")
}

// policyOwnedMode is the refusal for a mode change policyPermits turns down.
const policyOwnedMode = "The target's permission policy sets this setting. Use plan mode to enter or leave Plan."

// sessionConfigDebugString renders text as Rust's {:?} does for a str.
func sessionConfigDebugString(text string) string {
	var b strings.Builder
	b.WriteByte('"')
	for i, r := range text {
		switch {
		case r == '\t':
			b.WriteString(`\t`)
		case r == '\r':
			b.WriteString(`\r`)
		case r == '\n':
			b.WriteString(`\n`)
		case r == '\\':
			b.WriteString(`\\`)
		case r == '"':
			b.WriteString(`\"`)
		case r == 0:
			b.WriteString(`\0`)
		case r == utf8.RuneError && !strings.HasPrefix(text[i:], "�"):
			// Invalid UTF-8 cannot occur in a Rust str; keep the byte visible.
			fmt.Fprintf(&b, `\u{%x}`, text[i])
		case !unicode.IsGraphic(r) || unicode.In(r, unicode.Mn, unicode.Me):
			// Rust escapes non-printable and grapheme-extending characters.
			fmt.Fprintf(&b, `\u{%x}`, r)
		default:
			b.WriteRune(r)
		}
	}
	b.WriteByte('"')
	return b.String()
}
