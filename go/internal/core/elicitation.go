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
	"math"
	"net/url"
	"reflect"
	"regexp"
	"slices"
	"sort"
	"strconv"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

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

// Equal is Rust's derived PartialEq: an empty list equals an absent one, as
// two empty Vecs do.
func (r *ElicitationRequest) Equal(other *ElicitationRequest) bool {
	if r.ID != other.ID || r.Message != other.Message ||
		!configOptionalEqual(r.Title, other.Title) ||
		!configOptionalEqual(r.Description, other.Description) ||
		len(r.Fields) != len(other.Fields) {
		return false
	}
	for i := range r.Fields {
		if !r.Fields[i].Equal(&other.Fields[i]) {
			return false
		}
	}
	if r.ToolPermission == nil || other.ToolPermission == nil {
		return r.ToolPermission == nil && other.ToolPermission == nil
	}
	return configOptionalEqual(r.ToolPermission.ToolCallID, other.ToolPermission.ToolCallID) &&
		r.ToolPermission.Skip == other.ToolPermission.Skip
}

// Equal is Rust's derived PartialEq.
func (f *ElicitationField) Equal(other *ElicitationField) bool {
	if f.ID != other.ID || f.Title != other.Title || f.Required != other.Required || f.Secret != other.Secret ||
		!configOptionalEqual(f.Description, other.Description) ||
		!configOptionalEqual(f.CustomAnswerFor, other.CustomAnswerFor) ||
		!configOptionalEqual(f.CustomAnswerOption, other.CustomAnswerOption) {
		return false
	}
	optionsEqual := func(a, b []ElicitationOption) bool {
		return slices.EqualFunc(a, b, func(a, b ElicitationOption) bool {
			return a.Value == b.Value && a.Title == b.Title &&
				configOptionalEqual(a.Description, b.Description) && configOptionalEqual(a.Preview, b.Preview)
		})
	}
	switch kind := f.Kind.(type) {
	case ElicitationFieldKindText:
		other, ok := other.Kind.(ElicitationFieldKindText)
		return ok && configOptionalEqual(kind.Default, other.Default) &&
			configOptionalEqual(kind.MinLength, other.MinLength) && configOptionalEqual(kind.MaxLength, other.MaxLength) &&
			configOptionalEqual(kind.Pattern, other.Pattern) && configOptionalEqual(kind.Format, other.Format)
	case ElicitationFieldKindSingleSelect:
		other, ok := other.Kind.(ElicitationFieldKindSingleSelect)
		return ok && optionsEqual(kind.Options, other.Options) && configOptionalEqual(kind.Default, other.Default)
	case ElicitationFieldKindMultiSelect:
		other, ok := other.Kind.(ElicitationFieldKindMultiSelect)
		return ok && optionsEqual(kind.Options, other.Options) && slices.Equal(kind.Default, other.Default) &&
			configOptionalEqual(kind.MinItems, other.MinItems) && configOptionalEqual(kind.MaxItems, other.MaxItems)
	case ElicitationFieldKindBoolean:
		other, ok := other.Kind.(ElicitationFieldKindBoolean)
		return ok && configOptionalEqual(kind.Default, other.Default)
	case ElicitationFieldKindInteger:
		other, ok := other.Kind.(ElicitationFieldKindInteger)
		return ok && configOptionalEqual(kind.Default, other.Default) &&
			configOptionalEqual(kind.Minimum, other.Minimum) && configOptionalEqual(kind.Maximum, other.Maximum)
	case ElicitationFieldKindNumber:
		other, ok := other.Kind.(ElicitationFieldKindNumber)
		return ok && configOptionalEqual(kind.Default, other.Default) &&
			configOptionalEqual(kind.Minimum, other.Minimum) && configOptionalEqual(kind.Maximum, other.Maximum)
	}
	return f.Kind == nil && other.Kind == nil
}

