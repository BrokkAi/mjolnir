// Port of mj-core/src/subagent.rs.
//
// Durable contracts for Mjolnir-managed child-agent sessions.

package core

import (
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"slices"
	"strconv"
	"strings"
	"unicode"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// SubagentPolicyMode is the "mode" tag of a SubagentPolicy.
type SubagentPolicyMode string

const (
	SubagentPolicyNative  SubagentPolicyMode = "native"
	SubagentPolicyMjolnir SubagentPolicyMode = "mjolnir"
	SubagentPolicyNone    SubagentPolicyMode = "none"
)

// SubagentPolicy is a session's delegation policy, optionally overriding
// global task choices. One struct with its "mode" tag: the choices belong to
// mjolnir only. The zero value is the default, native.
type SubagentPolicy struct {
	Mode            SubagentPolicyMode
	Straightforward *SubagentChoice
	Subtle          *SubagentChoice
}

func (p SubagentPolicy) MarshalJSON() ([]byte, error) {
	if p.Mode == SubagentPolicyMjolnir {
		return transcriptMarshalTagged("mode", string(p.Mode), struct {
			Straightforward *SubagentChoice `json:"straightforward,omitempty"`
			Subtle          *SubagentChoice `json:"subtle,omitempty"`
		}{p.Straightforward, p.Subtle})
	}
	if p.Mode == "" {
		return transcriptMarshalTagged("mode", string(SubagentPolicyNative), nil)
	}
	return transcriptMarshalTagged("mode", string(p.Mode), nil)
}

// UnmarshalJSON reads the current form and the legacy ones: the
// all_models/single_model mode, and the single flattened choice b4374cb29
// briefly stored for both task types.
func (p *SubagentPolicy) UnmarshalJSON(data []byte) error {
	tag, err := transcriptTag(data, "mode", "SubagentPolicy")
	if err != nil {
		return err
	}
	switch tag {
	case "native":
		*p = SubagentPolicy{Mode: SubagentPolicyNative}
	case "none":
		*p = SubagentPolicy{Mode: SubagentPolicyNone}
	case "all_models", "single_model":
		*p = SubagentPolicy{Mode: SubagentPolicyMjolnir}
	case "mjolnir":
		var wire struct {
			Straightforward *SubagentChoice `json:"straightforward"`
			Subtle          *SubagentChoice `json:"subtle"`
			Model           *string         `json:"model"`
			Effort          *string         `json:"effort"`
		}
		if err := transcriptStrictDecode(data, &wire, false); err != nil {
			return err
		}
		adaptive := func(effort *string) bool { return effort != nil && *effort == "adaptive" }
		if adaptive(wire.Effort) ||
			(wire.Straightforward != nil && adaptive(wire.Straightforward.Effort)) ||
			(wire.Subtle != nil && adaptive(wire.Subtle.Effort)) {
			return errors.New("adaptive sub-agent effort is not supported")
		}
		if wire.Model == nil && wire.Effort != nil {
			return errors.New("sub-agent effort requires a model override")
		}
		policy := SubagentPolicy{Mode: SubagentPolicyMjolnir, Straightforward: wire.Straightforward, Subtle: wire.Subtle}
		if wire.Model != nil {
			if policy.Straightforward == nil {
				policy.Straightforward = &SubagentChoice{Model: *wire.Model, Effort: wire.Effort}
			}
			if policy.Subtle == nil {
				policy.Subtle = &SubagentChoice{Model: *wire.Model, Effort: wire.Effort}
			}
		}
		*p = policy
	default:
		return transcriptUnknownVariant(tag, "native", "mjolnir", "all_models", "single_model", "none")
	}
	return nil
}

type SubagentTaskType string

const (
	SubagentTaskTypeStraightforward SubagentTaskType = "straightforward"
	SubagentTaskTypeSubtle          SubagentTaskType = "subtle"
	SubagentTaskTypeOracle          SubagentTaskType = "oracle"
)

func (t SubagentTaskType) ID() string {
	if t == "" {
		return string(SubagentTaskTypeStraightforward)
	}
	return string(t)
}

func (p *SubagentPolicy) IsNative() bool {
	return p.Mode == SubagentPolicyNative || p.Mode == ""
}

func (p *SubagentPolicy) UsesMjolnir() bool { return p.Mode == SubagentPolicyMjolnir }

func (p *SubagentPolicy) OverrideChoice(taskType SubagentTaskType) *SubagentChoice {
	if p.Mode != SubagentPolicyMjolnir {
		return nil
	}
	switch taskType {
	case SubagentTaskTypeStraightforward, "":
		return p.Straightforward
	case SubagentTaskTypeSubtle:
		return p.Subtle
	}
	return nil
}

func (p *SubagentPolicy) SuppressesNative() bool { return !p.IsNative() }

func (p *SubagentPolicy) ParentRole() (SubagentMcpRole, bool) {
	if p.UsesMjolnir() {
		return SubagentMcpRoleParent, true
	}
	return "", false
}

// SubagentPolicyDefaultFor is the policy a new session gets when its
// creator names none: its harness's own delegation, or None for a harness
// that has none.
func SubagentPolicyDefaultFor(harness HarnessKind) SubagentPolicy {
	native := SubagentPolicy{Mode: SubagentPolicyNative}
	return native.AsChoiceFor(harness)
}

// AsChoiceFor is this policy as the delegation choice shows it for harness.
// A harness without sub-agents of its own reads Native as None, because
// that is what Native does there, so a session stored with Native keeps
// working.
func (p *SubagentPolicy) AsChoiceFor(harness HarnessKind) SubagentPolicy {
	if p.IsNative() && !harness.HasNativeSubagents() {
		return SubagentPolicy{Mode: SubagentPolicyNone}
	}
	return *p
}

// UpdatesRememberedChoice reports whether this harness can remember the
// selected policy for its profile.
func (p *SubagentPolicy) UpdatesRememberedChoice(harness HarnessKind) bool {
	return harness.SupportsDelegationTools()
}

func (p *SubagentPolicy) ForLaunch(harness HarnessKind, isChild bool) SubagentPolicy {
	switch {
	case !harness.SupportsDelegationTools():
		return SubagentPolicy{Mode: SubagentPolicyNative}
	case isChild:
		return SubagentPolicy{Mode: SubagentPolicyNone}
	}
	return *p
}

// DeserializeOptionalPolicy is upgrade compatibility for persisted records
// and internal handoff only: a legacy boolean (true is mjolnir, false is
// native) or a policy; null is none. Public creation APIs decode the policy
// directly and reject booleans.
func DeserializeOptionalPolicy(data []byte) (*SubagentPolicy, error) {
	trimmed := strings.TrimSpace(string(data))
	switch trimmed {
	case "null":
		return nil, nil
	case "true":
		return &SubagentPolicy{Mode: SubagentPolicyMjolnir}, nil
	case "false":
		return &SubagentPolicy{Mode: SubagentPolicyNative}, nil
	}
	var policy SubagentPolicy
	if err := json.Unmarshal(data, &policy); err != nil {
		return nil, errors.New("data did not match any variant of untagged enum Stored")
	}
	return &policy, nil
}

func DeserializeLaunchPolicy(data []byte) (SubagentPolicy, error) {
	policy, err := DeserializeOptionalPolicy(data)
	if err != nil || policy == nil {
		return SubagentPolicy{Mode: SubagentPolicyNative}, err
	}
	return *policy, nil
}

// ParseStoredPolicy reads a policy JSON column using the same legacy decoder
// as durable records.
func ParseStoredPolicy(text string) (SubagentPolicy, error) {
	if !json.Valid([]byte(text)) {
		var probe any
		return SubagentPolicy{}, json.Unmarshal([]byte(text), &probe)
	}
	return DeserializeLaunchPolicy([]byte(text))
}

// DeserializePolicyMap reads the remembered per-profile choices while
// accepting the old scalar field (stored under the empty profile name) and
// legacy boolean policy values used by durable snapshots.
func DeserializePolicyMap(data []byte) (map[string]SubagentPolicy, error) {
	value, err := jsonvalue.Parse(data)
	if err != nil {
		return nil, err
	}
	policies := map[string]SubagentPolicy{}
	object, isObject := value.(jsonvalue.Object)
	if _, hasMode := transcriptGetStr(value, "mode"); isObject && !hasMode {
		for _, member := range object {
			encoded, err := jsonvalue.Marshal(member.Value)
			if err != nil {
				return nil, err
			}
			policy, err := ParseStoredPolicy(string(encoded))
			if err != nil {
				return nil, err
			}
			policies[member.Key] = policy
		}
		return policies, nil
	}
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		return nil, err
	}
	policy, err := ParseStoredPolicy(string(encoded))
	if err != nil {
		return nil, err
	}
	policies[""] = policy
	return policies, nil
}

// ChoiceUnavailableCode is the refusal code for an unavailable sub-agent
// choice. A client picks its own remedy from it: the daemon's message names
// no surface, and only the client knows whether the person has a Settings
// screen or command-line flags to change the choice with.
const ChoiceUnavailableCode = "subagent_choice_unavailable"

// eligibilityNote is how a choice is matched to a profile. Surface-neutral,
// so it can follow any message from SubagentOptions.Validate.
const eligibilityNote = "Mjolnir chooses an enabled profile offering that selection by remaining quota."

// SubagentOptions are choices before a parent session exists, using the
// same eligibility as spawn.
type SubagentOptions struct {
	Models      []SessionConfigChoice `json:"models"`
	Efforts     []SessionConfigChoice `json:"efforts"`
	Unavailable []string              `json:"unavailable"`
}

func (o *SubagentOptions) UnmarshalJSON(data []byte) error {
	type plain SubagentOptions
	return transcriptStrictDecode(data, (*plain)(o), false)
}

// Validate checks choice against the offered models and efforts. The error
// text is shown as is.
func (o *SubagentOptions) Validate(choice *SubagentChoice) error {
	model := choice.Model
	if _, ok := ModelCatalogResolve(o.Models, model); strings.TrimSpace(model) == "" || !ok {
		return fmt.Errorf("Selected subagent model %s is unavailable.%s %s",
			debugString(model), choiceList(" Available models", o.Models), eligibilityNote)
	}
	if choice.Effort == nil {
		return nil
	}
	if effort := *choice.Effort; effort != "adaptive" {
		for _, offered := range o.Efforts {
			if offered.Value == effort {
				return nil
			}
		}
	}
	return fmt.Errorf("Select an available effort for subagent model %s.%s %s",
		debugString(model), choiceList(" Available efforts", o.Efforts), eligibilityNote)
}

// choiceList is "<label>: a, b." for a non-empty list of choices, otherwise
// nothing.
func choiceList(label string, choices []SessionConfigChoice) string {
	if len(choices) == 0 {
		return ""
	}
	values := make([]string, len(choices))
	for i, choice := range choices {
		values[i] = choice.Value
	}
	return label + ": " + strings.Join(values, ", ") + "."
}

