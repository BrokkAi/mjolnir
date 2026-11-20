// Port of toml_edit 0.20.2 src/value.rs

package tomledit

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Value is toml_edit::Value: representation of a TOML Value (as part of a
// Key/Value Pair).
//
// It is a Rust enum, so a value is a pointer to one of the variant structs
// *Value_String, *Value_Integer, *Value_Float, *Value_Boolean,
// *Value_Datetime, *Value_Array and *Value_InlineTable. The methods are the
// enum's; value_variants.go gives them to each variant.
type Value interface {
	isValue()

	// TypeName is Value::type_name: text description of value type.
	TypeName() string

	// AsStr is Value::as_str: casts `self` to str.
	AsStr() rs.Option[string]
	// IsStr is Value::is_str: returns true iff `self` is a string.
	IsStr() bool
	// AsInteger is Value::as_integer: casts `self` to integer.
	AsInteger() rs.Option[int64]
	// IsInteger is Value::is_integer: returns true iff `self` is an integer.
	IsInteger() bool
	// AsFloat is Value::as_float: casts `self` to float.
	AsFloat() rs.Option[float64]
	// IsFloat is Value::is_float: returns true iff `self` is a float.
	IsFloat() bool
	// AsBool is Value::as_bool: casts `self` to boolean.
	AsBool() rs.Option[bool]
	// IsBool is Value::is_bool: returns true iff `self` is a boolean.
	IsBool() bool
	// AsDatetime is Value::as_datetime: casts `self` to date-time.
	AsDatetime() rs.Option[Datetime]
	// IsDatetime is Value::is_datetime: returns true iff `self` is a date-time.
	IsDatetime() bool
	// AsArray is Value::as_array: casts `self` to array.
	AsArray() rs.Option[*Array]
	// AsArrayMut is Value::as_array_mut: casts `self` to mutable array.
	AsArrayMut() rs.Option[*Array]
	// IsArray is Value::is_array: returns true iff `self` is an array.
	IsArray() bool
	// AsInlineTable is Value::as_inline_table: casts `self` to inline table.
	AsInlineTable() rs.Option[*InlineTable]
	// AsInlineTableMut is Value::as_inline_table_mut: casts `self` to mutable
	// inline table.
	AsInlineTableMut() rs.Option[*InlineTable]
	// IsInlineTable is Value::is_inline_table: returns true iff `self` is an
	// inline table.
	IsInlineTable() bool

	// DecorMut is Value::decor_mut: get the decoration of the value.
	//
	//	let v = toml_edit::Value::from(true);
	//	assert_eq!(v.decor().suffix(), None);
	DecorMut() *Decor
	// Decor is Value::decor: get the decoration of the value.
	//
	//	let v = toml_edit::Value::from(true);
	//	assert_eq!(v.decor().suffix(), None);
	Decor() *Decor
	// Decorated is Value::decorated: sets the prefix and the suffix for value
	// (each a string or a RawString).
	//
	//	let mut v = toml_edit::Value::from(42);
	//	assert_eq!(&v.to_string(), "42");
	//	let d = v.decorated(" ", " ");
	//	assert_eq!(&d.to_string(), " 42 ");
	Decorated(prefix any, suffix any) Value

	// Clone is the derived `Clone for Value`.
	Clone() Value
	// String is `impl Display for Value`.
	String() string
	// FmtDisplay is `impl Display for Value`.
	FmtDisplay(f *rs.Formatter) error

	decorate(prefix RawString, suffix RawString)
	// span is Value::span: the location within the original document.
	span() rs.Option[Range]
	despan(input string)
}

// Value_String is Value::String: a string value.
type Value_String struct{ F0 Formatted[string] }

// Value_Integer is Value::Integer: a 64-bit integer value.
type Value_Integer struct{ F0 Formatted[int64] }

// Value_Float is Value::Float: a 64-bit float value.
type Value_Float struct{ F0 Formatted[float64] }

// Value_Boolean is Value::Boolean: a boolean value.
type Value_Boolean struct{ F0 Formatted[bool] }

// Value_Datetime is Value::Datetime: an RFC 3339 formatted date-time with
// offset.
type Value_Datetime struct{ F0 Formatted[Datetime] }

// Value_Array is Value::Array: an inline array of values.
type Value_Array struct{ F0 Array }

// Value_InlineTable is Value::InlineTable: an inline table of key/value
// pairs.
type Value_InlineTable struct{ F0 InlineTable }

var _ = rs.RegisterEnum[Value]("Value",
	rs.Variant[Value_String]("String", rs.ShapeTuple),
	rs.Variant[Value_Integer]("Integer", rs.ShapeTuple),
	rs.Variant[Value_Float]("Float", rs.ShapeTuple),
	rs.Variant[Value_Boolean]("Boolean", rs.ShapeTuple),
	rs.Variant[Value_Datetime]("Datetime", rs.ShapeTuple),
	rs.Variant[Value_Array]("Array", rs.ShapeTuple),
	rs.Variant[Value_InlineTable]("InlineTable", rs.ShapeTuple),
)

/// Downcasting

func valueTypeName(v Value) string {
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
	case *Value_InlineTable:
		return "inline table"
	default:
		panic(rs.Unreachable(v))
	}
}

