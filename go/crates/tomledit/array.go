// Port of toml_edit 0.20.2 src/array.rs

package tomledit

import (
	"iter"
	"slices"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Array is toml_edit::Array: type representing a TOML array, payload of the
// `Value::Array` variant's value.
type Array struct {
	// `trailing` represents whitespaces, newlines
	// and comments in an empty array or after the trailing comma
	trailing      RawString
	trailingComma bool
	// prefix before `[` and suffix after `]`
	decor Decor
	span  rs.Option[Range]
	// always Vec<Item::Value>
	values []Item
}

/// Constructors
///
/// See also `FromIterator`

// ArrayNew is Array::new: create an empty `Array`.
//
//	let mut arr = toml_edit::Array::new();
func ArrayNew() Array {
	return Array{}
}

// ArrayDefault is `impl Default for Array`.
func ArrayDefault() Array {
	return Array{}
}

func arrayWithVec(values []Item) Array {
	return Array{values: values}
}

/// Formatting

// Fmt is Array::fmt: auto formats the array.
func (a *Array) Fmt() {
	decorateArray(a)
}

// SetTrailingComma is Array::set_trailing_comma: set whether the array will
// use a trailing comma.
func (a *Array) SetTrailingComma(yes bool) {
	a.trailingComma = yes
}

// TrailingComma is Array::trailing_comma: whether the array will use a
// trailing comma.
func (a *Array) TrailingComma() bool {
	return a.trailingComma
}

// SetTrailing is Array::set_trailing: set whitespace after last element
// (a string or a RawString).
func (a *Array) SetTrailing(trailing any) {
	a.trailing = intoRawString(trailing)
}

// Trailing is Array::trailing: whitespace after last element.
func (a *Array) Trailing() *RawString {
	return &a.trailing
}

// DecorMut is Array::decor_mut: returns the surrounding whitespace.
func (a *Array) DecorMut() *Decor {
	return &a.decor
}

// Decor is Array::decor: returns the surrounding whitespace.
func (a *Array) Decor() *Decor {
	return &a.decor
}

func (a *Array) despan(input string) {
	a.span = rs.None[Range]()
	a.decor.despan(input)
	a.trailing.despan(input)
	for index := range a.values {
		a.values[index].despan(input)
	}
}

// Iter is Array::iter: an iterator over all values.
func (a *Array) Iter() iter.Seq[Value] {
	return func(yield func(Value) bool) {
		for _, item := range a.values {
			if value, ok := item.(*Item_Value); ok {
				if !yield(value.F0) {
					return
				}
			}
		}
	}
}

// IterMut is Array::iter_mut: an iterator over all values. Assigning through
// a yielded pointer replaces the value in the array.
func (a *Array) IterMut() iter.Seq[*Value] {
	return func(yield func(*Value) bool) {
		for _, item := range a.values {
			if value, ok := item.(*Item_Value); ok {
				if !yield(&value.F0) {
					return
				}
			}
		}
	}
}

// Len is Array::len: the length of the underlying Vec.
//
// In some rare cases, placeholder elements will exist. For a more accurate
// count, call `a.iter().count()`
//
//	let mut arr = toml_edit::Array::new();
//	arr.push(1);
//	arr.push("foo");
//	assert_eq!(arr.len(), 2);
func (a *Array) Len() int {
	return len(a.values)
}

// IsEmpty is Array::is_empty: return true iff `self.len() == 0`.
//
//	let mut arr = toml_edit::Array::new();
//	assert!(arr.is_empty());
//
//	arr.push(1);
//	arr.push("foo");
//	assert!(! arr.is_empty());
func (a *Array) IsEmpty() bool {
	return a.Len() == 0
}

// Clear is Array::clear: clears the array, removing all values. Keeps the
// allocated memory for reuse.
func (a *Array) Clear() {
	a.values = nil
}

// Get is Array::get: a reference to the value at the given index, or `None`
// if the index is out of bounds.
func (a *Array) Get(index int) rs.Option[Value] {
	if index < 0 || index >= len(a.values) {
		return rs.None[Value]()
	}
	return a.values[index].AsValue()
}

// GetMut is Array::get_mut: a reference to the value at the given index, or
// `None` if the index is out of bounds.
func (a *Array) GetMut(index int) rs.Option[*Value] {
	if index < 0 || index >= len(a.values) {
		return rs.None[*Value]()
	}
	return a.values[index].AsValueMut()
}

// Push is Array::push: appends a new value (`V: Into<Value>`, see IntoValue)
// to the end of the array, applying default formatting to it.
//
//	let mut arr = toml_edit::Array::new();
//	arr.push(1);
//	arr.push("foo");
func (a *Array) Push(v any) {
	value := a.valueOp(IntoValue(v), true)
	a.values = append(a.values, &Item_Value{F0: value})
}

// PushFormatted is Array::push_formatted: appends a new, already formatted
// value to the end of the array.
//
//	let formatted_value = "'literal'".parse::<toml_edit::Value>().unwrap();
//	let mut arr = toml_edit::Array::new();
//	arr.push_formatted(formatted_value);
func (a *Array) PushFormatted(v Value) {
	a.values = append(a.values, &Item_Value{F0: v})
}

// Insert is Array::insert: inserts an element at the given position within
// the array, applying default formatting to it and shifting all values after
// it to the right.
//
// # Panics
//
// Panics if `index > len`.
//
//	let mut arr = toml_edit::Array::new();
//	arr.push(1);
//	arr.push("foo");
//
//	arr.insert(0, "start");
func (a *Array) Insert(index int, v any) {
	value := a.valueOp(IntoValue(v), true)
	a.vecInsert(index, &Item_Value{F0: value})
}

// InsertFormatted is Array::insert_formatted: inserts an already formatted
// value at the given position within the array, shifting all values after it
// to the right.
//
// # Panics
//
// Panics if `index > len`.
//
//	let mut arr = toml_edit::Array::new();
//	arr.push(1);
//	arr.push("foo");
//
//	let formatted_value = "'start'".parse::<toml_edit::Value>().unwrap();
//	arr.insert_formatted(0, formatted_value);
func (a *Array) InsertFormatted(index int, v Value) {
	a.vecInsert(index, &Item_Value{F0: v})
}

// vecInsert is Vec::insert, with its panic.
func (a *Array) vecInsert(index int, item Item) {
	if index < 0 || index > len(a.values) {
		rs.PanicAt(2, "insertion index (is "+strconv.Itoa(index)+") should be <= len (is "+strconv.Itoa(len(a.values))+")")
	}
	a.values = slices.Insert(a.values, index, item)
}

// Replace is Array::replace: replaces the element at the given position
// within the array, preserving existing formatting.
//
// # Panics
//
// Panics if `index >= len`.
//
//	let mut arr = toml_edit::Array::new();
//	arr.push(1);
//	arr.push("foo");
//
//	arr.replace(0, "start");
func (a *Array) Replace(index int, v any) Value {
	// Read the existing value's decor and preserve it.
	existing, ok := a.Get(index).Get()
	if !ok {
		rs.PanicAt(1, "index "+strconv.Itoa(index)+" out of bounds (len = "+strconv.Itoa(a.Len())+")")
	}
	existingDecor := existing.Decor()
	value := IntoValue(v)
	*value.DecorMut() = existingDecor.Clone()
	return a.ReplaceFormatted(index, value)
}

// ReplaceFormatted is Array::replace_formatted: replaces the element at the
// given position within the array with an already formatted value.
//
// # Panics
//
// Panics if `index >= len`.
//
//	let mut arr = toml_edit::Array::new();
//	arr.push(1);
//	arr.push("foo");
//
//	let formatted_value = "'start'".parse::<toml_edit::Value>().unwrap();
//	arr.replace_formatted(0, formatted_value);
func (a *Array) ReplaceFormatted(index int, v Value) Value {
	if index < 0 || index >= len(a.values) {
		rs.PanicAt(1, "index out of bounds: the len is "+strconv.Itoa(len(a.values))+" but the index is "+strconv.Itoa(index))
	}
	old := a.values[index]
	a.values[index] = &Item_Value{F0: v}
	if oldValue, ok := old.(*Item_Value); ok {
		return oldValue.F0
	}
	panic(rs.Panicf("non-value item {:?} in an array", old))
}

// Remove is Array::remove: removes the value at the given index.
//
//	let mut arr = toml_edit::Array::new();
//	arr.push(1);
//	arr.push("foo");
//
//	arr.remove(0);
//	assert_eq!(arr.len(), 1);
func (a *Array) Remove(index int) Value {
	if index < 0 || index >= len(a.values) {
		rs.PanicAt(1, "removal index (is "+strconv.Itoa(index)+") should be < len (is "+strconv.Itoa(len(a.values))+")")
	}
	removed := a.values[index]
	a.values = slices.Delete(a.values, index, index+1)
	if value, ok := removed.(*Item_Value); ok {
		return value.F0
	}
	panic(rs.Panicf("non-value item {:?} in an array", removed))
}

// Retain is Array::retain: retains only the values specified by the `keep`
// predicate.
//
// In other words, remove all values for which `keep(&value)` returns `false`.
//
// This method operates in place, visiting each element exactly once in the
// original order, and preserves the order of the retained elements.
func (a *Array) Retain(keep func(Value) bool) {
	a.values = slices.DeleteFunc(a.values, func(item Item) bool {
		if value, ok := item.(*Item_Value); ok {
			return !keep(value.F0)
		}
		return true
	})
}

func (a *Array) valueOp(v Value, decorate bool) Value {
	value := v
	if !a.IsEmpty() && decorate {
		value.decorate(RawStringFromString(" "), RawStringFromString(""))
	} else if decorate {
		value.decorate(RawStringFromString(""), RawStringFromString(""))
	}
	return value
}

// Clone is the derived `Clone for Array`.
func (a *Array) Clone() Array {
	cloned := *a
	cloned.values = make([]Item, len(a.values))
	for index, item := range a.values {
		cloned.values[index] = item.Clone()
	}
	return cloned
}

// String is `impl Display for Array`.
func (a *Array) String() string {
	var buf strings.Builder
	encodeArray(a, &buf, rs.None[string](), "", "")
	return buf.String()
}

// FmtDisplay is `impl Display for Array`.
func (a *Array) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(a.String()) }