// twoTaskTypes is how the delegation policy describes spawn choices when
// both task types are offered.
const twoTaskTypes = "Mjolnir supplies the configured\n" +
	"child model and effort for each task type. Set `task_type` to `straightforward`\n" +
	"for work with a clear method, including locating code, running commands or\n" +
	"test suites, and implementing a settled design. Use `subtle` for ambiguous\n" +
	"diagnosis, design choices, and work with a high risk of subtle bugs. A subtle\n" +
	"task gets a smarter, more expensive agent. Judge difficulty, not brief length."

// oneTaskType is the same description when only the straightforward type is
// configured. There is no type to choose, so the parent delegates
// straightforward work and keeps work that needs judgment.
const oneTaskType = "Mjolnir supplies the configured\n" +
	"child model and effort. Delegate straightforward work: work with a clear\n" +
	"method, including locating code, running commands or test suites, and\n" +
	"implementing a settled design. Do work that needs judgment yourself, including\n" +
	"ambiguous diagnosis, design choices, and changes with a high risk of subtle\n" +
	"bugs. Judge difficulty, not brief length."

//go:embed assets/subagent-delegation.md
var subagentDelegationTemplate string

// DelegationPolicy is the parent's delegation policy. offersSubtle is false
// when only the straightforward task type is configured.
func DelegationPolicy(limit int, offersSubtle bool) string {
	taskTypes := oneTaskType
	if offersSubtle {
		taskTypes = twoTaskTypes
	}
	policy := strings.ReplaceAll(subagentDelegationTemplate, "$TASK_TYPES", taskTypes)
	return strings.ReplaceAll(policy, "$N", strconv.Itoa(limit))
}

// SubagentFinishIdentityKind is the "kind" tag of a SubagentFinishIdentity.
type SubagentFinishIdentityKind string

const (
	SubagentFinishIdentityTurn     SubagentFinishIdentityKind = "turn"
	SubagentFinishIdentityTerminal SubagentFinishIdentityKind = "terminal"
)

// SubagentFinishIdentity is the identity of one finish already sent to the
// parent in a notification.
//
// Turn spans distinguish repeated finishes of a child resumed with
// send_message. Terminal failures without a turn span use their state,
// detail, and the last completed turn ordinal. Session metadata such as a
// title or updated timestamp does not change the identity.
//
// One struct with its "kind" tag: StartPosition and CompletedOrdinal belong
// to turn, Detail and LastCompletedOrdinal to terminal; both have State.
type SubagentFinishIdentity struct {
	Kind             SubagentFinishIdentityKind
	StartPosition    uint64
	CompletedOrdinal uint64
	State            string
	Detail           *string
	// LastCompletedOrdinal separates terminal failures after distinct runs.
	// Old records stored an updated_at string here; it is read as nil so
	// those records remain readable without treating renames as new
	// finishes.
	LastCompletedOrdinal *uint64
}

func (i SubagentFinishIdentity) MarshalJSON() ([]byte, error) {
	if i.Kind == SubagentFinishIdentityTurn {
		return transcriptMarshalTagged("kind", string(i.Kind), struct {
			StartPosition    uint64 `json:"start_position"`
			CompletedOrdinal uint64 `json:"completed_ordinal"`
			State            string `json:"state"`
		}{i.StartPosition, i.CompletedOrdinal, i.State})
	}
	return transcriptMarshalTagged("kind", string(i.Kind), struct {
		State                string  `json:"state"`
		Detail               *string `json:"detail,omitempty"`
		LastCompletedOrdinal *uint64 `json:"last_completed_ordinal,omitempty"`
	}{i.State, i.Detail, i.LastCompletedOrdinal})
}

func (i *SubagentFinishIdentity) UnmarshalJSON(data []byte) error {
	tag, err := transcriptTag(data, "kind", "SubagentFinishIdentity")
	if err != nil {
		return err
	}
	switch kind := SubagentFinishIdentityKind(tag); kind {
	case SubagentFinishIdentityTurn:
		var v struct {
			StartPosition    uint64 `json:"start_position"`
			CompletedOrdinal uint64 `json:"completed_ordinal"`
			State            string `json:"state"`
		}
		if err := transcriptStrictDecode(data, &v, true, "kind"); err != nil {
			return err
		}
		*i = SubagentFinishIdentity{Kind: kind, StartPosition: v.StartPosition,
			CompletedOrdinal: v.CompletedOrdinal, State: v.State}
		return nil
	case SubagentFinishIdentityTerminal:
		var v struct {
			State                string          `json:"state"`
			Detail               *string         `json:"detail,omitempty"`
			LastCompletedOrdinal json.RawMessage `json:"last_completed_ordinal,omitempty"`
			UpdatedAt            json.RawMessage `json:"updated_at,omitempty"`
		}
		if err := transcriptStrictDecode(data, &v, true, "kind"); err != nil {
			return err
		}
		raw := v.LastCompletedOrdinal
		if raw == nil {
			raw = v.UpdatedAt
		}
		*i = SubagentFinishIdentity{Kind: kind, State: v.State, Detail: v.Detail,
			LastCompletedOrdinal: deserializeTerminalLastCompletedOrdinal(raw)}
		return nil
	}
	return transcriptUnknownVariant(tag, "turn", "terminal")
}

// Covers reports whether a report with this identity was made after the
// turn that completed at ordinal, or for a later one: the parent has been
// told of that turn's ending.
func (i *SubagentFinishIdentity) Covers(ordinal uint64) bool {
	completed, ok := i.CompletedOrdinalOf()
	return ok && completed >= ordinal
}

// CompletedOrdinalOf is the ordinal of the turn this finish completed,
// whatever the kind of identity: a terminal one carries the last completed
// turn, and has none for a child that never completed a turn. The one place
// that says which turn a finish's recorded facts belong to. (Rust
// completed_ordinal; renamed because CompletedOrdinal is a field.)
func (i *SubagentFinishIdentity) CompletedOrdinalOf() (uint64, bool) {
	if i.Kind == SubagentFinishIdentityTurn {
		return i.CompletedOrdinal, true
	}
	if i.LastCompletedOrdinal == nil {
		return 0, false
	}
	return *i.LastCompletedOrdinal, true
}

// deserializeTerminalLastCompletedOrdinal reads any JSON value, keeping
// only a u64.
func deserializeTerminalLastCompletedOrdinal(raw json.RawMessage) *uint64 {
	if raw == nil {
		return nil
	}
	var ordinal uint64
	if json.Unmarshal(raw, &ordinal) != nil {
		return nil
	}
	return &ordinal
}

// ReportNotificationNextAction is guidance for parents whose older workers
// still call a removed wait tool.
const ReportNotificationNextAction = "Each finished child's report arrives as a message from Mjolnir. Do independent work; when none remains, end your turn with a brief waiting status. Your session stays live and children keep running. While you are working reports arrive between tool calls. When you are idle a report starts a new turn. Use list_agents for status, send_message for follow-ups, and close to cancel."

// LineRange is an inclusive, one-based line range within a file.
type LineRange struct {
	Start uint64 `json:"start"`
	End   uint64 `json:"end"`
}

func (r *LineRange) UnmarshalJSON(data []byte) error {
	type plain LineRange
	return transcriptStrictDecode(data, (*plain)(r), true)
}

// FileSourceRanges are one or more line ranges captured from a single file
// for a child's initial context. Grouping by file lets a parent pull several
// disjoint ranges out of the same file in one entry, rather than one range
// per file.
type FileSourceRanges struct {
	File   string      `json:"file"`
	Ranges []LineRange `json:"ranges"`
}

func (r *FileSourceRanges) UnmarshalJSON(data []byte) error {
	type plain FileSourceRanges
	return transcriptStrictDecode(data, (*plain)(r), true)
}

// ReviewOverride is one before-push review finding the agent declines to
// fix, in the agent's own words, and why.
type ReviewOverride struct {
	Finding string `json:"finding"`
	Reason  string `json:"reason"`
}

func (o *ReviewOverride) UnmarshalJSON(data []byte) error {
	type plain ReviewOverride
	return transcriptStrictDecode(data, (*plain)(o), true)
}

// LegacyWholeVerdictFinding is the finding recorded for a queued override
// from an older worker, which overrode the whole verdict with one reason.
const LegacyWholeVerdictFinding = "Every finding in the reviewed verdict"

// SubagentToolRequest is one MCP request created inside a parent worker and
// consumed by the controller. RequestID is the idempotency identity across
// reconnects.
type SubagentToolRequest struct {
	// OriginatingCommandID is stamped by the worker owner at durable
	// admission, never trusted from MCP.
	OriginatingCommandID *string            `json:"originating_command_id,omitempty"`
	RequestID            string             `json:"request_id"`
	CreatedAtMs          int64              `json:"created_at_ms"`
	Action               SubagentToolAction `json:"action"`
}

func (r *SubagentToolRequest) UnmarshalJSON(data []byte) error {
	var v struct {
		OriginatingCommandID *string         `json:"originating_command_id,omitempty"`
		RequestID            string          `json:"request_id"`
		CreatedAtMs          int64           `json:"created_at_ms"`
		Action               json.RawMessage `json:"action"`
	}
	if err := transcriptStrictDecode(data, &v, true); err != nil {
		return err
	}
	action, err := DecodeSubagentToolAction(v.Action)
	if err != nil {
		return err
	}
	*r = SubagentToolRequest{v.OriginatingCommandID, v.RequestID, v.CreatedAtMs, action}
	return nil
}

// SubagentToolAction is one delegation tool call, adjacently tagged as
// {"action": ..., "params": {...}} with unknown fields denied. Decode with
// DecodeSubagentToolAction.
type SubagentToolAction interface {
	isSubagentToolAction()
	// MutatesChildState reports whether accepting this action may change a
	// child session's lifecycle or queued work. In-place Move drains these
	// actions before stopping the parent worker; observations can be
	// interrupted with the old harness.
	MutatesChildState() bool
}

type SubagentToolActionFetch struct {
	SessionID string `json:"session_id"`
	Path      string `json:"path"`
	Ls        bool   `json:"ls" serde:"default"`
}

type SubagentToolActionListProfiles struct{}

type SubagentToolActionRequestReview struct {
	Path *string `json:"path,omitempty"`
}

type SubagentToolActionAskOracle struct {
	Message string `json:"message"`
}

type SubagentToolActionOverrideReview struct {
	Overrides []ReviewOverride `json:"overrides"`
}

// SubagentToolActionSetReviewBaseline marks commits through Commit as
// reviewed, so later reviews start there.
type SubagentToolActionSetReviewBaseline struct {
	Commit string  `json:"commit"`
	Path   *string `json:"path,omitempty"`
}

type SubagentToolActionSpawn struct {
	NoGithubCredentials bool             `json:"no_github_credentials,omitempty"`
	TaskName            string           `json:"task_name"`
	Instructions        string           `json:"instructions"`
	TaskType            SubagentTaskType `json:"task_type" serde:"default"`
	// WorkingDirectory is absolute, or relative to the parent session's
	// working directory. Empty means the parent's own working directory.
	// The directory must exist on the target; no other restriction applies.
	WorkingDirectory string             `json:"working_directory" serde:"default"`
	Context          *string            `json:"context,omitempty"`
	Files            []FileSourceRanges `json:"files,omitempty"`
}

