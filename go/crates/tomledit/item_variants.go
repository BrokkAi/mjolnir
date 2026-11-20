// Port of toml_edit 0.20.2 src/item.rs
//
// The methods of the enum Item on each of its variants. Each forwards to the
// function in item.go that holds the Rust method's body.

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

func (*Item_None) isItem()          {}
func (*Item_Value) isItem()         {}
func (*Item_Table) isItem()         {}
func (*Item_ArrayOfTables) isItem() {}

// TypeName is Item::type_name.
func (v *Item_None) TypeName() string { return itemTypeName(v) }

// TypeName is Item::type_name.
func (v *Item_Value) TypeName() string { return itemTypeName(v) }

// TypeName is Item::type_name.
func (v *Item_Table) TypeName() string { return itemTypeName(v) }

// TypeName is Item::type_name.
func (v *Item_ArrayOfTables) TypeName() string { return itemTypeName(v) }

// Get is Item::get.
func (v *Item_None) Get(index any) rs.Option[Item] { return indexIndex(index, v) }

// Get is Item::get.
func (v *Item_Value) Get(index any) rs.Option[Item] { return indexIndex(index, v) }

// Get is Item::get.
func (v *Item_Table) Get(index any) rs.Option[Item] { return indexIndex(index, v) }

// Get is Item::get.
func (v *Item_ArrayOfTables) Get(index any) rs.Option[Item] { return indexIndex(index, v) }

// AsValue is Item::as_value.
func (v *Item_None) AsValue() rs.Option[Value] { return itemAsValue(v) }

// AsValue is Item::as_value.
func (v *Item_Value) AsValue() rs.Option[Value] { return itemAsValue(v) }

// AsValue is Item::as_value.
func (v *Item_Table) AsValue() rs.Option[Value] { return itemAsValue(v) }

// AsValue is Item::as_value.
func (v *Item_ArrayOfTables) AsValue() rs.Option[Value] { return itemAsValue(v) }

// AsTable is Item::as_table.
func (v *Item_None) AsTable() rs.Option[*Table] { return itemAsTable(v) }

// AsTable is Item::as_table.
func (v *Item_Value) AsTable() rs.Option[*Table] { return itemAsTable(v) }

// AsTable is Item::as_table.
func (v *Item_Table) AsTable() rs.Option[*Table] { return itemAsTable(v) }

// AsTable is Item::as_table.
func (v *Item_ArrayOfTables) AsTable() rs.Option[*Table] { return itemAsTable(v) }

// AsArrayOfTables is Item::as_array_of_tables.
func (v *Item_None) AsArrayOfTables() rs.Option[*ArrayOfTables] { return itemAsArrayOfTables(v) }

// AsArrayOfTables is Item::as_array_of_tables.
func (v *Item_Value) AsArrayOfTables() rs.Option[*ArrayOfTables] { return itemAsArrayOfTables(v) }

// AsArrayOfTables is Item::as_array_of_tables.
func (v *Item_Table) AsArrayOfTables() rs.Option[*ArrayOfTables] { return itemAsArrayOfTables(v) }

// AsArrayOfTables is Item::as_array_of_tables.
func (v *Item_ArrayOfTables) AsArrayOfTables() rs.Option[*ArrayOfTables] {
	return itemAsArrayOfTables(v)
}

// AsValueMut is Item::as_value_mut.
func (v *Item_None) AsValueMut() rs.Option[*Value] { return itemAsValueMut(v) }

// AsValueMut is Item::as_value_mut.
func (v *Item_Value) AsValueMut() rs.Option[*Value] { return itemAsValueMut(v) }

// AsValueMut is Item::as_value_mut.
func (v *Item_Table) AsValueMut() rs.Option[*Value] { return itemAsValueMut(v) }

// AsValueMut is Item::as_value_mut.
func (v *Item_ArrayOfTables) AsValueMut() rs.Option[*Value] { return itemAsValueMut(v) }

// AsTableMut is Item::as_table_mut.
func (v *Item_None) AsTableMut() rs.Option[*Table] { return itemAsTable(v) }

// AsTableMut is Item::as_table_mut.
func (v *Item_Value) AsTableMut() rs.Option[*Table] { return itemAsTable(v) }

// AsTableMut is Item::as_table_mut.
func (v *Item_Table) AsTableMut() rs.Option[*Table] { return itemAsTable(v) }

// AsTableMut is Item::as_table_mut.
func (v *Item_ArrayOfTables) AsTableMut() rs.Option[*Table] { return itemAsTable(v) }

