// Port of mj-core/src/elicitation.rs.
//
// ACP form elicitation shapes shared by the runtime, relay, and chat UI.
//
// The ACP crate version Hel currently uses predates enum-option
// descriptions, so requests are parsed from their raw JSON here. Keeping
// that interpretation in one place also lets Claude and Codex metadata
// conventions share one UI.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"reflect"
	"strings"
	"unicode"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// OperatorAwayAction is the resolution action for a request refused while
// the operator is away.
const OperatorAwayAction = "away"

// ToolPermissionIDPrefix: worker-owned permission forms use this namespace
// in durable resolution records.
const ToolPermissionIDPrefix = "tool-permission-"

const MaxElicitationBytes = 64 * 1024
const maxFields = 32
const maxOptions = 100
const maxTextBytes = 16 * 1024

type ElicitationRequest struct {
	ID          string             `json:"id"`
	Message     string             `json:"message"`
	Title       *string            `json:"title,omitempty"`
	Description *string            `json:"description,omitempty"`
	Fields      []ElicitationField `json:"fields"`
	// ToolPermission is set when the form shows a tool permission request.
	// Its Skip rejects only that tool call and its Cancel stops the agent's
	// turn.
	ToolPermission *ToolPermissionForm `json:"tool_permission,omitempty"`
}

func (r *ElicitationRequest) UnmarshalJSON(data []byte) error {
	type plain ElicitationRequest
	return transcriptStrictDecode(data, (*plain)(r), true)
}

// ToolPermissionForm is what a tool permission form offers beyond the
// request's own options.
type ToolPermissionForm struct {
	// Skip says whether the request has a one-time rejection for Skip to
	// select. A request without one offers no Skip.
	Skip bool `json:"skip"`
}

func (f *ToolPermissionForm) UnmarshalJSON(data []byte) error {
	type plain ToolPermissionForm
	return transcriptStrictDecode(data, (*plain)(f), true)
}

// QuestionText renders a question for an agent receiving it through its
// mailbox.
func (r *ElicitationRequest) QuestionText() string {
	panic("unported: mj-core/src/elicitation.rs ElicitationRequest::question_text")
}

// GoesToParent reports whether a managed child sends this form to its
// parent agent instead of a person. A parent answers questions with a
// message, but it has no way to grant a tool permission or approve a plan,
// so those stay open for a person, as harnesses do for their native
// sub-agents. A secret stays with the person too: a message would keep it
// in plain text, where a form answer is masked.
func (r *ElicitationRequest) GoesToParent() bool {
	if r.ToolPermission != nil || IsPlanReviewID(r.ID) {
		return false
	}
	for _, field := range r.Fields {
		if field.Secret {
			return false
		}
	}
	return true
}

// OffersSkip reports whether the form offers Skip (the decline action).
// Every surface asks this, and HarnessResponse refuses a Skip it denies.
func (r *ElicitationRequest) OffersSkip() bool {
	return r.ToolPermission == nil || r.ToolPermission.Skip
}

func ElicitationRequestFromACPParams(id string, params json.RawMessage) (ElicitationRequest, error) {
	panic("unported: mj-core/src/elicitation.rs ElicitationRequest::from_acp_params")
}

// ValidateResponse reports whether a submitted answer satisfies this
// request's own schema. The error is the user-facing reason.
func (r *ElicitationRequest) ValidateResponse(response *ElicitationResponse) error {
	_, err := r.HarnessResponse(*response)
	return err
}