type SubagentToolActionListAgents struct{}

type SubagentToolActionSendInput struct {
	ChildSessionID string `json:"child_session_id"`
	Message        string `json:"message"`
}

// SubagentToolActionSendMessage: persisted requests from older workers use
// this action and its historical child_session_id field for the target.
// New MCP calls expose session_id, which is mapped here so stored requests
// remain readable.
type SubagentToolActionSendMessage struct {
	ChildSessionID string `json:"child_session_id"`
	Message        string `json:"message"`
}

// SubagentToolActionLegacyWaitAgents is a decode-only request from an older
// worker; reports now arrive as messages.
type SubagentToolActionLegacyWaitAgents struct{}

// SubagentToolActionLegacyAwaitChildren is a decode-only request from an
// older worker; no wait is armed.
type SubagentToolActionLegacyAwaitChildren struct{}

// SubagentToolActionLegacyInterruptAgent is an accepted request from an
// older parent worker. New MCP servers do not expose interrupt, and the
// daemon rejects this legacy action.
type SubagentToolActionLegacyInterruptAgent struct {
	ChildSessionID string `json:"child_session_id"`
}

type SubagentToolActionCloseAgent struct {
	ChildSessionID string `json:"child_session_id"`
}

// SubagentToolActionHandback is a child's report for the session that
// started it. Only a child's worker serves this action; the daemon runs it
// for the requesting child.
type SubagentToolActionHandback struct {
	Message string `json:"message"`
}

func (SubagentToolActionFetch) isSubagentToolAction()                {}
func (SubagentToolActionListProfiles) isSubagentToolAction()         {}
func (SubagentToolActionRequestReview) isSubagentToolAction()        {}
func (SubagentToolActionAskOracle) isSubagentToolAction()            {}
func (SubagentToolActionOverrideReview) isSubagentToolAction()       {}
func (SubagentToolActionSetReviewBaseline) isSubagentToolAction()    {}
func (SubagentToolActionSpawn) isSubagentToolAction()                {}
func (SubagentToolActionListAgents) isSubagentToolAction()           {}
func (SubagentToolActionSendInput) isSubagentToolAction()            {}
func (SubagentToolActionSendMessage) isSubagentToolAction()          {}
func (SubagentToolActionLegacyWaitAgents) isSubagentToolAction()     {}
func (SubagentToolActionLegacyAwaitChildren) isSubagentToolAction()  {}
func (SubagentToolActionLegacyInterruptAgent) isSubagentToolAction() {}
func (SubagentToolActionCloseAgent) isSubagentToolAction()           {}
func (SubagentToolActionHandback) isSubagentToolAction()             {}

func (SubagentToolActionFetch) MutatesChildState() bool                { return false }
func (SubagentToolActionListProfiles) MutatesChildState() bool         { return false }
func (SubagentToolActionRequestReview) MutatesChildState() bool        { return true }
func (SubagentToolActionAskOracle) MutatesChildState() bool            { return true }
func (SubagentToolActionOverrideReview) MutatesChildState() bool       { return true }
func (SubagentToolActionSetReviewBaseline) MutatesChildState() bool    { return true }
func (SubagentToolActionSpawn) MutatesChildState() bool                { return true }
func (SubagentToolActionListAgents) MutatesChildState() bool           { return false }
func (SubagentToolActionSendInput) MutatesChildState() bool            { return true }
func (SubagentToolActionSendMessage) MutatesChildState() bool          { return true }
func (SubagentToolActionLegacyWaitAgents) MutatesChildState() bool     { return false }
func (SubagentToolActionLegacyAwaitChildren) MutatesChildState() bool  { return false }
func (SubagentToolActionLegacyInterruptAgent) MutatesChildState() bool { return true }
func (SubagentToolActionCloseAgent) MutatesChildState() bool           { return true }
func (SubagentToolActionHandback) MutatesChildState() bool             { return false }

func (a SubagentToolActionFetch) MarshalJSON() ([]byte, error) {
	type plain SubagentToolActionFetch
	return transcriptMarshalAdjacent("action", "fetch", "params", plain(a))
}

func (SubagentToolActionListProfiles) MarshalJSON() ([]byte, error) {
	return transcriptMarshalAdjacent("action", "list_profiles", "params", nil)
}

func (a SubagentToolActionRequestReview) MarshalJSON() ([]byte, error) {
	type plain SubagentToolActionRequestReview
	return transcriptMarshalAdjacent("action", "request_review", "params", plain(a))
}

func (a SubagentToolActionAskOracle) MarshalJSON() ([]byte, error) {
	type plain SubagentToolActionAskOracle
	return transcriptMarshalAdjacent("action", "ask_oracle", "params", plain(a))
}

func (a SubagentToolActionOverrideReview) MarshalJSON() ([]byte, error) {
	type plain SubagentToolActionOverrideReview
	return transcriptMarshalAdjacent("action", "override_review", "params", plain(a))
}

func (a SubagentToolActionSetReviewBaseline) MarshalJSON() ([]byte, error) {
	type plain SubagentToolActionSetReviewBaseline
	return transcriptMarshalAdjacent("action", "set_review_baseline", "params", plain(a))
}

func (a SubagentToolActionSpawn) MarshalJSON() ([]byte, error) {
	type plain SubagentToolActionSpawn
	v := plain(a)
	if v.TaskType == "" {
		v.TaskType = SubagentTaskTypeStraightforward
	}
	return transcriptMarshalAdjacent("action", "spawn", "params", v)
}

func (SubagentToolActionListAgents) MarshalJSON() ([]byte, error) {
	return transcriptMarshalAdjacent("action", "list_agents", "params", nil)
}

func (a SubagentToolActionSendInput) MarshalJSON() ([]byte, error) {
	type plain SubagentToolActionSendInput
	return transcriptMarshalAdjacent("action", "send_input", "params", plain(a))
}

func (a SubagentToolActionSendMessage) MarshalJSON() ([]byte, error) {
	type plain SubagentToolActionSendMessage
	return transcriptMarshalAdjacent("action", "send_message", "params", plain(a))
}

func (SubagentToolActionLegacyWaitAgents) MarshalJSON() ([]byte, error) {
	return transcriptMarshalAdjacent("action", "wait_agents", "params", nil)
}

func (SubagentToolActionLegacyAwaitChildren) MarshalJSON() ([]byte, error) {
	return transcriptMarshalAdjacent("action", "await_children", "params", nil)
}

func (a SubagentToolActionLegacyInterruptAgent) MarshalJSON() ([]byte, error) {
	type plain SubagentToolActionLegacyInterruptAgent
	return transcriptMarshalAdjacent("action", "interrupt_agent", "params", plain(a))
}

func (a SubagentToolActionCloseAgent) MarshalJSON() ([]byte, error) {
	type plain SubagentToolActionCloseAgent
	return transcriptMarshalAdjacent("action", "close_agent", "params", plain(a))
}

func (a SubagentToolActionHandback) MarshalJSON() ([]byte, error) {
	type plain SubagentToolActionHandback
	return transcriptMarshalAdjacent("action", "handback", "params", plain(a))
}

// DecodeSubagentToolAction reads a SubagentToolAction, accepting what
// older workers queued: retired wait fields, a whole-verdict override
// reason, and the profile_id/model/effort spawn fields Mjolnir now chooses
// itself.
func DecodeSubagentToolAction(data []byte) (SubagentToolAction, error) {
	var outer struct {
		Action string          `json:"action"`
		Params json.RawMessage `json:"params,omitempty"`
	}
	if err := transcriptStrictDecode(data, &outer, true); err != nil {
		return nil, err
	}
	params := outer.Params
	if outer.Action == "wait_agents" && params != nil {
		// Durable worker queues can outlive the build that wrote them.
		// Ignore retired wait fields, but preserve errors for every field
		// that is not part of an old wait request.
		if members, err := transcriptObject(params); err == nil {
			delete(members, "child_session_ids")
			delete(members, "return_when")
			delete(members, "timeout_seconds")
			if len(members) == 0 {
				params = nil
			} else if params, err = json.Marshal(members); err != nil {
				return nil, err
			}
		} else if string(params) == "null" {
			params = nil
		}
	}
	// variant is the Rust wire enum's variant, which serde names in the
	// error.
	unit := func(action SubagentToolAction, variant string) (SubagentToolAction, error) {
		if params != nil && string(params) != "null" {
			return nil, errors.New("invalid type: map, expected unit variant SubagentToolActionWire::" + variant)
		}
		return action, nil
	}
	if params == nil {
		params = json.RawMessage("null")
	}
	var err error
	switch outer.Action {
	case "fetch":
		type plain SubagentToolActionFetch
		var v plain
		err = transcriptStrictDecode(params, &v, true)
		return SubagentToolActionFetch(v), err
	case "list_profiles":
		return unit(SubagentToolActionListProfiles{}, "ListProfiles")
	case "request_review":
		type plain SubagentToolActionRequestReview
		var v plain
		err = transcriptStrictDecode(params, &v, true)
		return SubagentToolActionRequestReview(v), err
	case "ask_oracle":
		type plain SubagentToolActionAskOracle
		var v plain
		err = transcriptStrictDecode(params, &v, true)
		return SubagentToolActionAskOracle(v), err
	case "override_review":
		var v struct {
			Overrides []ReviewOverride `json:"overrides" serde:"default"`
			Reason    *string          `json:"reason"`
		}
		if err := transcriptStrictDecode(params, &v, true); err != nil {
			return nil, err
		}
		overrides := v.Overrides
		if overrides == nil {
			overrides = []ReviewOverride{}
		}
		if v.Reason != nil {
			overrides = append(overrides, ReviewOverride{Finding: LegacyWholeVerdictFinding, Reason: *v.Reason})
		}
		return SubagentToolActionOverrideReview{Overrides: overrides}, nil
	case "set_review_baseline":
		type plain SubagentToolActionSetReviewBaseline
		var v plain
		err = transcriptStrictDecode(params, &v, true)
		return SubagentToolActionSetReviewBaseline(v), err
	case "spawn":
		type plain SubagentToolActionSpawn
		var v struct {
			plain
			ProfileID *string `json:"profile_id,omitempty"`
			Model     *string `json:"model,omitempty"`
			Effort    *string `json:"effort,omitempty"`
		}
		v.TaskType = SubagentTaskTypeStraightforward
		err = transcriptStrictDecode(params, &v, true)
		return SubagentToolActionSpawn(v.plain), err
	case "list_agents":
		return unit(SubagentToolActionListAgents{}, "ListAgents")
	case "send_input":
		type plain SubagentToolActionSendInput
		var v plain
		err = transcriptStrictDecode(params, &v, true)
		return SubagentToolActionSendInput(v), err
	case "send_message":
		type plain SubagentToolActionSendMessage
		var v plain
		err = transcriptStrictDecode(params, &v, true)
		return SubagentToolActionSendMessage(v), err
	case "wait_agents":
		return unit(SubagentToolActionLegacyWaitAgents{}, "LegacyWaitAgents")
	case "await_children":
		return unit(SubagentToolActionLegacyAwaitChildren{}, "LegacyAwaitChildren")
	case "interrupt_agent":
		type plain SubagentToolActionLegacyInterruptAgent
		var v plain
		err = transcriptStrictDecode(params, &v, true)
		return SubagentToolActionLegacyInterruptAgent(v), err
	case "close_agent":
		type plain SubagentToolActionCloseAgent
		var v plain
		err = transcriptStrictDecode(params, &v, true)
		return SubagentToolActionCloseAgent(v), err
	case "handback":
		type plain SubagentToolActionHandback
		var v plain
		err = transcriptStrictDecode(params, &v, true)
		return SubagentToolActionHandback(v), err
	}
	return nil, transcriptUnknownVariant(outer.Action, "fetch", "list_profiles", "request_review",
		"ask_oracle", "override_review", "set_review_baseline", "spawn", "list_agents", "send_input",
		"send_message", "wait_agents", "await_children", "interrupt_agent", "close_agent", "handback")
}

