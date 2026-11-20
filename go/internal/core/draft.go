// Port of mj-core/src/draft.rs.
//
// Shared persisted composer-envelope operations. Clipboard processing stays
// in the composer; storage merges envelopes without interpreting native
// images.

package core

import (
	"errors"
	"fmt"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

const ChatDraftPrefix = "mjolnir-chat-draft-v1:"

// envelope holds the members as serde_json's Map does in this workspace,
// where serde_json is built with preserve_order: in order, and removed with
// swap_remove.
type envelope struct {
	fields jsonvalue.Object
	text   string
	images []any
}

func envelopeDecode(draft string) (*envelope, error) {
	var fields jsonvalue.Object
	if body, ok := strings.CutPrefix(draft, ChatDraftPrefix); ok {
		value, err := jsonvalue.Parse([]byte(body))
		if err != nil {
			return nil, fmt.Errorf("decode composer draft envelope: %w", err)
		}
		object, isObject := value.(jsonvalue.Object)
		if !isObject || object == nil {
			return nil, errors.New("decode composer draft envelope: invalid type: expected a map")
		}
		fields = object
	} else {
		fields = jsonvalue.Object{{Key: "text", Value: draft}}
	}
	textValue, hasText := draftRemove(&fields, "text")
	text, isString := textValue.(string)
	if !hasText || !isString {
		return nil, errors.New("composer draft is missing text")
	}
	images, hasImages := draftRemove(&fields, "images")
	hasImages = hasImages && images != nil
	legacy, hasLegacy := draftRemove(&fields, "image")
	hasLegacy = hasLegacy && legacy != nil
	if hasImages && hasLegacy {
		return nil, errors.New("composer draft has conflicting image fields")
	}
	var list []any
	switch {
	case hasImages:
		array, ok := images.([]any)
		if !ok {
			return nil, errors.New("composer draft has invalid images")
		}
		list = array
	case hasLegacy:
		start := len(text)
		text += "[image 1]"
		list = []any{jsonvalue.Object{
			{Key: "range", Value: jsonvalue.Object{
				{Key: "start", Value: uint64(start)},
				{Key: "end", Value: uint64(len(text))},
			}},
			{Key: "number", Value: uint64(1)},
			{Key: "image", Value: legacy},
		}}
	default:
		list = []any{}
	}
	if len(list) > MaxImages {
		return nil, errors.New("a message can contain at most 10 images")
	}
	previousEnd := 0
	numbers := map[uint64]bool{}
	for _, image := range list {
		start, end, number, err := imageRange(image)
		if err != nil {
			return nil, err
		}
		valid := start >= previousEnd && start < end && end <= len(text) &&
			text[start:end] == fmt.Sprintf("[image %d]", number) && number != 0 && !numbers[number]
		if !valid {
			return nil, errors.New("composer draft has invalid image ranges or numbers")
		}
		numbers[number] = true
		metadata, _ := draftGet(image, "image")
		if object, ok := metadata.(jsonvalue.Object); !ok || object == nil {
			return nil, errors.New("composer draft has invalid image metadata")
		}
		previousEnd = end
	}
	return &envelope{fields: fields, text: text, images: list}, nil
}

func (e *envelope) encode() (string, error) {
	if len(e.images) == 0 && len(e.fields) == 0 && !strings.HasPrefix(e.text, ChatDraftPrefix) {
		return e.text, nil
	}
	fields := e.fields
	if fields == nil {
		fields = jsonvalue.Object{}
	}
	fields.Set("text", e.text)
	images := e.images
	if images == nil {
		images = []any{}
	}
	fields.Set("images", images)
	e.fields = fields
	encoded, err := jsonvalue.Marshal(fields)
	if err != nil {
		return "", err
	}
	return ChatDraftPrefix + string(encoded), nil
}

// draftRemove removes a member as serde_json's Map::remove does with
// preserve_order (swap_remove) and returns its value.
func draftRemove(fields *jsonvalue.Object, key string) (any, bool) {
	value, ok := fields.Get(key)
	if ok {
		fields.SwapDelete(key)
	}
	return value, ok
}

// draftGet is serde_json's Value::get on an object key.
func draftGet(value any, key string) (any, bool) {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil, false
	}
	return object.Get(key)
}

// draftAsU64 is serde_json's Value::as_u64.
func draftAsU64(value any) (uint64, bool) {
	switch number := value.(type) {
	case uint64:
		return number, true
	case int64:
		if number >= 0 {
			return uint64(number), true
		}
	}
	return 0, false
}

// draftValueEqual compares values as serde_json's Value equality does: object
// members compare regardless of order.
func draftValueEqual(a, b any) bool {
	switch x := a.(type) {
	case jsonvalue.Object:
		y, ok := b.(jsonvalue.Object)
		if !ok || x == nil || y == nil {
			return jsonvalue.Equal(a, b)
		}
		if len(x.Keys()) != len(y.Keys()) {
			return false
		}
		for _, key := range x.Keys() {
			left, _ := x.Get(key)
			right, present := y.Get(key)
			if !present || !draftValueEqual(left, right) {
				return false
			}
		}
		return true
	case []any:
		y, ok := b.([]any)
		if !ok || len(x) != len(y) {
			return false
		}
		for i := range x {
			if !draftValueEqual(x[i], y[i]) {
				return false
			}
		}
		return true
	}
	return jsonvalue.Equal(a, b)
}

