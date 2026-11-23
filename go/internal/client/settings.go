// Port of mj-client/src/settings/mod.rs.

// The settings model every interface shares: which settings exist, how a
// page lists them, what they are called and set to, how an edit changes the
// draft, and how a draft is saved. Surfaces own only their own drawing.
//
// The modules are the settings_*.go files of this package. A settings draft
// is a serde_json::Value, held here in the form of
// github.com/BrokkAi/acp-go/jsonvalue (nil, bool, string, int64, uint64,
// float64, []any and jsonvalue.Object, whose members keep their order as
// serde_json's preserve_order does). A `&mut Value` parameter is a *any.
//
// The helpers below read and change such values the way serde_json's Value
// API does (indexing, pointers, as_* accessors and equality), so the
// settings files translate line for line.

package client

import (
	"encoding/json"
	"fmt"
	"strconv"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// settingsGet is serde_json's `value[key]`: the member, or null when value
// is not an object or has no such member.
func settingsGet(value any, key string) any {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil
	}
	member, _ := object.Get(key)
	return member
}

// settingsPath is `value[k1][k2]...` with object keys.
func settingsPath(value any, keys ...string) any {
	for _, key := range keys {
		value = settingsGet(value, key)
	}
	return value
}

// settingsAt is `value[index]` on an array: null when out of range or not an
// array.
func settingsAt(value any, index int) any {
	array, ok := value.([]any)
	if !ok || index < 0 || index >= len(array) {
		return nil
	}
	return array[index]
}

// settingsHas is serde_json's `value.get(key).is_some()` on an object.
func settingsHas(value any, key string) bool {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return false
	}
	_, found := object.Get(key)
	return found
}

func settingsString(value any) (string, bool) {
	text, ok := value.(string)
	return text, ok
}

func settingsBool(value any) (bool, bool) {
	flag, ok := value.(bool)
	return flag, ok
}

// settingsU64 is serde_json's as_u64: a non-negative integer.
func settingsU64(value any) (uint64, bool) {
	switch number := value.(type) {
	case uint64:
		return number, true
	case int64:
		if number >= 0 {
			return uint64(number), true
		}
	case int:
		if number >= 0 {
			return uint64(number), true
		}
	case uint32:
		return uint64(number), true
	case uint:
		return uint64(number), true
	}
	return 0, false
}

func settingsObject(value any) (jsonvalue.Object, bool) {
	object, ok := value.(jsonvalue.Object)
	return object, ok && object != nil
}

func settingsArray(value any) ([]any, bool) {
	array, ok := value.([]any)
	return array, ok && array != nil
}

func settingsIsObject(value any) bool { _, ok := settingsObject(value); return ok }

func settingsIsArray(value any) bool { _, ok := settingsArray(value); return ok }

// settingsEqual is serde_json's Value equality: objects compare members
// whatever their order, numbers by their class and value.
func settingsEqual(a, b any) bool {
	switch left := a.(type) {
	case jsonvalue.Object:
		right, ok := b.(jsonvalue.Object)
		if !ok || (left == nil) != (right == nil) {
			return jsonvalue.Equal(a, b)
		}
		leftKeys, rightKeys := left.Keys(), right.Keys()
		if len(leftKeys) != len(rightKeys) {
			return false
		}
		for _, key := range leftKeys {
			leftValue, _ := left.Get(key)
			rightValue, found := right.Get(key)
			if !found || !settingsEqual(leftValue, rightValue) {
				return false
			}
		}
		return true
	case []any:
		right, ok := b.([]any)
		if !ok || (left == nil) != (right == nil) {
			return jsonvalue.Equal(a, b)
		}
		if len(left) != len(right) {
			return false
		}
		for i := range left {
			if !settingsEqual(left[i], right[i]) {
				return false
			}
		}
		return true
	}
	return jsonvalue.Equal(a, b)
}

// settingsContains is `values.contains(value)`.
func settingsContains(values []any, value any) bool {
	for _, candidate := range values {
		if settingsEqual(candidate, value) {
			return true
		}
	}
	return false
}

// settingsDisplay is serde_json's Display (`{value}`, Value::to_string):
// compact JSON.
func settingsDisplay(value any) string {
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		panic(fmt.Sprintf("a settings value serializes: %v", err))
	}
	return string(encoded)
}

// settingsToValue is serde_json::to_value: v as the Value its serialization
// reads back as.
func settingsToValue(v any) (any, error) {
	encoded, err := json.Marshal(v)
	if err != nil {
		return nil, err
	}
	return jsonvalue.Parse(encoded)
}

// settingsFromValue is serde_json::from_value: value decoded into target.
func settingsFromValue(value any, target any) error {
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		return err
	}
	return json.Unmarshal(encoded, target)
}

