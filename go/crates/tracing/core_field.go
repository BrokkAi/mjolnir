// Port of tracing-core 0.1.36 src/field.rs

package tracing

import (
	"math"
	"reflect"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// FieldField is tracing::field::Field: one named field of a span or an
// event. It is a key, the index of a name in a FieldFieldSet, not a value.
type FieldField struct {
	i      int
	fields FieldFieldSet
}

// FieldFieldKey identifies a field for use as a map key: two fields are the
// same when they have the same callsite and index (FieldField.Equal).
type FieldFieldKey struct {
	callsite CallsiteIdentifier
	index    int
}

// FieldEmpty is tracing::field::Empty: the value of a field that is declared
// now and recorded later with Span.Record.
type FieldEmpty struct{}

// FieldFieldSet is tracing::field::FieldSet: the names of the fields of one
// callsite, in the order they were written.
type FieldFieldSet struct {
	names    []string
	callsite CallsiteIdentifier
}

// FieldValueSet is tracing::field::ValueSet: values for some or all of the
// fields of a FieldFieldSet.
type FieldValueSet struct {
	// explicit holds field and value pairs (Values::Explicit); all holds
	// one value for each field in order (Values::All). A nil value is None.
	explicit []fieldValuePair
	all      []any
	isAll    bool
	fields   *FieldFieldSet
}

type fieldValuePair struct {
	field FieldField
	value any
}

// FieldVisit is tracing::field::Visit: what a subscriber implements to read
// the values of a span or an event. Each value arrives through the method
// for its kind. Only RecordDebug has no default; an implementation that does
// not treat a kind specially forwards it with the matching
// FieldVisitDefault function, as the Rust trait's default methods do.
//
// A value given to RecordDebug is one to print with {:?}; FieldDebugString
// gives that text.
//
// record_i128 and record_u128 are not here: rs has no 128-bit integers yet.
// A Rust u128 such as `elapsed.as_millis()` is a uint64 in the port and
// arrives through RecordU64.
type FieldVisit interface {
	// RecordF64 visits a double-precision floating point value.
	RecordF64(field *FieldField, value float64)
	// RecordI64 visits a signed 64-bit integer value.
	RecordI64(field *FieldField, value int64)
	// RecordU64 visits an unsigned 64-bit integer value.
	RecordU64(field *FieldField, value uint64)
	// RecordBool visits a boolean value.
	RecordBool(field *FieldField, value bool)
	// RecordStr visits a string value.
	RecordStr(field *FieldField, value string)
	// RecordBytes visits a byte slice.
	RecordBytes(field *FieldField, value []byte)
	// RecordError records an error value, which Rust passes as a
	// `&(dyn Error + 'static)`.
	RecordError(field *FieldField, value error)
	// RecordDebug visits a value that prints with {:?}.
	RecordDebug(field *FieldField, value any)
}

// FieldVisitDefaultRecordF64 is the default body of Visit::record_f64.
func FieldVisitDefaultRecordF64(visitor FieldVisit, field *FieldField, value float64) {
	visitor.RecordDebug(field, value)
}

// FieldVisitDefaultRecordI64 is the default body of Visit::record_i64.
func FieldVisitDefaultRecordI64(visitor FieldVisit, field *FieldField, value int64) {
	visitor.RecordDebug(field, value)
}

// FieldVisitDefaultRecordU64 is the default body of Visit::record_u64.
func FieldVisitDefaultRecordU64(visitor FieldVisit, field *FieldField, value uint64) {
	visitor.RecordDebug(field, value)
}

// FieldVisitDefaultRecordBool is the default body of Visit::record_bool.
func FieldVisitDefaultRecordBool(visitor FieldVisit, field *FieldField, value bool) {
	visitor.RecordDebug(field, value)
}

// FieldVisitDefaultRecordStr is the default body of Visit::record_str.
func FieldVisitDefaultRecordStr(visitor FieldVisit, field *FieldField, value string) {
	visitor.RecordDebug(field, value)
}

// FieldVisitDefaultRecordBytes is the default body of Visit::record_bytes:
// the bytes print in hexadecimal, as `[de ad be ef]`.
func FieldVisitDefaultRecordBytes(visitor FieldVisit, field *FieldField, value []byte) {
	visitor.RecordDebug(field, hexBytes(value))
}

// FieldVisitDefaultRecordError is the default body of Visit::record_error:
// the error prints with its Display text.
func FieldVisitDefaultRecordError(visitor FieldVisit, field *FieldField, value error) {
	visitor.RecordDebug(field, FieldDisplayValue{value: value})
}

// FieldValue is tracing::field::Value: a value that can be recorded as a
// field. The Go values the crate implements Value for (strings, booleans,
// integers, floats, byte slices, errors, options of those) need no method and
// are recorded by their type; this interface is for the wrappers below and
// for a type that records itself.
type FieldValue interface {
	// Record visits the value with visitor.
	Record(key *FieldField, visitor FieldVisit)
}

// FieldDisplayValue is tracing::field::DisplayValue: a value recorded by its
// Display text, the `%value` of the macros.
type FieldDisplayValue struct {
	value any
}

// FieldDebugValue is tracing::field::DebugValue: a value recorded by its
// Debug text, the `?value` of the macros.
type FieldDebugValue struct {
	value any
}

// FieldDisplay is tracing::field::display: it wraps a value that is Display
// so that it is recorded by its Display text.
func FieldDisplay(value any) FieldDisplayValue {
	return FieldDisplayValue{value: value}
}

// FieldDebug is tracing::field::debug: it wraps a value that is Debug so
// that it is recorded by its Debug text.
func FieldDebug(value any) FieldDebugValue {
	return FieldDebugValue{value: value}
}

// hexBytes is the crate's HexBytes: bytes whose Debug form is hexadecimal.
type hexBytes []byte

func (b hexBytes) FmtDebug(f *rs.Formatter) error {
	return f.WriteStr(b.debugString())
}

func (b hexBytes) debugString() string {
	const digits = "0123456789abcdef"
	var out strings.Builder
	out.WriteByte('[')
	for i, value := range b {
		if i > 0 {
			out.WriteByte(' ')
		}
		out.WriteByte(digits[value>>4])
		out.WriteByte(digits[value&0xf])
	}
	out.WriteByte(']')
	return out.String()
}

// ===== impl Visit =====

// FieldVisitFn is `impl Visit for F where F: FnMut(&Field, &dyn Debug)`: a
// function is a visitor that receives every value through RecordDebug.
type FieldVisitFn func(field *FieldField, value any)

func (f FieldVisitFn) RecordF64(field *FieldField, value float64) {
	FieldVisitDefaultRecordF64(f, field, value)
}

func (f FieldVisitFn) RecordI64(field *FieldField, value int64) {
	FieldVisitDefaultRecordI64(f, field, value)
}

func (f FieldVisitFn) RecordU64(field *FieldField, value uint64) {
	FieldVisitDefaultRecordU64(f, field, value)
}

func (f FieldVisitFn) RecordBool(field *FieldField, value bool) {
	FieldVisitDefaultRecordBool(f, field, value)
}

func (f FieldVisitFn) RecordStr(field *FieldField, value string) {
	FieldVisitDefaultRecordStr(f, field, value)
}

func (f FieldVisitFn) RecordBytes(field *FieldField, value []byte) {
	FieldVisitDefaultRecordBytes(f, field, value)
}

func (f FieldVisitFn) RecordError(field *FieldField, value error) {
	FieldVisitDefaultRecordError(f, field, value)
}

func (f FieldVisitFn) RecordDebug(field *FieldField, value any) { f(field, value) }

// ===== impl Value =====

var rsPackagePath = reflect.TypeFor[rs.Unit]().PkgPath()

// recordValue is Value::record for a Go value: it calls the visitor method
// of the value's kind, as the crate's `impl Value` for that type does.
//
// A value of a type Rust has no `impl Value` for is a compile error there
// (it has to be passed with `%` or `?`), and is a panic here.
func recordValue(value any, key *FieldField, visitor FieldVisit) {
	switch value := value.(type) {
	case nil:
		// Option::None
	case FieldValue:
		value.Record(key, visitor)
	case string:
		visitor.RecordStr(key, value)
	case bool:
		visitor.RecordBool(key, value)
	case int:
		visitor.RecordI64(key, int64(value))
	case int8:
		visitor.RecordI64(key, int64(value))
	case int16:
		visitor.RecordI64(key, int64(value))
	case int32:
		visitor.RecordI64(key, int64(value))
	case int64:
		visitor.RecordI64(key, value)
	case uint:
		visitor.RecordU64(key, uint64(value))
	case uint8:
		visitor.RecordU64(key, uint64(value))
	case uint16:
		visitor.RecordU64(key, uint64(value))
	case uint32:
		visitor.RecordU64(key, uint64(value))
	case uint64:
		visitor.RecordU64(key, value)
	case float32:
		visitor.RecordF64(key, float64(value))
	case float64:
		visitor.RecordF64(key, value)
	case []byte:
		visitor.RecordBytes(key, value)
	case error:
		visitor.RecordError(key, value)
	default:
		recordReflected(value, key, visitor)
	}
}

// recordReflected records the two shapes a type switch cannot name: an
// rs.Option (`impl Value for Option<T>`) and a pointer (`impl Value for &T`).
func recordReflected(value any, key *FieldField, visitor FieldVisit) {
	reflected := reflect.ValueOf(value)
	kind := reflected.Type()
	switch {
	case kind.Kind() == reflect.Struct && kind.PkgPath() == rsPackagePath && strings.HasPrefix(kind.Name(), "Option["):
		results := reflected.MethodByName("Get").Call(nil)
		if results[1].Bool() {
			recordValue(results[0].Interface(), key, visitor)
		}
	case kind.Kind() == reflect.Pointer && !reflected.IsNil():
		recordValue(reflected.Elem().Interface(), key, visitor)
	default:
		rs.PanicAt(3, "tracing: field `"+key.Name()+"` has a value of type "+kind.String()+
			", which is not a tracing Value; pass it with FDisplay (`%`) or FDebug (`?`)")
	}
}

// ===== impl DisplayValue =====

// Record is `impl Value for DisplayValue`.
func (v FieldDisplayValue) Record(key *FieldField, visitor FieldVisit) {
	visitor.RecordDebug(key, v)
}

// FmtDebug is `impl Debug for DisplayValue`: the value's Display text.
func (v FieldDisplayValue) FmtDebug(f *rs.Formatter) error { return rs.DisplayFmt(v.value, f) }

// FmtDisplay is `impl Display for DisplayValue`.
func (v FieldDisplayValue) FmtDisplay(f *rs.Formatter) error { return rs.DisplayFmt(v.value, f) }

// String is the value's Display text.
func (v FieldDisplayValue) String() string { return displayString(v.value) }

// ===== impl DebugValue =====

// Record is `impl Value for DebugValue`.
func (v FieldDebugValue) Record(key *FieldField, visitor FieldVisit) {
	visitor.RecordDebug(key, v.value)
}

// FmtDebug is `impl Debug for DebugValue`.
func (v FieldDebugValue) FmtDebug(f *rs.Formatter) error { return rs.DebugFmt(v.value, f) }

// Record is `impl Value for Empty`: nothing is recorded.
func (FieldEmpty) Record(*FieldField, FieldVisit) {}

// FmtDebug is the derived Debug of Empty.
func (FieldEmpty) FmtDebug(f *rs.Formatter) error { return f.WriteStr("Empty") }

// ===== impl Field =====

// Callsite returns the identity of the callsite that defines the field.
func (f *FieldField) Callsite() CallsiteIdentifier { return f.fields.callsite }

// Name returns the field's name.
func (f *FieldField) Name() string { return f.fields.names[f.i] }

// Index returns the field's position in its callsite's fields.
func (f *FieldField) Index() int { return f.i }

// Key returns the field's identity as a map key.
func (f *FieldField) Key() FieldFieldKey {
	return FieldFieldKey{callsite: f.fields.callsite, index: f.i}
}

// String is the field's Display text: its name.
func (f *FieldField) String() string { return f.Name() }

// FmtDisplay is `impl Display for Field`.
func (f *FieldField) FmtDisplay(formatter *rs.Formatter) error { return formatter.Pad(f.Name()) }

// FmtDebug is the derived Debug of Field.
func (f *FieldField) FmtDebug(formatter *rs.Formatter) error {
	return formatter.DebugStruct("Field").Field("i", f.i).Field("fields", &f.fields).Finish()
}

// AsRef is `impl AsRef<str> for Field`.
func (f *FieldField) AsRef() string { return f.Name() }

// Equal is `impl PartialEq for Field`.
func (f *FieldField) Equal(other *FieldField) bool {
	return f.Callsite() == other.Callsite() && f.i == other.i
}

// Clone is `impl Clone for Field`.
func (f *FieldField) Clone() FieldField {
	return FieldField{i: f.i, fields: f.fields}
}

// ===== impl FieldSet =====

// FieldFieldSetNew is FieldSet::new.
func FieldFieldSetNew(names []string, callsite CallsiteIdentifier) FieldFieldSet {
	return FieldFieldSet{names: names, callsite: callsite}
}

// Field is FieldSet::field: the field named name, if there is one.
func (s *FieldFieldSet) Field(name string) rs.Option[FieldField] {
	for i, candidate := range s.names {
		if candidate == name {
			return rs.Some(FieldField{i: i, fields: *s})
		}
	}
	return rs.None[FieldField]()
}

// Contains is FieldSet::contains.
func (s *FieldFieldSet) Contains(field *FieldField) bool {
	return field.Callsite() == s.callsite && field.i <= s.Len()
}

// Iter is FieldSet::iter: the fields in order.
func (s *FieldFieldSet) Iter() []FieldField {
	fields := make([]FieldField, len(s.names))
	for i := range s.names {
		fields[i] = FieldField{i: i, fields: *s}
	}
	return fields
}

// ValueSet is FieldSet::value_set: values for the given fields. A nil value
// is None.
func (s *FieldFieldSet) ValueSet(fields []FieldField, values []any) *FieldValueSet {
	rs.Assert(len(fields) == len(values))
	pairs := make([]fieldValuePair, len(fields))
	for i := range fields {
		pairs[i] = fieldValuePair{field: fields[i], value: values[i]}
	}
	return &FieldValueSet{explicit: pairs, fields: s}
}

// ValueSetAll is FieldSet::value_set_all: one value for each field, in
// order. A nil value is None.
func (s *FieldFieldSet) ValueSetAll(values []any) *FieldValueSet {
	rs.DebugAssert(len(values) == s.Len())
	return &FieldValueSet{all: values, isAll: true, fields: s}
}

// Len returns the number of fields.
func (s *FieldFieldSet) Len() int { return len(s.names) }

// IsEmpty reports whether there are no fields.
func (s *FieldFieldSet) IsEmpty() bool { return len(s.names) == 0 }

// FmtDebug is `impl Debug for FieldSet`.
func (s *FieldFieldSet) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("FieldSet").Field("names", s.names).Field("callsite", s.callsite).Finish()
}

