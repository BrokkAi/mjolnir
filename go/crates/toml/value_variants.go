// Port of toml 0.8.2 src/value.rs
//
// The methods of the Value enum, given to each variant. Each one forwards to
// the function in value.go that holds the body.
// Code generated for the Value variants; keep in step with the Value
// interface.

package toml

import "github.com/BrokkAi/mjolnir/go/rs"

func (*Value_String) isValue()   {}
func (*Value_Integer) isValue()  {}
func (*Value_Float) isValue()    {}
func (*Value_Boolean) isValue()  {}
func (*Value_Datetime) isValue() {}
func (*Value_Array) isValue()    {}
func (*Value_Table) isValue()    {}

func (v *Value_String) Get(index any) rs.Option[Value]   { return valueGet(v, index) }
func (v *Value_Integer) Get(index any) rs.Option[Value]  { return valueGet(v, index) }
func (v *Value_Float) Get(index any) rs.Option[Value]    { return valueGet(v, index) }
func (v *Value_Boolean) Get(index any) rs.Option[Value]  { return valueGet(v, index) }
func (v *Value_Datetime) Get(index any) rs.Option[Value] { return valueGet(v, index) }
func (v *Value_Array) Get(index any) rs.Option[Value]    { return valueGet(v, index) }
func (v *Value_Table) Get(index any) rs.Option[Value]    { return valueGet(v, index) }

func (v *Value_String) GetMut(index any) rs.Option[*Value]   { return valueGetMut(v, index) }
func (v *Value_Integer) GetMut(index any) rs.Option[*Value]  { return valueGetMut(v, index) }
func (v *Value_Float) GetMut(index any) rs.Option[*Value]    { return valueGetMut(v, index) }
func (v *Value_Boolean) GetMut(index any) rs.Option[*Value]  { return valueGetMut(v, index) }
func (v *Value_Datetime) GetMut(index any) rs.Option[*Value] { return valueGetMut(v, index) }
func (v *Value_Array) GetMut(index any) rs.Option[*Value]    { return valueGetMut(v, index) }
func (v *Value_Table) GetMut(index any) rs.Option[*Value]    { return valueGetMut(v, index) }

func (v *Value_String) AsInteger() rs.Option[int64]   { return valueAsInteger(v) }
func (v *Value_Integer) AsInteger() rs.Option[int64]  { return valueAsInteger(v) }
func (v *Value_Float) AsInteger() rs.Option[int64]    { return valueAsInteger(v) }
func (v *Value_Boolean) AsInteger() rs.Option[int64]  { return valueAsInteger(v) }
func (v *Value_Datetime) AsInteger() rs.Option[int64] { return valueAsInteger(v) }
func (v *Value_Array) AsInteger() rs.Option[int64]    { return valueAsInteger(v) }
func (v *Value_Table) AsInteger() rs.Option[int64]    { return valueAsInteger(v) }

func (v *Value_String) IsInteger() bool   { return valueAsInteger(v).IsSome() }
func (v *Value_Integer) IsInteger() bool  { return valueAsInteger(v).IsSome() }
func (v *Value_Float) IsInteger() bool    { return valueAsInteger(v).IsSome() }
func (v *Value_Boolean) IsInteger() bool  { return valueAsInteger(v).IsSome() }
func (v *Value_Datetime) IsInteger() bool { return valueAsInteger(v).IsSome() }
func (v *Value_Array) IsInteger() bool    { return valueAsInteger(v).IsSome() }
func (v *Value_Table) IsInteger() bool    { return valueAsInteger(v).IsSome() }

func (v *Value_String) AsFloat() rs.Option[float64]   { return valueAsFloat(v) }
func (v *Value_Integer) AsFloat() rs.Option[float64]  { return valueAsFloat(v) }
func (v *Value_Float) AsFloat() rs.Option[float64]    { return valueAsFloat(v) }
func (v *Value_Boolean) AsFloat() rs.Option[float64]  { return valueAsFloat(v) }
func (v *Value_Datetime) AsFloat() rs.Option[float64] { return valueAsFloat(v) }
func (v *Value_Array) AsFloat() rs.Option[float64]    { return valueAsFloat(v) }
func (v *Value_Table) AsFloat() rs.Option[float64]    { return valueAsFloat(v) }