// HarnessResponse is the answer the harness receives for a submitted one,
// or why the submitted answer does not fit this request (the error text is
// shown as is).
//
// Every surface (terminal, web, `mj respond`) submits each field's own
// value: a select's picks, and the text of its companion field. How the two
// combine depends on the companion's CompanionRole:
//
//   - CompanionRoleAlongside (claude-agent-acp's "Other") goes beside any
//     pick: the bridge joins a multi-select's picks with the text and
//     attaches the text to a single pick as a note, so both are forwarded.
//   - CompanionRoleAlternative (codex-acp's note) is the answer in place of
//     a pick. Its text becomes the select's own value, the Air convention
//     codex-acp reads on both its question paths (typedChoiceAnswer and the
//     async answer), and a pick sent with it is refused: codex-acp's async
//     path would read the note in place of the pick.
//   - CompanionRoleDetail (plan-review feedback on Revise) goes with its
//     option.
//
// A required select answered only by its companion gets the text as its
// own value, so the harness never receives an answer that omits a required
// field. Only that move puts text outside the offered options into a
// select; a submitted select value must still be one of them.
//
// Surfaces that answer out of band (the phone posts an answer over HTTP)
// can send content for a request the agent has already replaced, or content
// the agent never offered. Checking the answer against the fields this
// request actually published means a stale or malformed payload is refused
// instead of being forwarded as something the harness never asked for.
func (r *ElicitationRequest) HarnessResponse(response ElicitationResponse) (ElicitationResponse, error) {
	panic("unported: mj-core/src/elicitation.rs ElicitationRequest::harness_response")
}

// Companion is the select's companion text field and its role: the field
// whose CustomAnswerFor names the select, as its index in Fields. The first
// one counts. This is the one rule every surface lays out by; the web
// viewer's buildElicitationForm mirrors it.
func (r *ElicitationRequest) Companion(selectField *ElicitationField) (int, CompanionRole, bool) {
	panic("unported: mj-core/src/elicitation.rs ElicitationRequest::companion")
}

// FieldIsSecret: companion answers inherit the secrecy of the field they
// answer.
func (r *ElicitationRequest) FieldIsSecret(field *ElicitationField) bool {
	if field.Secret {
		return true
	}
	if field.CustomAnswerFor == nil {
		return false
	}
	for i := range r.Fields {
		if r.Fields[i].ID == *field.CustomAnswerFor {
			return r.Fields[i].Secret
		}
	}
	return false
}

// ReplyText is the user's reply for conversation history. Mask secrets
// here, before the runtime event or durable journal can retain them.
func (r *ElicitationRequest) ReplyText(response *ElicitationResponse) string {
	panic("unported: mj-core/src/elicitation.rs ElicitationRequest::reply_text")
}

// ValidateFieldValue reports whether one answered value satisfies the
// constraints of the field it answers. Presence and required-ness are the
// caller's business: this checks only that the value the caller does have
// is one the schema allows. The error text is shown as is.
func ValidateFieldValue(field *ElicitationField, value ElicitationValue) error {
	panic("unported: mj-core/src/elicitation.rs validate_field_value")
}

// validateTextFormat checks the string formats the ACP schema can request.
// Unknown formats are accepted: the schema keyword is open-ended and
// refusing an answer over a format Hel does not model would block a
// question the agent can otherwise use. The error completes a sentence
// that starts with the field title.
func validateTextFormat(value, format string) error {
	panic("unported: mj-core/src/elicitation.rs validate_text_format")
}

// CompanionAlongside is the CustomAnswerOption of a companion that goes
// beside any pick of its select, or answers on its own when nothing is
// picked.
//
// It is not an option value. It rides in the existing field so the relayed,
// journaled and stored form keeps its shape: a reader from before it finds
// no option of that value and treats the companion as the select's
// free-text alternative, the earlier behavior, which the bridge also reads.
const CompanionAlongside = "\x1falongside"

// CompanionRole is how a select's companion text field (the field whose
// CustomAnswerFor names the select) takes part in the select's answer. The
// parser decides it from the bridge's own marker on the field.
type CompanionRole int

const (
	// CompanionRoleAlternative is the answer in place of any pick:
	// codex-acp's user_note, which its async question path reads instead of
	// the pick.
	CompanionRoleAlternative CompanionRole = iota
	// CompanionRoleAlongside is beside any pick, or the whole answer when
	// nothing is picked: claude-agent-acp's "Other" ("Type your own answer,
	// or add a note to the option you chose above").
	CompanionRoleAlongside
	// CompanionRoleDetail is the detail of the one option CustomAnswerOption
	// names, sent with it: Mjolnir's plan-review feedback on Revise.
	CompanionRoleDetail
)

