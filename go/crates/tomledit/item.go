// Port of toml_edit 0.20.2 src/item.rs

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Item is toml_edit::Item: type representing either a value, a table, an
// array of tables, or none.
//
// It is a Rust enum, so an item is a pointer to one of the variant structs
// *Item_None, *Item_Value, *Item_Table and *Item_ArrayOfTables. The methods
// are the enum's; item_variants.go gives them to each variant.
//
// A table holds its items in slots, and the Rust methods that hand out
// `&mut Item` give a *Item here: assigning through it replaces the item in
// the table. The Rust methods that replace `self` with another variant
// (`or_insert`, `make_value`, `get_mut` and `index_mut`, which turn a None
// into an inline table) need that slot, so they are the functions
// ItemOrInsert, ItemMakeValue, ItemGetMut and ItemIndexMut.
type Item interface {
	isItem()

	// TypeName is Item::type_name: text description of value type.
	TypeName() string

	// Get is Item::get: index into a TOML array or map. A string index can
	// be used to access a value in a map, and a usize (int) index can be
	// used to access an element of an array.
	//
	// Returns `None` if:
	//   - The type of `self` does not match the type of the index, for
	//     example if the index is a string and `self` is an array or a number.
	//   - The given key does not exist in the map or the given index is not
	//     within the bounds of the array.
	Get(index any) rs.Option[Item]

	// AsValue is Item::as_value: casts `self` to value.
	AsValue() rs.Option[Value]
	// AsTable is Item::as_table: casts `self` to table.
	AsTable() rs.Option[*Table]
	// AsArrayOfTables is Item::as_array_of_tables: casts `self` to array of
	// tables.
	AsArrayOfTables() rs.Option[*ArrayOfTables]
	// AsValueMut is Item::as_value_mut: casts `self` to mutable value.
	AsValueMut() rs.Option[*Value]
	// AsTableMut is Item::as_table_mut: casts `self` to mutable table.
	AsTableMut() rs.Option[*Table]
	// AsArrayOfTablesMut is Item::as_array_of_tables_mut: casts `self` to
	// mutable array of tables.
	AsArrayOfTablesMut() rs.Option[*ArrayOfTables]
	// IntoValue is Item::into_value: casts `self` to value. The error is an
	// *ItemError holding the item that is not a value.
	IntoValue() (Value, error)
	// IntoTable is Item::into_table: casts `self` to table. The error is an
	// *ItemError holding the item that is not a table.
	IntoTable() (Table, error)
	// IntoArrayOfTables is Item::into_array_of_tables: casts `self` to array
	// of tables. The error is an *ItemError holding the item that is not one.
	IntoArrayOfTables() (ArrayOfTables, error)
	// IsValue is Item::is_value: returns true iff `self` is a value.
	IsValue() bool
	// IsTable is Item::is_table: returns true iff `self` is a table.
	IsTable() bool
	// IsArrayOfTables is Item::is_array_of_tables: returns true iff `self` is
	// an array of tables.
	IsArrayOfTables() bool
	// IsNone is Item::is_none: returns true iff `self` is `None`.
	IsNone() bool

	// Duplicate Value downcasting API

	// AsInteger is Item::as_integer: casts `self` to integer.
	AsInteger() rs.Option[int64]
	// IsInteger is Item::is_integer: returns true iff `self` is an integer.
	IsInteger() bool
	// AsFloat is Item::as_float: casts `self` to float.
	AsFloat() rs.Option[float64]
	// IsFloat is Item::is_float: returns true iff `self` is a float.
	IsFloat() bool
	// AsBool is Item::as_bool: casts `self` to boolean.
	AsBool() rs.Option[bool]
	// IsBool is Item::is_bool: returns true iff `self` is a boolean.
	IsBool() bool
	// AsStr is Item::as_str: casts `self` to str.
	AsStr() rs.Option[string]
	// IsStr is Item::is_str: returns true iff `self` is a string.
	IsStr() bool
	// AsDatetime is Item::as_datetime: casts `self` to date-time.
	AsDatetime() rs.Option[Datetime]
	// IsDatetime is Item::is_datetime: returns true iff `self` is a date-time.
	IsDatetime() bool
	// AsArray is Item::as_array: casts `self` to array.
	AsArray() rs.Option[*Array]
	// AsArrayMut is Item::as_array_mut: casts `self` to mutable array.
	AsArrayMut() rs.Option[*Array]
	// IsArray is Item::is_array: returns true iff `self` is an array.
	IsArray() bool
	// AsInlineTable is Item::as_inline_table: casts `self` to inline table.
	AsInlineTable() rs.Option[*InlineTable]
	// AsInlineTableMut is Item::as_inline_table_mut: casts `self` to mutable
	// inline table.
	AsInlineTableMut() rs.Option[*InlineTable]
	// IsInlineTable is Item::is_inline_table: returns true iff `self` is an
	// inline table.
	IsInlineTable() bool
	// AsTableLike is Item::as_table_like: casts `self` to either a table or
	// an inline table.
	AsTableLike() rs.Option[TableLike]
	// AsTableLikeMut is Item::as_table_like_mut: casts `self` to either a
	// table or an inline table.
	AsTableLikeMut() rs.Option[TableLike]
	// IsTableLike is Item::is_table_like: returns true iff `self` is either a
	// table, or an inline table.
	IsTableLike() bool

	// Index is `impl ops::Index<I> for Item`: the item at a string or usize
	// (int) index. It panics with "index not found" where Get returns None.
	Index(index any) Item

	// Clone is `impl Clone for Item`.
	Clone() Item
	// String is `impl Display for Item`.
	String() string
	// FmtDisplay is `impl Display for Item`.
	FmtDisplay(f *rs.Formatter) error

	// span is Item::span: the location within the original document.
	span() rs.Option[Range]
	despan(input string)
}

