// Port of toml_edit 0.20.2 src/value.rs
//
// The methods of the enum Value on each of its variants. Each forwards to the
// function in value.go that holds the Rust method's body.

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

func (*Value_String) isValue()      {}
func (*Value_Integer) isValue()     {}
func (*Value_Float) isValue()       {}
func (*Value_Boolean) isValue()     {}
func (*Value_Datetime) isValue()    {}
func (*Value_Array) isValue()       {}
func (*Value_InlineTable) isValue() {}

// TypeName is Value::type_name.
func (v *Value_String) TypeName() string { return valueTypeName(v) }

// TypeName is Value::type_name.
func (v *Value_Integer) TypeName() string { return valueTypeName(v) }

// TypeName is Value::type_name.
func (v *Value_Float) TypeName() string { return valueTypeName(v) }

// TypeName is Value::type_name.
func (v *Value_Boolean) TypeName() string { return valueTypeName(v) }

// TypeName is Value::type_name.
func (v *Value_Datetime) TypeName() string { return valueTypeName(v) }

// TypeName is Value::type_name.
func (v *Value_Array) TypeName() string { return valueTypeName(v) }

// TypeName is Value::type_name.
func (v *Value_InlineTable) TypeName() string { return valueTypeName(v) }

// AsStr is Value::as_str.
func (v *Value_String) AsStr() rs.Option[string] { return valueAsStr(v) }

// AsStr is Value::as_str.
func (v *Value_Integer) AsStr() rs.Option[string] { return valueAsStr(v) }

// AsStr is Value::as_str.
func (v *Value_Float) AsStr() rs.Option[string] { return valueAsStr(v) }

// AsStr is Value::as_str.
func (v *Value_Boolean) AsStr() rs.Option[string] { return valueAsStr(v) }

// AsStr is Value::as_str.
func (v *Value_Datetime) AsStr() rs.Option[string] { return valueAsStr(v) }

// AsStr is Value::as_str.
func (v *Value_Array) AsStr() rs.Option[string] { return valueAsStr(v) }

// AsStr is Value::as_str.
func (v *Value_InlineTable) AsStr() rs.Option[string] { return valueAsStr(v) }

// IsStr is Value::is_str.
func (v *Value_String) IsStr() bool { return valueAsStr(v).IsSome() }

// IsStr is Value::is_str.
func (v *Value_Integer) IsStr() bool { return valueAsStr(v).IsSome() }

// IsStr is Value::is_str.
func (v *Value_Float) IsStr() bool { return valueAsStr(v).IsSome() }

// IsStr is Value::is_str.
func (v *Value_Boolean) IsStr() bool { return valueAsStr(v).IsSome() }

// IsStr is Value::is_str.
func (v *Value_Datetime) IsStr() bool { return valueAsStr(v).IsSome() }

// IsStr is Value::is_str.
func (v *Value_Array) IsStr() bool { return valueAsStr(v).IsSome() }

// IsStr is Value::is_str.
func (v *Value_InlineTable) IsStr() bool { return valueAsStr(v).IsSome() }

// AsInteger is Value::as_integer.
func (v *Value_String) AsInteger() rs.Option[int64] { return valueAsInteger(v) }

// AsInteger is Value::as_integer.
func (v *Value_Integer) AsInteger() rs.Option[int64] { return valueAsInteger(v) }

// AsInteger is Value::as_integer.
func (v *Value_Float) AsInteger() rs.Option[int64] { return valueAsInteger(v) }

// AsInteger is Value::as_integer.
func (v *Value_Boolean) AsInteger() rs.Option[int64] { return valueAsInteger(v) }

// AsInteger is Value::as_integer.
func (v *Value_Datetime) AsInteger() rs.Option[int64] { return valueAsInteger(v) }

// AsInteger is Value::as_integer.
func (v *Value_Array) AsInteger() rs.Option[int64] { return valueAsInteger(v) }

// AsInteger is Value::as_integer.
func (v *Value_InlineTable) AsInteger() rs.Option[int64] { return valueAsInteger(v) }

