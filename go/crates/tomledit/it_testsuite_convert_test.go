// Port of toml_edit 0.20.2 tests/testsuite/convert.rs

package tomledit_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestItTestsuiteConvert_table_into_inline(t *testing.T) {
	toml := "\n[table]\nstring = \"value\"\narray = [1, 2, 3]\ninline = { \"1\" = 1, \"2\" = 2 }\n\n[table.child]\nother = \"world\"\n"
	doc := itParseDocument(t, toml)

	tomledit.ItemMakeValue(doc.AsTableMut().GetMut("table").Unwrap())

	actual := doc.String()
	// `table=` is because we didn't re-format the table key, only the value
	expected := "table= { string = \"value\", array = [1, 2, 3], inline = { \"1\" = 1, \"2\" = 2 }, child = { other = \"world\" } }\n"
	snapboxAssertEq(t, expected, actual)
}

func TestItTestsuiteConvert_inline_table_to_table(t *testing.T) {
	toml := "table = { string = \"value\", array = [1, 2, 3], inline = { \"1\" = 1, \"2\" = 2 }, child = { other = \"world\" } }\n"
	doc := itParseDocument(t, toml)

	removed := doc.AsTableMut().Remove("table").Unwrap()
	var inline tomledit.InlineTable
	if item, ok := removed.(*tomledit.Item_Value); ok {
		if table, ok := item.F0.(*tomledit.Value_InlineTable); ok {
			inline = table.F0
		} else {
			panic(rs.Unreachable("Unexpected {:?}", removed))
		}
	} else {
		panic(rs.Unreachable("Unexpected {:?}", removed))
	}
	table := inline.IntoTable()
	doc.AsTableMut().Insert("table", &tomledit.Item_Table{F0: table})

	actual := doc.String()
	expected := "[table]\nstring = \"value\"\narray = [1, 2, 3]\ninline = { \"1\" = 1, \"2\" = 2 }\nchild = { other = \"world\" }\n"
	snapboxAssertEq(t, expected, actual)
}

func TestItTestsuiteConvert_array_of_tables_to_array(t *testing.T) {
	toml := "\n[[table]]\nstring = \"value\"\narray = [1, 2, 3]\ninline = { \"1\" = 1, \"2\" = 2 }\n\n[table.child]\nother = \"world\"\n\n[[table]]\nstring = \"value\"\narray = [1, 2, 3]\ninline = { \"1\" = 1, \"2\" = 2 }\n\n[table.child]\nother = \"world\"\n"
	doc := itParseDocument(t, toml)

	tomledit.ItemMakeValue(doc.AsTableMut().GetMut("table").Unwrap())

	actual := doc.String()
	// `table=` is because we didn't re-format the table key, only the value
	expected := "table= [{ string = \"value\", array = [1, 2, 3], inline = { \"1\" = 1, \"2\" = 2 }, child = { other = \"world\" } }, { string = \"value\", array = [1, 2, 3], inline = { \"1\" = 1, \"2\" = 2 }, child = { other = \"world\" } }]\n"
	snapboxAssertEq(t, expected, actual)
}