type ElicitationField struct {
	ID              string  `json:"id"`
	Title           string  `json:"title"`
	Description     *string `json:"description,omitempty"`
	Required        bool    `json:"required"`
	Secret          bool    `json:"secret" serde:"default"`
	CustomAnswerFor *string `json:"custom_answer_for,omitempty"`
	// CustomAnswerOption is, for a companion, the option its text belongs
	// to, or CompanionAlongside. See CompanionRole.
	CustomAnswerOption *string `json:"custom_answer_option,omitempty"`
	// Kind is flattened into the field's own object.
	Kind ElicitationFieldKind `json:"-"`
}

// MarshalJSON writes the field's members followed by its flattened kind.
func (f ElicitationField) MarshalJSON() ([]byte, error) {
	type plain ElicitationField
	return archiveMarshalFlattened(plain(f), f.Kind)
}

// UnmarshalJSON reads the field's members and decodes the rest as its kind,
// whose deny_unknown_fields rejects anything else.
func (f *ElicitationField) UnmarshalJSON(data []byte) error {
	type plain ElicitationField
	var v plain
	if err := transcriptStrictDecode(data, &v, false); err != nil {
		return err
	}
	members, err := transcriptObject(data)
	if err != nil {
		return err
	}
	own, _ := transcriptFields(reflect.TypeOf((*plain)(nil)))
	for _, name := range own {
		delete(members, name)
	}
	rest, err := json.Marshal(members)
	if err != nil {
		return err
	}
	kind, err := DecodeElicitationFieldKind(rest)
	if err != nil {
		return err
	}
	v.Kind = kind
	*f = ElicitationField(v)
	return nil
}

// CompanionRole is the role this field plays as a companion. Only
// meaningful for a text field whose CustomAnswerFor names a select.
func (f *ElicitationField) CompanionRole() CompanionRole {
	switch {
	case f.CustomAnswerOption == nil:
		return CompanionRoleAlternative
	case *f.CustomAnswerOption == CompanionAlongside:
		return CompanionRoleAlongside
	}
	return CompanionRoleDetail
}

func (f *ElicitationField) replyValueText(value ElicitationValue) string {
	panic("unported: mj-core/src/elicitation.rs ElicitationField::reply_value_text")
}

// ElicitationFieldKind is a field's type and constraints, tagged by "kind"
// with unknown fields denied. Decode with DecodeElicitationFieldKind.
type ElicitationFieldKind interface {
	isElicitationFieldKind()
}

type ElicitationFieldKindText struct {
	Default   *string `json:"default,omitempty"`
	MinLength *int    `json:"min_length,omitempty"`
	MaxLength *int    `json:"max_length,omitempty"`
	Pattern   *string `json:"pattern,omitempty"`
	Format    *string `json:"format,omitempty"`
}

type ElicitationFieldKindSingleSelect struct {
	Options []ElicitationOption `json:"options"`
	Default *string             `json:"default,omitempty"`
}

type ElicitationFieldKindMultiSelect struct {
	Options  []ElicitationOption `json:"options"`
	Default  []string            `json:"default,omitempty"`
	MinItems *int                `json:"min_items,omitempty"`
	MaxItems *int                `json:"max_items,omitempty"`
}

type ElicitationFieldKindBoolean struct {
	Default *bool `json:"default,omitempty"`
}

type ElicitationFieldKindInteger struct {
	Default *int64 `json:"default,omitempty"`
	Minimum *int64 `json:"minimum,omitempty"`
	Maximum *int64 `json:"maximum,omitempty"`
}

// ElicitationFieldKindNumber writes its numbers as serde_json writes an f64.
type ElicitationFieldKindNumber struct {
	Default *float64 `json:"default,omitempty"`
	Minimum *float64 `json:"minimum,omitempty"`
	Maximum *float64 `json:"maximum,omitempty"`
}