// IsInteger is Value::is_integer.
func (v *Value_String) IsInteger() bool { return valueAsInteger(v).IsSome() }

// IsInteger is Value::is_integer.
func (v *Value_Integer) IsInteger() bool { return valueAsInteger(v).IsSome() }

// IsInteger is Value::is_integer.
func (v *Value_Float) IsInteger() bool { return valueAsInteger(v).IsSome() }

// IsInteger is Value::is_integer.
func (v *Value_Boolean) IsInteger() bool { return valueAsInteger(v).IsSome() }

// IsInteger is Value::is_integer.
func (v *Value_Datetime) IsInteger() bool { return valueAsInteger(v).IsSome() }

// IsInteger is Value::is_integer.
func (v *Value_Array) IsInteger() bool { return valueAsInteger(v).IsSome() }

// IsInteger is Value::is_integer.
func (v *Value_InlineTable) IsInteger() bool { return valueAsInteger(v).IsSome() }

// AsFloat is Value::as_float.
func (v *Value_String) AsFloat() rs.Option[float64] { return valueAsFloat(v) }

// AsFloat is Value::as_float.
func (v *Value_Integer) AsFloat() rs.Option[float64] { return valueAsFloat(v) }

// AsFloat is Value::as_float.
func (v *Value_Float) AsFloat() rs.Option[float64] { return valueAsFloat(v) }

// AsFloat is Value::as_float.
func (v *Value_Boolean) AsFloat() rs.Option[float64] { return valueAsFloat(v) }

// AsFloat is Value::as_float.
func (v *Value_Datetime) AsFloat() rs.Option[float64] { return valueAsFloat(v) }

// AsFloat is Value::as_float.
func (v *Value_Array) AsFloat() rs.Option[float64] { return valueAsFloat(v) }

// AsFloat is Value::as_float.
func (v *Value_InlineTable) AsFloat() rs.Option[float64] { return valueAsFloat(v) }

// IsFloat is Value::is_float.
func (v *Value_String) IsFloat() bool { return valueAsFloat(v).IsSome() }

// IsFloat is Value::is_float.
func (v *Value_Integer) IsFloat() bool { return valueAsFloat(v).IsSome() }

// IsFloat is Value::is_float.
func (v *Value_Float) IsFloat() bool { return valueAsFloat(v).IsSome() }

// IsFloat is Value::is_float.
func (v *Value_Boolean) IsFloat() bool { return valueAsFloat(v).IsSome() }

// IsFloat is Value::is_float.
func (v *Value_Datetime) IsFloat() bool { return valueAsFloat(v).IsSome() }

// IsFloat is Value::is_float.
func (v *Value_Array) IsFloat() bool { return valueAsFloat(v).IsSome() }

// IsFloat is Value::is_float.
func (v *Value_InlineTable) IsFloat() bool { return valueAsFloat(v).IsSome() }

// AsBool is Value::as_bool.
func (v *Value_String) AsBool() rs.Option[bool] { return valueAsBool(v) }

// AsBool is Value::as_bool.
func (v *Value_Integer) AsBool() rs.Option[bool] { return valueAsBool(v) }

// AsBool is Value::as_bool.
func (v *Value_Float) AsBool() rs.Option[bool] { return valueAsBool(v) }

// AsBool is Value::as_bool.
func (v *Value_Boolean) AsBool() rs.Option[bool] { return valueAsBool(v) }

// AsBool is Value::as_bool.
func (v *Value_Datetime) AsBool() rs.Option[bool] { return valueAsBool(v) }

// AsBool is Value::as_bool.
func (v *Value_Array) AsBool() rs.Option[bool] { return valueAsBool(v) }

// AsBool is Value::as_bool.
func (v *Value_InlineTable) AsBool() rs.Option[bool] { return valueAsBool(v) }

// IsBool is Value::is_bool.
func (v *Value_String) IsBool() bool { return valueAsBool(v).IsSome() }

// IsBool is Value::is_bool.
func (v *Value_Integer) IsBool() bool { return valueAsBool(v).IsSome() }

