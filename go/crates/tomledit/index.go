// Port of toml_edit 0.20.2 src/index.rs

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

// copied from
// https://github.com/serde-rs/json/blob/master/src/value/index.rs

// toml_edit::index::Index is implemented for usize, str, String and
// references to them. Here an index is an `any` holding an int or a string;
// Rust rejects any other type when it compiles, and these functions panic.

func indexIndex(index any, val Item) rs.Option[Item] {
	switch i := index.(type) {
	case int:
		return usizeIndex(i, val)
	case string:
		return strIndex(i, val)
	default:
		panic(rs.Unreachable("toml_edit: Index is implemented for usize and strings only"))
	}
}

func indexIndexMut(index any, val *Item) rs.Option[*Item] {
	switch i := index.(type) {
	case int:
		return usizeIndexMut(i, val)
	case string:
		return strIndexMut(i, val)
	default:
		panic(rs.Unreachable("toml_edit: Index is implemented for usize and strings only"))
	}
}

func usizeIndex(self int, v Item) rs.Option[Item] {
	switch item := v.(type) {
	case *Item_ArrayOfTables:
		if self >= 0 && self < len(item.F0.values) {
			return rs.Some(item.F0.values[self])
		}
	case *Item_Value:
		if array, ok := item.F0.AsArray().Get(); ok {
			if self >= 0 && self < len(array.values) {
				return rs.Some(array.values[self])
			}
		}
	}
	return rs.None[Item]()
}

func usizeIndexMut(self int, v *Item) rs.Option[*Item] {
	switch item := (*v).(type) {
	case *Item_ArrayOfTables:
		if self >= 0 && self < len(item.F0.values) {
			return rs.Some(&item.F0.values[self])
		}
	case *Item_Value:
		if array, ok := item.F0.AsArrayMut().Get(); ok {
			if self >= 0 && self < len(array.values) {
				return rs.Some(&array.values[self])
			}
		}
	}
	return rs.None[*Item]()
}

func strIndex(self string, v Item) rs.Option[Item] {
	switch item := v.(type) {
	case *Item_Table:
		return item.F0.Get(self)
	case *Item_Value:
		if table, ok := item.F0.AsInlineTable().Get(); ok {
			if kv := table.items.get(self); kv != nil && !kv.value.IsNone() {
				return rs.Some(kv.value)
			}
		}
	}
	return rs.None[Item]()
}

func strIndexMut(self string, v *Item) rs.Option[*Item] {
	if (*v).IsNone() {
		t := InlineTableDefault()
		t.items.insert(self, tableKeyValueNew(KeyNew(self), &Item_None{}))
		*v = ItemValue(&Value_InlineTable{F0: t})
	}
	switch item := (*v).(type) {
	case *Item_Table:
		return rs.Some(item.F0.Entry(self).OrInsert(&Item_None{}))
	case *Item_Value:
		if table, ok := item.F0.AsInlineTableMut().Get(); ok {
			kv := table.items.get(self)
			if kv == nil {
				table.items.insert(self, tableKeyValueNew(KeyNew(self), &Item_None{}))
				kv = table.items.get(self)
			}
			return rs.Some(&kv.value)
		}
	}
	return rs.None[*Item]()
}

func itemIndex(self Item, index any) Item {
	return indexIndex(index, self).Expect("index not found")
}

// ItemIndexMut is `impl ops::IndexMut<I> for Item`: `&mut item[index]`, for
// a string or usize (int) index. A string index creates the key when it is
// missing, and turns a None item into an inline table holding it. It panics
// with "index not found" where ItemGetMut returns None.
func ItemIndexMut(self *Item, index any) *Item {
	return indexIndexMut(index, self).Expect("index not found")
}

// Index is `impl ops::Index<&str> for Table`: `&table[key]`. It panics with
// "index not found" when the key is missing.
func (t *Table) Index(key string) Item {
	return t.Get(key).Expect("index not found")
}

// IndexMut is `impl ops::IndexMut<&str> for Table`: `&mut table[key]`, which
// creates the key, holding None, when it is missing.
func (t *Table) IndexMut(key string) *Item {
	return t.Entry(key).OrInsert(&Item_None{})
}

// Index is `impl ops::Index<&str> for InlineTable`: `&table[key]`. It panics
// with "index not found" when the key is missing.
func (t *InlineTable) Index(key string) Value {
	return t.Get(key).Expect("index not found")
}

// IndexMut is `impl ops::IndexMut<&str> for InlineTable`: `&mut table[key]`.
// It panics with "index not found" when the key is missing.
func (t *InlineTable) IndexMut(key string) *Value {
	return t.GetMut(key).Expect("index not found")
}

// Index is `impl ops::Index<&str> for Document`: `&document[key]`. It panics
// with "index not found" when the key is missing.
func (d *Document) Index(key string) Item {
	return d.root.Index(key)
}

// IndexMut is `impl ops::IndexMut<&str> for Document`: `&mut document[key]`,
// which creates the key, holding None, when it is missing.
func (d *Document) IndexMut(key string) *Item {
	return ItemIndexMut(&d.root, key)
}
