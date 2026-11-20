// Port of toml_edit 0.20.2 src/array_of_tables.rs

package tomledit

import (
	"iter"
	"slices"
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// ArrayOfTables is toml_edit::ArrayOfTables: type for a TOML array of
// tables.
type ArrayOfTables struct {
	// Always Vec<Item::Table>, just `Item` to make `Index` work
	span   rs.Option[Range]
	values []Item
}

/// Constructors
///
/// See also `FromIterator`

// ArrayOfTablesNew is ArrayOfTables::new: creates an empty array of tables.
func ArrayOfTablesNew() ArrayOfTables {
	return ArrayOfTables{}
}

// ArrayOfTablesDefault is `impl Default for ArrayOfTables`.
func ArrayOfTablesDefault() ArrayOfTables {
	return ArrayOfTables{}
}

/// Formatting

// IntoArray is ArrayOfTables::into_array: convert to an inline array.
func (a *ArrayOfTables) IntoArray() Array {
	for index := range a.values {
		ItemMakeValue(&a.values[index])
	}
	array := arrayWithVec(a.values)
	array.Fmt()
	return array
}

func (a *ArrayOfTables) despan(input string) {
	a.span = rs.None[Range]()
	for index := range a.values {
		a.values[index].despan(input)
	}
}

// Iter is ArrayOfTables::iter: an iterator over tables.
func (a *ArrayOfTables) Iter() iter.Seq[*Table] {
	return func(yield func(*Table) bool) {
		for _, item := range a.values {
			if table, ok := item.(*Item_Table); ok {
				if !yield(&table.F0) {
					return
				}
			}
		}
	}
}

// IterMut is ArrayOfTables::iter_mut: an iterator over tables.
func (a *ArrayOfTables) IterMut() iter.Seq[*Table] {
	return a.Iter()
}

// Len is ArrayOfTables::len: the length of the underlying Vec. To get the
// actual number of items use `a.iter().count()`.
func (a *ArrayOfTables) Len() int {
	return len(a.values)
}

// IsEmpty is ArrayOfTables::is_empty: returns true iff `self.len() == 0`.
func (a *ArrayOfTables) IsEmpty() bool {
	return a.Len() == 0
}

// Clear is ArrayOfTables::clear: removes all the tables.
func (a *ArrayOfTables) Clear() {
	a.values = nil
}

// Get is ArrayOfTables::get: an optional reference to the table.
func (a *ArrayOfTables) Get(index int) rs.Option[*Table] {
	if index < 0 || index >= len(a.values) {
		return rs.None[*Table]()
	}
	return a.values[index].AsTable()
}

// GetMut is ArrayOfTables::get_mut: an optional mutable reference to the
// table.
func (a *ArrayOfTables) GetMut(index int) rs.Option[*Table] {
	if index < 0 || index >= len(a.values) {
		return rs.None[*Table]()
	}
	return a.values[index].AsTableMut()
}

// Push is ArrayOfTables::push: appends a table to the array.
func (a *ArrayOfTables) Push(table Table) {
	a.values = append(a.values, &Item_Table{F0: table})
}

// Remove is ArrayOfTables::remove: removes a table with the given index.
func (a *ArrayOfTables) Remove(index int) {
	if index < 0 || index >= len(a.values) {
		rs.PanicAt(1, "removal index (is "+strconv.Itoa(index)+") should be < len (is "+strconv.Itoa(len(a.values))+")")
	}
	a.values = slices.Delete(a.values, index, index+1)
}

// Retain is ArrayOfTables::retain: retains only the elements specified by
// the `keep` predicate.
//
// In other words, remove all tables for which `keep(&table)` returns `false`.
//
// This method operates in place, visiting each element exactly once in the
// original order, and preserves the order of the retained elements.
func (a *ArrayOfTables) Retain(keep func(*Table) bool) {
	a.values = slices.DeleteFunc(a.values, func(item Item) bool {
		if table, ok := item.(*Item_Table); ok {
			return !keep(&table.F0)
		}
		return true
	})
}

// Extend is `impl Extend<Table> for ArrayOfTables`.
func (a *ArrayOfTables) Extend(tables ...Table) {
	for _, table := range tables {
		a.Push(table)
	}
}

// ArrayOfTablesFromIter is `impl FromIterator<Table> for ArrayOfTables`.
func ArrayOfTablesFromIter(tables ...Table) ArrayOfTables {
	values := make([]Item, 0, len(tables))
	for _, table := range tables {
		values = append(values, &Item_Table{F0: table})
	}
	return ArrayOfTables{values: values}
}

// IntoIter is `impl IntoIterator for ArrayOfTables`: the tables of the
// array.
func (a *ArrayOfTables) IntoIter() iter.Seq[Table] {
	return func(yield func(Table) bool) {
		for _, item := range a.values {
			if table, ok := item.(*Item_Table); ok {
				if !yield(table.F0) {
					return
				}
			}
		}
	}
}

// Clone is the derived `Clone for ArrayOfTables`.
func (a *ArrayOfTables) Clone() ArrayOfTables {
	cloned := *a
	cloned.values = make([]Item, len(a.values))
	for index, item := range a.values {
		cloned.values[index] = item.Clone()
	}
	return cloned
}

// String is `impl Display for ArrayOfTables`.
func (a *ArrayOfTables) String() string {
	// HACK: Without the header, we don't really have a proper way of printing this
	cloned := a.Clone()
	array := cloned.IntoArray()
	return array.String()
}

// FmtDisplay is `impl Display for ArrayOfTables`.
func (a *ArrayOfTables) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(a.String()) }

// FmtDebug is the derived `Debug for ArrayOfTables`.
func (a *ArrayOfTables) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("ArrayOfTables").Field("span", a.span).Field("values", a.values).Finish()
}