// String is the set's Display text: the names between braces.
func (s *FieldFieldSet) String() string {
	return "{" + strings.Join(s.names, ", ") + "}"
}

// FmtDisplay is `impl Display for FieldSet`.
func (s *FieldFieldSet) FmtDisplay(f *rs.Formatter) error {
	set := f.DebugSet()
	for _, name := range s.names {
		set.Entry(FieldDisplay(name))
	}
	return set.Finish()
}

// Equal is `impl PartialEq for FieldSet`.
func (s *FieldFieldSet) Equal(other *FieldFieldSet) bool {
	if s == other {
		return true
	}
	if !rs.DebugAssertions {
		// In a well-behaving application, two `FieldSet`s can be assumed to
		// be totally equal so long as they share the same callsite.
		return s.callsite == other.callsite
	}
	if s.callsite != other.callsite || len(s.names) != len(other.names) {
		return false
	}
	for i := range s.names {
		if s.names[i] != other.names[i] {
			return false
		}
	}
	return true
}

// ===== impl ValueSet =====

// Callsite returns the identity of the callsite the values belong to.
func (v *FieldValueSet) Callsite() CallsiteIdentifier { return v.fields.callsite }

// Record is ValueSet::record: it visits every value that is present, in
// order.
func (v *FieldValueSet) Record(visitor FieldVisit) {
	if v.isAll {
		for i, value := range v.all {
			if value != nil {
				field := FieldField{i: i, fields: *v.fields}
				recordValue(value, &field, visitor)
			}
		}
		return
	}
	myCallsite := v.Callsite()
	for i := range v.explicit {
		pair := &v.explicit[i]
		if pair.field.Callsite() != myCallsite {
			continue
		}
		if pair.value != nil {
			recordValue(pair.value, &pair.field, visitor)
		}
	}
}