// IsBool is Value::is_bool.
func (v *Value_Float) IsBool() bool { return valueAsBool(v).IsSome() }

// IsBool is Value::is_bool.
func (v *Value_Boolean) IsBool() bool { return valueAsBool(v).IsSome() }

// IsBool is Value::is_bool.
func (v *Value_Datetime) IsBool() bool { return valueAsBool(v).IsSome() }

// IsBool is Value::is_bool.
func (v *Value_Array) IsBool() bool { return valueAsBool(v).IsSome() }

// IsBool is Value::is_bool.
func (v *Value_InlineTable) IsBool() bool { return valueAsBool(v).IsSome() }

// AsDatetime is Value::as_datetime.
func (v *Value_String) AsDatetime() rs.Option[Datetime] { return valueAsDatetime(v) }

// AsDatetime is Value::as_datetime.
func (v *Value_Integer) AsDatetime() rs.Option[Datetime] { return valueAsDatetime(v) }

// AsDatetime is Value::as_datetime.
func (v *Value_Float) AsDatetime() rs.Option[Datetime] { return valueAsDatetime(v) }

// AsDatetime is Value::as_datetime.
func (v *Value_Boolean) AsDatetime() rs.Option[Datetime] { return valueAsDatetime(v) }

// AsDatetime is Value::as_datetime.
func (v *Value_Datetime) AsDatetime() rs.Option[Datetime] { return valueAsDatetime(v) }

// AsDatetime is Value::as_datetime.
func (v *Value_Array) AsDatetime() rs.Option[Datetime] { return valueAsDatetime(v) }

// AsDatetime is Value::as_datetime.
func (v *Value_InlineTable) AsDatetime() rs.Option[Datetime] { return valueAsDatetime(v) }

// IsDatetime is Value::is_datetime.
func (v *Value_String) IsDatetime() bool { return valueAsDatetime(v).IsSome() }

// IsDatetime is Value::is_datetime.
func (v *Value_Integer) IsDatetime() bool { return valueAsDatetime(v).IsSome() }

// IsDatetime is Value::is_datetime.
func (v *Value_Float) IsDatetime() bool { return valueAsDatetime(v).IsSome() }

// IsDatetime is Value::is_datetime.
func (v *Value_Boolean) IsDatetime() bool { return valueAsDatetime(v).IsSome() }

// IsDatetime is Value::is_datetime.
func (v *Value_Datetime) IsDatetime() bool { return valueAsDatetime(v).IsSome() }

// IsDatetime is Value::is_datetime.
func (v *Value_Array) IsDatetime() bool { return valueAsDatetime(v).IsSome() }

// IsDatetime is Value::is_datetime.
func (v *Value_InlineTable) IsDatetime() bool { return valueAsDatetime(v).IsSome() }

// AsArray is Value::as_array.
func (v *Value_String) AsArray() rs.Option[*Array] { return valueAsArray(v) }

// AsArray is Value::as_array.
func (v *Value_Integer) AsArray() rs.Option[*Array] { return valueAsArray(v) }

// AsArray is Value::as_array.
func (v *Value_Float) AsArray() rs.Option[*Array] { return valueAsArray(v) }

// AsArray is Value::as_array.
func (v *Value_Boolean) AsArray() rs.Option[*Array] { return valueAsArray(v) }

// AsArray is Value::as_array.
func (v *Value_Datetime) AsArray() rs.Option[*Array] { return valueAsArray(v) }

// AsArray is Value::as_array.
func (v *Value_Array) AsArray() rs.Option[*Array] { return valueAsArray(v) }

// AsArray is Value::as_array.
func (v *Value_InlineTable) AsArray() rs.Option[*Array] { return valueAsArray(v) }

// AsArrayMut is Value::as_array_mut.
func (v *Value_String) AsArrayMut() rs.Option[*Array] { return valueAsArray(v) }

// AsArrayMut is Value::as_array_mut.
func (v *Value_Integer) AsArrayMut() rs.Option[*Array] { return valueAsArray(v) }