// Item_None is Item::None: type representing none.
type Item_None struct{}

// Item_Value is Item::Value: type representing value.
type Item_Value struct{ F0 Value }

// Item_Table is Item::Table: type representing table.
type Item_Table struct{ F0 Table }

// Item_ArrayOfTables is Item::ArrayOfTables: type representing array of
// tables.
type Item_ArrayOfTables struct{ F0 ArrayOfTables }

var _ = rs.RegisterEnum[Item]("Item",
	rs.Variant[Item_None]("None", rs.ShapeUnit),
	rs.Variant[Item_Value]("Value", rs.ShapeTuple),
	rs.Variant[Item_Table]("Table", rs.ShapeTuple),
	rs.Variant[Item_ArrayOfTables]("ArrayOfTables", rs.ShapeTuple),
)

// ItemError is the `Err(Self)` of Item::into_value, Item::into_table and
// Item::into_array_of_tables: the item, handed back because it is not of the
// kind that was asked for.
type ItemError struct {
	Item Item
}

// Error describes the item that could not be converted.
func (e *ItemError) Error() string {
	return "toml_edit: item of type " + e.Item.TypeName() + " cannot be converted"
}

// ItemOrInsert is Item::or_insert: sets `self` to the given item iff `self`
// is none and returns a mutable reference to `self`.
func ItemOrInsert(self *Item, item Item) *Item {
	if (*self).IsNone() {
		*self = item
	}
	return self
}

// TODO: This should be generated by macro or derive
/// Downcasting

func itemTypeName(i Item) string {
	switch item := i.(type) {
	case *Item_None:
		return "none"
	case *Item_Value:
		return item.F0.TypeName()
	case *Item_Table:
		return "table"
	case *Item_ArrayOfTables:
		return "array of tables"
	default:
		panic(rs.Unreachable(i))
	}
}

// ItemGetMut is Item::get_mut: mutably index into a TOML array or map. A
// string index can be used to access a value in a map, and a usize (int)
// index can be used to access an element of an array.
//
// Returns `None` if:
//   - The type of `self` does not match the type of the index, for example if
//     the index is a string and `self` is an array or a number.
//   - The given key does not exist in the map or the given index is not within
//     the bounds of the array.
func ItemGetMut(self *Item, index any) rs.Option[*Item] {
	return indexIndexMut(index, self)
}

func itemAsValue(i Item) rs.Option[Value] {
	if item, ok := i.(*Item_Value); ok {
		return rs.Some(item.F0)
	}
	return rs.None[Value]()
}

func itemAsTable(i Item) rs.Option[*Table] {
	if item, ok := i.(*Item_Table); ok {
		return rs.Some(&item.F0)
	}
	return rs.None[*Table]()
}

func itemAsArrayOfTables(i Item) rs.Option[*ArrayOfTables] {
	if item, ok := i.(*Item_ArrayOfTables); ok {
		return rs.Some(&item.F0)
	}
	return rs.None[*ArrayOfTables]()
}

func itemAsValueMut(i Item) rs.Option[*Value] {
	if item, ok := i.(*Item_Value); ok {
		return rs.Some(&item.F0)
	}
	return rs.None[*Value]()
}

func itemIntoValue(i Item) (Value, error) {
	switch item := i.(type) {
	case *Item_None:
		return nil, &ItemError{Item: i}
	case *Item_Value:
		return item.F0, nil
	case *Item_Table:
		v := item.F0.IntoInlineTable()
		return &Value_InlineTable{F0: v}, nil
	case *Item_ArrayOfTables:
		v := item.F0.IntoArray()
		return &Value_Array{F0: v}, nil
	default:
		panic(rs.Unreachable(i))
	}
}

// ItemMakeValue is Item::make_value: in-place convert to a value.
func ItemMakeValue(self *Item) {
	other := *self
	*self = &Item_None{}
	if value, err := other.IntoValue(); err == nil {
		*self = &Item_Value{F0: value}
	} else {
		*self = &Item_None{}
	}
}

func itemIntoTable(i Item) (Table, error) {
	switch item := i.(type) {
	case *Item_Table:
		return item.F0, nil
	case *Item_Value:
		if inline, ok := item.F0.(*Value_InlineTable); ok {
			return inline.F0.IntoTable(), nil
		}
	}
	return Table{}, &ItemError{Item: i}
}