// AsArrayOfTablesMut is Item::as_array_of_tables_mut.
func (v *Item_None) AsArrayOfTablesMut() rs.Option[*ArrayOfTables] { return itemAsArrayOfTables(v) }

// AsArrayOfTablesMut is Item::as_array_of_tables_mut.
func (v *Item_Value) AsArrayOfTablesMut() rs.Option[*ArrayOfTables] { return itemAsArrayOfTables(v) }

// AsArrayOfTablesMut is Item::as_array_of_tables_mut.
func (v *Item_Table) AsArrayOfTablesMut() rs.Option[*ArrayOfTables] { return itemAsArrayOfTables(v) }

// AsArrayOfTablesMut is Item::as_array_of_tables_mut.
func (v *Item_ArrayOfTables) AsArrayOfTablesMut() rs.Option[*ArrayOfTables] {
	return itemAsArrayOfTables(v)
}

// IntoValue is Item::into_value.
func (v *Item_None) IntoValue() (Value, error) { return itemIntoValue(v) }

// IntoValue is Item::into_value.
func (v *Item_Value) IntoValue() (Value, error) { return itemIntoValue(v) }

// IntoValue is Item::into_value.
func (v *Item_Table) IntoValue() (Value, error) { return itemIntoValue(v) }

// IntoValue is Item::into_value.
func (v *Item_ArrayOfTables) IntoValue() (Value, error) { return itemIntoValue(v) }

// IntoTable is Item::into_table.
func (v *Item_None) IntoTable() (Table, error) { return itemIntoTable(v) }

// IntoTable is Item::into_table.
func (v *Item_Value) IntoTable() (Table, error) { return itemIntoTable(v) }

// IntoTable is Item::into_table.
func (v *Item_Table) IntoTable() (Table, error) { return itemIntoTable(v) }

// IntoTable is Item::into_table.
func (v *Item_ArrayOfTables) IntoTable() (Table, error) { return itemIntoTable(v) }

// IntoArrayOfTables is Item::into_array_of_tables.
func (v *Item_None) IntoArrayOfTables() (ArrayOfTables, error) { return itemIntoArrayOfTables(v) }

// IntoArrayOfTables is Item::into_array_of_tables.
func (v *Item_Value) IntoArrayOfTables() (ArrayOfTables, error) { return itemIntoArrayOfTables(v) }

// IntoArrayOfTables is Item::into_array_of_tables.
func (v *Item_Table) IntoArrayOfTables() (ArrayOfTables, error) { return itemIntoArrayOfTables(v) }

// IntoArrayOfTables is Item::into_array_of_tables.
func (v *Item_ArrayOfTables) IntoArrayOfTables() (ArrayOfTables, error) {
	return itemIntoArrayOfTables(v)
}

// IsValue is Item::is_value.
func (v *Item_None) IsValue() bool { return itemAsValue(v).IsSome() }

// IsValue is Item::is_value.
func (v *Item_Value) IsValue() bool { return itemAsValue(v).IsSome() }

// IsValue is Item::is_value.
func (v *Item_Table) IsValue() bool { return itemAsValue(v).IsSome() }

// IsValue is Item::is_value.
func (v *Item_ArrayOfTables) IsValue() bool { return itemAsValue(v).IsSome() }

// IsTable is Item::is_table.
func (v *Item_None) IsTable() bool { return itemAsTable(v).IsSome() }

// IsTable is Item::is_table.
func (v *Item_Value) IsTable() bool { return itemAsTable(v).IsSome() }

// IsTable is Item::is_table.
func (v *Item_Table) IsTable() bool { return itemAsTable(v).IsSome() }

// IsTable is Item::is_table.
func (v *Item_ArrayOfTables) IsTable() bool { return itemAsTable(v).IsSome() }

// IsArrayOfTables is Item::is_array_of_tables.
func (v *Item_None) IsArrayOfTables() bool { return itemAsArrayOfTables(v).IsSome() }

// IsArrayOfTables is Item::is_array_of_tables.
func (v *Item_Value) IsArrayOfTables() bool { return itemAsArrayOfTables(v).IsSome() }

// IsArrayOfTables is Item::is_array_of_tables.
func (v *Item_Table) IsArrayOfTables() bool { return itemAsArrayOfTables(v).IsSome() }

// IsArrayOfTables is Item::is_array_of_tables.
func (v *Item_ArrayOfTables) IsArrayOfTables() bool { return itemAsArrayOfTables(v).IsSome() }

// IsNone is Item::is_none.
func (v *Item_None) IsNone() bool { return itemIsNone(v) }