// ToolPermissionForm is what a tool permission form offers beyond the
// request's own options.
type ToolPermissionForm struct {
	// ToolCallID is the raw ACP tool call ID when the call belongs to this
	// transcript.
	ToolCallID *string `json:"tool_call_id,omitempty"`
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
	lines := []string{r.Message}
	if r.Title != nil {
		lines = append(lines, *r.Title)
	}
	if r.Description != nil {
		lines = append(lines, *r.Description)
	}
	for i := range r.Fields {
		field := &r.Fields[i]
		lines = append(lines, "\n"+field.Title)
		if field.Description != nil {
			lines = append(lines, *field.Description)
		}
		for _, option := range elicitationSelectOptions(field.Kind) {
			lines = append(lines, "- "+option.Title)
			if option.Description != nil {
				lines = append(lines, *option.Description)
			}
		}
	}
	return strings.Join(lines, "\n")
}

// elicitationSelectOptions is the options of a single- or multi-select; nil
// for any other kind.
func elicitationSelectOptions(kind ElicitationFieldKind) []ElicitationOption {
	switch kind := kind.(type) {
	case ElicitationFieldKindSingleSelect:
		return kind.Options
	case ElicitationFieldKindMultiSelect:
		return kind.Options
	}
	return nil
}

func elicitationIsSelect(kind ElicitationFieldKind) bool {
	switch kind.(type) {
	case ElicitationFieldKindSingleSelect, ElicitationFieldKindMultiSelect:
		return true
	}
	return false
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
	encoded, err := transcriptNormalizeValue(params)
	if err != nil {
		return ElicitationRequest{}, fmt.Errorf("serialize ACP elicitation request: %w", err)
	}
	if len(encoded) > MaxElicitationBytes {
		return ElicitationRequest{}, fmt.Errorf("elicitation request exceeds %d bytes", MaxElicitationBytes)
	}
	object, err := elicitationObject(transcriptParse(encoded), "elicitation params")
	if err != nil {
		return ElicitationRequest{}, err
	}
	mode, err := elicitationString(object, "mode")
	if err != nil {
		return ElicitationRequest{}, err
	}
	if mode != "form" {
		return ElicitationRequest{}, errors.New("Mjolnir only supports form elicitations")
	}
	_, hasSession := transcriptGetStr(object, "sessionId")
	_, hasRequest := object.Get("requestId")
	if !hasSession && !hasRequest {
		return ElicitationRequest{}, errors.New("elicitation has no sessionId or requestId scope")
	}
	message, err := elicitationString(object, "message")
	if err != nil {
		return ElicitationRequest{}, err
	}
	if err := elicitationEnsureText(message, "elicitation message"); err != nil {
		return ElicitationRequest{}, err
	}
	requested, ok := object.Get("requestedSchema")
	if !ok {
		return ElicitationRequest{}, errors.New("form elicitation has no requestedSchema")
	}
	schema, err := elicitationObject(requested, "requestedSchema")
	if err != nil {
		return ElicitationRequest{}, err
	}
	if kind, ok := transcriptGetStr(schema, "type"); ok && kind != "object" {
		return ElicitationRequest{}, errors.New("requestedSchema type must be object")
	}
	title, err := elicitationOptionalText(schema, "title")
	if err != nil {
		return ElicitationRequest{}, err
	}
	description, err := elicitationOptionalText(schema, "description")
	if err != nil {
		return ElicitationRequest{}, err
	}
	var properties jsonvalue.Object
	if value, ok := schema.Get("properties"); ok {
		if properties, err = elicitationObject(value, "requestedSchema.properties"); err != nil {
			return ElicitationRequest{}, err
		}
	}
	if len(properties) > maxFields {
		return ElicitationRequest{}, fmt.Errorf("elicitation has more than %d fields", maxFields)
	}
	requiredValue, requiredPresent := schema.Get("required")
	required, err := elicitationStringSet(requiredValue, requiredPresent, "requestedSchema.required")
	if err != nil {
		return ElicitationRequest{}, err
	}
	fields := make([]ElicitationField, 0, len(properties))
	for _, property := range properties {
		_, isRequired := required[property.Key]
		field, err := elicitationParseField(property.Key, property.Value, isRequired)
		if err != nil {
			return ElicitationRequest{}, err
		}
		fields = append(fields, field)
	}
	return ElicitationRequest{
		ID:          id,
		Message:     message,
		Title:       title,
		Description: description,
		Fields:      fields,
	}, nil
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
	if response.Action != ElicitationResponseAccept {
		if response.Action == ElicitationResponseDecline && !r.OffersSkip() {
			return ElicitationResponse{}, errors.New("this permission request offers no one-time rejection to skip with; " +
				"choose one of its options, or cancel to stop the turn")
		}
		return response, nil
	}
	// The content is copied: the caller's map is not the harness's answer.
	content := make(map[string]ElicitationValue, len(response.Content))
	for id, value := range response.Content {
		content[id] = value
	}
	// Rust walks a BTreeMap, so the first offending field is the first in
	// key order.
	ids := make([]string, 0, len(content))
	for id := range content {
		ids = append(ids, id)
	}
	sort.Strings(ids)
	for _, id := range ids {
		field := r.fieldByID(id)
		if field == nil {
			return ElicitationResponse{}, fmt.Errorf("%s is not a field of this request", debugString(id))
		}
		if err := ValidateFieldValue(field, content[id]); err != nil {
			return ElicitationResponse{}, err
		}
	}
	for i := range r.Fields {
		field := &r.Fields[i]
		index, role, ok := r.Companion(field)
		if !ok {
			continue
		}
		companion := &r.Fields[index]
		text, ok := content[companion.ID].(ElicitationValueString)
		if !ok || strings.TrimSpace(string(text)) == "" {
			continue
		}
		if _, picked := content[field.ID]; picked {
			if role == CompanionRoleAlternative {
				return ElicitationResponse{}, fmt.Errorf("%s takes either an offered option or %s, not both", field.Title, companion.Title)
			}
			continue
		}
		// Detail belongs to its option and never answers the select; text
		// beside a pick answers an optional select as it is.
		if role == CompanionRoleDetail || (role == CompanionRoleAlongside && !field.Required) {
			continue
		}
		delete(content, companion.ID)
		if _, multi := field.Kind.(ElicitationFieldKindMultiSelect); multi {
			content[field.ID] = ElicitationValueStringArray{string(text)}
		} else {
			content[field.ID] = text
		}
	}
	for i := range r.Fields {
		field := &r.Fields[i]
		if !field.Required {
			continue
		}
		value, ok := content[field.ID]
		if !ok {
			return ElicitationResponse{}, fmt.Errorf("%s is required", field.Title)
		}
		if text, isText := value.(ElicitationValueString); isText && strings.TrimSpace(string(text)) == "" {
			return ElicitationResponse{}, fmt.Errorf("%s is required", field.Title)
		}
	}
	return ElicitationResponse{Action: ElicitationResponseAccept, Content: content}, nil
}

