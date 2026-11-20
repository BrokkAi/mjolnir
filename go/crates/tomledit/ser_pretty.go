// Port of toml_edit 0.20.2 src/ser/pretty.rs

package tomledit

// serPretty is Pretty: the formatting to_string_pretty applies to the
// document a value serializes to.
type serPretty struct{}

func (p *serPretty) VisitDocumentMut(node *Document) {
	VisitMutVisitDocumentMut(p, node)
}

func (p *serPretty) VisitItemMut(node *Item) {
	itemMakeItem(node)

	VisitMutVisitItemMut(p, node)
}

func (p *serPretty) VisitTableMut(node *Table) {
	node.DecorMut().Clear()

	// Empty tables could be semantically meaningful, so make sure they are not implicit
	if !node.IsEmpty() {
		node.SetImplicit(true)
	}

	VisitMutVisitTableMut(p, node)
}

func (p *serPretty) VisitValueMut(node *Value) {
	(*node).DecorMut().Clear()

	VisitMutVisitValueMut(p, node)
}

func (p *serPretty) VisitArrayMut(node *Array) {
	VisitMutVisitArrayMut(p, node)

	if 0 <= node.Len() && node.Len() <= 1 {
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

func (p *serPretty) VisitInlineTableMut(node *InlineTable) {
	VisitMutVisitInlineTableMut(p, node)
}

func (p *serPretty) VisitTableLikeMut(node TableLike) {
	VisitMutVisitTableLikeMut(p, node)
}

func (p *serPretty) VisitTableLikeKvMut(key KeyMut, node *Item) {
	VisitMutVisitTableLikeKvMut(p, key, node)
}

func (p *serPretty) VisitArrayOfTablesMut(node *ArrayOfTables) {
	VisitMutVisitArrayOfTablesMut(p, node)
}

func (p *serPretty) VisitBooleanMut(node *Formatted[bool]) { VisitMutVisitBooleanMut(p, node) }

func (p *serPretty) VisitDatetimeMut(node *Formatted[Datetime]) { VisitMutVisitDatetimeMut(p, node) }

func (p *serPretty) VisitFloatMut(node *Formatted[float64]) { VisitMutVisitFloatMut(p, node) }

func (p *serPretty) VisitIntegerMut(node *Formatted[int64]) { VisitMutVisitIntegerMut(p, node) }

func (p *serPretty) VisitStringMut(node *Formatted[string]) { VisitMutVisitStringMut(p, node) }

var _ VisitMut = (*serPretty)(nil)