func (ElicitationFieldKindText) isElicitationFieldKind()         {}
func (ElicitationFieldKindSingleSelect) isElicitationFieldKind() {}
func (ElicitationFieldKindMultiSelect) isElicitationFieldKind()  {}
func (ElicitationFieldKindBoolean) isElicitationFieldKind()      {}
func (ElicitationFieldKindInteger) isElicitationFieldKind()      {}
func (ElicitationFieldKindNumber) isElicitationFieldKind()       {}

func (k ElicitationFieldKindText) MarshalJSON() ([]byte, error) {
	type plain ElicitationFieldKindText
	return transcriptMarshalTagged("kind", "text", plain(k))
}

func (k ElicitationFieldKindSingleSelect) MarshalJSON() ([]byte, error) {
	type plain ElicitationFieldKindSingleSelect
	return transcriptMarshalTagged("kind", "single_select", plain(k))
}

func (k ElicitationFieldKindMultiSelect) MarshalJSON() ([]byte, error) {
	type plain ElicitationFieldKindMultiSelect
	return transcriptMarshalTagged("kind", "multi_select", plain(k))
}

func (k ElicitationFieldKindBoolean) MarshalJSON() ([]byte, error) {
	type plain ElicitationFieldKindBoolean
	return transcriptMarshalTagged("kind", "boolean", plain(k))
}

func (k ElicitationFieldKindInteger) MarshalJSON() ([]byte, error) {
	type plain ElicitationFieldKindInteger
	return transcriptMarshalTagged("kind", "integer", plain(k))
}

func (k ElicitationFieldKindNumber) MarshalJSON() ([]byte, error) {
	members := []transcriptMember{{"kind", "number"}}
	for _, member := range []struct {
		key   string
		value *float64
	}{{"default", k.Default}, {"minimum", k.Minimum}, {"maximum", k.Maximum}} {
		if member.value != nil {
			members = append(members, transcriptMember{member.key, ElicitationValueNumber(*member.value)})
		}
	}
	return transcriptMarshalMembers(members)
}

// DecodeElicitationFieldKind reads an ElicitationFieldKind, tagged by
// "kind", denying unknown fields.
func DecodeElicitationFieldKind(data []byte) (ElicitationFieldKind, error) {
	tag, err := transcriptTag(data, "kind", "ElicitationFieldKind")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "text":
		type plain ElicitationFieldKindText
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return ElicitationFieldKindText(v), err
	case "single_select":
		type plain ElicitationFieldKindSingleSelect
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return ElicitationFieldKindSingleSelect(v), err
	case "multi_select":
		type plain ElicitationFieldKindMultiSelect
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return ElicitationFieldKindMultiSelect(v), err
	case "boolean":
		type plain ElicitationFieldKindBoolean
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return ElicitationFieldKindBoolean(v), err
	case "integer":
		type plain ElicitationFieldKindInteger
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return ElicitationFieldKindInteger(v), err
	case "number":
		type plain ElicitationFieldKindNumber
		var v plain
		err = transcriptStrictDecode(data, &v, true, "kind")
		return ElicitationFieldKindNumber(v), err
	}
	return nil, transcriptUnknownVariant(tag, "text", "single_select", "multi_select", "boolean",
		"integer", "number")
}

type ElicitationOption struct {
	Value       string  `json:"value"`
	Title       string  `json:"title"`
	Description *string `json:"description,omitempty"`
	Preview     *string `json:"preview,omitempty"`
}

func (o *ElicitationOption) UnmarshalJSON(data []byte) error {
	type plain ElicitationOption
	return transcriptStrictDecode(data, (*plain)(o), true)
}

// ElicitationAction is the "action" tag of an ElicitationResponse.
type ElicitationAction string

const (
	ElicitationResponseAccept  ElicitationAction = "accept"
	ElicitationResponseDecline ElicitationAction = "decline"
	ElicitationResponseCancel  ElicitationAction = "cancel"
)