func valueAsStr(v Value) rs.Option[string] {
	if value, ok := v.(*Value_String); ok {
		return rs.Some(value.F0.value)
	}
	return rs.None[string]()
}

func valueAsInteger(v Value) rs.Option[int64] {
	if value, ok := v.(*Value_Integer); ok {
		return rs.Some(value.F0.value)
	}
	return rs.None[int64]()
}

func valueAsFloat(v Value) rs.Option[float64] {
	if value, ok := v.(*Value_Float); ok {
		return rs.Some(value.F0.value)
	}
	return rs.None[float64]()
}

func valueAsBool(v Value) rs.Option[bool] {
	if value, ok := v.(*Value_Boolean); ok {
		return rs.Some(value.F0.value)
	}
	return rs.None[bool]()
}

func valueAsDatetime(v Value) rs.Option[Datetime] {
	if value, ok := v.(*Value_Datetime); ok {
		return rs.Some(value.F0.value)
	}
	return rs.None[Datetime]()
}

func valueAsArray(v Value) rs.Option[*Array] {
	if value, ok := v.(*Value_Array); ok {
		return rs.Some(&value.F0)
	}
	return rs.None[*Array]()
}

func valueAsInlineTable(v Value) rs.Option[*InlineTable] {
	if value, ok := v.(*Value_InlineTable); ok {
		return rs.Some(&value.F0)
	}
	return rs.None[*InlineTable]()
}

func valueDecorMut(v Value) *Decor {
	switch value := v.(type) {
	case *Value_String:
		return value.F0.DecorMut()
	case *Value_Integer:
		return value.F0.DecorMut()
	case *Value_Float:
		return value.F0.DecorMut()
	case *Value_Boolean:
		return value.F0.DecorMut()
	case *Value_Datetime:
		return value.F0.DecorMut()
	case *Value_Array:
		return value.F0.DecorMut()
	case *Value_InlineTable:
		return value.F0.DecorMut()
	default:
		panic(rs.Unreachable(v))
	}
}

func valueDecorated(v Value, prefix any, suffix any) Value {
	v.decorate(intoRawString(prefix), intoRawString(suffix))
	return v
}

func valueDecorate(v Value, prefix RawString, suffix RawString) {
	decor := v.DecorMut()
	*decor = Decor{prefix: rs.Some(prefix), suffix: rs.Some(suffix)}
}

func valueSpan(v Value) rs.Option[Range] {
	switch value := v.(type) {
	case *Value_String:
		return value.F0.span()
	case *Value_Integer:
		return value.F0.span()
	case *Value_Float:
		return value.F0.span()
	case *Value_Boolean:
		return value.F0.span()
	case *Value_Datetime:
		return value.F0.span()
	case *Value_Array:
		return value.F0.span
	case *Value_InlineTable:
		return value.F0.span
	default:
		panic(rs.Unreachable(v))
	}
}

func valueDespan(v Value, input string) {
	switch value := v.(type) {
	case *Value_String:
		value.F0.despan(input)
	case *Value_Integer:
		value.F0.despan(input)
	case *Value_Float:
		value.F0.despan(input)
	case *Value_Boolean:
		value.F0.despan(input)
	case *Value_Datetime:
		value.F0.despan(input)
	case *Value_Array:
		value.F0.despan(input)
	case *Value_InlineTable:
		value.F0.despan(input)
	default:
		panic(rs.Unreachable(v))
	}
}