// fieldByID is the first field with id, or nil.
func (r *ElicitationRequest) fieldByID(id string) *ElicitationField {
	for i := range r.Fields {
		if r.Fields[i].ID == id {
			return &r.Fields[i]
		}
	}
	return nil
}

// Companion is the select's companion text field and its role: the field
// whose CustomAnswerFor names the select, as its index in Fields. The first
// one counts. This is the one rule every surface lays out by; the web
// viewer's buildElicitationForm mirrors it.
func (r *ElicitationRequest) Companion(selectField *ElicitationField) (int, CompanionRole, bool) {
	if !elicitationIsSelect(selectField.Kind) {
		return 0, 0, false
	}
	for i := range r.Fields {
		field := &r.Fields[i]
		if _, text := field.Kind.(ElicitationFieldKindText); text &&
			field.CustomAnswerFor != nil && *field.CustomAnswerFor == selectField.ID {
			return i, field.CompanionRole(), true
		}
	}
	return 0, 0, false
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
	heading := r.Message
	if r.Title != nil {
		heading = *r.Title
	}
	lines := []string{heading, ""}
	switch response.Action {
	case ElicitationResponseAccept:
		for i := range r.Fields {
			field := &r.Fields[i]
			value, ok := response.Content[field.ID]
			if !ok {
				continue
			}
			title := field.Title
			if field.CustomAnswerFor != nil {
				if parent := r.fieldByID(*field.CustomAnswerFor); parent != nil {
					if _, answered := response.Content[parent.ID]; !answered {
						title = parent.Title
					}
				}
			}
			text := "[hidden]"
			if !r.FieldIsSecret(field) {
				text = field.replyValueText(value)
			}
			lines = append(lines, title+": "+text)
		}
		if len(lines) == 2 {
			lines = append(lines, "Accepted")
		}
	case ElicitationResponseDecline:
		lines = append(lines, "Skipped")
	case ElicitationResponseCancel:
		lines = append(lines, "Cancelled")
	}
	return strings.Join(lines, "\n")
}