// Len is ValueSet::len: the number of fields the set has entries for.
func (v *FieldValueSet) Len() int {
	if v.isAll {
		return len(v.all)
	}
	myCallsite := v.Callsite()
	count := 0
	for i := range v.explicit {
		if v.explicit[i].field.Callsite() == myCallsite {
			count++
		}
	}
	return count
}

// Contains is ValueSet::contains: whether the set holds a value for field.
func (v *FieldValueSet) Contains(field *FieldField) bool {
	if field.Callsite() != v.Callsite() {
		return false
	}
	if v.isAll {
		return v.all[field.i] != nil
	}
	for i := range v.explicit {
		if v.explicit[i].field.Equal(field) && v.explicit[i].value != nil {
			return true
		}
	}
	return false
}

// IsEmpty is ValueSet::is_empty: whether no value is present.
func (v *FieldValueSet) IsEmpty() bool {
	if v.isAll {
		for _, value := range v.all {
			if value != nil {
				return false
			}
		}
		return true
	}
	myCallsite := v.Callsite()
	for i := range v.explicit {
		if v.explicit[i].value != nil && v.explicit[i].field.Callsite() == myCallsite {
			return false
		}
	}
	return true
}

// FieldSet returns the fields the values are for.
func (v *FieldValueSet) FieldSet() *FieldFieldSet { return v.fields }