func valueClone(v Value) Value {
	switch value := v.(type) {
	case *Value_String:
		return &Value_String{F0: value.F0}
	case *Value_Integer:
		return &Value_Integer{F0: value.F0}
	case *Value_Float:
		return &Value_Float{F0: value.F0}
	case *Value_Boolean:
		return &Value_Boolean{F0: value.F0}
	case *Value_Datetime:
		return &Value_Datetime{F0: value.F0}
	case *Value_Array:
		return &Value_Array{F0: value.F0.Clone()}
	case *Value_InlineTable:
		return &Value_InlineTable{F0: value.F0.Clone()}
	default:
		panic(rs.Unreachable(v))
	}
}

// ValueFromStr is `impl FromStr for Value`: parses a value from a &str.
func ValueFromStr(s string) (Value, error) {
	value, err := parseValue(s)
	if err != nil {
		return nil, err
	}
	return value, nil
}

// ValueFromValue is `impl From<&Value> for Value`: a clone.
func ValueFromValue(s Value) Value {
	return s.Clone()
}

// ValueFromString is `impl From<String> for Value` (and for &str, &String
// and InternalString).
func ValueFromString(s string) Value {
	return &Value_String{F0: FormattedNew(s)}
}

// ValueFromInt64 is `impl From<i64> for Value`.
func ValueFromInt64(i int64) Value {
	return &Value_Integer{F0: FormattedNew(i)}
}

// ValueFromFloat64 is `impl From<f64> for Value`.
func ValueFromFloat64(f float64) Value {
	return &Value_Float{F0: FormattedNew(f)}
}

// ValueFromBool is `impl From<bool> for Value`.
func ValueFromBool(b bool) Value {
	return &Value_Boolean{F0: FormattedNew(b)}
}

// ValueFromDatetime is `impl From<Datetime> for Value`.
func ValueFromDatetime(d Datetime) Value {
	return &Value_Datetime{F0: FormattedNew(d)}
}

// ValueFromDate is `impl From<Date> for Value`.
func ValueFromDate(d Date) Value {
	return ValueFromDatetime(DatetimeFromDate(d))
}

// ValueFromTime is `impl From<Time> for Value`.
func ValueFromTime(d Time) Value {
	return ValueFromDatetime(DatetimeFromTime(d))
}

// ValueFromArray is `impl From<Array> for Value`.
func ValueFromArray(array Array) Value {
	return &Value_Array{F0: array}
}

// ValueFromInlineTable is `impl From<InlineTable> for Value`.
func ValueFromInlineTable(table InlineTable) Value {
	return &Value_InlineTable{F0: table}
}

// ValueFromIter is `impl FromIterator<V> for Value` with V: Into<Value>: an
// array of the values.
func ValueFromIter(values ...any) Value {
	return &Value_Array{F0: ArrayFromIter(values...)}
}

// ValueFromPairs is `impl FromIterator<(K, V)> for Value` with K: Into<Key>
// and V: Into<Value>: an inline table of the pairs.
func ValueFromPairs(pairs ...rs.Tuple2[any, any]) Value {
	return &Value_InlineTable{F0: InlineTableFromIter(pairs...)}
}

// IntoValue is `Into<Value>` for the types toml_edit implements it for: a
// Value (which stays itself), a string, an int64, a float64, a bool, a
// Datetime, a Date, a Time, an Array and an InlineTable. Rust selects the
// conversion at compile time; a value of another type panics here.
func IntoValue(value any) Value {
	switch v := value.(type) {
	case Value:
		return v
	case string:
		return ValueFromString(v)
	case int64:
		return ValueFromInt64(v)
	case float64:
		return ValueFromFloat64(v)
	case bool:
		return ValueFromBool(v)
	case Datetime:
		return ValueFromDatetime(v)
	case Date:
		return ValueFromDate(v)
	case Time:
		return ValueFromTime(v)
	case Array:
		return ValueFromArray(v)
	case *Array:
		return ValueFromArray(*v)
	case InlineTable:
		return ValueFromInlineTable(v)
	case *InlineTable:
		return ValueFromInlineTable(*v)
	default:
		panic(rs.Unreachable("toml_edit: Into<Value> is not implemented for this type"))
	}
}

func valueString(v Value) string {
	var buf strings.Builder
	encodeValue(v, &buf, rs.None[string](), "", "")
	return buf.String()
}

// `key1 = value1`
const (
	defaultValueDecorPrefix = " "
	defaultValueDecorSuffix = ""
)

// `{ key = value }`
const (
	defaultTrailingValueDecorPrefix = " "
	defaultTrailingValueDecorSuffix = " "
)

// `[value1, value2]`
const (
	defaultLeadingValueDecorPrefix = ""
	defaultLeadingValueDecorSuffix = ""
)