// ValidateFieldValue reports whether one answered value satisfies the
// constraints of the field it answers. Presence and required-ness are the
// caller's business: this checks only that the value the caller does have
// is one the schema allows. The error text is shown as is.
func ValidateFieldValue(field *ElicitationField, value ElicitationValue) error {
	offered := func(options []ElicitationOption, value string) bool {
		for _, option := range options {
			if option.Value == value {
				return true
			}
		}
		return false
	}
	switch kind := field.Kind.(type) {
	case ElicitationFieldKindText:
		text, ok := value.(ElicitationValueString)
		if !ok {
			break
		}
		length := utf8.RuneCountInString(string(text))
		if kind.MinLength != nil && length < *kind.MinLength {
			return fmt.Errorf("%s is too short", field.Title)
		}
		if kind.MaxLength != nil && length > *kind.MaxLength {
			return fmt.Errorf("%s is too long", field.Title)
		}
		if kind.Pattern != nil && !regexp.MustCompile(*kind.Pattern).MatchString(string(text)) {
			return fmt.Errorf("%s does not match the required format", field.Title)
		}
		if kind.Format != nil {
			if err := validateTextFormat(string(text), *kind.Format); err != nil {
				return fmt.Errorf("%s %s", field.Title, err)
			}
		}
		return nil
	case ElicitationFieldKindSingleSelect:
		text, ok := value.(ElicitationValueString)
		if !ok {
			break
		}
		if offered(kind.Options, string(text)) {
			return nil
		}
		return fmt.Errorf("%s is not one of the offered options", field.Title)
	case ElicitationFieldKindMultiSelect:
		values, ok := value.(ElicitationValueStringArray)
		if !ok {
			break
		}
		if kind.MinItems != nil && len(values) < *kind.MinItems {
			return fmt.Errorf("%s needs more selections", field.Title)
		}
		if kind.MaxItems != nil && len(values) > *kind.MaxItems {
			return fmt.Errorf("%s has too many selections", field.Title)
		}
		for _, value := range values {
			if !offered(kind.Options, value) {
				return fmt.Errorf("%s is not one of the offered options", field.Title)
			}
		}
		return nil
	case ElicitationFieldKindBoolean:
		if _, ok := value.(ElicitationValueBoolean); ok {
			return nil
		}
	case ElicitationFieldKindInteger:
		number, ok := value.(ElicitationValueInteger)
		if !ok {
			break
		}
		if (kind.Minimum != nil && int64(number) < *kind.Minimum) ||
			(kind.Maximum != nil && int64(number) > *kind.Maximum) {
			return fmt.Errorf("%s is outside the allowed range", field.Title)
		}
		return nil
	case ElicitationFieldKindNumber:
		// JSON carries a whole number for a number field as an integer, and
		// an untagged decode reports it as one, so both arrive here.
		var number float64
		switch value := value.(type) {
		case ElicitationValueNumber:
			number = float64(value)
		case ElicitationValueInteger:
			number = float64(value)
		default:
			return fmt.Errorf("%s has an incompatible value", field.Title)
		}
		if math.IsNaN(number) || math.IsInf(number, 0) ||
			(kind.Minimum != nil && number < *kind.Minimum) ||
			(kind.Maximum != nil && number > *kind.Maximum) {
			return fmt.Errorf("%s is outside the allowed range", field.Title)
		}
		return nil
	}
	return fmt.Errorf("%s has an incompatible value", field.Title)
}