// IsNone is Item::is_none.
func (v *Item_Value) IsNone() bool { return itemIsNone(v) }

// IsNone is Item::is_none.
func (v *Item_Table) IsNone() bool { return itemIsNone(v) }

// IsNone is Item::is_none.
func (v *Item_ArrayOfTables) IsNone() bool { return itemIsNone(v) }

// AsInteger is Item::as_integer.
func (v *Item_None) AsInteger() rs.Option[int64] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsInteger)
}

// AsInteger is Item::as_integer.
func (v *Item_Value) AsInteger() rs.Option[int64] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsInteger)
}

// AsInteger is Item::as_integer.
func (v *Item_Table) AsInteger() rs.Option[int64] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsInteger)
}

// AsInteger is Item::as_integer.
func (v *Item_ArrayOfTables) AsInteger() rs.Option[int64] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsInteger)
}

// IsInteger is Item::is_integer.
func (v *Item_None) IsInteger() bool { return v.AsInteger().IsSome() }

// IsInteger is Item::is_integer.
func (v *Item_Value) IsInteger() bool { return v.AsInteger().IsSome() }

// IsInteger is Item::is_integer.
func (v *Item_Table) IsInteger() bool { return v.AsInteger().IsSome() }

// IsInteger is Item::is_integer.
func (v *Item_ArrayOfTables) IsInteger() bool { return v.AsInteger().IsSome() }

// AsFloat is Item::as_float.
func (v *Item_None) AsFloat() rs.Option[float64] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsFloat)
}

// AsFloat is Item::as_float.
func (v *Item_Value) AsFloat() rs.Option[float64] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsFloat)
}

// AsFloat is Item::as_float.
func (v *Item_Table) AsFloat() rs.Option[float64] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsFloat)
}

// AsFloat is Item::as_float.
func (v *Item_ArrayOfTables) AsFloat() rs.Option[float64] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsFloat)
}

// IsFloat is Item::is_float.
func (v *Item_None) IsFloat() bool { return v.AsFloat().IsSome() }

// IsFloat is Item::is_float.
func (v *Item_Value) IsFloat() bool { return v.AsFloat().IsSome() }

// IsFloat is Item::is_float.
func (v *Item_Table) IsFloat() bool { return v.AsFloat().IsSome() }

// IsFloat is Item::is_float.
func (v *Item_ArrayOfTables) IsFloat() bool { return v.AsFloat().IsSome() }

// AsBool is Item::as_bool.
func (v *Item_None) AsBool() rs.Option[bool] { return rs.OptionAndThen(itemAsValue(v), Value.AsBool) }

// AsBool is Item::as_bool.
func (v *Item_Value) AsBool() rs.Option[bool] { return rs.OptionAndThen(itemAsValue(v), Value.AsBool) }

// AsBool is Item::as_bool.
func (v *Item_Table) AsBool() rs.Option[bool] { return rs.OptionAndThen(itemAsValue(v), Value.AsBool) }

// AsBool is Item::as_bool.
func (v *Item_ArrayOfTables) AsBool() rs.Option[bool] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsBool)
}

// IsBool is Item::is_bool.
func (v *Item_None) IsBool() bool { return v.AsBool().IsSome() }

// IsBool is Item::is_bool.
func (v *Item_Value) IsBool() bool { return v.AsBool().IsSome() }

// IsBool is Item::is_bool.
func (v *Item_Table) IsBool() bool { return v.AsBool().IsSome() }

// IsBool is Item::is_bool.
func (v *Item_ArrayOfTables) IsBool() bool { return v.AsBool().IsSome() }

// AsStr is Item::as_str.
func (v *Item_None) AsStr() rs.Option[string] { return rs.OptionAndThen(itemAsValue(v), Value.AsStr) }

// AsStr is Item::as_str.
func (v *Item_Value) AsStr() rs.Option[string] { return rs.OptionAndThen(itemAsValue(v), Value.AsStr) }

// AsStr is Item::as_str.
func (v *Item_Table) AsStr() rs.Option[string] { return rs.OptionAndThen(itemAsValue(v), Value.AsStr) }

// AsStr is Item::as_str.
func (v *Item_ArrayOfTables) AsStr() rs.Option[string] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsStr)
}

// IsStr is Item::is_str.
func (v *Item_None) IsStr() bool { return v.AsStr().IsSome() }

// IsStr is Item::is_str.
func (v *Item_Value) IsStr() bool { return v.AsStr().IsSome() }

// IsStr is Item::is_str.
func (v *Item_Table) IsStr() bool { return v.AsStr().IsSome() }