type SubagentToolResult struct {
	RequestID     string `json:"request_id"`
	CompletedAtMs int64  `json:"completed_at_ms"`
	IsError       bool   `json:"is_error"`
	Message       string `json:"message"`
}

func (r *SubagentToolResult) UnmarshalJSON(data []byte) error {
	type plain SubagentToolResult
	return transcriptStrictDecode(data, (*plain)(r), true)
}

// SubagentCompletion is the control-plane projection of a completed tool.
// Report bodies stay on the worker.
type SubagentCompletion struct {
	RequestID     string          `json:"request_id"`
	CompletedAtMs int64           `json:"completed_at_ms"`
	IsError       bool            `json:"is_error"`
	InputDelivery json.RawMessage `json:"input_delivery,omitempty"`
	// InterruptedWait is a legacy worker completion field, retained for decoding.
	InterruptedWait *SubagentWaitInterruption `json:"interrupted_wait,omitempty"`
}

func (c *SubagentCompletion) UnmarshalJSON(data []byte) error {
	type plain SubagentCompletion
	return transcriptStrictDecode(data, (*plain)(c), false)
}

type SubagentWaitInterruption struct {
	WaitedSeconds uint64  `json:"waited_seconds"`
	NextAction    *string `json:"next_action,omitempty"`
}

func (w *SubagentWaitInterruption) UnmarshalJSON(data []byte) error {
	type plain SubagentWaitInterruption
	return transcriptStrictDecode(data, (*plain)(w), false)
}

// SubagentCompletionFrom projects a tool result, keeping the input-delivery
// fields of a message's result.
func SubagentCompletionFrom(result *SubagentToolResult) SubagentCompletion {
	completion := SubagentCompletion{
		RequestID:     result.RequestID,
		CompletedAtMs: result.CompletedAtMs,
		IsError:       result.IsError,
	}
	value, err := jsonvalue.Parse([]byte(result.Message))
	if err != nil {
		return completion
	}
	_, hasChild := transcriptGetStr(value, "child_session_id")
	_, hasCreated := transcriptGet(value, "created_at_ms")
	_, hasStatus := transcriptGet(value, "status")
	if !hasChild || !hasCreated || !hasStatus {
		return completion
	}
	delivery := jsonvalue.Object{}
	for _, key := range []string{
		"child_session_id",
		"created_at_ms",
		"status",
		"via",
		"kind",
		"turn_id",
		"command_id",
		"accepted_ordinal",
		"error",
	} {
		if member, ok := transcriptGet(value, key); ok {
			delivery.Set(key, member)
		}
	}
	if encoded, err := jsonvalue.Marshal(delivery); err == nil {
		completion.InputDelivery = encoded
	}
	return completion
}

// SubagentQueueChange is what a connection exchanges: only changed queue
// entries. Completion replaces a request. Adjacently tagged as
// {"kind": ..., "data": ...}. Decode with DecodeSubagentQueueChange.
type SubagentQueueChange interface {
	isSubagentQueueChange()
	RequestID() string
}

type SubagentQueueChangeRequest struct {
	Request SubagentToolRequest
}

type SubagentQueueChangeCompleted struct {
	Completion SubagentCompletion
}

type SubagentQueueChangeRetired struct {
	RequestIDValue string `json:"request_id"`
}

func (SubagentQueueChangeRequest) isSubagentQueueChange()   {}
func (SubagentQueueChangeCompleted) isSubagentQueueChange() {}
func (SubagentQueueChangeRetired) isSubagentQueueChange()   {}

func (c SubagentQueueChangeRequest) RequestID() string   { return c.Request.RequestID }
func (c SubagentQueueChangeCompleted) RequestID() string { return c.Completion.RequestID }
func (c SubagentQueueChangeRetired) RequestID() string   { return c.RequestIDValue }

func (c SubagentQueueChangeRequest) MarshalJSON() ([]byte, error) {
	return transcriptMarshalAdjacent("kind", "request", "data", c.Request)
}

func (c SubagentQueueChangeCompleted) MarshalJSON() ([]byte, error) {
	return transcriptMarshalAdjacent("kind", "completed", "data", c.Completion)
}

func (c SubagentQueueChangeRetired) MarshalJSON() ([]byte, error) {
	type plain SubagentQueueChangeRetired
	return transcriptMarshalAdjacent("kind", "retired", "data", plain(c))
}

func DecodeSubagentQueueChange(data []byte) (SubagentQueueChange, error) {
	var outer struct {
		Kind string          `json:"kind"`
		Data json.RawMessage `json:"data"`
	}
	if err := transcriptStrictDecode(data, &outer, false); err != nil {
		return nil, err
	}
	switch outer.Kind {
	case "request":
		var v SubagentToolRequest
		err := json.Unmarshal(outer.Data, &v)
		return SubagentQueueChangeRequest{v}, err
	case "completed":
		var v SubagentCompletion
		err := json.Unmarshal(outer.Data, &v)
		return SubagentQueueChangeCompleted{v}, err
	case "retired":
		type plain SubagentQueueChangeRetired
		var v plain
		err := transcriptStrictDecode(outer.Data, &v, false)
		return SubagentQueueChangeRetired(v), err
	}
	return nil, transcriptUnknownVariant(outer.Kind, "request", "completed", "retired")
}

type SubagentQueuePage struct {
	Sequence uint64                `json:"sequence"`
	Changes  []SubagentQueueChange `json:"changes"`
	HasMore  bool                  `json:"has_more"`
}

func (p *SubagentQueuePage) UnmarshalJSON(data []byte) error {
	var v struct {
		Sequence uint64            `json:"sequence"`
		Changes  []json.RawMessage `json:"changes"`
		HasMore  bool              `json:"has_more"`
	}
	if err := transcriptStrictDecode(data, &v, false); err != nil {
		return err
	}
	changes := make([]SubagentQueueChange, 0, len(v.Changes))
	for _, raw := range v.Changes {
		change, err := DecodeSubagentQueueChange(raw)
		if err != nil {
			return err
		}
		changes = append(changes, change)
	}
	*p = SubagentQueuePage{Sequence: v.Sequence, Changes: changes, HasMore: v.HasMore}
	return nil
}

// SubagentKind orders as declared: worker, reviewer, oracle.
type SubagentKind string

const (
	SubagentKindWorker   SubagentKind = "worker"
	SubagentKindReviewer SubagentKind = "reviewer"
	SubagentKindOracle   SubagentKind = "oracle"
)

// MaxHandbackChars is the maximum report length, counted in Unicode
// characters.
func (k SubagentKind) MaxHandbackChars() int {
	if k == SubagentKindReviewer || k == SubagentKindOracle {
		return MaxAdvisorHandbackChars
	}
	return MaxHandbackChars
}

// RequestKey is the key in the request index; the record keeps the
// caller's original id.
func (k SubagentKind) RequestKey(key string) string {
	if k == "" {
		k = SubagentKindWorker
	}
	return string(k) + ":" + key
}

// SubagentRecord is durable ownership and launch intent for one child
// session.
type SubagentRecord struct {
	Kind            SubagentKind          `json:"kind" serde:"default"`
	ReviewerLaunch  *ReviewerLaunchConfig `json:"reviewer_launch,omitempty"`
	ChildSessionID  string                `json:"child_session_id"`
	ParentSessionID string                `json:"parent_session_id"`
	TaskName        string                `json:"task_name"`
	ProfileID       string                `json:"profile_id"`
	Model           *string               `json:"model,omitempty"`
	Effort          *string               `json:"effort,omitempty"`
	// WorkingDirectory is the launch directory for the child on the
	// parent's target: absolute, or relative to the parent session's
	// working directory. Empty means the parent's own working directory.
	// The directory must exist; no other restriction applies.
	WorkingDirectory string `json:"working_directory"`
	// InitialPrompt is the complete first prompt after the controller
	// captures requested ranges.
	InitialPrompt string `json:"initial_prompt"`
	RequestKey    string `json:"request_key"`
	CreatedAt     string `json:"created_at"`
	// NoticedTurn is the child turn whose completion notice the parent's
	// transcript has already recorded, so restarts do not repeat the notice.
	// Stored under the historical field name delivered_turn.
	NoticedTurn *uint64 `json:"delivered_turn,omitempty"`
	// ReportedFinish is the last child finish carried by an admitted parent
	// notification. Stored in record_json without changing the database
	// schema.
	ReportedFinish *SubagentFinishIdentity `json:"reported_finish,omitempty"`
	// HandbackTool says whether this child was given the handback tool. It
	// is decided once, when the child is registered. A child recorded before
	// the tool existed reads as false and keeps reporting through its last
	// message.
	HandbackTool bool `json:"handback_tool,omitempty"`
}

func (r *SubagentRecord) UnmarshalJSON(data []byte) error {
	type plain SubagentRecord
	v := plain{Kind: SubagentKindWorker}
	if err := transcriptStrictDecode(data, &v, true); err != nil {
		return err
	}
	*r = SubagentRecord(v)
	return nil
}

func (r *SubagentRecord) IsChild(sessionID string) bool {
	return r.ChildSessionID == sessionID
}

// SubagentMetadata is relationship and lifecycle metadata published to
// control surfaces. Complete prompts and reviewer launch configuration
// remain in the store's SubagentRecord; a metadata snapshot cannot create a
// new child launch.
type SubagentMetadata struct {
	Kind             SubagentKind            `json:"kind" serde:"default"`
	ChildSessionID   string                  `json:"child_session_id"`
	ParentSessionID  string                  `json:"parent_session_id"`
	TaskName         string                  `json:"task_name"`
	ProfileID        string                  `json:"profile_id"`
	Model            *string                 `json:"model,omitempty"`
	Effort           *string                 `json:"effort,omitempty"`
	WorkingDirectory string                  `json:"working_directory"`
	RequestKey       string                  `json:"request_key"`
	CreatedAt        string                  `json:"created_at"`
	NoticedTurn      *uint64                 `json:"delivered_turn,omitempty"`
	ReportedFinish   *SubagentFinishIdentity `json:"reported_finish,omitempty"`
	HandbackTool     bool                    `json:"handback_tool" serde:"default"`
}