// FmtDebug is `impl Debug for ValueSet`.
func (v *FieldValueSet) FmtDebug(f *rs.Formatter) error {
	s := f.DebugStruct("ValueSet")
	v.Record(FieldVisitFn(func(field *FieldField, value any) {
		s.Field(field.Name(), value)
	}))
	return s.Field("callsite", v.Callsite()).Finish()
}

// FmtDisplay is `impl Display for ValueSet`.
func (v *FieldValueSet) FmtDisplay(f *rs.Formatter) error {
	s := f.DebugMap()
	v.Record(FieldVisitFn(func(field *FieldField, value any) {
		s.Entry(FieldDisplay(field.Name()), value)
	}))
	return s.Finish()
}

// ===== text of recorded values =====

// FieldDebugString is `format!("{:?}", value)` for a value a visitor was
// given: the text a subscriber writes for the field.
//
// The kinds the macros record without a sigil, and a `%value` whose value is
// a string, an error or has a String method, are written here directly. Any
// other value goes through the formatter.
func FieldDebugString(value any) string {
	switch value := value.(type) {
	case FieldDisplayValue:
		return displayString(value.value)
	case FieldDebugValue:
		return FieldDebugString(value.value)
	case *formatArguments:
		return value.String()
	case string:
		return debugStr(value)
	case bool:
		return strconv.FormatBool(value)
	case int:
		return strconv.FormatInt(int64(value), 10)
	case int8:
		return strconv.FormatInt(int64(value), 10)
	case int16:
		return strconv.FormatInt(int64(value), 10)
	case int32:
		return strconv.FormatInt(int64(value), 10)
	case int64:
		return strconv.FormatInt(value, 10)
	case uint:
		return strconv.FormatUint(uint64(value), 10)
	case uint8:
		return strconv.FormatUint(uint64(value), 10)
	case uint16:
		return strconv.FormatUint(uint64(value), 10)
	case uint32:
		return strconv.FormatUint(uint64(value), 10)
	case uint64:
		return strconv.FormatUint(value, 10)
	case float64:
		return debugF64(value)
	case hexBytes:
		return value.debugString()
	}
	return rs.DebugString(value)
}