func (v *Value_String) IsFloat() bool   { return valueAsFloat(v).IsSome() }
func (v *Value_Integer) IsFloat() bool  { return valueAsFloat(v).IsSome() }
func (v *Value_Float) IsFloat() bool    { return valueAsFloat(v).IsSome() }
func (v *Value_Boolean) IsFloat() bool  { return valueAsFloat(v).IsSome() }
func (v *Value_Datetime) IsFloat() bool { return valueAsFloat(v).IsSome() }
func (v *Value_Array) IsFloat() bool    { return valueAsFloat(v).IsSome() }
func (v *Value_Table) IsFloat() bool    { return valueAsFloat(v).IsSome() }

func (v *Value_String) AsBool() rs.Option[bool]   { return valueAsBool(v) }
func (v *Value_Integer) AsBool() rs.Option[bool]  { return valueAsBool(v) }
func (v *Value_Float) AsBool() rs.Option[bool]    { return valueAsBool(v) }
func (v *Value_Boolean) AsBool() rs.Option[bool]  { return valueAsBool(v) }
func (v *Value_Datetime) AsBool() rs.Option[bool] { return valueAsBool(v) }
func (v *Value_Array) AsBool() rs.Option[bool]    { return valueAsBool(v) }
func (v *Value_Table) AsBool() rs.Option[bool]    { return valueAsBool(v) }

func (v *Value_String) IsBool() bool   { return valueAsBool(v).IsSome() }
func (v *Value_Integer) IsBool() bool  { return valueAsBool(v).IsSome() }
func (v *Value_Float) IsBool() bool    { return valueAsBool(v).IsSome() }
func (v *Value_Boolean) IsBool() bool  { return valueAsBool(v).IsSome() }
func (v *Value_Datetime) IsBool() bool { return valueAsBool(v).IsSome() }
func (v *Value_Array) IsBool() bool    { return valueAsBool(v).IsSome() }
func (v *Value_Table) IsBool() bool    { return valueAsBool(v).IsSome() }

func (v *Value_String) AsStr() rs.Option[string]   { return valueAsStr(v) }
func (v *Value_Integer) AsStr() rs.Option[string]  { return valueAsStr(v) }
func (v *Value_Float) AsStr() rs.Option[string]    { return valueAsStr(v) }
func (v *Value_Boolean) AsStr() rs.Option[string]  { return valueAsStr(v) }
func (v *Value_Datetime) AsStr() rs.Option[string] { return valueAsStr(v) }
func (v *Value_Array) AsStr() rs.Option[string]    { return valueAsStr(v) }
func (v *Value_Table) AsStr() rs.Option[string]    { return valueAsStr(v) }

func (v *Value_String) IsStr() bool   { return valueAsStr(v).IsSome() }
func (v *Value_Integer) IsStr() bool  { return valueAsStr(v).IsSome() }
func (v *Value_Float) IsStr() bool    { return valueAsStr(v).IsSome() }
func (v *Value_Boolean) IsStr() bool  { return valueAsStr(v).IsSome() }
func (v *Value_Datetime) IsStr() bool { return valueAsStr(v).IsSome() }
func (v *Value_Array) IsStr() bool    { return valueAsStr(v).IsSome() }
func (v *Value_Table) IsStr() bool    { return valueAsStr(v).IsSome() }

func (v *Value_String) AsDatetime() rs.Option[ValueDatetime]   { return valueAsDatetime(v) }
func (v *Value_Integer) AsDatetime() rs.Option[ValueDatetime]  { return valueAsDatetime(v) }
func (v *Value_Float) AsDatetime() rs.Option[ValueDatetime]    { return valueAsDatetime(v) }
func (v *Value_Boolean) AsDatetime() rs.Option[ValueDatetime]  { return valueAsDatetime(v) }
func (v *Value_Datetime) AsDatetime() rs.Option[ValueDatetime] { return valueAsDatetime(v) }
func (v *Value_Array) AsDatetime() rs.Option[ValueDatetime]    { return valueAsDatetime(v) }
func (v *Value_Table) AsDatetime() rs.Option[ValueDatetime]    { return valueAsDatetime(v) }