// AsArrayMut is Value::as_array_mut.
func (v *Value_Float) AsArrayMut() rs.Option[*Array] { return valueAsArray(v) }

// AsArrayMut is Value::as_array_mut.
func (v *Value_Boolean) AsArrayMut() rs.Option[*Array] { return valueAsArray(v) }

// AsArrayMut is Value::as_array_mut.
func (v *Value_Datetime) AsArrayMut() rs.Option[*Array] { return valueAsArray(v) }

// AsArrayMut is Value::as_array_mut.
func (v *Value_Array) AsArrayMut() rs.Option[*Array] { return valueAsArray(v) }

// AsArrayMut is Value::as_array_mut.
func (v *Value_InlineTable) AsArrayMut() rs.Option[*Array] { return valueAsArray(v) }

// IsArray is Value::is_array.
func (v *Value_String) IsArray() bool { return valueAsArray(v).IsSome() }

// IsArray is Value::is_array.
func (v *Value_Integer) IsArray() bool { return valueAsArray(v).IsSome() }

// IsArray is Value::is_array.
func (v *Value_Float) IsArray() bool { return valueAsArray(v).IsSome() }

// IsArray is Value::is_array.
func (v *Value_Boolean) IsArray() bool { return valueAsArray(v).IsSome() }

// IsArray is Value::is_array.
func (v *Value_Datetime) IsArray() bool { return valueAsArray(v).IsSome() }

// IsArray is Value::is_array.
func (v *Value_Array) IsArray() bool { return valueAsArray(v).IsSome() }

// IsArray is Value::is_array.
func (v *Value_InlineTable) IsArray() bool { return valueAsArray(v).IsSome() }

// AsInlineTable is Value::as_inline_table.
func (v *Value_String) AsInlineTable() rs.Option[*InlineTable] { return valueAsInlineTable(v) }

// AsInlineTable is Value::as_inline_table.
func (v *Value_Integer) AsInlineTable() rs.Option[*InlineTable] { return valueAsInlineTable(v) }

// AsInlineTable is Value::as_inline_table.
func (v *Value_Float) AsInlineTable() rs.Option[*InlineTable] { return valueAsInlineTable(v) }

// AsInlineTable is Value::as_inline_table.
func (v *Value_Boolean) AsInlineTable() rs.Option[*InlineTable] { return valueAsInlineTable(v) }

// AsInlineTable is Value::as_inline_table.
func (v *Value_Datetime) AsInlineTable() rs.Option[*InlineTable] { return valueAsInlineTable(v) }

// AsInlineTable is Value::as_inline_table.
func (v *Value_Array) AsInlineTable() rs.Option[*InlineTable] { return valueAsInlineTable(v) }

// AsInlineTable is Value::as_inline_table.
func (v *Value_InlineTable) AsInlineTable() rs.Option[*InlineTable] { return valueAsInlineTable(v) }

// AsInlineTableMut is Value::as_inline_table_mut.
func (v *Value_String) AsInlineTableMut() rs.Option[*InlineTable] { return valueAsInlineTable(v) }

// AsInlineTableMut is Value::as_inline_table_mut.
func (v *Value_Integer) AsInlineTableMut() rs.Option[*InlineTable] { return valueAsInlineTable(v) }

// AsInlineTableMut is Value::as_inline_table_mut.
func (v *Value_Float) AsInlineTableMut() rs.Option[*InlineTable] { return valueAsInlineTable(v) }

// AsInlineTableMut is Value::as_inline_table_mut.
func (v *Value_Boolean) AsInlineTableMut() rs.Option[*InlineTable] { return valueAsInlineTable(v) }

// AsInlineTableMut is Value::as_inline_table_mut.
func (v *Value_Datetime) AsInlineTableMut() rs.Option[*InlineTable] { return valueAsInlineTable(v) }

// AsInlineTableMut is Value::as_inline_table_mut.
func (v *Value_Array) AsInlineTableMut() rs.Option[*InlineTable] { return valueAsInlineTable(v) }

// AsInlineTableMut is Value::as_inline_table_mut.
func (v *Value_InlineTable) AsInlineTableMut() rs.Option[*InlineTable] { return valueAsInlineTable(v) }