// validateTextFormat checks the string formats the ACP schema can request.
// Unknown formats are accepted: the schema keyword is open-ended and
// refusing an answer over a format Hel does not model would block a
// question the agent can otherwise use. The error completes a sentence
// that starts with the field title.
func validateTextFormat(value, format string) error {
	switch format {
	case "email":
		local, domain, ok := strings.Cut(value, "@")
		if !ok || local == "" || !strings.Contains(domain, ".") || strings.HasPrefix(domain, ".") {
			return errors.New("must be an email address")
		}
	case "uri":
		if parsed, err := url.Parse(value); err != nil || parsed.Scheme == "" {
			return errors.New("must be a URI")
		}
	case "date":
		if !elicitationIsDate(value) {
			return errors.New("must be a YYYY-MM-DD date")
		}
	case "date-time":
		if !elicitationIsRFC3339(value) {
			return errors.New("must be an RFC 3339 date-time")
		}
	}
	return nil
}

// elicitationIsDate parses value as chrono's NaiveDate with "%Y-%m-%d": a
// signed year, then a month and a day of one or two digits that name a real
// calendar day.
func elicitationIsDate(value string) bool {
	sign := 1
	rest := value
	if strings.HasPrefix(rest, "+") || strings.HasPrefix(rest, "-") {
		if rest[0] == '-' {
			sign = -1
		}
		rest = rest[1:]
	}
	parts := strings.Split(rest, "-")
	if len(parts) != 3 {
		return false
	}
	digits := func(text string, most int) (int, bool) {
		if text == "" || len(text) > most {
			return 0, false
		}
		number := 0
		for i := 0; i < len(text); i++ {
			if text[i] < '0' || text[i] > '9' {
				return 0, false
			}
			number = number*10 + int(text[i]-'0')
		}
		return number, true
	}
	year, yearOK := digits(parts[0], 9)
	month, monthOK := digits(parts[1], 2)
	day, dayOK := digits(parts[2], 2)
	if !yearOK || !monthOK || !dayOK || month < 1 || month > 12 || day < 1 {
		return false
	}
	date := time.Date(sign*year, time.Month(month), day, 0, 0, 0, 0, time.UTC)
	return date.Day() == day && int(date.Month()) == month
}

// elicitationIsRFC3339 parses value as chrono's parse_from_rfc3339, which
// also takes a lowercase "t" or a space between date and time and a
// lowercase "z".
func elicitationIsRFC3339(value string) bool {
	if len(value) > 10 && (value[10] == 't' || value[10] == ' ') {
		value = value[:10] + "T" + value[11:]
	}
	if strings.HasSuffix(value, "z") {
		value = strings.TrimSuffix(value, "z") + "Z"
	}
	_, err := time.Parse(time.RFC3339Nano, value)
	return err == nil
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
	optionTitle := func(value string) string {
		for _, option := range elicitationSelectOptions(f.Kind) {
			if option.Value == value {
				return option.Title
			}
		}
		return value
	}
	switch value := value.(type) {
	case ElicitationValueString:
		return optionTitle(string(value))
	case ElicitationValueStringArray:
		titles := make([]string, len(value))
		for i, item := range value {
			titles[i] = optionTitle(item)
		}
		return strings.Join(titles, ", ")
	case ElicitationValueInteger:
		return strconv.FormatInt(int64(value), 10)
	case ElicitationValueNumber:
		return elicitationFloatText(float64(value))
	case ElicitationValueBoolean:
		if value {
			return "Yes"
		}
		return "No"
	}
	return ""
}

// elicitationFloatText writes an f64 as Rust's Display does: the shortest
// decimal that round-trips, never in exponent form.
func elicitationFloatText(value float64) string {
	switch {
	case math.IsNaN(value):
		return "NaN"
	case math.IsInf(value, 1):
		return "inf"
	case math.IsInf(value, -1):
		return "-inf"
	}
	return strconv.FormatFloat(value, 'f', -1, 64)
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
	if r.Action != other.Action || len(r.Content) != len(other.Content) {
		return false
	}
	for id, value := range r.Content {
		otherValue, ok := other.Content[id]
		if !ok || !elicitationValueEqual(value, otherValue) {
			return false
		}
	}
	return true
}