func itemIntoArrayOfTables(i Item) (ArrayOfTables, error) {
	switch item := i.(type) {
	case *Item_ArrayOfTables:
		return item.F0, nil
	case *Item_Value:
		if array, ok := item.F0.(*Value_Array); ok {
			a := &array.F0
			if a.IsEmpty() {
				return ArrayOfTables{}, &ItemError{Item: i}
			}
			allInlineTables := true
			for v := range a.Iter() {
				if !v.IsInlineTable() {
					allInlineTables = false
					break
				}
			}
			if allInlineTables {
				aot := ArrayOfTablesNew()
				aot.values = a.values
				for index := range aot.values {
					itemMakeItem(&aot.values[index])
				}
				return aot, nil
			}
			return ArrayOfTables{}, &ItemError{Item: i}
		}
	}
	return ArrayOfTables{}, &ItemError{Item: i}
}

// Starting private because the name is unclear
func itemMakeItem(self *Item) {
	other := *self
	*self = &Item_None{}
	if table, err := other.IntoTable(); err == nil {
		other = &Item_Table{F0: table}
	} else {
		other = err.(*ItemError).Item
	}
	if array, err := other.IntoArrayOfTables(); err == nil {
		other = &Item_ArrayOfTables{F0: array}
	} else {
		other = err.(*ItemError).Item
	}
	*self = other
}

func itemAsTableLike(i Item) rs.Option[TableLike] {
	switch item := i.(type) {
	case *Item_Table:
		return rs.Some[TableLike](&item.F0)
	case *Item_Value:
		if inline, ok := item.F0.(*Value_InlineTable); ok {
			return rs.Some[TableLike](inline.F0.asTableLike())
		}
	}
	return rs.None[TableLike]()
}

func itemSpan(i Item) rs.Option[Range] {
	switch item := i.(type) {
	case *Item_None:
		return rs.None[Range]()
	case *Item_Value:
		return item.F0.span()
	case *Item_Table:
		return item.F0.span
	case *Item_ArrayOfTables:
		return item.F0.span
	default:
		panic(rs.Unreachable(i))
	}
}

func itemDespan(i Item, input string) {
	switch item := i.(type) {
	case *Item_None:
	case *Item_Value:
		item.F0.despan(input)
	case *Item_Table:
		item.F0.despan(input)
	case *Item_ArrayOfTables:
		item.F0.despan(input)
	default:
		panic(rs.Unreachable(i))
	}
}

func itemClone(i Item) Item {
	switch item := i.(type) {
	case *Item_None:
		return &Item_None{}
	case *Item_Value:
		return &Item_Value{F0: item.F0.Clone()}
	case *Item_Table:
		return &Item_Table{F0: item.F0.Clone()}
	case *Item_ArrayOfTables:
		return &Item_ArrayOfTables{F0: item.F0.Clone()}
	default:
		panic(rs.Unreachable(i))
	}
}

// ItemDefault is `impl Default for Item`: None.
func ItemDefault() Item {
	return &Item_None{}
}

// ItemFromStr is `impl FromStr for Item`: parses a value from a &str.
func ItemFromStr(s string) (Item, error) {
	value, err := ValueFromStr(s)
	if err != nil {
		return nil, err
	}
	return &Item_Value{F0: value}, nil
}

func itemString(i Item) string {
	switch item := i.(type) {
	case *Item_None:
		return ""
	case *Item_Value:
		return item.F0.String()
	case *Item_Table:
		return item.F0.String()
	case *Item_ArrayOfTables:
		return item.F0.String()
	default:
		panic(rs.Unreachable(i))
	}
}

// ItemValue is toml_edit::value: returns a formatted value.
//
// Since formatting is part of a `Value`, the right hand side of the
// assignment needs to be decorated with a space before the value.
// The `value` function does just that and converts its argument to an item,
// which may be a Value, a string, an int64, a float64, a bool, a Datetime, a
// Date, a Time, an Array or an InlineTable (`V: Into<Value>`).
//
// The function is named for its module (`toml_edit::item::value`), because
// tomledit.Value is the type.
//
//	let mut table = Table::default();
//	let mut array = Array::default();
//	array.push("hello");
//	array.push("\\, world"); // \ is only allowed in a literal string
//	table["key1"] = value("value1");
//	table["key2"] = value(42);
//	table["key3"] = value(array);
//	assert_eq!(table.to_string(),
//	r#"key1 = "value1"
//	key2 = 42
//	key3 = ["hello", '\, world']
//	"#);
func ItemValue(v any) Item {
	return &Item_Value{F0: IntoValue(v)}
}

// ItemTable is toml_edit::table: returns an empty table.
func ItemTable() Item {
	return &Item_Table{F0: TableNew()}
}

// ItemArray is toml_edit::array: returns an empty array of tables.
func ItemArray() Item {
	return &Item_ArrayOfTables{F0: ArrayOfTablesNew()}
}

func itemIsNone(i Item) bool {
	_, ok := i.(*Item_None)
	return ok
}