// FmtDebug is the derived `Debug for Array`.
func (a *Array) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Array").Field("trailing", a.trailing).Field("trailing_comma", a.trailingComma).
		Field("decor", a.decor).Field("span", a.span).Field("values", a.values).Finish()
}

// Extend is `impl Extend<V> for Array` with V: Into<Value> (see IntoValue).
func (a *Array) Extend(values ...any) {
	for _, value := range values {
		a.PushFormatted(IntoValue(value))
	}
}

// ArrayFromIter is `impl FromIterator<V> for Array` with V: Into<Value> (see
// IntoValue).
func ArrayFromIter(values ...any) Array {
	items := make([]Item, 0, len(values))
	for _, value := range values {
		items = append(items, &Item_Value{F0: IntoValue(value)})
	}
	return Array{values: items}
}

// IntoIter is `impl IntoIterator for Array`: the values of the array.
func (a *Array) IntoIter() iter.Seq[Value] {
	return func(yield func(Value) bool) {
		for _, item := range a.values {
			if item.IsValue() {
				value, _ := item.IntoValue()
				if !yield(value) {
					return
				}
			}
		}
	}
}

func decorateArray(array *Array) {
	index := 0
	for _, item := range array.values {
		value, ok := item.(*Item_Value)
		if !ok {
			continue
		}
		// [value1, value2, value3]
		if index == 0 {
			value.F0.decorate(RawStringFromString(defaultLeadingValueDecorPrefix), RawStringFromString(defaultLeadingValueDecorSuffix))
		} else {
			value.F0.decorate(RawStringFromString(defaultValueDecorPrefix), RawStringFromString(defaultValueDecorSuffix))
		}
		index++
	}
	// Since everything is now on the same line, remove trailing commas and whitespace.
	array.SetTrailingComma(false)
	array.SetTrailing("")
}