// FieldDisplayString is `value.to_string()` for a value recorded with `%`.
func FieldDisplayString(value any) string {
	return displayString(value)
}

// displayString is the Display text of a value with the default flags. A
// string is itself; an error's Error method and a Display type's String
// method hold that text by the port's convention.
func displayString(value any) string {
	switch value := value.(type) {
	case string:
		return value
	case error:
		return value.Error()
	case interface{ String() string }:
		return value.String()
	case bool:
		return strconv.FormatBool(value)
	case int:
		return strconv.FormatInt(int64(value), 10)
	case int8:
		return strconv.FormatInt(int64(value), 10)
	case int16:
		return strconv.FormatInt(int64(value), 10)
	case int32:
		return strconv.FormatInt(int64(value), 10)
	case int64:
		return strconv.FormatInt(value, 10)
	case uint:
		return strconv.FormatUint(uint64(value), 10)
	case uint8:
		return strconv.FormatUint(uint64(value), 10)
	case uint16:
		return strconv.FormatUint(uint64(value), 10)
	case uint32:
		return strconv.FormatUint(uint64(value), 10)
	case uint64:
		return strconv.FormatUint(value, 10)
	}
	return rs.ToString(value)
}

// debugStr is `format!("{:?}", s)` for a string. A string of printable ASCII
// with no quote or backslash, which is what Rust's own fast path covers, is
// quoted as it is; any other goes through the formatter's escaping.
func debugStr(s string) string {
	for i := 0; i < len(s); i++ {
		if b := s[i]; b > 0x7E || b < 0x20 || b == '\\' || b == '"' {
			return rs.DebugString(s)
		}
	}
	return `"` + s + `"`
}

// debugF64 is `format!("{:?}", value)` for an f64: the shortest digits that
// read back as the same number, with at least one fractional digit, and in
// exponent form below 1e-4 and from 1e16 up.
func debugF64(value float64) string {
	switch {
	case math.IsNaN(value):
		return "NaN"
	case math.IsInf(value, 1):
		return "inf"
	case math.IsInf(value, -1):
		return "-inf"
	}
	abs := math.Abs(value)
	if abs != 0 && (abs < 1e-4 || abs >= 1e16) {
		text := strconv.FormatFloat(value, 'e', -1, 64)
		mantissa, exponent, _ := strings.Cut(text, "e")
		exponent = strings.TrimPrefix(exponent, "+")
		negative := strings.HasPrefix(exponent, "-")
		exponent = strings.TrimLeft(strings.TrimPrefix(exponent, "-"), "0")
		if negative {
			exponent = "-" + exponent
		}
		return mantissa + "e" + exponent
	}
	text := strconv.FormatFloat(value, 'f', -1, 64)
	if !strings.Contains(text, ".") {
		text += ".0"
	}
	return text
}
