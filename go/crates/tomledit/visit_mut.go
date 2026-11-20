// Port of toml_edit 0.20.2 src/visit_mut.rs

package tomledit

import "github.com/BrokkAi/mjolnir/go/rs"

// Document tree traversal to mutate an exclusive borrow of a document tree in place.
//
// Each method of the [`VisitMut`] trait is a hook that can be overridden
// to customize the behavior when mutating the corresponding type of node.
// By default, every method recursively visits the substructure of the
// input by invoking the right visitor method of each of its fields.
//
//	pub trait VisitMut {
//	    /* ... */
//
//	    fn visit_item_mut(&mut self, i: &mut Item) {
//	        visit_item_mut(self, i);
//	    }
//
//	    /* ... */
//	}
//
//	pub fn visit_item_mut<V>(v: &mut V, node: &mut Item)
//	where
//	    V: VisitMut + ?Sized,
//	{
//	    match node {
//	        Item::None => {}
//	        Item::Value(value) => v.visit_value_mut(value),
//	        Item::Table(table) => v.visit_table_mut(table),
//	        Item::ArrayOfTables(array) => v.visit_array_of_tables_mut(array),
//	    }
//	}
//
// The API is modeled after [`syn::visit_mut`](https://docs.rs/syn/1/syn/visit_mut).
//
// A Go implementation writes every method; a method that keeps the trait's
// default calls the VisitMutVisit* function of the same name, which is the
// default's body.

// VisitMut is toml_edit::visit_mut::VisitMut: document tree traversal to
// mutate an exclusive borrow of a document tree in-place.
type VisitMut interface {
	VisitDocumentMut(node *Document)
	VisitItemMut(node *Item)
	VisitTableMut(node *Table)
	VisitInlineTableMut(node *InlineTable)
	// VisitTableLikeMut is reached from both VisitTableMut and
	// VisitInlineTableMut.
	VisitTableLikeMut(node TableLike)
	VisitTableLikeKvMut(key KeyMut, node *Item)
	VisitArrayMut(node *Array)
	VisitArrayOfTablesMut(node *ArrayOfTables)
	VisitValueMut(node *Value)
	VisitBooleanMut(node *Formatted[bool])
	VisitDatetimeMut(node *Formatted[Datetime])
	VisitFloatMut(node *Formatted[float64])
	VisitIntegerMut(node *Formatted[int64])
	VisitStringMut(node *Formatted[string])
}

// VisitMutVisitDocumentMut is visit_mut::visit_document_mut.
func VisitMutVisitDocumentMut(v VisitMut, node *Document) {
	v.VisitTableMut(node.AsTableMut())
}

// VisitMutVisitItemMut is visit_mut::visit_item_mut.
func VisitMutVisitItemMut(v VisitMut, node *Item) {
	switch item := (*node).(type) {
	case *Item_None:
	case *Item_Value:
		v.VisitValueMut(&item.F0)
	case *Item_Table:
		v.VisitTableMut(&item.F0)
	case *Item_ArrayOfTables:
		v.VisitArrayOfTablesMut(&item.F0)
	default:
		panic(rs.Unreachable(*node))
	}
}

// VisitMutVisitTableMut is visit_mut::visit_table_mut.
func VisitMutVisitTableMut(v VisitMut, node *Table) {
	v.VisitTableLikeMut(node)
}

// VisitMutVisitInlineTableMut is visit_mut::visit_inline_table_mut.
func VisitMutVisitInlineTableMut(v VisitMut, node *InlineTable) {
	v.VisitTableLikeMut(node.asTableLike())
}

// VisitMutVisitTableLikeMut is visit_mut::visit_table_like_mut.
func VisitMutVisitTableLikeMut(v VisitMut, node TableLike) {
	for key, item := range node.IterMut() {
		v.VisitTableLikeKvMut(key, item)
	}
}

// VisitMutVisitTableLikeKvMut is visit_mut::visit_table_like_kv_mut.
func VisitMutVisitTableLikeKvMut(v VisitMut, _ KeyMut, node *Item) {
	v.VisitItemMut(node)
}

// VisitMutVisitArrayMut is visit_mut::visit_array_mut.
func VisitMutVisitArrayMut(v VisitMut, node *Array) {
	for value := range node.IterMut() {
		v.VisitValueMut(value)
	}
}

// VisitMutVisitArrayOfTablesMut is visit_mut::visit_array_of_tables_mut.
func VisitMutVisitArrayOfTablesMut(v VisitMut, node *ArrayOfTables) {
	for table := range node.IterMut() {
		v.VisitTableMut(table)
	}
}

// VisitMutVisitValueMut is visit_mut::visit_value_mut.
func VisitMutVisitValueMut(v VisitMut, node *Value) {
	switch value := (*node).(type) {
	case *Value_String:
		v.VisitStringMut(&value.F0)
	case *Value_Integer:
		v.VisitIntegerMut(&value.F0)
	case *Value_Float:
		v.VisitFloatMut(&value.F0)
	case *Value_Boolean:
		v.VisitBooleanMut(&value.F0)
	case *Value_Datetime:
		v.VisitDatetimeMut(&value.F0)
	case *Value_Array:
		v.VisitArrayMut(&value.F0)
	case *Value_InlineTable:
		v.VisitInlineTableMut(&value.F0)
	default:
		panic(rs.Unreachable(*node))
	}
}

// VisitMutVisitBooleanMut is visit_mut::visit_boolean_mut, which does
// nothing.
func VisitMutVisitBooleanMut(VisitMut, *Formatted[bool]) {}

// VisitMutVisitDatetimeMut is visit_mut::visit_datetime_mut, which does
// nothing.
func VisitMutVisitDatetimeMut(VisitMut, *Formatted[Datetime]) {}

// VisitMutVisitFloatMut is visit_mut::visit_float_mut, which does nothing.
func VisitMutVisitFloatMut(VisitMut, *Formatted[float64]) {}

// VisitMutVisitIntegerMut is visit_mut::visit_integer_mut, which does
// nothing.
func VisitMutVisitIntegerMut(VisitMut, *Formatted[int64]) {}

// VisitMutVisitStringMut is visit_mut::visit_string_mut, which does nothing.
func VisitMutVisitStringMut(VisitMut, *Formatted[string]) {}