// elicitationValueEqual is the Rust PartialEq of ElicitationValue: the same
// variant holding the same value.
func elicitationValueEqual(a, b ElicitationValue) bool {
	switch a := a.(type) {
	case ElicitationValueStringArray:
		b, ok := b.(ElicitationValueStringArray)
		return ok && slices.Equal(a, b)
	case ElicitationValueString, ElicitationValueInteger, ElicitationValueNumber, ElicitationValueBoolean:
		return a == b
	}
	return false
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
	if err := elicitationEnsureText(fieldID, "elicitation field id"); err != nil {
		return ElicitationField{}, err
	}
	schema, err := elicitationObject(value, fmt.Sprintf("schema for field %s", debugString(fieldID)))
	if err != nil {
		return ElicitationField{}, err
	}
	titleText, err := elicitationOptionalText(schema, "title")
	if err != nil {
		return ElicitationField{}, err
	}
	title := fieldID
	if titleText != nil {
		title = *titleText
	}
	description, err := elicitationOptionalText(schema, "description")
	if err != nil {
		return ElicitationField{}, err
	}
	metaValue, _ := schema.Get("_meta")
	meta, _ := metaValue.(jsonvalue.Object)
	codexValue, _ := transcriptGet(meta, "codex")
	codex, _ := codexValue.(jsonvalue.Object)
	secretValue, _ := transcriptGet(codex, "isSecret")
	secret, _ := secretValue.(bool)
	// A companion names its select with the bridge's own marker, and the
	// marker also says how its text relates to the pick. codex-acp's note
	// (_meta.codex, role: "user_note") is the answer in place of a pick;
	// claude-agent-acp's "Other" (the Air customAnswer key, sent because
	// Mjolnir declares _meta.jetbrains.air) goes beside it.
	jetbrains, _ := transcriptGet(meta, "jetbrains")
	air, _ := transcriptGet(jetbrains, "air")
	customAnswerValue, _ := transcriptGet(air, "customAnswer")
	airCustomAnswer, _ := customAnswerValue.(jsonvalue.Object)
	var codexNoteFor, claudeOtherFor *string
	if role, _ := transcriptGetStr(codex, "role"); codex != nil && role == "user_note" {
		if question, ok := transcriptGetStr(codex, "questionId"); ok {
			codexNoteFor = &question
		}
	}
	if isCustom, _ := transcriptGet(airCustomAnswer, "isCustomAnswer"); airCustomAnswer != nil && isCustom == true {
		if question, ok := transcriptGetStr(airCustomAnswer, "questionId"); ok {
			claudeOtherFor = &question
		}
	}
	var customAnswerFor, customAnswerOption *string
	switch {
	case codexNoteFor != nil:
		customAnswerFor = codexNoteFor
	case claudeOtherFor != nil:
		alongside := CompanionAlongside
		customAnswerFor, customAnswerOption = claudeOtherFor, &alongside
	}
	kindName, err := elicitationString(schema, "type")
	if err != nil {
		return ElicitationField{}, err
	}
	var kind ElicitationFieldKind
	switch kindName {
	case "string":
		options, err := elicitationParseStringOptions(schema)
		if err != nil {
			return ElicitationField{}, err
		}
		if len(options) == 0 {
			pattern, err := elicitationOptionalText(schema, "pattern")
			if err != nil {
				return ElicitationField{}, err
			}
			if pattern != nil {
				if _, err := regexp.Compile(*pattern); err != nil {
					return ElicitationField{}, fmt.Errorf("field %s has an invalid pattern: %w", debugString(fieldID), err)
				}
			}
			text := ElicitationFieldKindText{Pattern: pattern}
			if text.Default, err = elicitationOptionalText(schema, "default"); err != nil {
				return ElicitationField{}, err
			}
			if text.MinLength, err = elicitationOptionalUsize(schema, "minLength"); err != nil {
				return ElicitationField{}, err
			}
			if text.MaxLength, err = elicitationOptionalUsize(schema, "maxLength"); err != nil {
				return ElicitationField{}, err
			}
			if text.Format, err = elicitationOptionalText(schema, "format"); err != nil {
				return ElicitationField{}, err
			}
			kind = text
		} else {
			defaultValue, err := elicitationOptionalText(schema, "default")
			if err != nil {
				return ElicitationField{}, err
			}
			if defaultValue == nil {
				chosen := options[0].Value
				if recommended := recommendedOptions(options); len(recommended) > 0 {
					chosen = recommended[0].Value
				}
				defaultValue = &chosen
			}
			kind = ElicitationFieldKindSingleSelect{Options: options, Default: defaultValue}
		}
	case "array":
		itemsValue, ok := schema.Get("items")
		if !ok {
			return ElicitationField{}, errors.New("multi-select field has no items schema")
		}
		items, err := elicitationObject(itemsValue, "multi-select items")
		if err != nil {
			return ElicitationField{}, err
		}
		options, err := elicitationParseOptions(items, "anyOf")
		if err != nil {
			if options, err = elicitationParseEnumOptions(items); err != nil {
				return ElicitationField{}, err
			}
		}
		if len(options) == 0 {
			return ElicitationField{}, errors.New("multi-select field has no options")
		}
		defaults, err := elicitationOptionalStringArray(schema, "default")
		if err != nil {
			return ElicitationField{}, err
		}
		var defaultValues []string
		if defaults != nil {
			defaultValues = *defaults
		} else {
			defaultValues = []string{}
			for _, option := range recommendedOptions(options) {
				defaultValues = append(defaultValues, option.Value)
			}
		}
		multi := ElicitationFieldKindMultiSelect{Options: options, Default: defaultValues}
		if multi.MinItems, err = elicitationOptionalUsize(schema, "minItems"); err != nil {
			return ElicitationField{}, err
		}
		if multi.MaxItems, err = elicitationOptionalUsize(schema, "maxItems"); err != nil {
			return ElicitationField{}, err
		}
		kind = multi
	case "boolean":
		defaultValue, err := elicitationOptionalBool(schema, "default")
		if err != nil {
			return ElicitationField{}, err
		}
		kind = ElicitationFieldKindBoolean{Default: defaultValue}
	case "integer":
		var integer ElicitationFieldKindInteger
		if integer.Default, err = elicitationOptionalI64(schema, "default"); err != nil {
			return ElicitationField{}, err
		}
		if integer.Minimum, err = elicitationOptionalI64(schema, "minimum"); err != nil {
			return ElicitationField{}, err
		}
		if integer.Maximum, err = elicitationOptionalI64(schema, "maximum"); err != nil {
			return ElicitationField{}, err
		}
		kind = integer
	case "number":
		var number ElicitationFieldKindNumber
		if number.Default, err = elicitationOptionalF64(schema, "default"); err != nil {
			return ElicitationField{}, err
		}
		if number.Minimum, err = elicitationOptionalF64(schema, "minimum"); err != nil {
			return ElicitationField{}, err
		}
		if number.Maximum, err = elicitationOptionalF64(schema, "maximum"); err != nil {
			return ElicitationField{}, err
		}
		kind = number
	default:
		return ElicitationField{}, fmt.Errorf("field %s has unsupported type %s", debugString(fieldID), debugString(kindName))
	}
	return ElicitationField{
		ID:                 fieldID,
		Title:              title,
		Description:        description,
		Required:           required,
		Secret:             secret,
		CustomAnswerFor:    customAnswerFor,
		CustomAnswerOption: customAnswerOption,
		Kind:               kind,
	}, nil
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
	member, _ := schema.Get(key)
	values, ok := member.([]any)
	if !ok {
		return nil, fmt.Errorf("%s must be an array", key)
	}
	if len(values) > maxOptions {
		return nil, fmt.Errorf("%s has too many options", key)
	}
	options := make([]ElicitationOption, 0, len(values))
	for _, value := range values {
		option, err := elicitationObject(value, "elicitation option")
		if err != nil {
			return nil, err
		}
		constant, err := elicitationString(option, "const")
		if err != nil {
			return nil, err
		}
		titleText, err := elicitationOptionalText(option, "title")
		if err != nil {
			return nil, err
		}
		title := constant
		if titleText != nil {
			title = *titleText
		}
		description, err := elicitationOptionalText(option, "description")
		if err != nil {
			return nil, err
		}
		// claude-agent-acp puts the preview text under its AskUserQuestion
		// key; muse-acp sends {content, format} under _meta.muse.preview, and
		// the content is the text to show.
		metaValue, _ := option.Get("_meta")
		meta, _ := metaValue.(jsonvalue.Object)
		claude, _ := transcriptGet(meta, "_claude/askUserQuestionOption")
		previewValue, found := transcriptGet(claude, "preview")
		if !found {
			muse, _ := transcriptGet(meta, "muse")
			musePreview, _ := transcriptGet(muse, "preview")
			previewValue, _ = transcriptGet(musePreview, "content")
		}
		var preview *string
		if text, ok := previewValue.(string); ok {
			if err := elicitationEnsureText(text, "elicitation option preview"); err != nil {
				return nil, err
			}
			preview = &text
		}
		options = append(options, ElicitationOption{Value: constant, Title: title, Description: description, Preview: preview})
	}
	return options, nil
}

