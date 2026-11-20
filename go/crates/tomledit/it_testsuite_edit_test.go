// Port of toml_edit 0.20.2 tests/testsuite/edit.rs

package tomledit_test

import (
	"cmp"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// parseKeyEdit is the `parse_key!` macro.
func parseKeyEdit(t *testing.T, s string) tomledit.Key {
	t.Helper()
	key, err := tomledit.KeyFromStr(s)
	itAssert(t, err == nil, "key.is_ok()")
	return key
}

// asTable is the `as_table!` macro.
func asTable(t *testing.T, e *tomledit.Item) *tomledit.Table {
	t.Helper()
	itAssert(t, (*e).IsTable(), "e.is_table()")
	return (*e).AsTableMut().Unwrap()
}

// editTest is the `Test` of edit.rs: a document to run an edit on.
type editTest struct {
	t   *testing.T
	doc tomledit.Document
}

func given(t *testing.T, input string) *editTest {
	t.Helper()
	doc, err := tomledit.DocumentFromStr(input)
	itAssert(t, err == nil, "doc.is_ok()")
	return &editTest{t: t, doc: doc}
}

func (e *editTest) running(f func(root *tomledit.Table)) *editTest {
	root := e.doc.AsTableMut()
	f(root)
	return e
}

func (e *editTest) producesDisplay(expected string) *editTest {
	e.t.Helper()
	snapboxAssertEq(e.t, expected, e.doc.String())
	return e
}

// at is `item[k1][k2]..` in a place Rust writes to: IndexMut at each step.
func at(item *tomledit.Item, path ...any) *tomledit.Item {
	for _, index := range path {
		item = tomledit.ItemIndexMut(item, index)
	}
	return item
}

// insertion

func TestItTestsuiteEdit_test_insert_leaf_table(t *testing.T) {
	given(t,
		"[servers]\n\n        [servers.alpha]\n        ip = \"10.0.0.1\"\n        dc = \"eqdc10\"\n\n        [other.table]",
	).running(func(root *tomledit.Table) {
		*at(root.IndexMut("servers"), "beta") = tomledit.ItemTable()
		*at(root.IndexMut("servers"), "beta", "ip") = tomledit.ItemValue("10.0.0.2")
		*at(root.IndexMut("servers"), "beta", "dc") = tomledit.ItemValue("eqdc10")
	}).producesDisplay(
		"[servers]\n\n        [servers.alpha]\n        ip = \"10.0.0.1\"\n        dc = \"eqdc10\"\n\n[servers.beta]\nip = \"10.0.0.2\"\ndc = \"eqdc10\"\n\n        [other.table]\n",
	)
}

func TestItTestsuiteEdit_test_inserted_leaf_table_goes_after_last_sibling(t *testing.T) {
	given(t,
		"\n        [package]\n        [dependencies]\n        [[example]]\n        [dependencies.opencl]\n        [dev-dependencies]",
	).running(func(root *tomledit.Table) {
		*at(root.IndexMut("dependencies"), "newthing") = tomledit.ItemTable()
	}).producesDisplay(
		"\n        [package]\n        [dependencies]\n        [[example]]\n        [dependencies.opencl]\n\n[dependencies.newthing]\n        [dev-dependencies]\n",
	)
}

func TestItTestsuiteEdit_test_inserting_tables_from_different_parsed_docs(t *testing.T) {
	given(t, "[a]").
		running(func(root *tomledit.Table) {
			other := itParseDocument(t, "[b]")
			*root.IndexMut("b") = other.Index("b").Clone()
		}).
		producesDisplay("[a]\n[b]\n")
}

func TestItTestsuiteEdit_test_insert_nonleaf_table(t *testing.T) {
	given(t,
		"\n        [other.table]",
	).running(func(root *tomledit.Table) {
		*root.IndexMut("servers") = tomledit.ItemTable()
		*at(root.IndexMut("servers"), "alpha") = tomledit.ItemTable()
		*at(root.IndexMut("servers"), "alpha", "ip") = tomledit.ItemValue("10.0.0.1")
		*at(root.IndexMut("servers"), "alpha", "dc") = tomledit.ItemValue("eqdc10")
	}).producesDisplay(
		"\n        [other.table]\n\n[servers]\n\n[servers.alpha]\nip = \"10.0.0.1\"\ndc = \"eqdc10\"\n",
	)
}

func TestItTestsuiteEdit_test_insert_array(t *testing.T) {
	given(t,
		"\n        [package]\n        title = \"withoutarray\"",
	).running(func(root *tomledit.Table) {
		*root.IndexMut("bin") = tomledit.ItemArray()
		itAssert(t, root.Index("bin").IsArrayOfTables(), "root[\"bin\"].is_array_of_tables()")
		array := (*root.IndexMut("bin")).AsArrayOfTablesMut().Unwrap()
		{
			table := tomledit.TableNew()
			*table.IndexMut("hello") = tomledit.ItemValue("world")
			array.Push(table)
		}
		array.Push(tomledit.TableNew())
	}).producesDisplay(
		"\n        [package]\n        title = \"withoutarray\"\n\n[[bin]]\nhello = \"world\"\n\n[[bin]]\n",
	)
}

func TestItTestsuiteEdit_test_insert_values(t *testing.T) {
	given(t,
		"\n        [tbl.son]",
	).running(func(root *tomledit.Table) {
		*at(root.IndexMut("tbl"), "key1") = tomledit.ItemValue("value1")
		*at(root.IndexMut("tbl"), "key2") = tomledit.ItemValue(int64(42))
		*at(root.IndexMut("tbl"), "key3") = tomledit.ItemValue(8.1415926)
	}).producesDisplay(
		"[tbl]\nkey1 = \"value1\"\nkey2 = 42\nkey3 = 8.1415926\n\n        [tbl.son]\n",
	)
}

// removal

func TestItTestsuiteEdit_test_remove_leaf_table(t *testing.T) {
	given(t,
		"\n        [servers]\n\n        # Indentation (tabs and/or spaces) is allowed but not required\n[servers.alpha]\n        ip = \"10.0.0.1\"\n        dc = \"eqdc10\"\n\n        [servers.beta]\n        ip = \"10.0.0.2\"\n        dc = \"eqdc10\"",
	).running(func(root *tomledit.Table) {
		servers := root.GetMut("servers").Unwrap()
		table := asTable(t, servers)
		itAssert(t, table.Remove("alpha").IsSome(), "servers.remove(\"alpha\").is_some()")
	}).producesDisplay(
		"\n        [servers]\n\n        [servers.beta]\n        ip = \"10.0.0.2\"\n        dc = \"eqdc10\"\n",
	)
}

func TestItTestsuiteEdit_test_remove_nonleaf_table(t *testing.T) {
	given(t,
		"\n        title = \"not relevant\"\n\n        # comment 1\n        [a.b.c] # comment 1.1\n        key1 = 1 # comment 1.2\n        # comment 2\n        [b] # comment 2.1\n        key2 = 2 # comment 2.2\n\n        # comment 3\n        [a] # comment 3.1\n        key3 = 3 # comment 3.2\n        [[a.'array']]\n        b = 1\n\n        [[a.b.c.trololololololo]] # ohohohohoho\n        c = 2\n        key3 = 42\n\n           # comment on some other table\n           [some.other.table]\n\n\n\n\n        # comment 4\n        [a.b] # comment 4.1\n        key4 = 4 # comment 4.2\n        key41 = 41 # comment 4.3\n\n\n    ",
	).running(func(root *tomledit.Table) {
		itAssert(t, root.Remove("a").IsSome(), "root.remove(\"a\").is_some()")
	}).producesDisplay(
		"\n        title = \"not relevant\"\n        # comment 2\n        [b] # comment 2.1\n        key2 = 2 # comment 2.2\n\n           # comment on some other table\n           [some.other.table]\n\n\n    ",
	)
}

func TestItTestsuiteEdit_test_remove_array_entry(t *testing.T) {
	given(t,
		"\n        [package]\n        name = \"hello\"\n        version = \"1.0.0\"\n\n        [[bin]]\n        name = \"world\"\n        path = \"src/bin/world/main.rs\"\n\n        [dependencies]\n        nom = \"4.0\" # future is here\n\n        [[bin]]\n        name = \"delete me please\"\n        path = \"src/bin/dmp/main.rs\"",
	).running(func(root *tomledit.Table) {
		dmp := root.GetMut("bin").Unwrap()
		itAssert(t, (*dmp).IsArrayOfTables(), "dmp.is_array_of_tables()")
		array := (*dmp).AsArrayOfTablesMut().Unwrap()
		itAssertEq(t, array.Len(), 2)
		array.Remove(1)
		itAssertEq(t, array.Len(), 1)
	}).producesDisplay(
		"\n        [package]\n        name = \"hello\"\n        version = \"1.0.0\"\n\n        [[bin]]\n        name = \"world\"\n        path = \"src/bin/world/main.rs\"\n\n        [dependencies]\n        nom = \"4.0\" # future is here\n",
	)
}

func TestItTestsuiteEdit_test_remove_array(t *testing.T) {
	given(t,
		"\n        [package]\n        name = \"hello\"\n        version = \"1.0.0\"\n\n        [[bin]]\n        name = \"world\"\n        path = \"src/bin/world/main.rs\"\n\n        [dependencies]\n        nom = \"4.0\" # future is here\n\n        [[bin]]\n        name = \"delete me please\"\n        path = \"src/bin/dmp/main.rs\"",
	).running(func(root *tomledit.Table) {
		itAssert(t, root.Remove("bin").IsSome(), "root.remove(\"bin\").is_some()")
	}).producesDisplay(
		"\n        [package]\n        name = \"hello\"\n        version = \"1.0.0\"\n\n        [dependencies]\n        nom = \"4.0\" # future is here\n",
	)
}

func TestItTestsuiteEdit_test_remove_value(t *testing.T) {
	given(t,
		"\n        name = \"hello\"\n        # delete this\n        version = \"1.0.0\" # please\n        documentation = \"https://docs.rs/hello\"",
	).running(func(root *tomledit.Table) {
		removed := root.Remove("version")
		itAssert(t, removed.IsSome(), "value.is_some()")
		item := removed.Unwrap()
		itAssert(t, item.IsValue(), "value.is_value()")
		value := item.AsValue().Unwrap()
		itAssert(t, value.IsStr(), "value.is_str()")
		text := value.AsStr().Unwrap()
		snapboxAssertEq(t, text, "1.0.0")
	}).producesDisplay(
		"\n        name = \"hello\"\n        documentation = \"https://docs.rs/hello\"\n",
	)
}

func TestItTestsuiteEdit_test_remove_last_value_from_implicit(t *testing.T) {
	given(t,
		"\n        [a]\n        b = 1",
	).running(func(root *tomledit.Table) {
		a := root.GetMut("a").Unwrap()
		itAssert(t, (*a).IsTable(), "a.is_table()")
		table := asTable(t, a)
		table.SetImplicit(true)
		removed := table.Remove("b")
		itAssert(t, removed.IsSome(), "value.is_some()")
		item := removed.Unwrap()
		itAssert(t, item.IsValue(), "value.is_value()")
		value := item.AsValue().Unwrap()
		itAssertEq(t, value.AsInteger(), rs.Some[int64](1))
	}).producesDisplay("")
}

// values

func TestItTestsuiteEdit_test_sort_values(t *testing.T) {
	given(t,
		"\n        [a.z]\n\n        [a]\n        # this comment is attached to b\n        b = 2 # as well as this\n        a = 1\n        c = 3\n\n        [a.y]",
	).running(func(root *tomledit.Table) {
		a := root.GetMut("a").Unwrap()
		table := asTable(t, a)
		table.SortValues()
	}).producesDisplay(
		"\n        [a.z]\n\n        [a]\n        a = 1\n        # this comment is attached to b\n        b = 2 # as well as this\n        c = 3\n\n        [a.y]\n",
	)
}

func TestItTestsuiteEdit_test_sort_values_by(t *testing.T) {
	given(t,
		"\n        [a.z]\n\n        [a]\n        # this comment is attached to b\n        b = 2 # as well as this\n        a = 1\n        \"c\" = 3\n\n        [a.y]",
	).running(func(root *tomledit.Table) {
		a := root.GetMut("a").Unwrap()
		table := asTable(t, a)
		// Sort by the representation, not the value. So "\"c\"" sorts before "a" because '"' sorts
		// before 'a'.
		table.SortValuesBy(func(k1 *tomledit.Key, _ tomledit.Item, k2 *tomledit.Key, _ tomledit.Item) int {
			return cmp.Compare(k1.DisplayRepr(), k2.DisplayRepr())
		})
	}).producesDisplay(
		"\n        [a.z]\n\n        [a]\n        \"c\" = 3\n        a = 1\n        # this comment is attached to b\n        b = 2 # as well as this\n\n        [a.y]\n",
	)
}

func TestItTestsuiteEdit_test_set_position(t *testing.T) {
	given(t,
		"\n        [package]\n        [dependencies]\n        [dependencies.opencl]\n        [dev-dependencies]",
	).running(func(root *tomledit.Table) {
		for header, table := range root.IterMut() {
			if header.EqualStr("dependencies") {
				tab := asTable(t, table)
				tab.SetPosition(0)
				for _, segmented := range tab.IterMut() {
					asTable(t, segmented).SetPosition(5)
					break
				}
			}
		}
	}).producesDisplay(
		"        [dependencies]\n\n        [package]\n        [dev-dependencies]\n        [dependencies.opencl]\n",
	)
}

func TestItTestsuiteEdit_test_multiple_zero_positions(t *testing.T) {
	given(t,
		"\n        [package]\n        [dependencies]\n        [dependencies.opencl]\n        a=\"\"\n        [dev-dependencies]",
	).running(func(root *tomledit.Table) {
		for _, table := range root.IterMut() {
			asTable(t, table).SetPosition(0)
		}
	}).producesDisplay(
		"\n        [package]\n        [dependencies]\n        [dev-dependencies]\n        [dependencies.opencl]\n        a=\"\"\n",
	)
}

func TestItTestsuiteEdit_test_multiple_max_usize_positions(t *testing.T) {
	given(t,
		"\n        [package]\n        [dependencies]\n        [dependencies.opencl]\n        a=\"\"\n        [dev-dependencies]",
	).running(func(root *tomledit.Table) {
		for _, table := range root.IterMut() {
			asTable(t, table).SetPosition(rs.UsizeMax)
		}
	}).producesDisplay(
		"        [dependencies.opencl]\n        a=\"\"\n\n        [package]\n        [dependencies]\n        [dev-dependencies]\n",
	)
}

// asArray is the `as_array!` macro.
func asArray(t *testing.T, entry *tomledit.Item) *tomledit.Array {
	t.Helper()
	itAssert(t, (*entry).IsValue(), "entry.is_value()")
	a := (*entry).AsValueMut().Unwrap()
	itAssert(t, (*a).IsArray(), "a.is_array()")
	return (*a).AsArrayMut().Unwrap()
}

func TestItTestsuiteEdit_test_insert_replace_into_array(t *testing.T) {
	given(t,
		"\n        a = [1,2,3]\n        b = []",
	).running(func(root *tomledit.Table) {
		{
			entry := root.GetMut("a").Unwrap()
			a := asArray(t, entry)
			itAssertEq(t, a.Len(), 3)
			itAssert(t, a.Get(2).IsSome(), "a.get(2).is_some()")
			a.Push(int64(4))
			itAssertEq(t, a.Len(), 4)
			a.Fmt()
		}
		entry := root.GetMut("b").Unwrap()
		b := asArray(t, entry)
		itAssert(t, b.IsEmpty(), "b.is_empty()")
		b.Push("hello")
		itAssertEq(t, b.Len(), 1)

		b.PushFormatted(tomledit.ValueFromString("world").Decorated("\n", "\n"))
		b.PushFormatted(tomledit.ValueFromString("test").Decorated("", ""))

		b.Insert(1, "beep")
		b.InsertFormatted(2, tomledit.ValueFromString("boop").Decorated("   ", "   "))

		// This should preserve formatting.
		itAssertEq(t, b.Replace(2, "zoink").AsStr(), rs.Some("boop"))
		// This should replace formatting.
		itAssertEq(t,
			b.ReplaceFormatted(4, tomledit.ValueFromString("yikes").Decorated("  ", "")).
				AsStr(),
			rs.Some("test"),
		)
		t.Log(rs.DebugString(root))
	}).producesDisplay(
		"\n        a = [1, 2, 3, 4]\n        b = [\"hello\", \"beep\",   \"zoink\"   ,\n\"world\"\n,  \"yikes\"]\n",
	)
}

func TestItTestsuiteEdit_test_remove_from_array(t *testing.T) {
	given(t,
		"\n        a = [1, 2, 3, 4]\n        b = [\"hello\"]",
	).running(func(root *tomledit.Table) {
		{
			entry := root.GetMut("a").Unwrap()
			a := asArray(t, entry)
			itAssertEq(t, a.Len(), 4)
			itAssert(t, a.Remove(3).IsInteger(), "a.remove(3).is_integer()")
			itAssertEq(t, a.Len(), 3)
		}
		entry := root.GetMut("b").Unwrap()
		b := asArray(t, entry)
		itAssertEq(t, b.Len(), 1)
		itAssert(t, b.Remove(0).IsStr(), "b.remove(0).is_str()")
		itAssert(t, b.IsEmpty(), "b.is_empty()")
	}).producesDisplay(
		"\n        a = [1, 2, 3]\n        b = []\n",
	)
}

func TestItTestsuiteEdit_test_format_array(t *testing.T) {
	given(t,
		"\n    a = [\n      1,\n            \"2\",\n      3.0,\n    ]\n    ",
	).running(func(root *tomledit.Table) {
		for _, v := range root.IterMut() {
			if item, ok := (*v).(*tomledit.Item_Value); ok {
				if array, ok := item.F0.(*tomledit.Value_Array); ok {
					array.F0.Fmt()
				}
			}
		}
	}).producesDisplay(
		"\n    a = [1, \"2\", 3.0]\n    ",
	)
}

// asInlineTable is the `as_inline_table!` macro.
func asInlineTable(t *testing.T, entry *tomledit.Item) *tomledit.InlineTable {
	t.Helper()
	itAssert(t, (*entry).IsValue(), "entry.is_value()")
	a := (*entry).AsValueMut().Unwrap()
	itAssert(t, (*a).IsInlineTable(), "a.is_inline_table()")
	return (*a).AsInlineTableMut().Unwrap()
}

func TestItTestsuiteEdit_test_insert_into_inline_table(t *testing.T) {
	given(t,
		"\n        a = {a=2,  c = 3}\n        b = {}",
	).running(func(root *tomledit.Table) {
		{
			entry := root.GetMut("a").Unwrap()
			a := asInlineTable(t, entry)
			itAssertEq(t, a.Len(), 2)
			itAssert(t, a.ContainsKey("a") && a.Get("c").IsSome() && a.GetMut("c").IsSome(),
				"a.contains_key(\"a\") && a.get(\"c\").is_some() && a.get_mut(\"c\").is_some()")
			a.GetOrInsert("b", int64(42))
			itAssertEq(t, a.Len(), 3)
			a.Fmt()
		}
		entry := root.GetMut("b").Unwrap()
		b := asInlineTable(t, entry)
		itAssert(t, b.IsEmpty(), "b.is_empty()")
		b.GetOrInsert("hello", "world")
		itAssertEq(t, b.Len(), 1)
		b.Fmt()
	}).producesDisplay(
		"\n        a = { a = 2, c = 3, b = 42 }\n        b = { hello = \"world\" }\n",
	)
}

func TestItTestsuiteEdit_test_remove_from_inline_table(t *testing.T) {
	given(t,
		"\n        a = {a=2,  c = 3, b = 42}\n        b = {'hello' = \"world\"}",
	).running(func(root *tomledit.Table) {
		{
			entry := root.GetMut("a").Unwrap()
			a := asInlineTable(t, entry)
			itAssertEq(t, a.Len(), 3)
			itAssert(t, a.Remove("c").IsSome(), "a.remove(\"c\").is_some()")
			itAssertEq(t, a.Len(), 2)
		}
		entry := root.GetMut("b").Unwrap()
		b := asInlineTable(t, entry)
		itAssertEq(t, b.Len(), 1)
		itAssert(t, b.Remove("hello").IsSome(), "b.remove(\"hello\").is_some()")
		itAssert(t, b.IsEmpty(), "b.is_empty()")
	}).producesDisplay(
		"\n        a = {a=2, b = 42}\n        b = {}\n",
	)
}

func TestItTestsuiteEdit_test_as_table_like(t *testing.T) {
	given(t,
		"\n        a = {a=2,  c = 3, b = 42}\n        x = {}\n        [[bin]]\n        [b]\n        x = \"y\"\n        [empty]",
	).running(func(root *tomledit.Table) {
		count := func(table tomledit.TableLike) int {
			n := 0
			for range table.Iter() {
				n++
			}
			return n
		}

		a := root.Index("a").AsTableLike()
		itAssert(t, a.IsSome(), "a.is_some()")
		at := a.Unwrap()
		itAssertEq(t, count(at), 3)
		itAssertEq(t, at.Len(), 3)
		itAssertEq(t, rs.OptionAndThen(at.Get("a"), tomledit.Item.AsInteger), rs.Some[int64](2))

		b := root.Index("b").AsTableLike()
		itAssert(t, b.IsSome(), "b.is_some()")
		bt := b.Unwrap()
		itAssertEq(t, count(bt), 1)
		itAssertEq(t, bt.Len(), 1)
		itAssertEq(t, rs.OptionAndThen(bt.Get("x"), tomledit.Item.AsStr), rs.Some("y"))

		itAssertEq(t, rs.OptionMap(root.Index("x").AsTableLike(), count), rs.Some(0))
		itAssertEq(t,
			rs.OptionMap(root.Index("empty").AsTableLike(), tomledit.TableLike.IsEmpty),
			rs.Some(true),
		)

		itAssert(t, root.Index("bin").AsTableLike().IsNone(), "root[\"bin\"].as_table_like().is_none()")
	})
}

func TestItTestsuiteEdit_test_inline_table_append(t *testing.T) {
	a := tomledit.ValueFromPairs(
		rs.T2[any, any](parseKeyEdit(t, "a"), int64(1)),
		rs.T2[any, any](parseKeyEdit(t, "b"), int64(2)),
		rs.T2[any, any](parseKeyEdit(t, "c"), int64(3)),
	)
	aTable := a.AsInlineTableMut().Unwrap()

	b := tomledit.ValueFromPairs(
		rs.T2[any, any](parseKeyEdit(t, "c"), int64(4)),
		rs.T2[any, any](parseKeyEdit(t, "d"), int64(5)),
		rs.T2[any, any](parseKeyEdit(t, "e"), int64(6)),
	)
	bTable := b.AsInlineTableMut().Unwrap()

	var pairs []rs.Tuple2[any, any]
	for key, value := range bTable.Iter() {
		pairs = append(pairs, rs.T2[any, any](key, tomledit.ValueFromValue(value)))
	}
	aTable.Extend(pairs...)
	itAssertEq(t, aTable.Len(), 5)
	itAssert(t, aTable.ContainsKey("e"), "a.contains_key(\"e\")")
	itAssertEq(t, bTable.Len(), 3)
}

func TestItTestsuiteEdit_test_insert_dotted_into_std_table(t *testing.T) {
	given(t, "").
		running(func(root *tomledit.Table) {
			*root.IndexMut("nixpkgs") = tomledit.ItemTable()

			*at(root.IndexMut("nixpkgs"), "src") = tomledit.ItemTable()
			(*at(root.IndexMut("nixpkgs"), "src")).
				AsTableLikeMut().
				Unwrap().
				SetDotted(true)
			*at(root.IndexMut("nixpkgs"), "src", "git") = tomledit.ItemValue("https://github.com/nixos/nixpkgs")
		}).
		producesDisplay(
			"[nixpkgs]\nsrc.git = \"https://github.com/nixos/nixpkgs\"\n",
		)
}

func TestItTestsuiteEdit_test_insert_dotted_into_implicit_table(t *testing.T) {
	given(t, "").
		running(func(root *tomledit.Table) {
			*root.IndexMut("nixpkgs") = tomledit.ItemTable()

			*at(root.IndexMut("nixpkgs"), "src", "git") = tomledit.ItemValue("https://github.com/nixos/nixpkgs")
			(*at(root.IndexMut("nixpkgs"), "src")).
				AsTableLikeMut().
				Unwrap().
				SetDotted(true)
		}).
		producesDisplay(
			"[nixpkgs]\nsrc.git = \"https://github.com/nixos/nixpkgs\"\n",
		)
}
