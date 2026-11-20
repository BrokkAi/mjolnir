// Port of toml 0.8.2 src/value.rs

package toml

import (
	"math"
	"reflect"
	"strconv"

	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// Definition of a TOML [value][Value]

// The datetime types toml re-exports from toml_datetime (`toml::value::Date`
// and so on). go/crates/tomledit holds them, as toml_edit re-exports the same
// crate.
type (
	// ValueDate is toml::value::Date.
	ValueDate = tomledit.Date
	// ValueDatetime is toml::value::Datetime.
	ValueDatetime = tomledit.Datetime
	// ValueDatetimeParseError is toml::value::DatetimeParseError.
	ValueDatetimeParseError = tomledit.DatetimeParseError
	// ValueOffset is toml::value::Offset.
	ValueOffset = tomledit.Offset
	// ValueTime is toml::value::Time.
	ValueTime = tomledit.Time
)

// ValueArray is toml::value::Array: type representing a TOML array, payload
// of the `Value::Array` variant.
type ValueArray = []Value

// ValueTable is toml::value::Table, the same type as toml::Table.
type ValueTable = Table

// Value is toml::Value: representation of a TOML value.
//
// It is a Rust enum, so a value is a pointer to one of the variant structs
// *Value_String, *Value_Integer, *Value_Float, *Value_Boolean,
// *Value_Datetime, *Value_Array and *Value_Table. The methods are the enum's;
// value_variants.go gives them to each variant. Clone, PartialEq and Debug
// are derived: use rs.Clone, rs.Eq and the formatter.
type Value interface {
	isValue()

	// Get is Value::get: index into a TOML array or map. A string index
	// can be used to access a value in a map, and a usize (int) index can be
	// used to access an element of an array.
	//
	// Returns `None` if the type of `self` does not match the type of the
	// index, for example if the index is a string and `self` is an array or
	// a number. Also returns `None` if the given key does not exist in the
	// map or the given index is not within the bounds of the array.
	Get(index any) rs.Option[Value]
	// GetMut is Value::get_mut: mutably index into a TOML array or map. A
	// string index can be used to access a value in a map, and a usize (int)
	// index can be used to access an element of an array.
	//
	// Returns `None` if the type of `self` does not match the type of the
	// index, for example if the index is a string and `self` is an array or
	// a number. Also returns `None` if the given key does not exist in the
	// map or the given index is not within the bounds of the array.
	GetMut(index any) rs.Option[*Value]
	// AsInteger is Value::as_integer: extracts the integer value if it is an
	// integer.
	AsInteger() rs.Option[int64]
	// IsInteger is Value::is_integer: tests whether this value is an integer.
	IsInteger() bool
	// AsFloat is Value::as_float: extracts the float value if it is a float.
	AsFloat() rs.Option[float64]
	// IsFloat is Value::is_float: tests whether this value is a float.
	IsFloat() bool
	// AsBool is Value::as_bool: extracts the boolean value if it is a
	// boolean.
	AsBool() rs.Option[bool]
	// IsBool is Value::is_bool: tests whether this value is a boolean.
	IsBool() bool
	// AsStr is Value::as_str: extracts the string of this value if it is a
	// string.
	AsStr() rs.Option[string]
	// IsStr is Value::is_str: tests if this value is a string.
	IsStr() bool
	// AsDatetime is Value::as_datetime: extracts the datetime value if it is
	// a datetime.
	//
	// Note that a parsed TOML value will only contain ISO 8601 dates. An
	// example date is:
	//
	//	1979-05-27T07:32:00Z
	AsDatetime() rs.Option[ValueDatetime]
	// IsDatetime is Value::is_datetime: tests whether this value is a
	// datetime.
	IsDatetime() bool
	// AsArray is Value::as_array: extracts the array value if it is an
	// array.
	AsArray() rs.Option[*[]Value]
	// AsArrayMut is Value::as_array_mut: extracts the array value if it is an
	// array.
	AsArrayMut() rs.Option[*[]Value]
	// IsArray is Value::is_array: tests whether this value is an array.
	IsArray() bool
	// AsTable is Value::as_table: extracts the table value if it is a table.
	AsTable() rs.Option[*Table]
	// AsTableMut is Value::as_table_mut: extracts the table value if it is a
	// table.
	AsTableMut() rs.Option[*Table]
	// IsTable is Value::is_table: tests whether this value is a table.
	IsTable() bool
	// SameType is Value::same_type: tests whether this and another value
	// have the same type.
	SameType(other Value) bool
	// TypeStr is Value::type_str: returns a human-readable representation of
	// the type of this value.
	TypeStr() string

	// Index is `impl Index<I> for Value`: Get, panicking with "index not
	// found" on None.
	Index(index any) Value
	// IndexMut is `impl IndexMut<I> for Value`: GetMut, panicking with
	// "index not found" on None.
	IndexMut(index any) *Value

	// String is `impl Display for Value`.
	String() string
	// FmtDisplay is `impl Display for Value`.
	FmtDisplay(f *rs.Formatter) error
}

// Value_String is Value::String: represents a TOML string.
type Value_String struct{ F0 string }

// Value_Integer is Value::Integer: represents a TOML integer.
type Value_Integer struct{ F0 int64 }

// Value_Float is Value::Float: represents a TOML float.
type Value_Float struct{ F0 float64 }

// Value_Boolean is Value::Boolean: represents a TOML boolean.
type Value_Boolean struct{ F0 bool }

// Value_Datetime is Value::Datetime: represents a TOML datetime.
type Value_Datetime struct{ F0 ValueDatetime }

// Value_Array is Value::Array: represents a TOML array.
type Value_Array struct{ F0 []Value }

// Value_Table is Value::Table: represents a TOML table.
type Value_Table struct{ F0 Table }

var _ = rs.RegisterEnum[Value]("Value",
	rs.Variant[Value_String]("String", rs.ShapeTuple),
	rs.Variant[Value_Integer]("Integer", rs.ShapeTuple),
	rs.Variant[Value_Float]("Float", rs.ShapeTuple),
	rs.Variant[Value_Boolean]("Boolean", rs.ShapeTuple),
	rs.Variant[Value_Datetime]("Datetime", rs.ShapeTuple),
	rs.Variant[Value_Array]("Array", rs.ShapeTuple),
	rs.Variant[Value_Table]("Table", rs.ShapeTuple),
)

// ValueTryFrom is Value::try_from: convert a `T` into `toml::Value` which
// is an enum that can represent any valid TOML data.
//
// This conversion can fail if `T`'s implementation of `Serialize` decides to
// fail, or if `T` contains a map with non-string keys.
func ValueTryFrom(value any) (Value, error) {
	serialized, err := serde.Serialize(value, valueSerializer{})
	if err != nil {
		return nil, serErrorFrom(err)
	}
	return serialized.(Value), nil
}

// ValueTryInto is Value::try_into: interpret a `toml::Value` as an instance
// of type `T`.
//
// This conversion can fail if the structure of the `Value` does not match the
// structure expected by `T`, for example if `T` is a struct type but the
// `Value` contains something other than a TOML table. It can also fail if the
// structure is correct but `T`'s implementation of `Deserialize` decides that
// something is wrong with the data, for example required struct fields are
// missing from the TOML map or some number is too big to fit in the expected
// primitive type.
func ValueTryInto[T any](value Value) (T, error) {
	return deserializeWith[T](valueIntoDeserializer(value))
}

func valueGet(v Value, index any) rs.Option[Value] {
	return OptionDeref(indexIndex(index, v))
}

// OptionDeref turns the `Option<&mut Value>` the indexing functions find
// into the `Option<&Value>` Value::get and Table::get return.
func OptionDeref(found rs.Option[*Value]) rs.Option[Value] {
	if p, ok := found.Get(); ok {
		return rs.Some(*p)
	}
	return rs.None[Value]()
}

func valueGetMut(v Value, index any) rs.Option[*Value] {
	return indexIndexMut(index, v)
}

func valueAsInteger(v Value) rs.Option[int64] {
	if i, ok := v.(*Value_Integer); ok {
		return rs.Some(i.F0)
	}
	return rs.None[int64]()
}

func valueAsFloat(v Value) rs.Option[float64] {
	if f, ok := v.(*Value_Float); ok {
		return rs.Some(f.F0)
	}
	return rs.None[float64]()
}

func valueAsBool(v Value) rs.Option[bool] {
	if b, ok := v.(*Value_Boolean); ok {
		return rs.Some(b.F0)
	}
	return rs.None[bool]()
}

func valueAsStr(v Value) rs.Option[string] {
	if s, ok := v.(*Value_String); ok {
		return rs.Some(s.F0)
	}
	return rs.None[string]()
}

func valueAsDatetime(v Value) rs.Option[ValueDatetime] {
	if s, ok := v.(*Value_Datetime); ok {
		return rs.Some(s.F0)
	}
	return rs.None[ValueDatetime]()
}

func valueAsArray(v Value) rs.Option[*[]Value] {
	if s, ok := v.(*Value_Array); ok {
		return rs.Some(&s.F0)
	}
	return rs.None[*[]Value]()
}

func valueAsTable(v Value) rs.Option[*Table] {
	if s, ok := v.(*Value_Table); ok {
		return rs.Some(&s.F0)
	}
	return rs.None[*Table]()
}

func valueSameType(v Value, other Value) bool {
	return reflect.TypeOf(v) == reflect.TypeOf(other)
}

func valueTypeStr(v Value) string {
	switch v.(type) {
	case *Value_String:
		return "string"
	case *Value_Integer:
		return "integer"
	case *Value_Float:
		return "float"
	case *Value_Boolean:
		return "boolean"
	case *Value_Datetime:
		return "datetime"
	case *Value_Array:
		return "array"
	case *Value_Table:
		return "table"
	default:
		panic(rs.Unreachable(v))
	}
}

func valueIndex(v Value, index any) Value {
	return valueGet(v, index).Expect("index not found")
}

func valueIndexMut(v Value, index any) *Value {
	return valueGetMut(v, index).Expect("index not found")
}

// ValueFromString is `impl From<&str> for Value` and `impl From<String> for
// Value`.
func ValueFromString(val string) Value {
	return &Value_String{F0: val}
}

// ValueFromVec is `impl<V: Into<Value>> From<Vec<V>> for Value`: an array of
// the values, each converted with IntoValue.
func ValueFromVec[V any](val []V) Value {
	array := make([]Value, 0, len(val))
	for _, v := range val {
		array = append(array, IntoValue(v))
	}
	return &Value_Array{F0: array}
}

// ValueFromBTreeMap is `impl<S: Into<String>, V: Into<Value>>
// From<BTreeMap<S, V>> for Value` (and the same from a HashMap): a table of
// the entries, each value converted with IntoValue.
func ValueFromBTreeMap[V any](val map[string]V) Value {
	table := TableNew()
	for s, v := range val {
		table.Insert(s, IntoValue(v))
	}
	return &Value_Table{F0: table}
}

// ValueFromInt64 is `impl From<i64> for Value`.
func ValueFromInt64(val int64) Value { return &Value_Integer{F0: val} }

// ValueFromInt32 is `impl From<i32> for Value`.
func ValueFromInt32(val int32) Value { return &Value_Integer{F0: int64(val)} }

// ValueFromInt8 is `impl From<i8> for Value`.
func ValueFromInt8(val int8) Value { return &Value_Integer{F0: int64(val)} }

// ValueFromUint8 is `impl From<u8> for Value`.
func ValueFromUint8(val uint8) Value { return &Value_Integer{F0: int64(val)} }

// ValueFromUint32 is `impl From<u32> for Value`.
func ValueFromUint32(val uint32) Value { return &Value_Integer{F0: int64(val)} }

// ValueFromFloat64 is `impl From<f64> for Value`.
func ValueFromFloat64(val float64) Value { return &Value_Float{F0: val} }

// ValueFromFloat32 is `impl From<f32> for Value`.
func ValueFromFloat32(val float32) Value { return &Value_Float{F0: float64(val)} }

// ValueFromBool is `impl From<bool> for Value`.
func ValueFromBool(val bool) Value { return &Value_Boolean{F0: val} }

// ValueFromDatetime is `impl From<Datetime> for Value`.
func ValueFromDatetime(val ValueDatetime) Value { return &Value_Datetime{F0: val} }

// ValueFromTable is `impl From<Table> for Value`.
func ValueFromTable(val Table) Value { return &Value_Table{F0: val} }

// IntoValue is `Into<Value>` for the types toml implements it for: a Value
// (which stays itself), a string, an int64, an int32, an int8, a uint8, a
// uint32, a float64, a float32, a bool, a Datetime, a Table, and a slice or
// a map of those (`From<Vec<V>>`, `From<BTreeMap<S, V>>`). Rust selects the
// conversion at compile time; a value of another type panics here.
func IntoValue(val any) Value {
	switch v := val.(type) {
	case Value:
		return v
	case string:
		return ValueFromString(v)
	case int64:
		return ValueFromInt64(v)
	case int32:
		return ValueFromInt32(v)
	case int8:
		return ValueFromInt8(v)
	case uint8:
		return ValueFromUint8(v)
	case uint32:
		return ValueFromUint32(v)
	case float64:
		return ValueFromFloat64(v)
	case float32:
		return ValueFromFloat32(v)
	case bool:
		return ValueFromBool(v)
	case ValueDatetime:
		return ValueFromDatetime(v)
	case Table:
		return ValueFromTable(v)
	case []Value:
		return &Value_Array{F0: v}
	}
	reflected := reflect.ValueOf(val)
	switch reflected.Kind() {
	case reflect.Slice:
		array := make([]Value, 0, reflected.Len())
		for index := range reflected.Len() {
			array = append(array, IntoValue(reflected.Index(index).Interface()))
		}
		return &Value_Array{F0: array}
	case reflect.Map:
		if reflected.Type().Key().Kind() == reflect.String {
			table := TableNew()
			for iter := reflected.MapRange(); iter.Next(); {
				table.Insert(iter.Key().String(), IntoValue(iter.Value().Interface()))
			}
			return &Value_Table{F0: table}
		}
	}
	panic(rs.Unreachable("toml: Into<Value> is not implemented for " + reflected.Type().String()))
}

// Types that can be used to index a `toml::Value`
//
// Currently this is implemented for `usize` (int) to index arrays and `str`
// (string) to index tables.
//
// The trait `Index` is sealed; Go takes the index as `any` and accepts an
// int or a string.

// indexIndex is `Index::index_mut` for the index's type, read-only: the
// value at index, which Value::get turns into its `Option<&Value>`.
func indexIndex(index any, val Value) rs.Option[*Value] {
	return indexIndexMut(index, val)
}

// indexIndexMut is `Index::index_mut` for `usize`, `str` and `String`.
func indexIndexMut(index any, val Value) rs.Option[*Value] {
	switch index := index.(type) {
	case int:
		if a, ok := val.(*Value_Array); ok {
			if index >= 0 && index < len(a.F0) {
				return rs.Some(&a.F0[index])
			}
		}
		return rs.None[*Value]()
	case string:
		if a, ok := val.(*Value_Table); ok {
			return a.F0.GetMut(index)
		}
		return rs.None[*Value]()
	default:
		panic(rs.Unreachable("toml: Index is implemented for usize and str only"))
	}
}

func valueString(v Value) string {
	output := ""
	serializer := SerValueSerializerNew(&output)
	_, err := valueSerialize(v, serializer)
	rs.UnwrapUnit(err)
	return output
}

func valueFmtDisplay(v Value, f *rs.Formatter) error {
	// `output.fmt(f)`: the serialized text padded to the formatter's flags.
	return f.Pad(valueString(v))
}

// ValueFromStr is `impl FromStr for Value`: `crate::from_str(s)`.
func ValueFromStr(s string) (Value, error) {
	return FromStr[Value](s)
}

// valueSerialize is `impl Serialize for Value`.
func valueSerialize(v Value, serializer serde.Serializer) (any, error) {
	switch v := v.(type) {
	case *Value_String:
		return serializer.SerializeStr(v.F0)
	case *Value_Integer:
		return serializer.SerializeI64(v.F0)
	case *Value_Float:
		return serializer.SerializeF64(v.F0)
	case *Value_Boolean:
		return serializer.SerializeBool(v.F0)
	case *Value_Datetime:
		return v.F0.SerializeSerde(serializer)
	case *Value_Array:
		// `impl Serialize for Vec<T>`: `serializer.collect_seq(self)`.
		seq, err := serializer.SerializeSeq(rs.Some(len(v.F0)))
		if err != nil {
			return nil, err
		}
		for _, element := range v.F0 {
			if err := seq.SerializeElement(element); err != nil {
				return nil, err
			}
		}
		return seq.End()
	case *Value_Table:
		t := &v.F0
		m, err := serializer.SerializeMap(rs.Some(t.Len()))
		if err != nil {
			return nil, err
		}
		// Be sure to visit non-tables first (and also non
		// array-of-tables) as all keys must be emitted first.
		for k, v := range t.Iter() {
			if !v.IsTable() && !v.IsArray() ||
				rs.OptionMapOr(v.AsArray(), false, func(a *[]Value) bool { return !anyIsTable(*a) }) {
				if err := m.SerializeEntry(k, v); err != nil {
					return nil, err
				}
			}
		}
		for k, v := range t.Iter() {
			if rs.OptionMapOr(v.AsArray(), false, func(a *[]Value) bool { return anyIsTable(*a) }) {
				if err := m.SerializeEntry(k, v); err != nil {
					return nil, err
				}
			}
		}
		for k, v := range t.Iter() {
			if v.IsTable() {
				if err := m.SerializeEntry(k, v); err != nil {
					return nil, err
				}
			}
		}
		return m.End()
	default:
		panic(rs.Unreachable(v))
	}
}

func anyIsTable(values []Value) bool {
	for _, v := range values {
		if v.IsTable() {
			return true
		}
	}
	return false
}

// valueDeserialize is `impl Deserialize for Value`.
func valueDeserialize(deserializer serde.Deserializer) (Value, error) {
	value, err := deserializer.DeserializeAny(valueVisitor{})
	if err != nil {
		return nil, err
	}
	return value.(Value), nil
}

var (
	_ = serde.RegisterSerialize(func(value Value, serializer serde.Serializer) (any, error) {
		return valueSerialize(value, serializer)
	})
	_ = serde.RegisterDeserialize(valueDeserialize)
)

// valueVisitor is the ValueVisitor of `impl Deserialize for Value`.
type valueVisitor struct{}

func (valueVisitor) Expecting() string { return "any valid TOML value" }

func (valueVisitor) VisitBool(value bool) (any, error) {
	return Value(&Value_Boolean{F0: value}), nil
}

func (valueVisitor) VisitI64(value int64) (any, error) {
	return Value(&Value_Integer{F0: value}), nil
}

func (valueVisitor) VisitU64(value uint64) (any, error) {
	if value <= math.MaxInt64 {
		return Value(&Value_Integer{F0: int64(value)}), nil
	}
	return nil, serde.DeErrorCustom("u64 value was too large")
}

func (valueVisitor) VisitU32(value uint32) (any, error) {
	return Value(&Value_Integer{F0: int64(value)}), nil
}

func (valueVisitor) VisitI32(value int32) (any, error) {
	return Value(&Value_Integer{F0: int64(value)}), nil
}

func (valueVisitor) VisitF64(value float64) (any, error) {
	return Value(&Value_Float{F0: value}), nil
}

func (valueVisitor) VisitStr(value string) (any, error) {
	return Value(&Value_String{F0: value}), nil
}

func (valueVisitor) VisitString(value string) (any, error) {
	return Value(&Value_String{F0: value}), nil
}

func (valueVisitor) VisitSome(deserializer serde.Deserializer) (any, error) {
	return serde.Deserialize[Value](deserializer)
}

func (valueVisitor) VisitSeq(visitor serde.DeSeqAccess) (any, error) {
	vec := []Value{}
	for {
		elem, err := serde.DeSeqAccessNextElement[Value](visitor)
		if err != nil {
			return nil, err
		}
		e, ok := elem.Get()
		if !ok {
			break
		}
		vec = append(vec, e)
	}
	return Value(&Value_Array{F0: vec}), nil
}

func (valueVisitor) VisitMap(visitor serde.DeMapAccess) (any, error) {
	key := ""
	datetime, ok, err := visitor.NextKeySeed(&datetimeOrTable{key: &key})
	if err != nil {
		return nil, err
	}
	switch {
	case ok && datetime.(bool):
		date, err := serde.DeMapAccessNextValue[tomledit.DatetimeFromString](visitor)
		if err != nil {
			return nil, err
		}
		return Value(&Value_Datetime{F0: date.Value}), nil
	case !ok:
		return Value(&Value_Table{F0: TableNew()}), nil
	}
	m := TableNew()
	first, err := serde.DeMapAccessNextValue[Value](visitor)
	if err != nil {
		return nil, err
	}
	m.Insert(key, first)
	for {
		next, err := serde.DeMapAccessNextKey[string](visitor)
		if err != nil {
			return nil, err
		}
		key, ok := next.Get()
		if !ok {
			break
		}
		if vacant, ok := m.Entry(key).(*MapEntry_Vacant); ok {
			value, err := serde.DeMapAccessNextValue[Value](visitor)
			if err != nil {
				return nil, err
			}
			vacant.F0.Insert(value)
		} else {
			msg := "duplicate key: `" + key + "`"
			return nil, serde.DeErrorCustom(msg)
		}
	}
	return Value(&Value_Table{F0: m}), nil
}

// valueDeserializer is `impl Deserializer for Value`: a Value read back
// through serde. This is wrapped by `Table` and any trait methods
// implemented here need to be wrapped there.
type valueDeserializer struct {
	deForward
	value Value
}

// valueIntoDeserializer is `impl IntoDeserializer for Value`: the value
// itself, as a deserializer.
func valueIntoDeserializer(value Value) *valueDeserializer {
	d := &valueDeserializer{deForward: deForward{deserializeAny: nil}, value: value}
	d.deserializeAny = d.DeserializeAny
	return d
}

// ValueIntoDeserializer is `impl IntoDeserializer for Value` and `impl
// Deserializer for Value`: what `T::deserialize(value)` reads from.
func ValueIntoDeserializer(value Value) serde.Deserializer {
	return valueIntoDeserializer(value)
}

func (d *valueDeserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	value, err := d.visitValue(visitor)
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

// visitValue is the body of deserialize_any, before its error is taken as
// the format's.
func (d *valueDeserializer) visitValue(visitor serde.DeVisitor) (any, error) {
	switch v := d.value.(type) {
	case *Value_Boolean:
		return serde.DeVisitorVisitBool(visitor, v.F0)
	case *Value_Integer:
		return serde.DeVisitorVisitI64(visitor, v.F0)
	case *Value_Float:
		return serde.DeVisitorVisitF64(visitor, v.F0)
	case *Value_String:
		return serde.DeVisitorVisitString(visitor, v.F0)
	case *Value_Datetime:
		return serde.DeVisitorVisitString(visitor, v.F0.String())
	case *Value_Array:
		length := len(v.F0)
		deserializer := seqDeserializerNewValues(v.F0)
		seq, err := serde.DeVisitorVisitSeq(visitor, deserializer)
		if err != nil {
			return nil, err
		}
		remaining := len(deserializer.iter)
		if remaining == 0 {
			return seq, nil
		}
		return nil, serde.DeErrorInvalidLength(length, serde.DeExpectedStr("fewer elements in array"))
	case *Value_Table:
		length := v.F0.Len()
		deserializer := mapDeserializerNew(v.F0)
		m, err := serde.DeVisitorVisitMap(visitor, deserializer)
		if err != nil {
			return nil, err
		}
		remaining := len(deserializer.iter)
		if remaining == 0 {
			return m, nil
		}
		return nil, serde.DeErrorInvalidLength(length, serde.DeExpectedStr("fewer elements in map"))
	default:
		panic(rs.Unreachable(d.value))
	}
}

func (d *valueDeserializer) DeserializeEnum(_ string, _ []string, visitor serde.DeVisitor) (any, error) {
	var value any
	var err error
	switch variant := d.value.(type) {
	case *Value_String:
		value, err = serde.DeVisitorVisitEnum(visitor, stringDeserializerNew(variant.F0))
	case *Value_Table:
		if variant.F0.IsEmpty() {
			err = DeErrorCustom("wanted exactly 1 element, found 0 elements")
		} else if variant.F0.Len() != 1 {
			err = DeErrorCustom("wanted exactly 1 element, more than 1 element")
		} else {
			deserializer := mapDeserializerNew(variant.F0)
			value, err = serde.DeVisitorVisitEnum(visitor, deserializer)
		}
	default:
		err = serde.DeErrorInvalidType(&serde.DeUnexpectedUnitVariant{}, serde.DeExpectedStr("string only"))
	}
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

// `None` is interpreted as a missing field so be sure to implement `Some`
// as a present field.
func (d *valueDeserializer) DeserializeOption(visitor serde.DeVisitor) (any, error) {
	value, err := serde.DeVisitorVisitSome(visitor, d)
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

func (d *valueDeserializer) DeserializeNewtypeStruct(_ string, visitor serde.DeVisitor) (any, error) {
	value, err := serde.DeVisitorVisitNewtypeStruct(visitor, d)
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

// valueSeqDeserializer is value.rs's SeqDeserializer.
type valueSeqDeserializer struct {
	iter []Value
}

func seqDeserializerNewValues(vec []Value) *valueSeqDeserializer {
	return &valueSeqDeserializer{iter: vec}
}

func (s *valueSeqDeserializer) NextElementSeed(seed serde.DeDeserializeSeed) (any, bool, error) {
	if len(s.iter) == 0 {
		return nil, false, nil
	}
	value := s.iter[0]
	s.iter = s.iter[1:]
	element, err := seed.DeserializeSeed(valueIntoDeserializer(value))
	if err != nil {
		return nil, false, deErrorFrom(err)
	}
	return element, true, nil
}

func (s *valueSeqDeserializer) SizeHint() rs.Option[int] {
	return rs.Some(len(s.iter))
}

// mapDeserializer is value.rs's MapDeserializer.
type mapDeserializer struct {
	iter  []rs.Tuple2[string, Value]
	value rs.Option[rs.Tuple2[string, Value]]
}

func mapDeserializerNew(m Table) *mapDeserializer {
	var entries []rs.Tuple2[string, Value]
	for key, value := range m.IntoIter() {
		entries = append(entries, rs.Tuple2[string, Value]{F0: key, F1: value})
	}
	return &mapDeserializer{iter: entries, value: rs.None[rs.Tuple2[string, Value]]()}
}

func (m *mapDeserializer) next() (rs.Tuple2[string, Value], bool) {
	if len(m.iter) == 0 {
		return rs.Tuple2[string, Value]{}, false
	}
	pair := m.iter[0]
	m.iter = m.iter[1:]
	return pair, true
}

func (m *mapDeserializer) NextKeySeed(seed serde.DeDeserializeSeed) (any, bool, error) {
	pair, ok := m.next()
	if !ok {
		return nil, false, nil
	}
	m.value = rs.Some(pair)
	key, err := seed.DeserializeSeed(valueIntoDeserializer(&Value_String{F0: pair.F0}))
	if err != nil {
		return nil, false, deErrorFrom(err)
	}
	return key, true, nil
}

func (m *mapDeserializer) NextValueSeed(seed serde.DeDeserializeSeed) (any, error) {
	pair, ok := m.value.Take().Get()
	if !ok {
		return nil, DeErrorCustom("value is missing")
	}
	key := pair.F0
	res, err := seed.DeserializeSeed(valueIntoDeserializer(pair.F1))
	if err != nil {
		e := deErrorFrom(err)
		e.addKey(key)
		return nil, e
	}
	return res, nil
}

func (m *mapDeserializer) NextEntrySeed(keySeed serde.DeDeserializeSeed, valueSeed serde.DeDeserializeSeed) (any, any, bool, error) {
	return serde.DeMapAccessDefaultNextEntrySeed(m, keySeed, valueSeed)
}

func (m *mapDeserializer) SizeHint() rs.Option[int] {
	return rs.Some(len(m.iter))
}

// VariantSeed is `impl EnumAccess for MapDeserializer`.
func (m *mapDeserializer) VariantSeed(seed serde.DeDeserializeSeed) (any, serde.DeVariantAccess, error) {
	pair, ok := m.next()
	if !ok {
		return nil, nil, DeErrorCustom("expected table with exactly 1 entry, found empty table")
	}
	key, value := pair.F0, pair.F1

	val, err := seed.DeserializeSeed(stringDeserializerNew(key))
	if err != nil {
		return nil, nil, deErrorFrom(err)
	}

	variant := mapEnumDeserializerNew(value)

	return val, variant, nil
}

// mapEnumDeserializer is MapEnumDeserializer: deserializes table values into
// enum variants.
type mapEnumDeserializer struct {
	value Value
}

func mapEnumDeserializerNew(value Value) *mapEnumDeserializer {
	return &mapEnumDeserializer{value: value}
}

func (d *mapEnumDeserializer) UnitVariant() error {
	switch values := d.value.(type) {
	case *Value_Array:
		if len(values.F0) == 0 {
			return nil
		}
		return DeErrorCustom("expected empty array")
	case *Value_Table:
		if values.F0.IsEmpty() {
			return nil
		}
		return DeErrorCustom("expected empty table")
	default:
		return DeErrorCustom("expected table, found " + d.value.TypeStr())
	}
}

func (d *mapEnumDeserializer) NewtypeVariantSeed(seed serde.DeDeserializeSeed) (any, error) {
	value, err := seed.DeserializeSeed(valueIntoDeserializer(d.value))
	if err != nil {
		return nil, deErrorFrom(err)
	}
	return value, nil
}

func (d *mapEnumDeserializer) TupleVariant(length int, visitor serde.DeVisitor) (any, error) {
	switch values := d.value.(type) {
	case *Value_Array:
		if len(values.F0) == length {
			return seqDeserializerNew(values.F0).DeserializeSeq(visitor)
		}
		return nil, DeErrorCustom("expected tuple with length " + strconv.Itoa(length))
	case *Value_Table:
		tupleValues := make([]Value, 0, length)
		index := 0
		for key, value := range values.F0.IntoIter() {
			keyIndex, err := strs.ParseUsize(key)
			if err != nil || keyIndex != index {
				return nil, DeErrorCustom("expected table key `" + strconv.Itoa(index) + "`, but was `" + key + "`")
			}
			tupleValues = append(tupleValues, value)
			index++
		}

		if len(tupleValues) == length {
			return seqDeserializerNew(tupleValues).DeserializeSeq(visitor)
		}
		return nil, DeErrorCustom("expected tuple with length " + strconv.Itoa(length))
	default:
		return nil, DeErrorCustom("expected table, found " + d.value.TypeStr())
	}
}

func (d *mapEnumDeserializer) StructVariant(fields []string, visitor serde.DeVisitor) (any, error) {
	return valueIntoDeserializer(d.value).DeserializeStruct(
		"", // TODO: this should be the variant name
		fields,
		visitor,
	)
}

// valueSerializer is value.rs's ValueSerializer: a value serialized into a
// toml::Value. Its `Ok` is a Value.
type valueSerializer struct{}

func (s valueSerializer) SerializeBool(value bool) (any, error) {
	return Value(&Value_Boolean{F0: value}), nil
}

func (s valueSerializer) SerializeI8(value int8) (any, error) {
	return s.SerializeI64(int64(value))
}

func (s valueSerializer) SerializeI16(value int16) (any, error) {
	return s.SerializeI64(int64(value))
}

func (s valueSerializer) SerializeI32(value int32) (any, error) {
	return s.SerializeI64(int64(value))
}

func (s valueSerializer) SerializeI64(value int64) (any, error) {
	return Value(&Value_Integer{F0: value}), nil
}

func (s valueSerializer) SerializeU8(value uint8) (any, error) {
	return s.SerializeI64(int64(value))
}

func (s valueSerializer) SerializeU16(value uint16) (any, error) {
	return s.SerializeI64(int64(value))
}

func (s valueSerializer) SerializeU32(value uint32) (any, error) {
	return s.SerializeI64(int64(value))
}

func (s valueSerializer) SerializeU64(value uint64) (any, error) {
	if value <= math.MaxInt64 {
		return s.SerializeI64(int64(value))
	}
	return nil, SerErrorCustom("u64 value was too large")
}

func (s valueSerializer) SerializeF32(value float32) (any, error) {
	return s.SerializeF64(float64(value))
}

func (s valueSerializer) SerializeF64(value float64) (any, error) {
	return Value(&Value_Float{F0: value}), nil
}

func (s valueSerializer) SerializeChar(value rune) (any, error) {
	return s.SerializeStr(string(value))
}

func (s valueSerializer) SerializeStr(value string) (any, error) {
	return Value(&Value_String{F0: value}), nil
}

func (s valueSerializer) SerializeBytes(value []byte) (any, error) {
	vec := make([]Value, 0, len(value))
	for _, b := range value {
		vec = append(vec, &Value_Integer{F0: int64(b)})
	}
	return Value(&Value_Array{F0: vec}), nil
}

func (s valueSerializer) SerializeUnit() (any, error) {
	return nil, serErrorUnsupportedType(rs.Some("unit"))
}

func (s valueSerializer) SerializeUnitStruct(name string) (any, error) {
	return nil, serErrorUnsupportedType(rs.Some(name))
}

func (s valueSerializer) SerializeUnitVariant(_ string, _ uint32, variant string) (any, error) {
	return s.SerializeStr(variant)
}

func (s valueSerializer) SerializeNewtypeStruct(_ string, value any) (any, error) {
	return valueSerializeWith(value, s)
}

func (s valueSerializer) SerializeNewtypeVariant(_ string, _ uint32, variant string, value any) (any, error) {
	serialized, err := valueSerializeWith(value, valueSerializer{})
	if err != nil {
		return nil, err
	}
	table := TableNew()
	table.Insert(variant, serialized.(Value))
	return Value(&Value_Table{F0: table}), nil
}

func (s valueSerializer) SerializeNone() (any, error) {
	return nil, serErrorUnsupportedNone()
}

func (s valueSerializer) SerializeSome(value any) (any, error) {
	return valueSerializeWith(value, s)
}

func (s valueSerializer) SerializeSeq(length rs.Option[int]) (serde.SerSerializeSeq, error) {
	return &valueSerializeVec{vec: make([]Value, 0, length.UnwrapOr(0))}, nil
}

func (s valueSerializer) SerializeTuple(length int) (serde.SerSerializeTuple, error) {
	seq, err := s.SerializeSeq(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return seq.(*valueSerializeVec), nil
}

func (s valueSerializer) SerializeTupleStruct(_ string, length int) (serde.SerSerializeTupleStruct, error) {
	seq, err := s.SerializeSeq(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return seq.(*valueSerializeVec), nil
}

func (s valueSerializer) SerializeTupleVariant(_ string, _ uint32, variant string, length int) (serde.SerSerializeTupleVariant, error) {
	return valueSerializeTupleVariantTuple(variant, length), nil
}

func (s valueSerializer) SerializeMap(rs.Option[int]) (serde.SerSerializeMap, error) {
	return &valueSerializeMap{ser: serializeMap{m: TableNew(), nextKey: rs.None[string]()}}, nil
}

func (s valueSerializer) SerializeStruct(_ string, length int) (serde.SerSerializeStruct, error) {
	m, err := s.SerializeMap(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return m.(*valueSerializeMap), nil
}

func (s valueSerializer) SerializeStructVariant(_ string, _ uint32, variant string, length int) (serde.SerSerializeStructVariant, error) {
	return valueSerializeStructVariantStruct(variant, length), nil
}

// CollectStr is the default `Serializer::collect_str`.
func (s valueSerializer) CollectStr(value string) (any, error) {
	return serde.SerializerDefaultCollectStr(s, value)
}

// IsHumanReadable is the default `Serializer::is_human_readable`.
func (valueSerializer) IsHumanReadable() bool {
	return serde.SerializerDefaultIsHumanReadable()
}

// valueSerializeWith is `value.serialize(serializer)` for a serializer whose
// `Ok` is a toml::Value or a Table, with whatever error comes back taken as
// this format's.
func valueSerializeWith(value any, serializer serde.Serializer) (any, error) {
	serialized, err := serde.Serialize(value, serializer)
	if err != nil {
		return nil, serErrorFrom(err)
	}
	return serialized, nil
}

// tableSerializer is TableSerializer: a value serialized into a Table. Its
// `Ok` is a Table.
type tableSerializer struct{}

func (tableSerializer) SerializeBool(bool) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeI8(int8) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeI16(int16) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeI32(int32) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeI64(int64) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeU8(uint8) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeU16(uint16) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeU32(uint32) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeU64(uint64) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeF32(float32) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeF64(float64) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeChar(rune) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeStr(string) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeBytes([]byte) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeUnit() (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeUnitStruct(string) (any, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeUnitVariant(name string, _ uint32, _ string) (any, error) {
	return nil, serErrorUnsupportedType(rs.Some(name))
}

func (s tableSerializer) SerializeNewtypeStruct(_ string, value any) (any, error) {
	return valueSerializeWith(value, s)
}

func (tableSerializer) SerializeNewtypeVariant(_ string, _ uint32, variant string, value any) (any, error) {
	serialized, err := valueSerializeWith(value, valueSerializer{})
	if err != nil {
		return nil, err
	}
	table := TableNew()
	table.Insert(variant, serialized.(Value))
	return table, nil
}

func (tableSerializer) SerializeNone() (any, error) {
	return nil, serErrorUnsupportedNone()
}

func (s tableSerializer) SerializeSome(value any) (any, error) {
	return valueSerializeWith(value, s)
}

func (tableSerializer) SerializeSeq(rs.Option[int]) (serde.SerSerializeSeq, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeTuple(int) (serde.SerSerializeTuple, error) {
	return nil, serErrorUnsupportedType(rs.None[string]())
}

func (tableSerializer) SerializeTupleStruct(name string, _ int) (serde.SerSerializeTupleStruct, error) {
	return nil, serErrorUnsupportedType(rs.Some(name))
}

func (tableSerializer) SerializeTupleVariant(name string, _ uint32, _ string, _ int) (serde.SerSerializeTupleVariant, error) {
	return nil, serErrorUnsupportedType(rs.Some(name))
}

func (tableSerializer) SerializeMap(rs.Option[int]) (serde.SerSerializeMap, error) {
	return &serializeMap{m: TableNew(), nextKey: rs.None[string]()}, nil
}

func (s tableSerializer) SerializeStruct(_ string, length int) (serde.SerSerializeStruct, error) {
	m, err := s.SerializeMap(rs.Some(length))
	if err != nil {
		return nil, err
	}
	return m.(*serializeMap), nil
}

func (tableSerializer) SerializeStructVariant(name string, _ uint32, _ string, _ int) (serde.SerSerializeStructVariant, error) {
	return nil, serErrorUnsupportedType(rs.Some(name))
}

// CollectStr is the default `Serializer::collect_str`.
func (s tableSerializer) CollectStr(value string) (any, error) {
	return serde.SerializerDefaultCollectStr(s, value)
}

// IsHumanReadable is the default `Serializer::is_human_readable`.
func (tableSerializer) IsHumanReadable() bool {
	return serde.SerializerDefaultIsHumanReadable()
}

// valueSerializeVec is ValueSerializeVec: the SerializeSeq, SerializeTuple,
// SerializeTupleStruct and SerializeTupleVariant of ValueSerializer.
type valueSerializeVec struct {
	vec []Value
}

// SerializeElement is `SerializeSeq::serialize_element` (and
// `SerializeTuple::serialize_element`).
func (s *valueSerializeVec) SerializeElement(value any) error {
	v, err := ValueTryFrom(value)
	if err != nil {
		return err
	}
	s.vec = append(s.vec, v)
	return nil
}

// End is `SerializeSeq::end` (and the end of the other three).
func (s *valueSerializeVec) End() (any, error) {
	return Value(&Value_Array{F0: s.vec}), nil
}

// SerializeField is `SerializeTupleStruct::serialize_field` and
// `SerializeTupleVariant::serialize_field`.
func (s *valueSerializeVec) SerializeField(value any) error {
	return s.SerializeElement(value)
}

// serializeMap is value.rs's SerializeMap: the SerializeMap and
// SerializeStruct of TableSerializer. Its `Ok` is a Table.
type serializeMap struct {
	m       Table
	nextKey rs.Option[string]
}

func (s *serializeMap) SerializeKey(key any) error {
	v, err := ValueTryFrom(key)
	if err != nil {
		return err
	}
	if str, ok := v.(*Value_String); ok {
		s.nextKey = rs.Some(str.F0)
	} else {
		return serErrorKeyNotString()
	}
	return nil
}

func (s *serializeMap) SerializeValue(value any) error {
	key := s.nextKey.Take()
	k := key.Expect("serialize_value called before serialize_key")
	v, err := ValueTryFrom(value)
	if err == nil {
		s.m.Insert(k, v)
	} else if e := serErrorFrom(err); !e.isUnsupportedNone() {
		return e
	}
	return nil
}

// SerializeEntry is the default `SerializeMap::serialize_entry`.
func (s *serializeMap) SerializeEntry(key any, value any) error {
	return serde.SerSerializeMapDefaultSerializeEntry(s, key, value)
}

func (s *serializeMap) End() (any, error) {
	return s.m, nil
}

// SerializeField is `impl SerializeStruct for SerializeMap`.
func (s *serializeMap) SerializeField(key string, value any) error {
	if err := s.SerializeKey(key); err != nil {
		return err
	}
	return s.SerializeValue(value)
}

// SkipField is the default `SerializeStruct::skip_field`.
func (s *serializeMap) SkipField(string) error { return nil }

// valueSerializeMap is ValueSerializeMap: the SerializeMap and
// SerializeStruct of ValueSerializer. Its `Ok` is a Value.
type valueSerializeMap struct {
	ser serializeMap
}

func (s *valueSerializeMap) SerializeKey(key any) error {
	return s.ser.SerializeKey(key)
}

func (s *valueSerializeMap) SerializeValue(value any) error {
	return s.ser.SerializeValue(value)
}

// SerializeEntry is the default `SerializeMap::serialize_entry`.
func (s *valueSerializeMap) SerializeEntry(key any, value any) error {
	return serde.SerSerializeMapDefaultSerializeEntry(s, key, value)
}

func (s *valueSerializeMap) End() (any, error) {
	m, err := s.ser.End()
	if err != nil {
		return nil, err
	}
	return Value(&Value_Table{F0: m.(Table)}), nil
}

// SerializeField is `impl SerializeStruct for ValueSerializeMap`.
func (s *valueSerializeMap) SerializeField(key string, value any) error {
	if err := s.SerializeKey(key); err != nil {
		return err
	}
	return s.SerializeValue(value)
}

// SkipField is the default `SerializeStruct::skip_field`.
func (s *valueSerializeMap) SkipField(string) error { return nil }

// datetimeOrTable is DatetimeOrTable: the seed that reads a map's first key
// and says whether it is the private datetime field, keeping the key
// otherwise.
type datetimeOrTable struct {
	key *string
}

// DeserializeSeed is `impl DeserializeSeed for DatetimeOrTable`.
func (s *datetimeOrTable) DeserializeSeed(deserializer serde.Deserializer) (any, error) {
	return deserializer.DeserializeAny(s)
}

func (s *datetimeOrTable) Expecting() string { return "a string key" }

func (s *datetimeOrTable) VisitStr(v string) (any, error) {
	if v == tomledit.DatetimeField {
		return true, nil
	}
	*s.key += v
	return false, nil
}

func (s *datetimeOrTable) VisitString(v string) (any, error) {
	if v == tomledit.DatetimeField {
		return true, nil
	}
	*s.key = v
	return false, nil
}

// valueSerializeTupleVariant is ValueSerializeTupleVariant
// (`ValueSerializeVariant<ValueSerializeVec>`).
type valueSerializeTupleVariant struct {
	variant string
	inner   valueSerializeVec
}

func valueSerializeTupleVariantTuple(variant string, length int) *valueSerializeTupleVariant {
	return &valueSerializeTupleVariant{variant: variant, inner: valueSerializeVec{vec: make([]Value, 0, length)}}
}

func (s *valueSerializeTupleVariant) SerializeField(value any) error {
	return s.inner.SerializeElement(value)
}

func (s *valueSerializeTupleVariant) End() (any, error) {
	inner, err := s.inner.End()
	if err != nil {
		return nil, err
	}
	table := TableNew()
	table.Insert(s.variant, inner.(Value))
	return Value(&Value_Table{F0: table}), nil
}

// valueSerializeStructVariant is ValueSerializeStructVariant
// (`ValueSerializeVariant<ValueSerializeMap>`).
type valueSerializeStructVariant struct {
	variant string
	inner   valueSerializeMap
}

func valueSerializeStructVariantStruct(variant string, length int) *valueSerializeStructVariant {
	return &valueSerializeStructVariant{
		variant: variant,
		inner:   valueSerializeMap{ser: serializeMap{m: TableWithCapacity(length), nextKey: rs.None[string]()}},
	}
}

func (s *valueSerializeStructVariant) SerializeField(key string, value any) error {
	return s.inner.SerializeField(key, value)
}

// SkipField is the default `SerializeStructVariant::skip_field`.
func (s *valueSerializeStructVariant) SkipField(string) error { return nil }

func (s *valueSerializeStructVariant) End() (any, error) {
	inner, err := s.inner.End()
	if err != nil {
		return nil, err
	}
	table := TableNew()
	table.Insert(s.variant, inner.(Value))
	return Value(&Value_Table{F0: table}), nil
}

var (
	_ serde.Deserializer              = (*valueDeserializer)(nil)
	_ serde.DeSeqAccess               = (*valueSeqDeserializer)(nil)
	_ serde.DeMapAccess               = (*mapDeserializer)(nil)
	_ serde.DeEnumAccess              = (*mapDeserializer)(nil)
	_ serde.DeVariantAccess           = (*mapEnumDeserializer)(nil)
	_ serde.Serializer                = valueSerializer{}
	_ serde.Serializer                = tableSerializer{}
	_ serde.SerSerializeSeq           = (*valueSerializeVec)(nil)
	_ serde.SerSerializeTupleVariant  = (*valueSerializeVec)(nil)
	_ serde.SerSerializeMap           = (*serializeMap)(nil)
	_ serde.SerSerializeStruct        = (*serializeMap)(nil)
	_ serde.SerSerializeMap           = (*valueSerializeMap)(nil)
	_ serde.SerSerializeStruct        = (*valueSerializeMap)(nil)
	_ serde.SerSerializeTupleVariant  = (*valueSerializeTupleVariant)(nil)
	_ serde.SerSerializeStructVariant = (*valueSerializeStructVariant)(nil)
	_ serde.DeDeserializeSeed         = (*datetimeOrTable)(nil)
)