// IsInlineTable is Value::is_inline_table.
func (v *Value_String) IsInlineTable() bool { return valueAsInlineTable(v).IsSome() }

// IsInlineTable is Value::is_inline_table.
func (v *Value_Integer) IsInlineTable() bool { return valueAsInlineTable(v).IsSome() }

// IsInlineTable is Value::is_inline_table.
func (v *Value_Float) IsInlineTable() bool { return valueAsInlineTable(v).IsSome() }

// IsInlineTable is Value::is_inline_table.
func (v *Value_Boolean) IsInlineTable() bool { return valueAsInlineTable(v).IsSome() }

// IsInlineTable is Value::is_inline_table.
func (v *Value_Datetime) IsInlineTable() bool { return valueAsInlineTable(v).IsSome() }

// IsInlineTable is Value::is_inline_table.
func (v *Value_Array) IsInlineTable() bool { return valueAsInlineTable(v).IsSome() }

// IsInlineTable is Value::is_inline_table.
func (v *Value_InlineTable) IsInlineTable() bool { return valueAsInlineTable(v).IsSome() }

// DecorMut is Value::decor_mut.
func (v *Value_String) DecorMut() *Decor { return valueDecorMut(v) }

// DecorMut is Value::decor_mut.
func (v *Value_Integer) DecorMut() *Decor { return valueDecorMut(v) }

// DecorMut is Value::decor_mut.
func (v *Value_Float) DecorMut() *Decor { return valueDecorMut(v) }

// DecorMut is Value::decor_mut.
func (v *Value_Boolean) DecorMut() *Decor { return valueDecorMut(v) }

// DecorMut is Value::decor_mut.
func (v *Value_Datetime) DecorMut() *Decor { return valueDecorMut(v) }

// DecorMut is Value::decor_mut.
func (v *Value_Array) DecorMut() *Decor { return valueDecorMut(v) }

// DecorMut is Value::decor_mut.
func (v *Value_InlineTable) DecorMut() *Decor { return valueDecorMut(v) }

// Decor is Value::decor.
func (v *Value_String) Decor() *Decor { return valueDecorMut(v) }

// Decor is Value::decor.
func (v *Value_Integer) Decor() *Decor { return valueDecorMut(v) }

// Decor is Value::decor.
func (v *Value_Float) Decor() *Decor { return valueDecorMut(v) }

// Decor is Value::decor.
func (v *Value_Boolean) Decor() *Decor { return valueDecorMut(v) }

// Decor is Value::decor.
func (v *Value_Datetime) Decor() *Decor { return valueDecorMut(v) }

// Decor is Value::decor.
func (v *Value_Array) Decor() *Decor { return valueDecorMut(v) }

// Decor is Value::decor.
func (v *Value_InlineTable) Decor() *Decor { return valueDecorMut(v) }

// Decorated is Value::decorated.
func (v *Value_String) Decorated(prefix any, suffix any) Value {
	return valueDecorated(v, prefix, suffix)
}

// Decorated is Value::decorated.
func (v *Value_Integer) Decorated(prefix any, suffix any) Value {
	return valueDecorated(v, prefix, suffix)
}

// Decorated is Value::decorated.
func (v *Value_Float) Decorated(prefix any, suffix any) Value {
	return valueDecorated(v, prefix, suffix)
}

// Decorated is Value::decorated.
func (v *Value_Boolean) Decorated(prefix any, suffix any) Value {
	return valueDecorated(v, prefix, suffix)
}

// Decorated is Value::decorated.
func (v *Value_Datetime) Decorated(prefix any, suffix any) Value {
	return valueDecorated(v, prefix, suffix)
}

// Decorated is Value::decorated.
func (v *Value_Array) Decorated(prefix any, suffix any) Value {
	return valueDecorated(v, prefix, suffix)
}

// Decorated is Value::decorated.
func (v *Value_InlineTable) Decorated(prefix any, suffix any) Value {
	return valueDecorated(v, prefix, suffix)
}