func elicitationParseEnumOptions(schema jsonvalue.Object) ([]ElicitationOption, error) {
	member, _ := schema.Get("enum")
	values, ok := member.([]any)
	if !ok {
		return nil, errors.New("enum must be an array")
	}
	if len(values) > maxOptions {
		return nil, errors.New("enum has too many options")
	}
	options := make([]ElicitationOption, 0, len(values))
	for _, value := range values {
		text, ok := value.(string)
		if !ok {
			return nil, errors.New("elicitation enum values must be strings")
		}
		options = append(options, ElicitationOption{Value: text, Title: text})
	}
	return options, nil
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
	value, err := elicitationOptionalU64(object, key)
	if err != nil || value == nil {
		return nil, err
	}
	if *value > math.MaxInt {
		return nil, errors.New("numeric constraint does not fit usize")
	}
	number := int(*value)
	return &number, nil
}

func elicitationOptionalU64(object jsonvalue.Object, key string) (*uint64, error) {
	value, ok := object.Get(key)
	if !ok || value == nil {
		return nil, nil
	}
	number, ok := transcriptAsU64(value)
	if !ok {
		return nil, fmt.Errorf("%s must be a non-negative integer", key)
	}
	return &number, nil
}

func elicitationOptionalI64(object jsonvalue.Object, key string) (*int64, error) {
	value, ok := object.Get(key)
	if !ok || value == nil {
		return nil, nil
	}
	number, ok := value.(int64)
	if !ok {
		// serde_json's as_i64 also takes a non-negative integer that fits.
		unsigned, isUnsigned := value.(uint64)
		if !isUnsigned || unsigned > math.MaxInt64 {
			return nil, fmt.Errorf("%s must be an integer", key)
		}
		number = int64(unsigned)
	}
	return &number, nil
}

