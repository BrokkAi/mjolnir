// Port of toml 0.8.2 src/fmt.rs

package toml

import "github.com/BrokkAi/mjolnir/go/crates/tomledit"

// documentFormatter is DocumentFormatter: the formatting toml's serializer
// applies to the document a value serializes to.
type documentFormatter struct {
	multilineArray bool
	isValue        bool
}

// documentFormatterDefault is the derived `Default for DocumentFormatter`.
func documentFormatterDefault() documentFormatter {
	return documentFormatter{multilineArray: false, isValue: false}
}

func (d *documentFormatter) VisitDocumentMut(node *tomledit.Document) {
	tomledit.VisitMutVisitDocumentMut(d, node)
}

func (d *documentFormatter) VisitItemMut(node *tomledit.Item) {
	isParentValue := d.isValue
	if !isParentValue {
		other := *node
		*node = &tomledit.Item_None{}
		if table, err := other.IntoTable(); err == nil {
			other = &tomledit.Item_Table{F0: table}
		} else {
			other = err.(*tomledit.ItemError).Item
		}
		if array, err := other.IntoArrayOfTables(); err == nil {
			other = &tomledit.Item_ArrayOfTables{F0: array}
		} else {
			other = err.(*tomledit.ItemError).Item
		}
		d.isValue = other.IsValue()
		*node = other
	}

	tomledit.VisitMutVisitItemMut(d, node)
	d.isValue = isParentValue
}

func (d *documentFormatter) VisitTableMut(node *tomledit.Table) {
	node.DecorMut().Clear()

	// Empty tables could be semantically meaningful, so make sure they are not implicit
	if !node.IsEmpty() {
		node.SetImplicit(true)
	}

	tomledit.VisitMutVisitTableMut(d, node)
}

func (d *documentFormatter) VisitValueMut(node *tomledit.Value) {
	(*node).DecorMut().Clear()

	tomledit.VisitMutVisitValueMut(d, node)
}

func (d *documentFormatter) VisitArrayMut(node *tomledit.Array) {
	tomledit.VisitMutVisitArrayMut(d, node)

	if !d.multilineArray || (0 <= node.Len() && node.Len() <= 1) {
		node.SetTrailing("")
		node.SetTrailingComma(false)
	} else {
		for item := range node.IterMut() {
			(*item).DecorMut().SetPrefix("\n    ")
		}
		node.SetTrailing("\n")
		node.SetTrailingComma(true)
	}
}

func (d *documentFormatter) VisitInlineTableMut(node *tomledit.InlineTable) {
	tomledit.VisitMutVisitInlineTableMut(d, node)
}

func (d *documentFormatter) VisitTableLikeMut(node tomledit.TableLike) {
	tomledit.VisitMutVisitTableLikeMut(d, node)
}

func (d *documentFormatter) VisitTableLikeKvMut(key tomledit.KeyMut, node *tomledit.Item) {
	tomledit.VisitMutVisitTableLikeKvMut(d, key, node)
}

func (d *documentFormatter) VisitArrayOfTablesMut(node *tomledit.ArrayOfTables) {
	tomledit.VisitMutVisitArrayOfTablesMut(d, node)
}

func (d *documentFormatter) VisitBooleanMut(node *tomledit.Formatted[bool]) {
	tomledit.VisitMutVisitBooleanMut(d, node)
}

func (d *documentFormatter) VisitDatetimeMut(node *tomledit.Formatted[tomledit.Datetime]) {
	tomledit.VisitMutVisitDatetimeMut(d, node)
}

func (d *documentFormatter) VisitFloatMut(node *tomledit.Formatted[float64]) {
	tomledit.VisitMutVisitFloatMut(d, node)
}

func (d *documentFormatter) VisitIntegerMut(node *tomledit.Formatted[int64]) {
	tomledit.VisitMutVisitIntegerMut(d, node)
}

func (d *documentFormatter) VisitStringMut(node *tomledit.Formatted[string]) {
	tomledit.VisitMutVisitStringMut(d, node)
}

var _ tomledit.VisitMut = (*documentFormatter)(nil)
