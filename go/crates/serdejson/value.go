// Port of serde_json 1.0.150 src/value/mod.rs, src/value/index.rs,
// src/value/from.rs, src/value/partial_eq.rs, src/map.rs and src/macros.rs

package serdejson

import (
	"math"
	"reflect"
	"slices"
	"strconv"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Value is `serde_json::Value`: any valid JSON value, held in the
// representation of github.com/BrokkAi/acp-go/jsonvalue, so that ACP
// payloads need no conversion:
//
//	Value::Null       nil
//	Value::Bool       bool
//	Value::Number     int64 (an integer that fits i64), uint64 (a larger
//	                  one), or a finite float64
//	Value::String     string
//	Value::Array      []any, never nil
//	Value::Object     Map (jsonvalue.Object), never nil
//
// A Go map, struct or other Go value placed in a Value is read as the
// Value that serializing it gives. The methods of Rust's Value are the
// functions named Value<Method> in this package.
type Value = any

// Map is `serde_json::Map<String, Value>`: a JSON object with its members
// in insertion order (serde_json's `preserve_order`). It is jsonvalue's
// Object. Its methods are the functions named Map<Method>.
type Map = jsonvalue.Object

// ValueNull is `Value::Null`.
var ValueNull Value = nil

// valueString is `Display for Value`: compact JSON, or pretty JSON for the
// alternate form `{:#}`.
func valueString(v Value, pretty bool) string {
	var text string
	var err error
	if pretty {
		text, err = ToStringPretty(valueRef{v})
	} else {
		text, err = ToString(valueRef{v})
	}
	if err != nil {
		panic(rs.Panicf("serde_json: a Value that does not serialize: " + err.Error()))
	}
	return text
}

// ValueToString is `value.to_string()` (`Display for Value`): the value as
// compact JSON.
func ValueToString(v Value) string { return valueString(v, false) }

// ValueToStringPretty is `format!("{:#}", value)`: the value as pretty JSON.
func ValueToStringPretty(v Value) string { return valueString(v, true) }

// ValueFromF64 is `Value::from(f64)`: a Number, or Null for NaN and the
// infinities.
func ValueFromF64(f float64) Value {
	if math.IsNaN(f) || math.IsInf(f, 0) {
		return nil
	}
	return f
}

// ValueIsNull is `Value::is_null`.
func ValueIsNull(v Value) bool { return v == nil }

// ValueAsNull is `Value::as_null`.
func ValueAsNull(v Value) rs.Option[rs.Unit] { return rs.OptionOf(rs.Unit{}, v == nil) }

// ValueIsBoolean is `Value::is_boolean`.
func ValueIsBoolean(v Value) bool {
	_, ok := v.(bool)
	return ok
}

// ValueAsBool is `Value::as_bool`.
func ValueAsBool(v Value) rs.Option[bool] {
	b, ok := v.(bool)
	return rs.OptionOf(b, ok)
}

// ValueIsString is `Value::is_string`.
func ValueIsString(v Value) bool {
	_, ok := v.(string)
	return ok
}

// ValueAsStr is `Value::as_str`.
func ValueAsStr(v Value) rs.Option[string] {
	s, ok := v.(string)
	return rs.OptionOf(s, ok)
}

// ValueIsNumber is `Value::is_number`.
func ValueIsNumber(v Value) bool {
	_, ok := numberOf(v)
	return ok
}

// ValueAsNumber is `Value::as_number`.
func ValueAsNumber(v Value) rs.Option[Number] {
	n, ok := numberOf(v)
	return rs.OptionOf(n, ok)
}

// ValueIsI64 is `Value::is_i64`.
func ValueIsI64(v Value) bool {
	n, ok := numberOf(v)
	return ok && n.IsI64()
}

// ValueIsU64 is `Value::is_u64`.
func ValueIsU64(v Value) bool {
	n, ok := numberOf(v)
	return ok && n.IsU64()
}

// ValueIsF64 is `Value::is_f64`.
func ValueIsF64(v Value) bool {
	n, ok := numberOf(v)
	return ok && n.IsF64()
}

// ValueAsI64 is `Value::as_i64`.
func ValueAsI64(v Value) rs.Option[int64] {
	if n, ok := numberOf(v); ok {
		return n.AsI64()
	}
	return rs.None[int64]()
}

// ValueAsU64 is `Value::as_u64`.
func ValueAsU64(v Value) rs.Option[uint64] {
	if n, ok := numberOf(v); ok {
		return n.AsU64()
	}
	return rs.None[uint64]()
}

// ValueAsF64 is `Value::as_f64`.
func ValueAsF64(v Value) rs.Option[float64] {
	if n, ok := numberOf(v); ok {
		return n.AsF64()
	}
	return rs.None[float64]()
}

// ValueIsArray is `Value::is_array`.
func ValueIsArray(v Value) bool {
	_, ok := v.([]any)
	return ok
}

// ValueAsArray is `Value::as_array`. The slice shares the Value's elements.
func ValueAsArray(v Value) rs.Option[[]Value] {
	a, ok := v.([]any)
	return rs.OptionOf(a, ok)
}

// ValueAsArrayMut is `Value::as_array_mut`: f changes the array the Value
// at v holds, which is stored back afterwards. It reports whether the
// Value is an array (f is not called otherwise).
func ValueAsArrayMut(v *Value, f func(array *[]Value)) bool {
	a, ok := (*v).([]any)
	if !ok {
		return false
	}
	f(&a)
	*v = a
	return true
}

// ValueIsObject is `Value::is_object`.
func ValueIsObject(v Value) bool {
	_, ok := v.(Map)
	return ok
}

// ValueAsObject is `Value::as_object`. The Map shares the Value's members.
func ValueAsObject(v Value) rs.Option[Map] {
	m, ok := v.(Map)
	return rs.OptionOf(m, ok)
}

// ValueAsObjectMut is `Value::as_object_mut`: f changes the object the
// Value at v holds, which is stored back afterwards. It reports whether the
// Value is an object (f is not called otherwise).
func ValueAsObjectMut(v *Value, f func(object *Map)) bool {
	m, ok := (*v).(Map)
	if !ok {
		return false
	}
	f(&m)
	*v = m
	return true
}

// ValueTake is `Value::take`: the value, leaving Null in its place.
func ValueTake(v *Value) Value {
	taken := *v
	*v = nil
	return taken
}

// valueType is `Type`, the kind of a Value in index panics.
func valueType(v Value) string {
	switch v.(type) {
	case nil:
		return "null"
	case bool:
		return "boolean"
	case string:
		return "string"
	case []any:
		return "array"
	case Map:
		return "object"
	}
	if _, ok := numberOf(v); ok {
		return "number"
	}
	return "object"
}

// indexKey reads an index of Value::get and Value[...]: a string key, or an
// int (usize) array position.
func indexKey(index any) (key string, position int, isKey bool) {
	switch i := index.(type) {
	case string:
		return i, 0, true
	case int:
		return "", i, false
	}
	panic(rs.Panicf("serde_json: a Value index is a string key or an int position, not " + reflect.TypeOf(index).String()))
}

// ValueGet is `Value::get`: index into an array (with an int) or an object
// (with a string). None when the Value is not of that kind or has no such
// element.
func ValueGet(v Value, index any) rs.Option[Value] {
	key, position, isKey := indexKey(index)
	if isKey {
		if m, ok := v.(Map); ok {
			return MapGet(m, key)
		}
		return rs.None[Value]()
	}
	if a, ok := v.([]any); ok && position >= 0 && position < len(a) {
		return rs.Some(a[position])
	}
	return rs.None[Value]()
}

// ValueGetMut is `Value::get_mut`: a pointer to the element, nil when there
// is none. Writing through it changes the element in place.
func ValueGetMut(v Value, index any) *Value {
	key, position, isKey := indexKey(index)
	if isKey {
		if m, ok := v.(Map); ok {
			return MapGetMut(m, key)
		}
		return nil
	}
	if a, ok := v.([]any); ok && position >= 0 && position < len(a) {
		return &a[position]
	}
	return nil
}

// ValueIndex is `value[index]`: the element, or Null when there is none.
func ValueIndex(v Value, index any) Value {
	return ValueGet(v, index).UnwrapOr(nil)
}

// ValueIndexMut is `&mut value[index]`: a pointer to the element. A string
// index into Null first makes it an empty object, and a missing key is
// inserted as Null; anything else that cannot be indexed panics as Rust
// does.
func ValueIndexMut(v *Value, index any) *Value {
	key, position, isKey := indexKey(index)
	if isKey {
		if *v == nil {
			*v = Map{}
		}
		m, ok := (*v).(Map)
		if !ok {
			panic(rs.Panicf("cannot access key " + debugStr(key) + " in JSON " + valueType(*v)))
		}
		if slot := MapGetMut(m, key); slot != nil {
			return slot
		}
		m = append(m, jsonvalue.Member{Key: key, Value: nil})
		*v = m
		return &m[len(m)-1].Value
	}
	a, ok := (*v).([]any)
	if !ok {
		panic(rs.Panicf("cannot access index " + strconv.Itoa(position) + " of JSON " + valueType(*v)))
	}
	if position < 0 || position >= len(a) {
		panic(rs.Panicf("cannot access index " + strconv.Itoa(position) + " of JSON array of length " + strconv.Itoa(len(a))))
	}
	return &a[position]
}

// parseIndex is `parse_index`.
func parseIndex(s string) (int, bool) {
	if strings.HasPrefix(s, "+") || (strings.HasPrefix(s, "0") && len(s) != 1) {
		return 0, false
	}
	n, err := strconv.ParseUint(s, 10, 64)
	if err != nil || n > uint64(rs.UsizeMax) {
		return 0, false
	}
	return int(n), true
}

// ValuePointer is `Value::pointer`: look up a value by a JSON Pointer
// (RFC 6901), such as "/data/x" or "/items/0".
func ValuePointer(v Value, pointer string) rs.Option[Value] {
	slot := pointerSlot(&v, pointer)
	if slot == nil {
		return rs.None[Value]()
	}
	return rs.Some(*slot)
}

// ValuePointerMut is `Value::pointer_mut`: a pointer to the value a JSON
// Pointer names, nil when it names none.
func ValuePointerMut(v *Value, pointer string) *Value { return pointerSlot(v, pointer) }

func pointerSlot(v *Value, pointer string) *Value {
	if pointer == "" {
		return v
	}
	if !strings.HasPrefix(pointer, "/") {
		return nil
	}
	target := v
	for _, token := range strings.Split(pointer, "/")[1:] {
		token = strings.ReplaceAll(strings.ReplaceAll(token, "~1", "/"), "~0", "~")
		switch t := (*target).(type) {
		case Map:
			target = MapGetMut(t, token)
		case []any:
			position, ok := parseIndex(token)
			if !ok || position >= len(t) {
				return nil
			}
			target = &t[position]
		default:
			return nil
		}
		if target == nil {
			return nil
		}
	}
	return target
}

// ValueEq is `PartialEq for Value`: as serde_json compares values, objects
// regardless of member order, numbers by kind (1 is not 1.0).
func ValueEq(a, b Value) bool {
	switch x := a.(type) {
	case nil:
		return b == nil
	case bool:
		y, ok := b.(bool)
		return ok && x == y
	case string:
		y, ok := b.(string)
		return ok && x == y
	case []any:
		y, ok := b.([]any)
		if !ok || len(x) != len(y) {
			return false
		}
		for i := range x {
			if !ValueEq(x[i], y[i]) {
				return false
			}
		}
		return true
	case Map:
		y, ok := b.(Map)
		if !ok {
			return false
		}
		x, y = mapMerged(x), mapMerged(y)
		if len(x) != len(y) {
			return false
		}
		for _, member := range x {
			other := MapGet(y, member.Key)
			if other.IsNone() || !ValueEq(member.Value, other.Unwrap()) {
				return false
			}
		}
		return true
	}
	n, okA := numberOf(a)
	m, okB := numberOf(b)
	if !okA || !okB {
		return false
	}
	switch p := n.n.(type) {
	case float64:
		q, ok := m.n.(float64)
		return ok && p == q
	}
	if m.IsF64() {
		return false
	}
	return n.AsI64() == m.AsI64() && n.AsU64() == m.AsU64()
}

// MapNew is `Map::new`: an empty map.
func MapNew() Map { return Map{} }

// MapWithCapacity is `Map::with_capacity`.
func MapWithCapacity(capacity int) Map { return make(Map, 0, capacity) }

func mapIndex(m Map, key string) int {
	for i := len(m) - 1; i >= 0; i-- {
		if m[i].Key == key {
			return i
		}
	}
	return -1
}

// MapGet is `Map::get`.
func MapGet(m Map, key string) rs.Option[Value] {
	if i := mapIndex(m, key); i >= 0 {
		return rs.Some(m[i].Value)
	}
	return rs.None[Value]()
}

// MapGetMut is `Map::get_mut`: a pointer to the value, nil when the key is
// absent.
func MapGetMut(m Map, key string) *Value {
	if i := mapIndex(m, key); i >= 0 {
		return &m[i].Value
	}
	return nil
}

// MapContainsKey is `Map::contains_key`.
func MapContainsKey(m Map, key string) bool { return mapIndex(m, key) >= 0 }

// MapLen is `Map::len`.
func MapLen(m Map) int { return len(mapMerged(m)) }

// MapIsEmpty is `Map::is_empty`.
func MapIsEmpty(m Map) bool { return len(m) == 0 }

// MapInsert is `Map::insert`: an existing key keeps its position and takes
// the value, whose old value is returned; a new key goes at the end.
func MapInsert(m *Map, key string, value Value) rs.Option[Value] {
	*m = mapMerged(*m)
	if *m == nil {
		*m = Map{}
	}
	if i := mapIndex(*m, key); i >= 0 {
		old := (*m)[i].Value
		(*m)[i].Value = value
		return rs.Some(old)
	}
	*m = append(*m, jsonvalue.Member{Key: key, Value: value})
	return rs.None[Value]()
}

// MapRemove is `Map::remove`, which with preserve_order is swap_remove: the
// last member takes the removed one's place.
func MapRemove(m *Map, key string) rs.Option[Value] { return MapSwapRemove(m, key) }

// MapSwapRemove is `Map::swap_remove`.
func MapSwapRemove(m *Map, key string) rs.Option[Value] {
	*m = mapMerged(*m)
	i := mapIndex(*m, key)
	if i < 0 {
		return rs.None[Value]()
	}
	old := (*m)[i].Value
	last := len(*m) - 1
	(*m)[i] = (*m)[last]
	(*m)[last] = jsonvalue.Member{}
	*m = (*m)[:last]
	return rs.Some(old)
}

// MapShiftRemove is `Map::shift_remove`: the members after the removed one
// keep their order.
func MapShiftRemove(m *Map, key string) rs.Option[Value] {
	*m = mapMerged(*m)
	i := mapIndex(*m, key)
	if i < 0 {
		return rs.None[Value]()
	}
	old := (*m)[i].Value
	*m = slices.Delete(*m, i, i+1)
	return rs.Some(old)
}

// MapKeys is `Map::keys`.
func MapKeys(m Map) []string {
	m = mapMerged(m)
	keys := make([]string, len(m))
	for i, member := range m {
		keys[i] = member.Key
	}
	return keys
}

// MapValues is `Map::values`.
func MapValues(m Map) []Value {
	m = mapMerged(m)
	values := make([]Value, len(m))
	for i, member := range m {
		values[i] = member.Value
	}
	return values
}

// MapRetain is `Map::retain`: keep the members for which keep returns true,
// in order. keep may change the value through its pointer.
func MapRetain(m *Map, keep func(key string, value *Value) bool) {
	*m = mapMerged(*m)
	kept := (*m)[:0]
	for i := range *m {
		if keep((*m)[i].Key, &(*m)[i].Value) {
			kept = append(kept, (*m)[i])
		}
	}
	clear((*m)[len(kept):])
	*m = kept
}

// MapAppend is `Map::append`: move every member of other into m, leaving
// other empty.
func MapAppend(m *Map, other *Map) {
	for _, member := range mapMerged(*other) {
		MapInsert(m, member.Key, member.Value)
	}
	*other = Map{}
}

// MapExtend is `Map::extend` with (key, value) pairs.
func MapExtend(m *Map, pairs ...rs.Tuple2[string, Value]) {
	for _, pair := range pairs {
		MapInsert(m, pair.F0, pair.F1)
	}
}

// MapFromIter is `Map::from_iter`.
func MapFromIter(pairs ...rs.Tuple2[string, Value]) Map {
	m := Map{}
	MapExtend(&m, pairs...)
	return m
}

// MapEntryOrInsert is `map.entry(key).or_insert(default)`: a pointer to the
// value of key, inserted as default when absent.
func MapEntryOrInsert(m *Map, key string, value Value) *Value {
	*m = mapMerged(*m)
	if slot := MapGetMut(*m, key); slot != nil {
		return slot
	}
	MapInsert(m, key, value)
	return &(*m)[len(*m)-1].Value
}

// MapEntryOrInsertWith is `map.entry(key).or_insert_with(f)`.
func MapEntryOrInsertWith(m *Map, key string, f func() Value) *Value {
	*m = mapMerged(*m)
	if slot := MapGetMut(*m, key); slot != nil {
		return slot
	}
	MapInsert(m, key, f())
	return &(*m)[len(*m)-1].Value
}

// MapSortKeys is `Map::sort_keys`: sort the members by key, as strings
// compare in Rust (by bytes).
func MapSortKeys(m *Map) {
	*m = mapMerged(*m)
	slices.SortStableFunc(*m, func(a, b jsonvalue.Member) int { return strings.Compare(a.Key, b.Key) })
}

// JsonObj is the `{ ... }` of a `json!` invocation: its members in order,
// each a key and an expression. A key given twice keeps its first position
// and takes the last value, as `Map::insert` does.
type JsonObj []JsonKV

// JsonKV is one `"key": value` of a JsonObj.
type JsonKV struct {
	Key   string
	Value any
}

// JsonArr is the `[ ... ]` of a `json!` invocation.
type JsonArr []any

// Json is the `json!` macro: it builds a Value from Go values written like
// JSON. A JsonObj is an object and a JsonArr an array, nested as needed;
// nil is null; any other value is converted as `serde_json::to_value(&value)
// .unwrap()` converts it, panicking as that unwrap does when it fails.
//
//	serdejson.Json(serdejson.JsonObj{
//		{"code", 200},
//		{"features", serdejson.JsonArr{"serde", "json"}},
//	})
func Json(value any) Value {
	switch v := value.(type) {
	case nil:
		return nil
	case JsonObj:
		m := mapBuilder{m: Map{}}
		for _, member := range v {
			m.insert(member.Key, Json(member.Value))
		}
		return m.m
	case JsonArr:
		array := make([]any, len(v))
		for i, element := range v {
			array[i] = Json(element)
		}
		return array
	}
	converted, err := valueOfValue(value)
	if err != nil {
		panic(rs.Panicf("called `Result::unwrap()` on an `Err` value: " + err.(*Error).DebugString()))
	}
	return converted
}

// debugStr is `format!("{:?}", s)` for a str.
func debugStr(s string) string { return serde.DebugStr(s) }