func (m *SubagentMetadata) UnmarshalJSON(data []byte) error {
	type plain SubagentMetadata
	v := plain{Kind: SubagentKindWorker}
	if err := transcriptStrictDecode(data, &v, true); err != nil {
		return err
	}
	*m = SubagentMetadata(v)
	return nil
}

func SubagentMetadataFrom(record *SubagentRecord) SubagentMetadata {
	return SubagentMetadata{
		Kind:             record.Kind,
		ChildSessionID:   record.ChildSessionID,
		ParentSessionID:  record.ParentSessionID,
		TaskName:         record.TaskName,
		ProfileID:        record.ProfileID,
		Model:            record.Model,
		Effort:           record.Effort,
		WorkingDirectory: record.WorkingDirectory,
		RequestKey:       record.RequestKey,
		CreatedAt:        record.CreatedAt,
		NoticedTurn:      record.NoticedTurn,
		ReportedFinish:   record.ReportedFinish,
		HandbackTool:     record.HandbackTool,
	}
}

// ApplyTo applies metadata edits without replacing the durable launch intent.
func (m *SubagentMetadata) ApplyTo(record *SubagentRecord) {
	record.Kind = m.Kind
	record.ChildSessionID = m.ChildSessionID
	record.ParentSessionID = m.ParentSessionID
	record.TaskName = m.TaskName
	record.ProfileID = m.ProfileID
	record.Model = m.Model
	record.Effort = m.Effort
	record.WorkingDirectory = m.WorkingDirectory
	record.RequestKey = m.RequestKey
	record.CreatedAt = m.CreatedAt
	record.NoticedTurn = m.NoticedTurn
	record.ReportedFinish = m.ReportedFinish
	record.HandbackTool = m.HandbackTool
}

// SubagentStatus is the lifecycle group used by the tool and both user
// interfaces.
type SubagentStatus string

const (
	SubagentStatusPreparing     SubagentStatus = "preparing"
	SubagentStatusRunning       SubagentStatus = "running"
	SubagentStatusInputRequired SubagentStatus = "input_required"
	SubagentStatusCompleted     SubagentStatus = "completed"
	SubagentStatusFailed        SubagentStatus = "failed"
	SubagentStatusInterrupted   SubagentStatus = "interrupted"
	SubagentStatusStopped       SubagentStatus = "stopped"
)

// SubagentMCPServer is the name a worker's sub-agent MCP server is
// registered under in every harness, so its tools reach the model as
// mcp__mj-agents__<tool>.
const SubagentMCPServer = "mj-agents"

// ReviewMCPServer is the name of the separate MCP server that offers the
// before-push review tools, so they reach the model as
// mcp__mj-review__<tool>. Claude loads its tools on demand because they are
// needed only when a push is near.
const ReviewMCPServer = "mj-review"

// ReviewMCPTools are the tools the review server lists, in the order it
// lists them.
var ReviewMCPTools = []string{"request_review", "override_review", "set_review_baseline"}

// SubagentMcpRole is which tools a worker's mj-agents MCP server offers. A
// parent may delegate, a top-level session without Mjolnir delegation can
// message sessions, and a child also hands its report back. Messaging and
// fetch are common to every role. The review tools belong to the separate
// mj-review server. The zero value is the default, parent.
type SubagentMcpRole string

const (
	SubagentMcpRoleParent      SubagentMcpRole = "parent"
	SubagentMcpRoleMessageOnly SubagentMcpRole = "message_only"
	SubagentMcpRoleFetchOnly   SubagentMcpRole = "fetch_only"
	SubagentMcpRoleChild       SubagentMcpRole = "child"
)

// UnmarshalJSON also accepts the alias fixed_parent.
func (r *SubagentMcpRole) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch role := SubagentMcpRole(text); role {
	case SubagentMcpRoleParent, SubagentMcpRoleMessageOnly, SubagentMcpRoleFetchOnly, SubagentMcpRoleChild:
		*r = role
		return nil
	case "fixed_parent":
		*r = SubagentMcpRoleParent
		return nil
	}
	return transcriptUnknownVariant(text, "parent", "fixed_parent", "message_only", "fetch_only", "child")
}

func (r SubagentMcpRole) MarshalJSON() ([]byte, error) {
	return transcriptMarshal(r.ID())
}

// ReviewServerArgs are the arguments that start the mj-review server. Only
// a managed child names its role, which selects its instructions; a
// top-level session's arguments stay as earlier builds wrote them.
func (r SubagentMcpRole) ReviewServerArgs(socket string, harness string) []string {
	args := []string{"worker", "subagent-mcp", "--socket", socket, "--harness", harness, "--review-server"}
	if r == SubagentMcpRoleChild || r == SubagentMcpRoleFetchOnly {
		args = append(args, "--role", r.ID())
	}
	return args
}

func (r SubagentMcpRole) ID() string {
	if r == "" {
		return string(SubagentMcpRoleParent)
	}
	return string(r)
}

// ToolNames are the tools this role's server lists, in the order it lists
// them. A harness that asks before an MCP tool is allowed exactly these.
func (r SubagentMcpRole) ToolNames() []string {
	switch r {
	case SubagentMcpRoleMessageOnly:
		return []string{"send_message", "fetch", "ask_oracle"}
	case SubagentMcpRoleFetchOnly:
		return []string{"fetch", "send_message", "ask_oracle"}
	case SubagentMcpRoleChild:
		return []string{"handback", "fetch", "send_message"}
	}
	return []string{"spawn", "list_agents", "send_message", "close", "fetch", "ask_oracle"}
}

func (r SubagentMcpRole) String() string { return r.ID() }

// ParseSubagentMcpRole is Rust's FromStr.
func ParseSubagentMcpRole(value string) (SubagentMcpRole, error) {
	switch value {
	case "parent", "fixed_parent":
		return SubagentMcpRoleParent, nil
	case "message_only":
		return SubagentMcpRoleMessageOnly, nil
	case "fetch_only":
		return SubagentMcpRoleFetchOnly, nil
	case "child":
		return SubagentMcpRoleChild, nil
	}
	return "", fmt.Errorf("unknown sub-agent MCP role %s", strconv.Quote(value))
}

// SubagentHandbackReminderText is the reminder itself, sent as an ordinary
// prompt so the child's transcript shows it. A turn can end before the task
// is done (a harness failure, a question in plain text), so the reminder
// must not order a child that has not finished to stop: that turned a
// failed first turn into an empty report (issue 1217). (Rust
// HANDBACK_REMINDER_TEXT, prefixed because relay/snapshot has one.)
const SubagentHandbackReminderText = "[handback reminder] Your turn ended without delivering a report. If your assigned task is finished, call the mj-agents handback tool now with your full report, then stop. If it is not finished, continue working on it, and hand back when it is done."

// HandbackReportRules is a child's delegated authority and reporting
// requirements. Shared by the first-prompt note, the child's server
// instructions and the handback tool description, so the three cannot
// drift apart.
var HandbackReportRules = fmt.Sprintf(
	"Your parent reads your report, at most %d characters for workers or %d for oracles and reviewers: outcome, changed files, evidence paths, mechanical fixes, what needs a decision, and failures. For each failing test give its name, a one-line reason and the path of its log. Put longer logs, tables and findings in your report directory; list their paths instead of copying them into the report. Keep text evidence here, not build outputs, binaries or downloads. Handback is refused above the directory's byte limit. Fix compile, lint and format errors and tests your changes break; list each fix in one report line. Complete the assigned outcome within the parent's agreed design and constraints. Resolve routine implementation details and perform necessary supporting work, including edits and tests in files or artifacts not named explicitly. Named files are starting pointers unless explicitly declared an ownership boundary. Respect read-only assignments, explicit exclusions, and other agents' ownership; do not overwrite concurrent changes. Ask the parent before changing agreed behavior or design, introducing an unapproved public or persisted contract change, crossing an ownership boundary, or taking an action outside delegated authority. Changes already required by the assignment, including contract or epoch changes, need no duplicate approval. Report unrelated baseline failures and continue independent assigned work. When blocked on a parent decision, hand back the question and decisive evidence. The parent owns consequential design decisions and final acceptance. A question you ask is sent to your parent and closed right away; its answer arrives as a message.",
	SubagentKindWorker.MaxHandbackChars(),
	SubagentKindOracle.MaxHandbackChars(),
)

// ReadOnlyTaskScope: assignment safety scope also applies when an adapter
// cannot receive standing instructions.
const ReadOnlyTaskScope = "This is a read-only consultation: do not edit files, build, or run tests."

// ReportDirectoryNote is the per-task directory note; the standing role
// contract travels separately.
func ReportDirectoryNote(reportDir string) string {
	return "Your report directory is " + reportDir + "."
}

// HandbackPromptNote is what a child's first prompt opens with when it has
// the tool. It leads the prompt rather than trailing a parent's long
// instructions, where live runs showed children skipping it. reportDir is
// the directory Mjolnir created for this child's files. The sentence about
// long waits is here and not in HandbackReportRules because those rules
// already fill the length the harnesses keep of a tool description and of
// server instructions.
func HandbackPromptNote(reportDir string) string {
	return "You are a Mjolnir sub-agent. Finish every task by calling the mj-agents handback tool with your report: the session that started you reads only that report, not the rest of this conversation. When your assignment needs the result of a build, test run or other long command, wait for it to finish: a long wait is not a reason to hand back. Your report directory is " +
		reportDir + ". " + HandbackReportRules
}

// HandbackReminderGraceMs is how long a sent reminder may be neither
// queued, running nor finished before the child's report stops waiting for
// it. Someone removed it from the queue, or it never reached the child;
// either way it is not coming.
const HandbackReminderGraceMs int64 = 30_000

// MaxHandbackChars is the longest ordinary worker report a handback or
// notification includes. Details belong in files in the child's report
// directory.
const MaxHandbackChars = 4_000

// MaxAdvisorHandbackChars: oracles and reviewers return complete advice and
// findings in one handback.
const MaxAdvisorHandbackChars = 64 * 1024

// ReportRootDir is the directory under a parent's workspace root that holds
// its children's report directories, outside every repository.
const ReportRootDir = ".mj-agents"

// ProjectReportRootDir: for a session whose workspace root is not
// Mjolnir's (a bare project), the report root sits inside the project,
// under a path its info/exclude lists.
const ProjectReportRootDir = ".mj/agents"

// ArchiveReportDir is the archive prefix for the report files a checkpoint
// carries.
const ArchiveReportDir = "mj-agent-reports"