// ElicitationResponse is an answer to a form, tagged by "action" with
// unknown fields denied. One struct with its tag: Content belongs to accept
// only.
type ElicitationResponse struct {
	Action  ElicitationAction
	Content map[string]ElicitationValue
}

func (r ElicitationResponse) ActionName() string { return string(r.Action) }

// Equal is the Rust PartialEq.
func (r ElicitationResponse) Equal(other ElicitationResponse) bool {
	panic("unported: mj-core/src/elicitation.rs ElicitationResponse::eq")
}

func (r ElicitationResponse) MarshalJSON() ([]byte, error) {
	if r.Action == ElicitationResponseAccept && len(r.Content) > 0 {
		return transcriptMarshalTagged("action", string(r.Action), struct {
			Content map[string]ElicitationValue `json:"content"`
		}{r.Content})
	}
	return transcriptMarshalTagged("action", string(r.Action), nil)
}

func (r *ElicitationResponse) UnmarshalJSON(data []byte) error {
	tag, err := transcriptTag(data, "action", "ElicitationResponse")
	if err != nil {
		return err
	}
	switch action := ElicitationAction(tag); action {
	case ElicitationResponseAccept:
		var v struct {
			Content map[string]json.RawMessage `json:"content,omitempty"`
		}
		if err := transcriptStrictDecode(data, &v, true, "action"); err != nil {
			return err
		}
		content := map[string]ElicitationValue{}
		for key, raw := range v.Content {
			value, err := DecodeElicitationValue(raw)
			if err != nil {
				return err
			}
			content[key] = value
		}
		*r = ElicitationResponse{Action: action, Content: content}
		return nil
	case ElicitationResponseDecline, ElicitationResponseCancel:
		if err := transcriptStrictDecode(data, &struct{}{}, true, "action"); err != nil {
			return err
		}
		*r = ElicitationResponse{Action: action}
		return nil
	}
	return transcriptUnknownVariant(tag, "accept", "decline", "cancel")
}

// ElicitationValue is one answered value, untagged on the wire. Decode with
// DecodeElicitationValue.
type ElicitationValue interface {
	isElicitationValue()
}

type ElicitationValueString string
type ElicitationValueInteger int64

// ElicitationValueNumber writes as serde_json writes an f64, always with a
// decimal point or an exponent.
type ElicitationValueNumber float64
type ElicitationValueBoolean bool
type ElicitationValueStringArray []string

func (ElicitationValueString) isElicitationValue()      {}
func (ElicitationValueInteger) isElicitationValue()     {}
func (ElicitationValueNumber) isElicitationValue()      {}
func (ElicitationValueBoolean) isElicitationValue()     {}
func (ElicitationValueStringArray) isElicitationValue() {}

func (v ElicitationValueNumber) MarshalJSON() ([]byte, error) {
	return jsonvalue.Marshal(float64(v))
}

// DecodeElicitationValue tries the variants in declaration order, as
// serde's untagged enums do: string, i64, f64, bool, then an array of
// strings.
func DecodeElicitationValue(data []byte) (ElicitationValue, error) {
	value, err := jsonvalue.Parse(data)
	if err != nil {
		return nil, err
	}
	switch value := value.(type) {
	case string:
		return ElicitationValueString(value), nil
	case int64:
		return ElicitationValueInteger(value), nil
	case uint64:
		return ElicitationValueNumber(float64(value)), nil
	case float64:
		return ElicitationValueNumber(value), nil
	case bool:
		return ElicitationValueBoolean(value), nil
	case []any:
		values := make([]string, 0, len(value))
		for _, item := range value {
			text, ok := item.(string)
			if !ok {
				return nil, errors.New("data did not match any variant of untagged enum ElicitationValue")
			}
			values = append(values, text)
		}
		return ElicitationValueStringArray(values), nil
	}
	return nil, errors.New("data did not match any variant of untagged enum ElicitationValue")
}