// Clone is Value::clone.
func (v *Value_String) Clone() Value { return valueClone(v) }

// Clone is Value::clone.
func (v *Value_Integer) Clone() Value { return valueClone(v) }

// Clone is Value::clone.
func (v *Value_Float) Clone() Value { return valueClone(v) }

// Clone is Value::clone.
func (v *Value_Boolean) Clone() Value { return valueClone(v) }

// Clone is Value::clone.
func (v *Value_Datetime) Clone() Value { return valueClone(v) }

// Clone is Value::clone.
func (v *Value_Array) Clone() Value { return valueClone(v) }

// Clone is Value::clone.
func (v *Value_InlineTable) Clone() Value { return valueClone(v) }

// String is Value::string.
func (v *Value_String) String() string { return valueString(v) }

// String is Value::string.
func (v *Value_Integer) String() string { return valueString(v) }

// String is Value::string.
func (v *Value_Float) String() string { return valueString(v) }

// String is Value::string.
func (v *Value_Boolean) String() string { return valueString(v) }

// String is Value::string.
func (v *Value_Datetime) String() string { return valueString(v) }

// String is Value::string.
func (v *Value_Array) String() string { return valueString(v) }

// String is Value::string.
func (v *Value_InlineTable) String() string { return valueString(v) }

// FmtDisplay is Value::fmt_display.
func (v *Value_String) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(valueString(v)) }

// FmtDisplay is Value::fmt_display.
func (v *Value_Integer) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(valueString(v)) }

// FmtDisplay is Value::fmt_display.
func (v *Value_Float) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(valueString(v)) }

// FmtDisplay is Value::fmt_display.
func (v *Value_Boolean) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(valueString(v)) }

// FmtDisplay is Value::fmt_display.
func (v *Value_Datetime) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(valueString(v)) }

// FmtDisplay is Value::fmt_display.
func (v *Value_Array) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(valueString(v)) }

// FmtDisplay is Value::fmt_display.
func (v *Value_InlineTable) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(valueString(v)) }

func (v *Value_String) decorate(prefix RawString, suffix RawString) { valueDecorate(v, prefix, suffix) }

func (v *Value_Integer) decorate(prefix RawString, suffix RawString) {
	valueDecorate(v, prefix, suffix)
}

func (v *Value_Float) decorate(prefix RawString, suffix RawString) { valueDecorate(v, prefix, suffix) }

func (v *Value_Boolean) decorate(prefix RawString, suffix RawString) {
	valueDecorate(v, prefix, suffix)
}

func (v *Value_Datetime) decorate(prefix RawString, suffix RawString) {
	valueDecorate(v, prefix, suffix)
}

func (v *Value_Array) decorate(prefix RawString, suffix RawString) { valueDecorate(v, prefix, suffix) }

func (v *Value_InlineTable) decorate(prefix RawString, suffix RawString) {
	valueDecorate(v, prefix, suffix)
}

func (v *Value_String) span() rs.Option[Range] { return valueSpan(v) }

func (v *Value_Integer) span() rs.Option[Range] { return valueSpan(v) }

func (v *Value_Float) span() rs.Option[Range] { return valueSpan(v) }

func (v *Value_Boolean) span() rs.Option[Range] { return valueSpan(v) }

func (v *Value_Datetime) span() rs.Option[Range] { return valueSpan(v) }

func (v *Value_Array) span() rs.Option[Range] { return valueSpan(v) }

func (v *Value_InlineTable) span() rs.Option[Range] { return valueSpan(v) }

func (v *Value_String) despan(input string) { valueDespan(v, input) }

func (v *Value_Integer) despan(input string) { valueDespan(v, input) }

func (v *Value_Float) despan(input string) { valueDespan(v, input) }

func (v *Value_Boolean) despan(input string) { valueDespan(v, input) }

func (v *Value_Datetime) despan(input string) { valueDespan(v, input) }

func (v *Value_Array) despan(input string) { valueDespan(v, input) }

func (v *Value_InlineTable) despan(input string) { valueDespan(v, input) }