// MaxCheckpointReportBytes is the most sub-agent report bytes one
// checkpoint carries. Reports are the details a child's short handback
// points to, so a checkpoint keeps the newest files up to this bound rather
// than failing over them.
const MaxCheckpointReportBytes uint64 = 64 * 1024 * 1024

// ChildReportDirEnv and ChildReportLimitEnv are the controller-authored
// launch environment used by every child's MCP server.
const ChildReportDirEnv = "MJ_CHILD_REPORT_DIR"
const ChildReportLimitEnv = "MJ_CHILD_REPORT_LIMIT_BYTES"

// MaxChildReportBytes is the most bytes one child's report directory may
// hold when it hands back: an equal share of MaxCheckpointReportBytes for
// each child that can run at once, so the live children's reports fit in
// one checkpoint.
func MaxChildReportBytes(maxConcurrentChildren int) uint64 {
	return MaxCheckpointReportBytes / uint64(max(maxConcurrentChildren, 1))
}

// BoundedReport cuts text to MaxHandbackChars characters, saying how much
// was left out and how the parent can get it. Returns the text and whether
// it was cut.
func BoundedReport(text string) (string, bool) {
	return boundedReportWithRecovery(text, SubagentKindWorker.MaxHandbackChars(),
		"use send_message to ask the child to write the details to files in its report directory and send you the paths")
}

// BoundedOracleReport bounds an oracle answer while keeping its recovery
// channel in the oracle.
func BoundedOracleReport(text string) (string, bool) {
	return boundedReportWithRecovery(text, SubagentKindOracle.MaxHandbackChars(),
		"use ask_oracle to ask for the rest; the oracle keeps its context")
}

func boundedReportWithRecovery(text string, maximum int, recovery string) (string, bool) {
	characters := []rune(text)
	if len(characters) <= maximum {
		return text, false
	}
	return fmt.Sprintf("%s\n[truncated: %d more characters; %s]",
		string(characters[:maximum]), len(characters)-maximum, recovery), true
}

// HoldGiveUpMs is how long Mjolnir holds a child that ended its turn to wait
// for processes it started before asking it to hand back what it has. The
// user chose 30 minutes on 2026-11-16: the measured 90th percentile of
// background work that agents waited for was 30 minutes (1,057 tasks since
// 2026-11-01; 99th percentile 115 minutes), and a longer run survives
// because a park waits for the parent's next turn, in which the parent can
// tell the child to continue.
const HoldGiveUpMs int64 = 30 * 60 * 1000

// HoldFollowUpKind is which follow-up prompt Mjolnir sends a child it is
// holding while the child waits for its own background processes.
type HoldFollowUpKind string

const (
	// HoldFollowUpKindProcessesExited: every process the child was waiting
	// for has exited.
	HoldFollowUpKindProcessesExited HoldFollowUpKind = "processes_exited"
	// HoldFollowUpKindGiveUp: the child waited HoldGiveUpMs and should hand
	// back what it has.
	HoldFollowUpKindGiveUp HoldFollowUpKind = "give_up"
)

// IsHandbackReminder reports whether a command id names the prompt that
// reminds a child to hand back its report, which is how ReportStateOf tells
// it from the task it follows up.
func IsHandbackReminder(commandID string) bool {
	_, ok := GeneratedPromptHandbackReminder.Suffix(CarriedCommand(commandID))
	return ok
}

// subagentParseU64 is Rust's str::parse::<u64>: ASCII digits after an
// optional "+", fitting a u64.
func subagentParseU64(text string) (uint64, bool) {
	digits := strings.TrimPrefix(text, "+")
	if digits == "" || strings.HasPrefix(digits, "+") || strings.HasPrefix(digits, "-") {
		return 0, false
	}
	value, err := strconv.ParseUint(digits, 10, 64)
	return value, err == nil
}

// HandbackReminderCommandID is the command id of the handback reminder for
// the turn that finished at completedOrdinal. The id is deterministic so a
// reminder is sent at most once per turn, and RemindedTurnOrdinal reads it
// back.
func HandbackReminderCommandID(completedOrdinal uint64) string {
	return GeneratedPromptHandbackReminder.CommandID(completedOrdinal)
}

// RemindedTurnOrdinal is the ordinal at which the reminded turn finished,
// when commandID names a handback reminder.
func RemindedTurnOrdinal(commandID string) (uint64, bool) {
	suffix, ok := GeneratedPromptHandbackReminder.Suffix(CarriedCommand(commandID))
	if !ok {
		return 0, false
	}
	return subagentParseU64(suffix)
}

func (k HoldFollowUpKind) prompt() GeneratedPrompt {
	if k == HoldFollowUpKindGiveUp {
		return GeneratedPromptHoldGiveUp
	}
	return GeneratedPromptHoldWake
}

// HoldFollowUpCommandID is the deterministic command id of the follow-up of
// kind for the reminder turn that finished at reminderCompletedOrdinal. The
// id makes a retry idempotent and lets a restarted daemon see what it
// already sent.
func HoldFollowUpCommandID(kind HoldFollowUpKind, reminderCompletedOrdinal uint64) string {
	return kind.prompt().CommandID(reminderCompletedOrdinal)
}

// HoldFollowUpOf is the kind and reminder-turn ordinal a hold follow-up
// command id names.
func HoldFollowUpOf(commandID string) (HoldFollowUpKind, uint64, bool) {
	carried := CarriedCommand(commandID)
	for _, kind := range []HoldFollowUpKind{HoldFollowUpKindProcessesExited, HoldFollowUpKindGiveUp} {
		suffix, ok := kind.prompt().Suffix(carried)
		if !ok {
			continue
		}
		if ordinal, ok := subagentParseU64(suffix); ok {
			return kind, ordinal, true
		}
	}
	return "", 0, false
}

// SubagentHandback is a report a child handed back during the turn
// CommandID names.
type SubagentHandback struct {
	CommandID    string `json:"command_id"`
	Message      string `json:"message"`
	RecordedAtMs int64  `json:"recorded_at_ms"`
}

func (h *SubagentHandback) UnmarshalJSON(data []byte) error {
	type plain SubagentHandback
	return transcriptStrictDecode(data, (*plain)(h), false)
}

// Answers reports whether this report answers the turn commandID names. A
// report from an earlier or later task of the same child answers none of
// the others.
func (h *SubagentHandback) Answers(commandID string) bool {
	return ReportAnswers(h.CommandID, commandID)
}

// ReportAnswers reports whether a report recorded for reportCommandID
// answers the turn commandID names; a restart continuation is the turn it
// carries out.
func ReportAnswers(reportCommandID, commandID string) bool {
	return CarriedCommand(reportCommandID) == CarriedCommand(commandID)
}

// HandbackReminder is the reminder Mjolnir sent after the turn
// ForCommandID ended without a report. CommandID is the reminder prompt's
// own command.
type HandbackReminder struct {
	CommandID    string `json:"command_id"`
	ForCommandID string `json:"for_command_id"`
	SentAtMs     int64  `json:"sent_at_ms"`
}

func (r *HandbackReminder) UnmarshalJSON(data []byte) error {
	type plain HandbackReminder
	return transcriptStrictDecode(data, (*plain)(r), false)
}

// SubagentReport is everything recorded about one child's report. Each part
// keeps only its latest value: a report answers the newest task, and each
// task gets at most one reminder.
type SubagentReport struct {
	Handback *SubagentHandback `json:"handback"`
	Reminder *HandbackReminder `json:"reminder"`
	// ReminderFailedFor is the turn whose reminder could not be sent.
	ReminderFailedFor *string `json:"reminder_failed_for"`
	// AwaitedOrdinal is the acceptance ordinal of the newest prompt the
	// parent gave this child. The child has not answered it until a finished
	// turn reaches it.
	AwaitedOrdinal *uint64 `json:"awaited_ordinal,omitempty"`
	// ReportDir is the absolute directory on the parent's target where this
	// child writes the details its report points to.
	ReportDir *string `json:"report_dir,omitempty"`
}

// AwaitingPrompt reports whether the parent's newest prompt has yet to be
// answered: no finished turn has reached its acceptance ordinal. The store
// learns of a new turn a moment after the prompt is accepted, and in that
// gap an idle child would otherwise read as finished with the previous
// turn's report, or none.
func AwaitingPrompt(awaitedOrdinal *uint64, lastTurn *MaterializedTurnOutcome) bool {
	return awaitedOrdinal != nil && (lastTurn == nil || !lastTurn.Answers(*awaitedOrdinal))
}

// FailedTurn is how a child's last finished turn failed, if it did:
// "interrupted" for a cancelled or interrupted turn, "failed" for an error,
// a refusal or a quota stop, with the most specific reason recorded. A turn
// that finished or stopped to ask for input did not fail.
func FailedTurn(turn *MaterializedTurnOutcome, lastMessage *string) (state string, reason string, failed bool) {
	var fallback string
	switch outcome := turn.Outcome.(type) {
	case TurnOutcomeKindCompleted:
		switch ClassifyPromptCompletion(outcome.StopReason) {
		case PromptCompletionFinished, PromptCompletionInputRequired:
			return "", "", false
		case PromptCompletionCancelled:
			state, fallback = "interrupted", "the turn was cancelled"
		default:
			state, fallback = "failed", "the turn ended with stop reason "+debugString(outcome.StopReason)
		}
	case TurnOutcomeKindInterrupted:
		state, fallback = "interrupted", outcome.Message
	case TurnOutcomeKindRejected:
		state, fallback = "failed", outcome.Message
	default:
		return "", "", false
	}
	switch {
	case turn.Diagnostic != nil && strings.TrimSpace(turn.Diagnostic.Message) != "":
		reason = turn.Diagnostic.Message
	case lastMessage != nil && strings.TrimSpace(*lastMessage) != "":
		reason = *lastMessage
	default:
		reason = fallback
	}
	return state, reason, true
}

// TurnFailedOnLogin reports whether a child's last finished turn reported
// an authentication failure. The diagnostic indicates it (an error kind
// such as Codex's unauthorized, or an auth failure sentence), or the turn
// failed with such a sentence as its reason. A usage limit is never one.
//
// Only the recorded failure is read, never the child's own messages: a
// child may well be writing about authentication errors.
func TurnFailedOnLogin(turn *MaterializedTurnOutcome) bool {
	if turn.Diagnostic != nil && TurnDiagnosticReportsAuthFailure(turn.Diagnostic) {
		return true
	}
	state, reason, failed := FailedTurn(turn, nil)
	return failed && state == "failed" && TextReportsAuthFailure(reason)
}

// WakeFailureKind is the "kind" tag of a WakeFailure.
type WakeFailureKind string

const WakeFailureAuthentication WakeFailureKind = "authentication"

// WakeFailure is authentication evidence from a wake that staged an
// identifiable login. Its only variant is authentication, so it is one
// struct with its tag.
type WakeFailure struct {
	Kind        WakeFailureKind
	ProfileID   string
	StagedLogin LoginFingerprint
}