func elicitationParseField(fieldID string, value any, required bool) (ElicitationField, error) {
	panic("unported: mj-core/src/elicitation.rs parse_field")
}

// recommendedOptions are options labelled "(Recommended)", the way Claude's
// AskUserQuestion marks the answer it suggests. A select with no default of
// its own starts with these picked. A single-select with none of them
// starts with its first option picked, so every question of a form starts
// the same way.
func recommendedOptions(options []ElicitationOption) []ElicitationOption {
	var recommended []ElicitationOption
	for _, option := range options {
		if strings.HasSuffix(strings.ToLower(strings.TrimRightFunc(option.Title, unicode.IsSpace)), "(recommended)") {
			recommended = append(recommended, option)
		}
	}
	return recommended
}

func elicitationParseStringOptions(schema jsonvalue.Object) ([]ElicitationOption, error) {
	if _, ok := schema.Get("oneOf"); ok {
		return elicitationParseOptions(schema, "oneOf")
	}
	if _, ok := schema.Get("enum"); ok {
		return elicitationParseEnumOptions(schema)
	}
	return []ElicitationOption{}, nil
}

func elicitationParseOptions(schema jsonvalue.Object, key string) ([]ElicitationOption, error) {
	panic("unported: mj-core/src/elicitation.rs parse_options")
}

func elicitationParseEnumOptions(schema jsonvalue.Object) ([]ElicitationOption, error) {
	panic("unported: mj-core/src/elicitation.rs parse_enum_options")
}

// elicitationObject is Rust's object helper (renamed: package core is flat).
func elicitationObject(value any, what string) (jsonvalue.Object, error) {
	object, ok := value.(jsonvalue.Object)
	if !ok || object == nil {
		return nil, fmt.Errorf("%s must be an object", what)
	}
	return object, nil
}

// elicitationString is Rust's string helper (renamed: string is a Go type).
func elicitationString(object jsonvalue.Object, key string) (string, error) {
	value, _ := object.Get(key)
	text, ok := value.(string)
	if !ok {
		return "", fmt.Errorf("%s must be a string", key)
	}
	return text, nil
}

func elicitationEnsureText(value, what string) error {
	if len(value) > maxTextBytes {
		return fmt.Errorf("%s is too long", what)
	}
	return nil
}

func elicitationOptionalText(object jsonvalue.Object, key string) (*string, error) {
	value, ok := object.Get(key)
	if !ok || value == nil {
		return nil, nil
	}
	text, ok := value.(string)
	if !ok {
		return nil, fmt.Errorf("%s must be a string", key)
	}
	if err := elicitationEnsureText(text, key); err != nil {
		return nil, err
	}
	return &text, nil
}

func elicitationOptionalUsize(object jsonvalue.Object, key string) (*int, error) {
	panic("unported: mj-core/src/elicitation.rs optional_usize")
}

func elicitationOptionalU64(object jsonvalue.Object, key string) (*uint64, error) {
	panic("unported: mj-core/src/elicitation.rs optional_u64")
}

func elicitationOptionalI64(object jsonvalue.Object, key string) (*int64, error) {
	panic("unported: mj-core/src/elicitation.rs optional_i64")
}

func elicitationOptionalF64(object jsonvalue.Object, key string) (*float64, error) {
	panic("unported: mj-core/src/elicitation.rs optional_f64")
}

func elicitationOptionalBool(object jsonvalue.Object, key string) (*bool, error) {
	value, ok := object.Get(key)
	if !ok || value == nil {
		return nil, nil
	}
	flag, ok := value.(bool)
	if !ok {
		return nil, fmt.Errorf("%s must be a boolean", key)
	}
	return &flag, nil
}

func elicitationOptionalStringArray(object jsonvalue.Object, key string) (*[]string, error) {
	panic("unported: mj-core/src/elicitation.rs optional_string_array")
}

// elicitationStringSet reads an optional array of strings as a set.
func elicitationStringSet(value any, present bool, what string) (map[string]struct{}, error) {
	panic("unported: mj-core/src/elicitation.rs string_set")
}