// RemoveStartupReceipt retires exactly one saved startup receipt when its
// payload becomes a draft. Native submissions and other startup identities
// keep their saved ownership.
func RemoveStartupReceipt(draft, commandID string) (string, error) {
	envelope, err := envelopeDecode(draft)
	if err != nil {
		return "", err
	}
	pendingValue, ok := envelope.fields.Get("pending")
	if !ok {
		return draft, nil
	}
	pending, ok := pendingValue.([]any)
	if !ok {
		return "", errors.New("invalid pending composer submissions")
	}
	kept := make([]any, 0, len(pending))
	for _, submission := range pending {
		id, _ := draftGet(submission, "id")
		idText, isString := id.(string)
		origin, _ := draftGet(submission, "startup_origin")
		originObject, isObject := origin.(jsonvalue.Object)
		matches := isString && idText == commandID && isObject && originObject != nil
		if !matches {
			kept = append(kept, submission)
		}
	}
	if len(kept) == len(pending) {
		return draft, nil
	}
	if len(kept) == 0 {
		envelope.fields.SwapDelete("pending")
	} else {
		envelope.fields.Set("pending", kept)
	}
	return envelope.encode()
}

// imageRange is an image's (start, end, number).
func imageRange(image any) (int, int, uint64, error) {
	rangeValue, _ := draftGet(image, "range")
	startValue, _ := draftGet(rangeValue, "start")
	start, ok := draftAsU64(startValue)
	if !ok || start > uint64(maxInt) {
		return 0, 0, 0, errors.New("invalid image range start")
	}
	endValue, _ := draftGet(rangeValue, "end")
	end, ok := draftAsU64(endValue)
	if !ok || end > uint64(maxInt) {
		return 0, 0, 0, errors.New("invalid image range end")
	}
	numberValue, _ := draftGet(image, "number")
	number, ok := draftAsU64(numberValue)
	if !ok {
		return 0, 0, 0, errors.New("invalid image number")
	}
	return int(start), int(end), number, nil
}

const maxInt = int(^uint(0) >> 1)

// MergeDrafts appends recovered input while preserving tracked images and
// envelope metadata. Conflicting unknown metadata fails visibly rather than
// silently discarding it.
func MergeDrafts(existing, recovered string) (string, error) {
	left, err := envelopeDecode(existing)
	if err != nil {
		return "", err
	}
	right, err := envelopeDecode(recovered)
	if err != nil {
		return "", err
	}
	if recovered == "" {
		return existing, nil
	}
	if existing == "" {
		return recovered, nil
	}
	if len(left.images)+len(right.images) > MaxImages {
		return "", errors.New("recovered draft would contain more than 10 images")
	}
	for _, member := range right.fields {
		if previous, ok := left.fields.Get(member.Key); ok {
			if !draftValueEqual(previous, member.Value) {
				return "", fmt.Errorf("cannot merge conflicting composer metadata %s", member.Key)
			}
		} else {
			left.fields.Set(member.Key, member.Value)
		}
	}
	var text strings.Builder
	text.WriteString(left.text)
	text.WriteString("\n\n")
	var next uint64
	for _, image := range left.images {
		value, _ := draftGet(image, "number")
		if number, ok := draftAsU64(value); ok && number > next {
			next = number
		}
	}
	cursor := 0
	for _, image := range right.images {
		start, end, _, err := imageRange(image)
		if err != nil {
			return "", err
		}
		text.WriteString(right.text[cursor:start])
		if next == ^uint64(0) {
			return "", errors.New("composer image number overflow")
		}
		next++
		offset := text.Len()
		fmt.Fprintf(&text, "[image %d]", next)
		object := image.(jsonvalue.Object).Clone()
		object.Set("number", next)
		rangeValue, _ := object.Get("range")
		rangeObject := rangeValue.(jsonvalue.Object).Clone()
		rangeObject.Set("start", uint64(offset))
		rangeObject.Set("end", uint64(text.Len()))
		object.Set("range", rangeObject)
		left.images = append(left.images, object)
		cursor = end
	}
	text.WriteString(right.text[cursor:])
	left.text = text.String()
	return left.encode()
}

// ValidateReferenceDraft: durable startup drafts must carry installed
// references, never inline bytes.
func ValidateReferenceDraft(draft string) error {
	envelope, err := envelopeDecode(draft)
	if err != nil {
		return err
	}
	for _, entry := range envelope.images {
		image, _ := draftGet(entry, "image")
		if data, present := draftGet(image, "data_base64"); present {
			if text, ok := data.(string); !ok || text != "" {
				return errors.New("queued drafts cannot contain inline image bytes")
			}
		}
		referenceValue, present := draftGet(image, "reference")
		if !present {
			return errors.New("queued draft image is not installed")
		}
		encoded, err := jsonvalue.Marshal(referenceValue)
		if err != nil {
			return err
		}
		var reference AttachmentRef
		if err := reference.UnmarshalJSON(encoded); err != nil {
			return err
		}
		if err := reference.Validate(); err != nil {
			return err
		}
	}
	return nil
}