// settingsClone is Value::clone: nested objects and arrays are copied.
func settingsClone(value any) any {
	switch typed := value.(type) {
	case jsonvalue.Object:
		return typed.Clone()
	case []any:
		if typed == nil {
			return typed
		}
		cloned := make([]any, len(typed))
		for i, item := range typed {
			cloned[i] = settingsClone(item)
		}
		return cloned
	}
	return value
}

// settingsPointerTokens splits a JSON pointer into its unescaped tokens, as
// serde_json's Value::pointer does; false for a pointer that is neither
// empty nor starts with `/`.
func settingsPointerTokens(pointer string) ([]string, bool) {
	if pointer == "" {
		return nil, true
	}
	if !strings.HasPrefix(pointer, "/") {
		return nil, false
	}
	tokens := strings.Split(pointer[1:], "/")
	for i, token := range tokens {
		tokens[i] = strings.ReplaceAll(strings.ReplaceAll(token, "~1", "/"), "~0", "~")
	}
	return tokens, true
}

// settingsArrayIndex is serde_json's parse_index: a plain decimal without a
// sign or leading zero.
func settingsArrayIndex(token string) (int, bool) {
	if strings.HasPrefix(token, "+") || (strings.HasPrefix(token, "0") && len(token) != 1) {
		return 0, false
	}
	index, err := strconv.ParseUint(token, 10, 63)
	if err != nil {
		return 0, false
	}
	return int(index), true
}

// settingsPointer is serde_json's Value::pointer.
func settingsPointer(value any, pointer string) (any, bool) {
	tokens, ok := settingsPointerTokens(pointer)
	if !ok {
		return nil, false
	}
	for _, token := range tokens {
		switch typed := value.(type) {
		case jsonvalue.Object:
			member, found := typed.Get(token)
			if !found {
				return nil, false
			}
			value = member
		case []any:
			index, ok := settingsArrayIndex(token)
			if !ok || index >= len(typed) {
				return nil, false
			}
			value = typed[index]
		default:
			return nil, false
		}
	}
	return value, true
}

// settingsPointerMut is serde_json's Value::pointer_mut: change runs on the
// value at pointer, and the result is written back into root. False, with
// nothing changed, when nothing is there.
func settingsPointerMut(root *any, pointer string, change func(*any)) bool {
	tokens, ok := settingsPointerTokens(pointer)
	if !ok {
		return false
	}
	return settingsMutateAt(root, tokens, change)
}

func settingsMutateAt(value *any, tokens []string, change func(*any)) bool {
	if len(tokens) == 0 {
		change(value)
		return true
	}
	switch typed := (*value).(type) {
	case jsonvalue.Object:
		child, found := typed.Get(tokens[0])
		if !found || !settingsMutateAt(&child, tokens[1:], change) {
			return false
		}
		typed.Set(tokens[0], child)
		*value = typed
		return true
	case []any:
		index, ok := settingsArrayIndex(tokens[0])
		if !ok || index >= len(typed) {
			return false
		}
		child := typed[index]
		if !settingsMutateAt(&child, tokens[1:], change) {
			return false
		}
		typed[index] = child
		*value = typed
		return true
	}
	return false
}

// settingsIndexMut is serde_json's IndexMut chain `root[k1][k2]... = ...`:
// a string key turns null into an object and inserts null for a missing
// member; an int key indexes an array. A shape serde_json would panic on
// panics here too.
func settingsIndexMut(root *any, keys []any, change func(*any)) {
	if len(keys) == 0 {
		change(root)
		return
	}
	switch key := keys[0].(type) {
	case string:
		if *root == nil {
			*root = jsonvalue.Object{}
		}
		object, ok := (*root).(jsonvalue.Object)
		if !ok {
			panic(fmt.Sprintf("cannot access key %q in JSON %s", key, settingsDisplay(*root)))
		}
		child, _ := object.Get(key)
		settingsIndexMut(&child, keys[1:], change)
		object.Set(key, child)
		*root = object
	case int:
		array, ok := (*root).([]any)
		if !ok || key < 0 || key >= len(array) {
			panic(fmt.Sprintf("cannot access index %d of JSON %s", key, settingsDisplay(*root)))
		}
		child := array[key]
		settingsIndexMut(&child, keys[1:], change)
		array[key] = child
	default:
		panic(fmt.Sprintf("a settings index is a string or an int, not %T", keys[0]))
	}
}

// settingsSet is `root[k1][k2]... = value`.
func settingsSet(root *any, value any, keys ...any) {
	settingsIndexMut(root, keys, func(target *any) { *target = value })
}