func elicitationOptionalF64(object jsonvalue.Object, key string) (*float64, error) {
	value, ok := object.Get(key)
	if !ok || value == nil {
		return nil, nil
	}
	var number float64
	switch value := value.(type) {
	case int64:
		number = float64(value)
	case uint64:
		number = float64(value)
	case float64:
		number = value
	default:
		return nil, fmt.Errorf("%s must be a number", key)
	}
	if math.IsNaN(number) || math.IsInf(number, 0) {
		return nil, fmt.Errorf("%s must be finite", key)
	}
	return &number, nil
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
	value, ok := object.Get(key)
	if !ok || value == nil {
		return nil, nil
	}
	items, ok := value.([]any)
	if !ok {
		return nil, fmt.Errorf("%s must be an array", key)
	}
	values := make([]string, 0, len(items))
	for _, item := range items {
		text, ok := item.(string)
		if !ok {
			return nil, fmt.Errorf("%s entries must be strings", key)
		}
		values = append(values, text)
	}
	return &values, nil
}

// elicitationStringSet reads an optional array of strings as a set.
func elicitationStringSet(value any, present bool, what string) (map[string]struct{}, error) {
	set := map[string]struct{}{}
	if !present {
		return set, nil
	}
	items, ok := value.([]any)
	if !ok {
		return nil, fmt.Errorf("%s must be an array", what)
	}
	for _, item := range items {
		text, ok := item.(string)
		if !ok {
			return nil, fmt.Errorf("%s entries must be strings", what)
		}
		set[text] = struct{}{}
	}
	return set, nil
}