func (v *Value_String) IsDatetime() bool   { return valueAsDatetime(v).IsSome() }
func (v *Value_Integer) IsDatetime() bool  { return valueAsDatetime(v).IsSome() }
func (v *Value_Float) IsDatetime() bool    { return valueAsDatetime(v).IsSome() }
func (v *Value_Boolean) IsDatetime() bool  { return valueAsDatetime(v).IsSome() }
func (v *Value_Datetime) IsDatetime() bool { return valueAsDatetime(v).IsSome() }
func (v *Value_Array) IsDatetime() bool    { return valueAsDatetime(v).IsSome() }
func (v *Value_Table) IsDatetime() bool    { return valueAsDatetime(v).IsSome() }

func (v *Value_String) AsArray() rs.Option[*[]Value]   { return valueAsArray(v) }
func (v *Value_Integer) AsArray() rs.Option[*[]Value]  { return valueAsArray(v) }
func (v *Value_Float) AsArray() rs.Option[*[]Value]    { return valueAsArray(v) }
func (v *Value_Boolean) AsArray() rs.Option[*[]Value]  { return valueAsArray(v) }
func (v *Value_Datetime) AsArray() rs.Option[*[]Value] { return valueAsArray(v) }
func (v *Value_Array) AsArray() rs.Option[*[]Value]    { return valueAsArray(v) }
func (v *Value_Table) AsArray() rs.Option[*[]Value]    { return valueAsArray(v) }

func (v *Value_String) AsArrayMut() rs.Option[*[]Value]   { return valueAsArray(v) }
func (v *Value_Integer) AsArrayMut() rs.Option[*[]Value]  { return valueAsArray(v) }
func (v *Value_Float) AsArrayMut() rs.Option[*[]Value]    { return valueAsArray(v) }
func (v *Value_Boolean) AsArrayMut() rs.Option[*[]Value]  { return valueAsArray(v) }
func (v *Value_Datetime) AsArrayMut() rs.Option[*[]Value] { return valueAsArray(v) }
func (v *Value_Array) AsArrayMut() rs.Option[*[]Value]    { return valueAsArray(v) }
func (v *Value_Table) AsArrayMut() rs.Option[*[]Value]    { return valueAsArray(v) }

func (v *Value_String) IsArray() bool   { return valueAsArray(v).IsSome() }
func (v *Value_Integer) IsArray() bool  { return valueAsArray(v).IsSome() }
func (v *Value_Float) IsArray() bool    { return valueAsArray(v).IsSome() }
func (v *Value_Boolean) IsArray() bool  { return valueAsArray(v).IsSome() }
func (v *Value_Datetime) IsArray() bool { return valueAsArray(v).IsSome() }
func (v *Value_Array) IsArray() bool    { return valueAsArray(v).IsSome() }
func (v *Value_Table) IsArray() bool    { return valueAsArray(v).IsSome() }

func (v *Value_String) AsTable() rs.Option[*Table]   { return valueAsTable(v) }
func (v *Value_Integer) AsTable() rs.Option[*Table]  { return valueAsTable(v) }
func (v *Value_Float) AsTable() rs.Option[*Table]    { return valueAsTable(v) }
func (v *Value_Boolean) AsTable() rs.Option[*Table]  { return valueAsTable(v) }
func (v *Value_Datetime) AsTable() rs.Option[*Table] { return valueAsTable(v) }
func (v *Value_Array) AsTable() rs.Option[*Table]    { return valueAsTable(v) }
func (v *Value_Table) AsTable() rs.Option[*Table]    { return valueAsTable(v) }

func (v *Value_String) AsTableMut() rs.Option[*Table]   { return valueAsTable(v) }
func (v *Value_Integer) AsTableMut() rs.Option[*Table]  { return valueAsTable(v) }
func (v *Value_Float) AsTableMut() rs.Option[*Table]    { return valueAsTable(v) }
func (v *Value_Boolean) AsTableMut() rs.Option[*Table]  { return valueAsTable(v) }
func (v *Value_Datetime) AsTableMut() rs.Option[*Table] { return valueAsTable(v) }
func (v *Value_Array) AsTableMut() rs.Option[*Table]    { return valueAsTable(v) }
func (v *Value_Table) AsTableMut() rs.Option[*Table]    { return valueAsTable(v) }