func (f WakeFailure) MarshalJSON() ([]byte, error) {
	kind := f.Kind
	if kind == "" {
		kind = WakeFailureAuthentication
	}
	return transcriptMarshalTagged("kind", string(kind), struct {
		ProfileID   string           `json:"profile_id"`
		StagedLogin LoginFingerprint `json:"staged_login"`
	}{f.ProfileID, f.StagedLogin})
}

func (f *WakeFailure) UnmarshalJSON(data []byte) error {
	tag, err := transcriptTag(data, "kind", "WakeFailure")
	if err != nil {
		return err
	}
	if WakeFailureKind(tag) != WakeFailureAuthentication {
		return transcriptUnknownVariant(tag, "authentication")
	}
	var v struct {
		ProfileID   string           `json:"profile_id"`
		StagedLogin LoginFingerprint `json:"staged_login"`
	}
	if err := transcriptStrictDecode(data, &v, false, "kind"); err != nil {
		return err
	}
	*f = WakeFailure{Kind: WakeFailureAuthentication, ProfileID: v.ProfileID, StagedLogin: v.StagedLogin}
	return nil
}

const wakeFailureSeparator = "\nMjolnir wake evidence: "

// Encode keeps the existing human-readable detail first for older readers.
func (f *WakeFailure) Encode(detail string) (string, error) {
	evidence, err := transcriptMarshal(f)
	if err != nil {
		return "", err
	}
	return detail + wakeFailureSeparator + string(evidence), nil
}

// WakeFailureDecode splits a wake error into its detail and evidence.
func WakeFailureDecode(text string) (string, *WakeFailure) {
	index := strings.LastIndex(text, wakeFailureSeparator)
	if index < 0 {
		return text, nil
	}
	var failure WakeFailure
	if json.Unmarshal([]byte(text[index+len(wakeFailureSeparator):]), &failure) != nil {
		return text, nil
	}
	return text[:index], &failure
}

// LoginInvalidReason is what a parent is told about a profile whose login
// the provider refused.
func LoginInvalidReason(profileID string) string {
	return "the login is no longer valid; run `mj login --profile " + profileID + "` and spawn again"
}

// Liveness is what a caller knows of the processes a child's last turn left
// running. "No worker to ask" is not "nothing is running": a reader that
// cannot reach the child's worker must not report a child that may be
// waiting.
//
// The zero value is unknown: the child's worker is not attached to this
// reader. Known is the connected worker's publication, with a nil Liveness
// when it publishes none (no processes were left, or the worker predates
// protocol 67).
type Liveness struct {
	Known    bool
	Liveness *WorkLiveness
}

// ReportState is where a child's report stands after its last finished turn.
type ReportState interface {
	isReportState()
}

// ReportStateDelivered: the child handed back this report for its last turn.
type ReportStateDelivered struct {
	Message string
}

// ReportStatePending: the child owes a report. Remind says no reminder has
// been sent for this turn yet; otherwise one is on its way.
type ReportStatePending struct {
	Remind bool
}

// ReportStateHeld: the child ended its reminder turn without a report while
// processes it started are still running (or just exited): it is waiting,
// not done. No report is owed to the parent and the child is not parked.
type ReportStateHeld struct {
	// SinceMs is when the reminder turn ended, which is when the hold began.
	SinceMs int64
	// GiveUpAtMs is when the hold gives up and asks the child to hand back.
	GiveUpAtMs int64
	// WaitingOn is the processes the child waits on, rendered one per line.
	WaitingOn []string
	// FollowUp is the follow-up prompt due now, if any. Nil while still waiting.
	FollowUp *HoldFollowUpKind
}

// ReportStateFallback: no handback is coming: report the turn's last
// message, as before.
type ReportStateFallback struct{}

func (ReportStateDelivered) isReportState() {}
func (ReportStatePending) isReportState()   {}
func (ReportStateHeld) isReportState()      {}
func (ReportStateFallback) isReportState()  {}

// RunningProcessLines lists the processes a finished turn's work facts list
// as running, one per line.
func RunningProcessLines(facts *WorkFacts) []string {
	processes := facts.Processes.Known()
	if processes == nil {
		return []string{}
	}
	lines := make([]string, 0, len(processes.Shown)+1)
	for i := range processes.Shown {
		lines = append(lines, RenderProcess(&processes.Shown[i]))
	}
	if processes.Omitted > 0 {
		lines = append(lines, fmt.Sprintf("and %d more", processes.Omitted))
	}
	return lines
}

// subagentTurnFinished reports a turn that completed normally.
func subagentTurnFinished(turn *MaterializedTurnOutcome) bool {
	completed, ok := turn.Outcome.(TurnOutcomeKindCompleted)
	return ok && ClassifyPromptCompletion(completed.StopReason) == PromptCompletionFinished
}

// ReportStateOf is the rule for a child's report, shared by notifications,
// session waiting and the reminder. (Rust report_state; renamed because
// ReportState is the type.)
//
// inFlight names the commands queued or running on the child, so a sent
// reminder is known to be on its way. A child without the tool, and a turn
// that did not finish normally, fall back to the last message; a child that
// already had its one reminder does too, unless the reminder turn ended
// with processes the child started still alive (liveness names that turn):
// then the child is held, see ReportStateHeld. A worker that publishes no
// liveness never holds a child.
func ReportStateOf(
	handbackTool bool,
	report *SubagentReport,
	lastTurn *MaterializedTurnOutcome,
	liveness *Liveness,
	inFlight []string,
	nowMs int64,
) ReportState {
	if !handbackTool || lastTurn == nil {
		return ReportStateFallback{}
	}
	turn := lastTurn
	if report.Handback != nil && report.Handback.Answers(turn.CommandID) {
		return ReportStateDelivered{Message: report.Handback.Message}
	}
	finished := subagentTurnFinished(turn)
	reminderFailed := report.ReminderFailedFor != nil && *report.ReminderFailedFor == turn.CommandID
	// A follow-up the worker refused as stale ends the hold: the child's last
	// message is reported.
	if finished && IsHandbackReminder(turn.CommandID) && !reminderFailed {
		for _, kind := range []HoldFollowUpKind{HoldFollowUpKindProcessesExited, HoldFollowUpKindGiveUp} {
			if slices.Contains(inFlight, HoldFollowUpCommandID(kind, turn.CompletedOrdinal)) {
				return ReportStatePending{Remind: false}
			}
		}
		// Nobody can say whether the child still waits: not an ending.
		if !liveness.Known {
			return ReportStatePending{Remind: false}
		}
		known := liveness.Liveness
		if known == nil || known.CompletedOrdinal != turn.CompletedOrdinal {
			return ReportStateFallback{}
		}
		giveUpAtMs := subagentSaturatingAdd(turn.CompletedAtMs, HoldGiveUpMs)
		var followUp *HoldFollowUpKind
		switch {
		case known.Alive == 0:
			kind := HoldFollowUpKindProcessesExited
			followUp = &kind
		case nowMs >= giveUpAtMs:
			kind := HoldFollowUpKindGiveUp
			followUp = &kind
		}
		waitingOn := []string{}
		if turn.WorkFacts != nil {
			waitingOn = RunningProcessLines(turn.WorkFacts)
		}
		return ReportStateHeld{
			SinceMs:    turn.CompletedAtMs,
			GiveUpAtMs: giveUpAtMs,
			WaitingOn:  waitingOn,
			FollowUp:   followUp,
		}
	}
	if kind, _, ok := HoldFollowUpOf(turn.CommandID); !finished || IsHandbackReminder(turn.CommandID) ||
		(ok && kind == HoldFollowUpKindGiveUp) || reminderFailed {
		return ReportStateFallback{}
	}
	reminder := report.Reminder
	if reminder == nil || reminder.ForCommandID != turn.CommandID {
		return ReportStatePending{Remind: true}
	}
	if slices.Contains(inFlight, reminder.CommandID) ||
		subagentSaturatingSub(nowMs, reminder.SentAtMs) < HandbackReminderGraceMs {
		return ReportStatePending{Remind: false}
	}
	return ReportStateFallback{}
}

// subagentSaturatingAdd is i64::saturating_add.
func subagentSaturatingAdd(a, b int64) int64 {
	sum := a + b
	switch {
	case b > 0 && sum < a:
		return math.MaxInt64
	case b < 0 && sum > a:
		return math.MinInt64
	}
	return sum
}

// subagentSaturatingSub is i64::saturating_sub.
func subagentSaturatingSub(a, b int64) int64 {
	difference := a - b
	switch {
	case b < 0 && difference < a:
		return math.MaxInt64
	case b > 0 && difference > a:
		return math.MinInt64
	}
	return difference
}

// StoppedSubagent is a sub-agent that Mjolnir stopped when its parent was
// suspended. The parent's record keeps one of these per child until the
// parent's model has been told, on the first prompt after the parent
// resumes.
type StoppedSubagent struct {
	ChildSessionID string `json:"child_session_id"`
	// Title is the child's listed title when it was stopped.
	Title string `json:"title"`
	// Task is one line of the task the parent gave it, when the spawn
	// recorded one.
	Task *string `json:"task,omitempty"`
	// HandedBack says whether the child had handed back its report for the
	// parent's newest task; see HasHandedBack.
	HandedBack bool `json:"handed_back"`
}

func (s *StoppedSubagent) UnmarshalJSON(data []byte) error {
	type plain StoppedSubagent
	return transcriptStrictDecode(data, (*plain)(s), false)
}

// StoppedOracleID is the key under which a stopped oracle is kept in the
// parent's stopped list. It stands in for the oracle's session ID, so that
// ID never reaches the primary: the list is rendered into its prompt
// context.
const StoppedOracleID = "oracle"

// StoppedOracleLine is the one line a primary is told when its oracle was
// stopped.
const StoppedOracleLine = "The oracle was stopped. If you were waiting for its answer, ask again; a fresh oracle will answer."

// StoppedSubagentOracle is the list entry for a stopped oracle: no session
// ID and no prompt.
func StoppedSubagentOracle() StoppedSubagent {
	return StoppedSubagent{ChildSessionID: StoppedOracleID, Title: "oracle", HandedBack: true}
}

// IsOracle reports whether this entry is the stopped oracle, which is
// rendered as StoppedOracleLine and never as a child.
func (s *StoppedSubagent) IsOracle() bool { return s.ChildSessionID == StoppedOracleID }

// InPlaceSubagent is a child whose parent remains live across an in-place
// harness swap.
type InPlaceSubagent struct {
	ChildSessionID string
	// Name is the child's name as the session list shows it.
	Name  string
	State InPlaceSubagentState
}

type InPlaceSubagentState string

const (
	InPlaceSubagentStateRunning InPlaceSubagentState = "running"
	InPlaceSubagentStateParked  InPlaceSubagentState = "parked"
)

func (s InPlaceSubagentState) label() string { return string(s) }

// InPlaceSubagentsTag is the reserved hidden-context tag for children
// retained across an in-place swap.
const InPlaceSubagentsTag = "mj-in-place-subagents"