// IsStr is Item::is_str.
func (v *Item_ArrayOfTables) IsStr() bool { return v.AsStr().IsSome() }

// AsDatetime is Item::as_datetime.
func (v *Item_None) AsDatetime() rs.Option[Datetime] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsDatetime)
}

// AsDatetime is Item::as_datetime.
func (v *Item_Value) AsDatetime() rs.Option[Datetime] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsDatetime)
}

// AsDatetime is Item::as_datetime.
func (v *Item_Table) AsDatetime() rs.Option[Datetime] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsDatetime)
}

// AsDatetime is Item::as_datetime.
func (v *Item_ArrayOfTables) AsDatetime() rs.Option[Datetime] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsDatetime)
}

// IsDatetime is Item::is_datetime.
func (v *Item_None) IsDatetime() bool { return v.AsDatetime().IsSome() }

// IsDatetime is Item::is_datetime.
func (v *Item_Value) IsDatetime() bool { return v.AsDatetime().IsSome() }

// IsDatetime is Item::is_datetime.
func (v *Item_Table) IsDatetime() bool { return v.AsDatetime().IsSome() }

// IsDatetime is Item::is_datetime.
func (v *Item_ArrayOfTables) IsDatetime() bool { return v.AsDatetime().IsSome() }

// AsArray is Item::as_array.
func (v *Item_None) AsArray() rs.Option[*Array] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsArray)
}

// AsArray is Item::as_array.
func (v *Item_Value) AsArray() rs.Option[*Array] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsArray)
}

// AsArray is Item::as_array.
func (v *Item_Table) AsArray() rs.Option[*Array] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsArray)
}

// AsArray is Item::as_array.
func (v *Item_ArrayOfTables) AsArray() rs.Option[*Array] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsArray)
}

// AsArrayMut is Item::as_array_mut.
func (v *Item_None) AsArrayMut() rs.Option[*Array] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsArrayMut)
}

// AsArrayMut is Item::as_array_mut.
func (v *Item_Value) AsArrayMut() rs.Option[*Array] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsArrayMut)
}

// AsArrayMut is Item::as_array_mut.
func (v *Item_Table) AsArrayMut() rs.Option[*Array] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsArrayMut)
}

// AsArrayMut is Item::as_array_mut.
func (v *Item_ArrayOfTables) AsArrayMut() rs.Option[*Array] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsArrayMut)
}

// IsArray is Item::is_array.
func (v *Item_None) IsArray() bool { return v.AsArray().IsSome() }

// IsArray is Item::is_array.
func (v *Item_Value) IsArray() bool { return v.AsArray().IsSome() }

// IsArray is Item::is_array.
func (v *Item_Table) IsArray() bool { return v.AsArray().IsSome() }

// IsArray is Item::is_array.
func (v *Item_ArrayOfTables) IsArray() bool { return v.AsArray().IsSome() }

// AsInlineTable is Item::as_inline_table.
func (v *Item_None) AsInlineTable() rs.Option[*InlineTable] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsInlineTable)
}

// AsInlineTable is Item::as_inline_table.
func (v *Item_Value) AsInlineTable() rs.Option[*InlineTable] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsInlineTable)
}

// AsInlineTable is Item::as_inline_table.
func (v *Item_Table) AsInlineTable() rs.Option[*InlineTable] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsInlineTable)
}

// AsInlineTable is Item::as_inline_table.
func (v *Item_ArrayOfTables) AsInlineTable() rs.Option[*InlineTable] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsInlineTable)
}

// AsInlineTableMut is Item::as_inline_table_mut.
func (v *Item_None) AsInlineTableMut() rs.Option[*InlineTable] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsInlineTableMut)
}

// AsInlineTableMut is Item::as_inline_table_mut.
func (v *Item_Value) AsInlineTableMut() rs.Option[*InlineTable] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsInlineTableMut)
}

// AsInlineTableMut is Item::as_inline_table_mut.
func (v *Item_Table) AsInlineTableMut() rs.Option[*InlineTable] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsInlineTableMut)
}

// AsInlineTableMut is Item::as_inline_table_mut.
func (v *Item_ArrayOfTables) AsInlineTableMut() rs.Option[*InlineTable] {
	return rs.OptionAndThen(itemAsValue(v), Value.AsInlineTableMut)
}

// IsInlineTable is Item::is_inline_table.
func (v *Item_None) IsInlineTable() bool { return v.AsInlineTable().IsSome() }

// IsInlineTable is Item::is_inline_table.
func (v *Item_Value) IsInlineTable() bool { return v.AsInlineTable().IsSome() }