func (v *Value_String) IsTable() bool   { return valueAsTable(v).IsSome() }
func (v *Value_Integer) IsTable() bool  { return valueAsTable(v).IsSome() }
func (v *Value_Float) IsTable() bool    { return valueAsTable(v).IsSome() }
func (v *Value_Boolean) IsTable() bool  { return valueAsTable(v).IsSome() }
func (v *Value_Datetime) IsTable() bool { return valueAsTable(v).IsSome() }
func (v *Value_Array) IsTable() bool    { return valueAsTable(v).IsSome() }
func (v *Value_Table) IsTable() bool    { return valueAsTable(v).IsSome() }

func (v *Value_String) SameType(other Value) bool   { return valueSameType(v, other) }
func (v *Value_Integer) SameType(other Value) bool  { return valueSameType(v, other) }
func (v *Value_Float) SameType(other Value) bool    { return valueSameType(v, other) }
func (v *Value_Boolean) SameType(other Value) bool  { return valueSameType(v, other) }
func (v *Value_Datetime) SameType(other Value) bool { return valueSameType(v, other) }
func (v *Value_Array) SameType(other Value) bool    { return valueSameType(v, other) }
func (v *Value_Table) SameType(other Value) bool    { return valueSameType(v, other) }

func (v *Value_String) TypeStr() string   { return valueTypeStr(v) }
func (v *Value_Integer) TypeStr() string  { return valueTypeStr(v) }
func (v *Value_Float) TypeStr() string    { return valueTypeStr(v) }
func (v *Value_Boolean) TypeStr() string  { return valueTypeStr(v) }
func (v *Value_Datetime) TypeStr() string { return valueTypeStr(v) }
func (v *Value_Array) TypeStr() string    { return valueTypeStr(v) }
func (v *Value_Table) TypeStr() string    { return valueTypeStr(v) }

func (v *Value_String) Index(index any) Value   { return valueIndex(v, index) }
func (v *Value_Integer) Index(index any) Value  { return valueIndex(v, index) }
func (v *Value_Float) Index(index any) Value    { return valueIndex(v, index) }
func (v *Value_Boolean) Index(index any) Value  { return valueIndex(v, index) }
func (v *Value_Datetime) Index(index any) Value { return valueIndex(v, index) }
func (v *Value_Array) Index(index any) Value    { return valueIndex(v, index) }
func (v *Value_Table) Index(index any) Value    { return valueIndex(v, index) }

func (v *Value_String) IndexMut(index any) *Value   { return valueIndexMut(v, index) }
func (v *Value_Integer) IndexMut(index any) *Value  { return valueIndexMut(v, index) }
func (v *Value_Float) IndexMut(index any) *Value    { return valueIndexMut(v, index) }
func (v *Value_Boolean) IndexMut(index any) *Value  { return valueIndexMut(v, index) }
func (v *Value_Datetime) IndexMut(index any) *Value { return valueIndexMut(v, index) }
func (v *Value_Array) IndexMut(index any) *Value    { return valueIndexMut(v, index) }
func (v *Value_Table) IndexMut(index any) *Value    { return valueIndexMut(v, index) }

func (v *Value_String) String() string   { return valueString(v) }
func (v *Value_Integer) String() string  { return valueString(v) }
func (v *Value_Float) String() string    { return valueString(v) }
func (v *Value_Boolean) String() string  { return valueString(v) }
func (v *Value_Datetime) String() string { return valueString(v) }
func (v *Value_Array) String() string    { return valueString(v) }
func (v *Value_Table) String() string    { return valueString(v) }

func (v *Value_String) FmtDisplay(f *rs.Formatter) error   { return valueFmtDisplay(v, f) }
func (v *Value_Integer) FmtDisplay(f *rs.Formatter) error  { return valueFmtDisplay(v, f) }
func (v *Value_Float) FmtDisplay(f *rs.Formatter) error    { return valueFmtDisplay(v, f) }
func (v *Value_Boolean) FmtDisplay(f *rs.Formatter) error  { return valueFmtDisplay(v, f) }
func (v *Value_Datetime) FmtDisplay(f *rs.Formatter) error { return valueFmtDisplay(v, f) }
func (v *Value_Array) FmtDisplay(f *rs.Formatter) error    { return valueFmtDisplay(v, f) }
func (v *Value_Table) FmtDisplay(f *rs.Formatter) error    { return valueFmtDisplay(v, f) }
