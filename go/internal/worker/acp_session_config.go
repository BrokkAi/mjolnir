// Port of mj-worker/src/acp/session_config.rs.

// Session configuration selectors and the execution policy's modes.

package worker

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"slices"
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
	listed := make([]string, 0, len(dropped))
	var current []string
	for _, selector := range dropped {
		listed = append(listed, selector.key+" "+sessionConfigDebugString(selector.value))
		option := core.FindSessionConfigOption(options, selector.key)
		if option == nil || option.Select == nil {
			continue
		}
		current = append(current, selector.key+" "+sessionConfigDebugString(string(option.Select.CurrentValue)))
	}
	message := fmt.Sprintf("Could not restore this session's saved %s.", strings.Join(listed, " and "))
	if len(current) > 0 {
		message += fmt.Sprintf(" The harness currently reports %s.", strings.Join(current, " and "))
	}
	return message
}

// sessionConfigRecoveryRequest is the one form that asks the operator to
// replace every selector this startup had to drop, or nil when the harness
// offers no choice for any of them.
func sessionConfigRecoveryRequest(dropped []sessionConfigSelector, options []schema.SessionConfigOption) *core.ElicitationRequest {
	var fields []core.ElicitationField
	for _, selector := range dropped {
		choices := core.SessionConfigChoices(options, selector.key)
		if len(choices) == 0 {
			continue
		}
		option := core.FindSessionConfigOption(options, selector.key)
		title := selector.key
		if option != nil {
			title = option.Name
		}
		offered := make([]core.ElicitationOption, 0, len(choices))
		for _, choice := range choices {
			offered = append(offered, core.ElicitationOption{
				Value:       choice.Value,
				Title:       choice.Name,
				Description: choice.Description,
			})
		}
		// Offer the currently reported value when it is selectable.
		var preselected *string
		if option != nil && option.Select != nil {
			current := string(option.Select.CurrentValue)
			if core.SelectContains(option, current) {
				preselected = &current
			}
		}
		fields = append(fields, core.ElicitationField{
			ID:       selector.key,
			Title:    title,
			Required: true,
			Kind:     core.ElicitationFieldKindSingleSelect{Options: offered, Default: preselected},
		})
	}
	if len(fields) == 0 {
		return nil
	}
	title := "Choose a replacement"
	description := "Pick what this session should use from now on. Declining keeps the current configuration."
	return &core.ElicitationRequest{
		ID:          core.SessionConfigRecoveryID,
		Title:       &title,
		Message:     droppedSelectorWarning(dropped, options),
		Description: &description,
		Fields:      fields,
	}
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
	request := *pending
	if request == nil || request.ID != elicitationID {
		return nil, false, nil
	}
	if err := request.ValidateResponse(response); err != nil {
		return err, true, nil
	}
	var chosen []sessionConfigSelector
	if response.Action == core.ElicitationResponseAccept {
		// Model before effort: a model change can replace the effort catalogue.
		for _, key := range []string{"model", "effort"} {
			if !slices.ContainsFunc(request.Fields, func(field core.ElicitationField) bool { return field.ID == key }) {
				continue
			}
			if value, ok := response.Content[key].(core.ElicitationValueString); ok && strings.TrimSpace(string(value)) != "" {
				chosen = append(chosen, sessionConfigSelector{key: key, value: string(value)})
			}
		}
	}
	// The same rule the equivalent SetConfig command follows. The question
	// stays pending so the operator can answer it once the turn ends.
	if !idle && len(chosen) > 0 {
		return errors.New("configuration can only be changed while the agent is idle"), true, nil
	}
	action := response.ActionName()
	reply := request.ReplyText(response)
	request.Close()
	*pending = nil
	if err := emitRuntimeEvent(ctx, events, core.RuntimeEventElicitationResolved{
		ElicitationID: elicitationID,
		Action:        action,
		Reply:         &reply,
	}); err != nil {
		return nil, true, err
	}
	var applied, refused []string
	for _, selector := range chosen {
		value, err := applySessionSelector(ctx, connection, sessionID, configOptions, grokModels, spec.Harness, selector.key, selector.value)
		if err != nil {
			refused = append(refused, fmt.Sprintf("%s %s (%s)", selector.key, sessionConfigDebugString(selector.value), err))
			continue
		}
		accepted, unlock := spec.AcceptedConfig.Lock()
		accepted.Remember(selector.key, value, *configOptions)
		unlock()
		// No request id: this answers Hel's own question, so there is no
		// durable relay command to complete. The worker still records the
		// accepted value, which is what stops the next restart from replaying
		// the one the harness dropped.
		if err := emitRuntimeEvent(ctx, events, core.RuntimeEventConfigApplied{
			Key:           selector.key,
			Value:         value,
			ConfigOptions: slices.Clone(*configOptions),
		}); err != nil {
			return nil, true, err
		}
		applied = append(applied, selector.key+" "+sessionConfigDebugString(value))
	}
	message := "This session keeps its current configuration."
	if len(applied) > 0 {
		message = fmt.Sprintf("This session now uses %s.", strings.Join(applied, " and "))
	}
	if len(refused) > 0 {
		message += fmt.Sprintf(" The harness refused %s.", strings.Join(refused, " and "))
	}
	if err := emitRuntimeEvent(ctx, events, core.RuntimeEventWarning{Message: message}); err != nil {
		return nil, true, err
	}
	return nil, true, nil
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
	// Muse advertises its default model as the first choice while reporting
	// an empty current value. Sending that same choice back through its
	// legacy `session/setModel` path is rejected as `invalid_target`; leaving
	// it alone is the only operation needed to select the advertised default.
	if harness == core.HarnessKindMuse && key == "model" && museImplicitDefault(*options, value) {
		return value, nil
	}
	if grokModels != nil && core.GrokHandlesConfigKey(key) {
		if err := core.GrokApplyModelChange(ctx, connection, sessionID, grokModels, key, value); err != nil {
			return "", sessionConfigCallError(err)
		}
		core.GrokMergeConfigOptions(options, grokModels)
		return value, nil
	}
	return setSessionConfig(ctx, connection, sessionID, options, harness, key, value)
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
	option := core.FindSessionConfigOption(options, key)
	if option == nil {
		return "", false, errors.New(core.MissingConfigSelectorRefusal(&harness, key))
	}
	if core.SelectContains(option, value) {
		return value, true, nil
	}
	choices := core.SessionConfigChoices(options, key)
	wanted := strings.TrimSpace(value)
	for _, choice := range choices {
		if sessionConfigEqualIgnoreASCIICase(choice.Value, wanted) || sessionConfigEqualIgnoreASCIICase(choice.Name, wanted) {
			return choice.Value, true, nil
		}
	}
	if harness == core.HarnessKindClaude && key == "model" && wanted != "" {
		return "", false, nil
	}
	return "", false, fmt.Errorf("%s is not an available %s value; %s",
		sessionConfigDebugString(value), key, acceptedValuesSentence(options, key))
}