// IsInlineTable is Item::is_inline_table.
func (v *Item_Table) IsInlineTable() bool { return v.AsInlineTable().IsSome() }

// IsInlineTable is Item::is_inline_table.
func (v *Item_ArrayOfTables) IsInlineTable() bool { return v.AsInlineTable().IsSome() }

// AsTableLike is Item::as_table_like.
func (v *Item_None) AsTableLike() rs.Option[TableLike] { return itemAsTableLike(v) }

// AsTableLike is Item::as_table_like.
func (v *Item_Value) AsTableLike() rs.Option[TableLike] { return itemAsTableLike(v) }

// AsTableLike is Item::as_table_like.
func (v *Item_Table) AsTableLike() rs.Option[TableLike] { return itemAsTableLike(v) }

// AsTableLike is Item::as_table_like.
func (v *Item_ArrayOfTables) AsTableLike() rs.Option[TableLike] { return itemAsTableLike(v) }

// AsTableLikeMut is Item::as_table_like_mut.
func (v *Item_None) AsTableLikeMut() rs.Option[TableLike] { return itemAsTableLike(v) }

// AsTableLikeMut is Item::as_table_like_mut.
func (v *Item_Value) AsTableLikeMut() rs.Option[TableLike] { return itemAsTableLike(v) }

// AsTableLikeMut is Item::as_table_like_mut.
func (v *Item_Table) AsTableLikeMut() rs.Option[TableLike] { return itemAsTableLike(v) }

// AsTableLikeMut is Item::as_table_like_mut.
func (v *Item_ArrayOfTables) AsTableLikeMut() rs.Option[TableLike] { return itemAsTableLike(v) }

// IsTableLike is Item::is_table_like.
func (v *Item_None) IsTableLike() bool { return itemAsTableLike(v).IsSome() }

// IsTableLike is Item::is_table_like.
func (v *Item_Value) IsTableLike() bool { return itemAsTableLike(v).IsSome() }

// IsTableLike is Item::is_table_like.
func (v *Item_Table) IsTableLike() bool { return itemAsTableLike(v).IsSome() }

// IsTableLike is Item::is_table_like.
func (v *Item_ArrayOfTables) IsTableLike() bool { return itemAsTableLike(v).IsSome() }

// Index is Item::index.
func (v *Item_None) Index(index any) Item { return itemIndex(v, index) }

// Index is Item::index.
func (v *Item_Value) Index(index any) Item { return itemIndex(v, index) }

// Index is Item::index.
func (v *Item_Table) Index(index any) Item { return itemIndex(v, index) }

// Index is Item::index.
func (v *Item_ArrayOfTables) Index(index any) Item { return itemIndex(v, index) }

// Clone is Item::clone.
func (v *Item_None) Clone() Item { return itemClone(v) }

// Clone is Item::clone.
func (v *Item_Value) Clone() Item { return itemClone(v) }

// Clone is Item::clone.
func (v *Item_Table) Clone() Item { return itemClone(v) }

// Clone is Item::clone.
func (v *Item_ArrayOfTables) Clone() Item { return itemClone(v) }

// String is Item::string.
func (v *Item_None) String() string { return itemString(v) }

// String is Item::string.
func (v *Item_Value) String() string { return itemString(v) }

// String is Item::string.
func (v *Item_Table) String() string { return itemString(v) }

// String is Item::string.
func (v *Item_ArrayOfTables) String() string { return itemString(v) }

// FmtDisplay is Item::fmt_display.
func (v *Item_None) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(itemString(v)) }

// FmtDisplay is Item::fmt_display.
func (v *Item_Value) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(itemString(v)) }

// FmtDisplay is Item::fmt_display.
func (v *Item_Table) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(itemString(v)) }

// FmtDisplay is Item::fmt_display.
func (v *Item_ArrayOfTables) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(itemString(v)) }

func (v *Item_None) span() rs.Option[Range] { return itemSpan(v) }

func (v *Item_Value) span() rs.Option[Range] { return itemSpan(v) }

func (v *Item_Table) span() rs.Option[Range] { return itemSpan(v) }

func (v *Item_ArrayOfTables) span() rs.Option[Range] { return itemSpan(v) }

func (v *Item_None) despan(input string) { itemDespan(v, input) }

func (v *Item_Value) despan(input string) { itemDespan(v, input) }

func (v *Item_Table) despan(input string) { itemDespan(v, input) }

func (v *Item_ArrayOfTables) despan(input string) { itemDespan(v, input) }