func safeSubagentName(name string) string {
	return strings.ReplaceAll(strings.ReplaceAll(BoundedOneLine(name, StoppedTaskChars), "<", "‹"), ">", "›")
}

// InPlaceSubagentsPromptContext is the hidden instructions for the first
// prompt after an in-place swap. The new harness has no outstanding tool
// call stack from the old harness process.
func InPlaceSubagentsPromptContext(children []InPlaceSubagent) (string, bool) {
	if len(children) == 0 {
		return "", false
	}
	lines := []string{
		"<" + InPlaceSubagentsTag + ">",
		"These sub-agents remain attached to this session after its in-place harness swap:",
	}
	for _, child := range children {
		lines = append(lines, fmt.Sprintf("- %s (child_session_id %s; %s)",
			safeSubagentName(child.Name), child.ChildSessionID, child.State.label()))
	}
	lines = append(lines,
		"Any wait that was in progress during the swap was interrupted; reissue wait without arguments to collect reports from these children.",
		"Before resending a message, call list_agents and check pending_messages so you do not send it twice.",
		"</"+InPlaceSubagentsTag+">")
	return strings.Join(lines, "\n"), true
}

// InPlaceSubagentsNotice is the conversation line that tells the person the
// same children are still live.
func InPlaceSubagentsNotice(children []InPlaceSubagent) (string, bool) {
	if len(children) == 0 {
		return "", false
	}
	noun := "sub-agents"
	if len(children) == 1 {
		noun = "sub-agent"
	}
	return fmt.Sprintf("In-place harness swap kept %d %s attached. Any wait in progress was interrupted; reissue wait without arguments to collect reports. Check list_agents for pending_messages before resending a message.",
		len(children), noun), true
}

// StoppedTaskChars is the longest task line a StoppedSubagent keeps, in
// characters.
const StoppedTaskChars = 160

// handbackNoteOpening is how HandbackPromptNote begins, whatever directory
// it names.
const handbackNoteOpening = "You are a Mjolnir sub-agent. "

// TaskSummary is one line of the task a parent gave a child, from the
// child's first prompt: the parent's instructions without the handback note
// Mjolnir puts before them, and without the context and file ranges the
// parent attached after them. reportDir is the directory that note names,
// when one was recorded.
func TaskSummary(initialPrompt string, reportDir *string) (string, bool) {
	task := initialPrompt
	if reportDir != nil {
		if rest, ok := strings.CutPrefix(task, ReportDirectoryNote(*reportDir)); ok {
			if rest, ok := strings.CutPrefix(rest, "\n\n"); ok {
				task = rest
			}
		}
	}
	stripped := false
	if reportDir != nil {
		if rest, ok := strings.CutPrefix(task, HandbackPromptNote(*reportDir)); ok {
			if rest, ok := strings.CutPrefix(rest, "\n\n"); ok {
				task = rest
				stripped = true
			}
		}
	}
	if !stripped && strings.HasPrefix(task, handbackNoteOpening) {
		if _, rest, ok := strings.Cut(task, "\n\n"); ok {
			task = rest
		}
	}
	if rest, ok := strings.CutPrefix(task, ReadOnlyTaskScope); ok {
		if rest, ok := strings.CutPrefix(rest, "\n\n"); ok {
			task = rest
		}
	}
	for _, attachment := range []string{"\n\n<parent_context>", "\n\n--- source "} {
		if instructions, _, ok := strings.Cut(task, attachment); ok {
			task = instructions
		}
	}
	line := strings.Join(strings.FieldsFunc(task, unicode.IsSpace), " ")
	if line == "" {
		return "", false
	}
	characters := []rune(line)
	if len(characters) <= StoppedTaskChars {
		return line, true
	}
	kept := string(characters[:StoppedTaskChars-1])
	return strings.TrimRightFunc(kept, unicode.IsSpace) + "…", true
}

// StoppedSubagentsTag is the tag of the hidden block that tells a resumed
// parent's model which sub-agents its suspend stopped. It is a reserved mj-
// block, so the relay's StripHiddenPromptContext removes it wherever a
// harness copied the prompt into text a person sees.
const StoppedSubagentsTag = "mj-stopped-subagents"

// StoppedSubagentsPromptContext is the hidden context the first prompt
// after a parent resumes carries when its suspend stopped sub-agents: one
// line per child, then what the model should do about them. False when
// nothing was stopped.
func StoppedSubagentsPromptContext(stopped []StoppedSubagent) (string, bool) {
	if len(stopped) == 0 {
		return "", false
	}
	oracleStopped, children := subagentSplitOracle(stopped)
	if len(children) == 0 {
		return "<" + StoppedSubagentsTag + ">\n" + StoppedOracleLine + "\n</" + StoppedSubagentsTag + ">", true
	}
	total := len(children)
	notHandedBack := 0
	for _, child := range children {
		if !child.HandedBack {
			notHandedBack++
		}
	}
	lines := []string{
		"<" + StoppedSubagentsTag + ">",
		fmt.Sprintf("Mjolnir stopped %s when this session was suspended:", Counted(total, "sub-agent", "sub-agents")),
	}
	for _, child := range children {
		task := ""
		if child.Task != nil {
			task = ", task: " + *child.Task
		}
		handedBack := "had not handed back"
		if child.HandedBack {
			handedBack = "had handed back"
		}
		lines = append(lines, fmt.Sprintf("- \"%s\" (child_session_id %s)%s; %s",
			child.Title, child.ChildSessionID, task, handedBack))
	}
	var outcome string
	switch {
	case total == 1 && notHandedBack == 1:
		outcome = "Its work was not handed back; spawn it again if you still need it."
	case total == 1:
		outcome = "It had handed back its report before it was stopped."
	case notHandedBack == 0:
		outcome = "Each had handed back its report before it was stopped."
	case total == notHandedBack:
		outcome = "Their work was not handed back; spawn them again if you still need it."
	default:
		outcome = "The work of those that had not handed back was lost; spawn them again if you still need it."
	}
	lines = append(lines, outcome,
		"A stopped sub-agent no longer exists: wait, send_message and close cannot reach it.")
	if oracleStopped {
		lines = append(lines, StoppedOracleLine)
	}
	lines = append(lines, "</"+StoppedSubagentsTag+">")
	return strings.Join(lines, "\n"), true
}

// subagentSplitOracle reports whether the stopped list holds the oracle and
// returns the other entries.
func subagentSplitOracle(stopped []StoppedSubagent) (bool, []StoppedSubagent) {
	oracleStopped := false
	children := []StoppedSubagent{}
	for i := range stopped {
		if stopped[i].IsOracle() {
			oracleStopped = true
		} else {
			children = append(children, stopped[i])
		}
	}
	return oracleStopped, children
}

// StoppedSubagentsNotice is the conversation line a person sees when a
// parent whose suspend stopped sub-agents resumes. It names only the
// children that had not handed back, since those lost work; the idle ones
// that had are counted. False when nothing was stopped.
func StoppedSubagentsNotice(stopped []StoppedSubagent) (string, bool) {
	if len(stopped) == 0 {
		return "", false
	}
	oracleStopped, children := subagentSplitOracle(stopped)
	notice, ok := stoppedChildrenNotice(children)
	if !oracleStopped {
		return notice, ok
	}
	if ok {
		return notice + " " + StoppedOracleLine, true
	}
	return StoppedOracleLine, true
}

func stoppedChildrenNotice(stopped []StoppedSubagent) (string, bool) {
	if len(stopped) == 0 {
		return "", false
	}
	var working []string
	for _, child := range stopped {
		if !child.HandedBack {
			working = append(working, "\""+child.Title+"\"")
		}
	}
	idle := len(stopped) - len(working)
	if len(working) == 0 {
		return fmt.Sprintf("Suspend stopped %s.", Counted(idle, "idle sub-agent", "idle sub-agents")), true
	}
	notice := fmt.Sprintf("Suspend stopped %s: %s",
		Counted(len(working), "working sub-agent", "working sub-agents"), strings.Join(working, ", "))
	if idle > 0 {
		notice += fmt.Sprintf("; %s stopped too", Counted(idle, "idle sub-agent was", "idle sub-agents were"))
	}
	return notice + ".", true
}

// SuspendWarning is what a person is told before a suspend stops
// sub-agents that have not handed back their reports. False when every
// child had handed back: those are stopped without a word, since their
// reports already reached the parent.
func SuspendWarning(notHandedBack int) (string, bool) {
	switch notHandedBack {
	case 0:
		return "", false
	case 1:
		return "1 sub-agent has not handed back; suspending stops it", true
	}
	return fmt.Sprintf("%d sub-agents have not handed back; suspending stops them", notHandedBack), true
}

// SuspendWarningNaming is the same warning naming the children by their
// listed titles, for a person choosing whether to suspend: up to three by
// name, then how many more. The API keeps SuspendWarning's count.
func SuspendWarningNaming(titles []string) (string, bool) {
	switch len(titles) {
	case 0:
		return "", false
	case 1:
		return "Sub-agent " + QuotedTitles(titles) + " has not handed back; suspending stops it", true
	}
	return "Sub-agents " + QuotedTitles(titles) + " have not handed back; suspending stops them", true
}

// QuotedTitles puts titles in quotes, in a sentence: "a", "a" and "b",
// "a", "b" and "c". Past three, the rest are counted: "a", "b", "c" and 2
// more.
func QuotedTitles(titles []string) string {
	const named = 3
	var quoted []string
	for i, title := range titles {
		if i == named {
			break
		}
		quoted = append(quoted, "\""+title+"\"")
	}
	var last string
	if len(titles) > named {
		last = fmt.Sprintf("%d more", len(titles)-named)
	} else {
		if len(quoted) == 0 {
			return ""
		}
		last = quoted[len(quoted)-1]
		quoted = quoted[:len(quoted)-1]
	}
	if len(quoted) == 0 {
		return last
	}
	return strings.Join(quoted, ", ") + " and " + last
}

// HasHandedBack reports whether a child had handed back its report for the
// parent's newest task: it is not working, a finished turn answers the
// parent's newest prompt, and that answer is final. The answer is the
// report the child handed back, or, for a child without the tool or one
// that ignored its reminder, the last message of a turn that finished
// normally. A turn that failed, was interrupted or stopped to ask a
// question has not handed anything back.
//
// working says the child has a turn running or its execution is not idle.
func HasHandedBack(
	handbackTool bool,
	report *SubagentReport,
	working bool,
	lastTurn *MaterializedTurnOutcome,
	liveness *Liveness,
	nowMs int64,
) bool {
	if lastTurn == nil {
		return false
	}
	if working || AwaitingPrompt(report.AwaitedOrdinal, lastTurn) {
		return false
	}
	switch ReportStateOf(handbackTool, report, lastTurn, liveness, nil, nowMs).(type) {
	case ReportStateDelivered:
		return true
	case ReportStateFallback:
		return subagentTurnFinished(lastTurn)
	}
	return false
}