func acceptedValuesSentence(options []schema.SessionConfigOption, key string) string {
	choices := core.SessionConfigChoices(options, key)
	if len(choices) == 0 {
		return "the agent lists no values"
	}
	listed := make([]string, 0, len(choices))
	for _, choice := range choices {
		if choice.Name == "" || choice.Name == choice.Value {
			listed = append(listed, sessionConfigDebugString(choice.Value))
		} else {
			listed = append(listed, fmt.Sprintf("%s (%s)", sessionConfigDebugString(choice.Value), choice.Name))
		}
	}
	return "choose one of " + strings.Join(listed, ", ")
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
	resolved, isResolved, err := resolveSelectorValue(*options, harness, key, value)
	if err != nil {
		return "", err
	}
	option := core.FindSessionConfigOption(*options, key)
	if option == nil {
		return "", errors.New(core.MissingConfigSelectorRefusal(&harness, key))
	}
	optionID := option.ID
	previous, hasPrevious := selectorCurrentValue(option)
	sent := value
	if isResolved {
		sent = resolved
	}
	var response schema.SetSessionConfigOptionResponse
	if err := connection.Call(ctx, schema.SessionSetConfigOptionMethodName, sessionConfigValueRequest(sessionID, optionID, sent), &response); err != nil {
		err = fmt.Errorf("set session %s to %s: %w", key, value, sessionConfigCallError(err))
		// The harness refused a value it was left to resolve: say what it
		// does accept, as a value refused locally would.
		if !isResolved {
			err = fmt.Errorf("%s is not an available %s value; %s: %w",
				sessionConfigDebugString(value), key, acceptedValuesSentence(*options, key), err)
		}
		return "", err
	}
	// An answer with no configuration at all says nothing about the session.
	// Adopting it would drop every selector the harness advertises, which is
	// worse than a stale value, so the catalogue in hand stays.
	if len(response.ConfigOptions) > 0 {
		*options = response.ConfigOptions
	}
	if !isResolved && landsOnTheDefaultModel(*options, optionID, sent) {
		// claude-agent-acp resolves full model ids, but its fuzzy last resort
		// places text it cannot resolve on the catch-all `default` entry, so
		// the bridge "accepts" a value it never recognized (R4-1). Select the
		// previous model again and refuse the value as before bc7495e4. A
		// full id that genuinely runs the default model is refused too; the
		// listed `default` value selects it.
		if hasPrevious && strings.TrimSpace(previous) != "" && previous != defaultModelValue {
			var restored schema.SetSessionConfigOptionResponse
			if err := connection.Call(ctx, schema.SessionSetConfigOptionMethodName, sessionConfigValueRequest(sessionID, optionID, previous), &restored); err != nil {
				return "", fmt.Errorf("select %s %s again after the bridge placed %s on its default: %w",
					key, previous, sessionConfigDebugString(value), sessionConfigCallError(err))
			}
			if len(restored.ConfigOptions) > 0 {
				*options = restored.ConfigOptions
			}
			fallback := defaultModelValue
			adoptRequestedValue(*options, optionID, &fallback, previous)
		}
		return "", fmt.Errorf("%s is not an available %s value; %s",
			sessionConfigDebugString(value), key, acceptedValuesSentence(*options, key))
	}
	var previousValue *string
	if hasPrevious {
		previousValue = &previous
	}
	adoptRequestedValue(*options, optionID, previousValue, sent)
	// A value the harness resolved itself is recorded as the one it now
	// reports, so a restart replays an advertised value and not the alias.
	if isResolved {
		return resolved, nil
	}
	for i := range *options {
		if (*options)[i].ID == optionID {
			if current, ok := selectorCurrentValue(&(*options)[i]); ok && strings.TrimSpace(current) != "" {
				return current, nil
			}
			break
		}
	}
	return sent, nil
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
	index := slices.IndexFunc(options, func(option schema.SessionConfigOption) bool { return option.ID == optionID })
	if index < 0 {
		return
	}
	applied := &options[index]
	if !core.SelectContains(applied, value) || applied.Select == nil {
		return
	}
	reported := string(applied.Select.CurrentValue)
	if reported != value && (strings.TrimSpace(reported) == "" || (previous != nil && *previous == reported)) {
		// Copy on write: other holders of this catalogue keep what the
		// harness reported to them.
		selected := *applied.Select
		selected.CurrentValue = schema.SessionConfigValueId(value)
		applied.Select = &selected
	}
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
	if option := core.ExecutionModeOption(*configOptions, selector, desired); option != nil {
		optionID := option.ID
		// Legacy session modes mirror only the Mode selector.
		mirrorsLegacyModes := option.Category != nil && *option.Category == schema.SessionConfigOptionCategoryMode
		applied, err := selectConfirmed(ctx, connection, sessionID, harness, optionID, "execution mode", desired, configOptions)
		if err != nil {
			return "", err
		}
		if mirrorsLegacyModes && legacyModes != nil {
			legacyModes.CurrentModeID = schema.SessionModeId(applied)
		}
		return applied, nil
	}
	if legacyModes != nil && slices.ContainsFunc(legacyModes.AvailableModes, func(mode schema.SessionMode) bool {
		return string(mode.ID) == desired
	}) {
		var response schema.SetSessionModeResponse
		if err := connection.Call(ctx, schema.SessionSetModeMethodName, schema.SetSessionModeRequest{
			SessionID: sessionID,
			ModeID:    schema.SessionModeId(desired),
		}, &response); err != nil {
			return "", fmt.Errorf("select required ACP execution mode %s: %w", desired, sessionConfigCallError(err))
		}
		// `session/set_mode` answers with an empty result, so the only
		// acknowledgement this path can check is that the request succeeded.
		// Harnesses that report an effective value expose the mode as a
		// config option, which the branch above verifies.
		legacyModes.CurrentModeID = schema.SessionModeId(desired)
		return desired, nil
	}
	return "", fmt.Errorf("ACP bridge does not expose required execution mode %s", desired)
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
	index := slices.IndexFunc(*configOptions, func(option schema.SessionConfigOption) bool {
		return string(option.ID) == key && core.SelectContains(&option, value)
	})
	if index < 0 {
		return fmt.Errorf("ACP bridge does not expose required %s %s", key, value)
	}
	optionID := (*configOptions)[index].ID
	_, err := selectConfirmed(ctx, connection, sessionID, harness, optionID, key, value, configOptions)
	return err
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
	previous := core.ConfigCurrentValue(*configOptions, string(optionID))
	var response schema.SetSessionConfigOptionResponse
	if err := connection.Call(ctx, schema.SessionSetConfigOptionMethodName, sessionConfigValueRequest(sessionID, optionID, desired), &response); err != nil {
		return "", fmt.Errorf("select required ACP %s %s: %w", label, desired, sessionConfigCallError(err))
	}
	*configOptions = response.ConfigOptions
	// Codex and Kimi answer with the configuration they held before the
	// change (see adoptRequestedValue). Every other harness must confirm the
	// value it now runs, since acknowledging one mode while running in
	// another is unsafe.
	if harness == core.HarnessKindCodex || harness == core.HarnessKindKimi {
		adoptRequestedValue(*configOptions, optionID, previous, desired)
	}
	// A harness can answer the request and still report another value, so
	// the session is only safe to use once it confirms the effective one.
	effective := core.ConfigCurrentValue(*configOptions, string(optionID))
	substitute, hasSubstitute := announcedModeSubstitute(harness, desired)
	if effective != nil && (*effective == desired || (hasSubstitute && *effective == substitute)) {
		return *effective, nil
	}
	reported := "no value"
	if effective != nil {
		reported = sessionConfigDebugString(*effective)
	}
	return "", fmt.Errorf("the harness acknowledged %s %s but reports %s", label, desired, reported)
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
	harnessName := harness.DisplayName()
	var current *string
	if selector != nil {
		current = core.ConfigCurrentValue(configOptions, *selector)
	}
	if current == nil {
		for i := range configOptions {
			category := configOptions[i].Category
			if category != nil && *category == schema.SessionConfigOptionCategoryMode {
				current = core.ConfigCurrentValue(configOptions, string(configOptions[i].ID))
				break
			}
		}
	}
	if current == nil && modes != nil {
		id := string(modes.CurrentModeID)
		current = &id
	}
	kept := "its own mode"
	if current != nil {
		id := *current
		var name *string
		if modes != nil {
			for _, mode := range modes.AvailableModes {
				if string(mode.ID) == id {
					if mode.Name != id {
						modeName := mode.Name
						name = &modeName
					}
					break
				}
			}
		}
		if name != nil {
			kept = fmt.Sprintf("its own mode, %s (%s)", *name, id)
		} else {
			kept = "its own mode, " + id
		}
	}
	return fmt.Sprintf("%s refused execution mode %s for this session (%s), so the session continues in %s.",
		harnessName, desired, refusal, kept)
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
	if enforcement == nil {
		return nil, nil
	}
	var refused *ModeRefusal
	settingKey, _, hasSetting := enforcement.ACPSetting()
	if desired, ok := enforcement.ACPMode(); ok {
		// The value on the selector the policy owns, or the legacy list when
		// the bridge has no such selector. Muse keeps Plan on another
		// selector, so its Plan never stands in for the approval policy.
		var current *string
		owned := enforcement.OwnedConfigKeys(*configOptions)
		selectorIndex := slices.IndexFunc(owned, func(key string) bool { return !hasSetting || key != settingKey })
		if selectorIndex >= 0 {
			current = core.ConfigCurrentValue(*configOptions, owned[selectorIndex])
		} else if modes != nil {
			id := string(modes.CurrentModeID)
			current = &id
		}
		// A bridge whose catalogue omits the policy mode, as Claude's does for
		// Auto on some models, has refused it as plainly as one that answers
		// the request with an error.
		modeSelector := sessionConfigOptionalString(enforcement.ACPModeSelector())
		offered := core.ExecutionModeOption(*configOptions, modeSelector, desired) != nil ||
			(modes != nil && slices.ContainsFunc(modes.AvailableModes, func(mode schema.SessionMode) bool {
				return string(mode.ID) == desired
			}))
		needed := sync == PolicySyncAlways ||
			((current == nil || *current != desired) && (current == nil || *current != "plan"))
		if needed && !offered {
			text := fmt.Sprintf("%s does not offer the %s mode here", harness.DisplayName(), desired)
			refused = &ModeRefusal{error: errors.New(text), text: text, unoffered: true}
		} else if needed {
			if _, err := enforceExecutionMode(ctx, connection, sessionID, harness, modeSelector, desired, configOptions, modes); err != nil {
				text, ok := modeRefusal(err)
				if !ok {
					return nil, err
				}
				refused = &ModeRefusal{error: err, text: text, unoffered: false}
			}
		}
	}
	if key, value, ok := enforcement.ACPSetting(); ok {
		if current := core.ConfigCurrentValue(*configOptions, key); sync == PolicySyncAlways || current == nil || *current != value {
			if err := enforcePolicySetting(ctx, connection, sessionID, harness, key, value, configOptions); err != nil {
				return nil, err
			}
		}
	}
	return refused, nil
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
	var desired string
	var selector *string
	if enforcement != nil {
		desired, _ = enforcement.ACPMode()
		selector = sessionConfigOptionalString(enforcement.ACPModeSelector())
	}
	slog.Warn("the harness refused the execution mode",
		"harness", harness, "mode", desired, "error", refusal.error.Error())
	return refusedModeWarning(harness, selector, desired, refusal.text, modes, configOptions)
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
	if harness != core.HarnessKindClaude {
		return "", false, nil
	}
	if _, err := enforceExecutionMode(ctx, connection, sessionID, harness, nil, planExitDefaultMode, configOptions, modes); err != nil {
		return "", false, err
	}
	return refusedPolicyWarning(harness, enforcement, refusal, modes, *configOptions), true, nil
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
	if _, hasMode := sessionConfigEnforcementMode(enforcement); !hasMode {
		if _, err := enforceExecutionMode(ctx, connection, sessionID, harness, nil, planExitDefaultMode, configOptions, modes); err != nil {
			return "", false, err
		}
	}
	refusal, err := reassertExecutionPolicy(ctx, connection, sessionID, harness, enforcement, configOptions, modes, PolicySyncAlways)
	if err != nil {
		return "", false, err
	}
	if refusal == nil {
		return "", false, nil
	}
	warning, ok, err = takeSafeMode(ctx, connection, sessionID, harness, enforcement, configOptions, modes, refusal)
	if err != nil {
		return "", false, err
	}
	if !ok {
		return "", false, refusal.error
	}
	return warning, true, nil
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
	sync := PolicySyncAlways
	if mode := currentSessionMode(*configOptions, modes); mode != nil && *mode == "plan" {
		sync = PolicySyncIfDrifted
	}
	refusal, err := reassertExecutionPolicy(ctx, connection, sessionID, harness, enforcement, configOptions, modes, sync)
	if err != nil {
		return err
	}
	if refusal == nil {
		return nil
	}
	message, ok, err := takeSafeMode(ctx, connection, sessionID, harness, enforcement, configOptions, modes, refusal)
	if err != nil {
		return err
	}
	if !ok {
		// Without a safe mode, a mode the bridge does not offer at all cannot
		// be worked around: the session would run without its policy.
		if refusal.unoffered {
			return refusal.error
		}
		message = refusedPolicyWarning(harness, enforcement, refusal, modes, *configOptions)
	}
	return emitRuntimeEvent(ctx, events, core.RuntimeEventWarning{Message: message})
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
	if enforcement == nil {
		return true
	}
	policyMode := func(value string) bool {
		mode, ok := enforcement.ACPMode()
		return value == "plan" || (ok && mode == value)
	}
	// The selector the change would reach: setSessionConfig resolves aliases
	// such as `mode` to the bridge's own selector id.
	if key == nil {
		// The legacy mode list mirrors the Mode selector, which carries the
		// policy unless the bridge has the policy's own named selector (Muse
		// images before muse-acp 0.10 do not).
		if _, ok := enforcement.ACPMode(); !ok {
			return true
		}
		if selector, ok := enforcement.ACPModeSelector(); ok && slices.ContainsFunc(options, func(option schema.SessionConfigOption) bool {
			return string(option.ID) == selector
		}) {
			return true
		}
		return policyMode(value)
	}
	resolved := *key
	if option := core.FindSessionConfigOption(options, *key); option != nil {
		resolved = string(option.ID)
	}
	if setting, settingValue, ok := enforcement.ACPSetting(); ok && setting == resolved {
		return settingValue == value
	}
	if slices.Contains(enforcement.OwnedConfigKeys(options), resolved) {
		return policyMode(value)
	}
	return true
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

// sessionConfigCallError shows an ACP refusal as the Rust SDK displays it,
// inside whatever context wraps it.
func sessionConfigCallError(err error) error {
	if rpc, ok := err.(*acp.RPCError); ok {
		return acpDisplayError{err: rpc}
	}
	return err
}

// sessionConfigValueRequest is SetSessionConfigOptionRequest::new with a
// value id.
func sessionConfigValueRequest(sessionID schema.SessionId, optionID schema.SessionConfigId, value string) schema.SetSessionConfigOptionRequest {
	return schema.SetSessionConfigOptionRequest{
		SessionID: sessionID,
		ConfigID:  optionID,
		ValueID:   &schema.SetSessionConfigOptionRequestValueID{Value: schema.SessionConfigValueId(value)},
	}
}

// sessionConfigOptionalString turns a (value, present) pair into Rust's
// Option<&str>.
func sessionConfigOptionalString(value string, ok bool) *string {
	if !ok {
		return nil
	}
	return &value
}

// sessionConfigEnforcementMode is
// `enforcement.and_then(ExecutionEnforcement::acp_mode)`.
func sessionConfigEnforcementMode(enforcement *core.ExecutionEnforcement) (string, bool) {
	if enforcement == nil {
		return "", false
	}
	return enforcement.ACPMode()
}

// sessionConfigEqualIgnoreASCIICase is str::eq_ignore_ascii_case.
func sessionConfigEqualIgnoreASCIICase(a, b string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := 0; i < len(a); i++ {
		x, y := a[i], b[i]
		if 'A' <= x && x <= 'Z' {
			x += 'a' - 'A'
		}
		if 'A' <= y && y <= 'Z' {
			y += 'a' - 'A'
		}
		if x != y {
			return false
		}
	}
	return true
}
