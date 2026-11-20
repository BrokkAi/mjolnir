package toml

import (
	"encoding/json"
	"math/rand"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// Mjolnir saves a configuration by editing the user's file in place
// (mj-core/src/config/document.rs): it parses the file with toml_edit, reads
// three toml::Tables (the file, the configuration as this build read it,
// and the configuration to save), and brings the document from one to the
// other while leaving what did not change as the user wrote it. The oracle's
// `edit_in_place` is that algorithm without the settings that are particular
// to Mjolnir's schema; editInPlace below is its Go side. Both use toml and
// toml_edit the way the product does: Table parsing and equality, Document
// parsing, TableLike get/insert/remove, Item conversions, decor and table
// positions, and Display.

// editInPlace is the oracle's `edit_in_place`.
func editInPlace(existing, loaded, updated string) rs.Option[string] {
	document, err := tomledit.DocumentFromStr(existing)
	if err != nil {
		return rs.None[string]()
	}
	raw, err := TableFromStr(existing)
	if err != nil {
		return rs.None[string]()
	}
	loadedTable, err := TableFromStr(loaded)
	if err != nil {
		return rs.None[string]()
	}
	updatedTable, err := TableFromStr(updated)
	if err != nil {
		return rs.None[string]()
	}
	source, err := tomledit.DocumentFromStr(updated)
	if err != nil {
		return rs.None[string]()
	}
	nextPosition := rs.UsizeMax / 2
	editApply(
		document.AsTableMut(),
		editTables{raw: rs.Some(&raw), before: rs.Some(&loadedTable), after: &updatedTable},
		source.AsTable(),
		true,
		&nextPosition,
	)
	if version, ok := updatedTable.Get("version").Get(); ok {
		current := rs.OptionAndThen(document.AsTable().Get("version"), tomledit.Item.AsInteger)
		if !rs.Eq(current, version.AsInteger()) {
			if item, ok := document.AsTableMut().GetMut("version").Get(); ok {
				if value, ok := (*item).AsValueMut().Get(); ok {
					decor := (*value).Decor().Clone()
					*value = tomledit.ValueFromInt64(version.AsInteger().UnwrapOrDefault())
					*(*value).DecorMut() = decor
				}
			}
		}
	}
	return rs.Some(document.String())
}

type editTables struct {
	raw    rs.Option[*Table]
	before rs.Option[*Table]
	after  *Table
}

// editApply is the oracle's `apply`.
func editApply(target tomledit.TableLike, tables editTables, source tomledit.TableLike, topLevel bool, nextPosition *int) {
	raw, before, after := tables.raw, tables.before, tables.after
	var stale []string
	for key := range target.Iter() {
		if !after.ContainsKey(key) && (topLevel || before.IsSomeAnd(func(before *Table) bool { return before.ContainsKey(key) })) {
			stale = append(stale, key)
		}
	}
	for _, key := range stale {
		target.Remove(key)
	}
	for key, value := range after.Iter() {
		previous := rs.OptionAndThen(before, func(before *Table) rs.Option[Value] { return before.Get(key) })
		spelled := rs.OptionAndThen(raw, func(raw *Table) rs.Option[Value] { return raw.Get(key) })
		if rs.Eq(previous, rs.Some(value)) && spelled.IsNoneOr(func(spelled Value) bool { return rs.Eq(spelled, value) }) {
			continue
		}
		replacement, ok := source.Get(key).Get()
		if !ok {
			continue
		}
		if existing, ok := target.GetMut(key).Get(); ok {
			valueTable, isTable := value.(*Value_Table)
			table, hasTable := (*existing).AsTableLikeMut().Get()
			replacementTable, hasReplacementTable := replacement.AsTableLike().Get()
			if isTable && hasTable && hasReplacementTable {
				nested := editTables{
					raw:    rs.OptionAndThen(spelled, Value.AsTable),
					before: rs.OptionAndThen(previous, Value.AsTable),
					after:  &valueTable.F0,
				}
				editApply(table, nested, replacementTable, false, nextPosition)
				continue
			}
			if existingValue, ok := (*existing).AsValueMut().Get(); ok {
				if converted, err := replacement.Clone().IntoValue(); err == nil {
					*converted.DecorMut() = (*existingValue).Decor().Clone()
					*existingValue = converted
					continue
				}
			}
			*existing = placedLast(replacement.Clone(), nextPosition)
		} else {
			target.Insert(key, placedLast(replacement.Clone(), nextPosition))
		}
	}
}

// placedLast is the oracle's `placed_last`.
func placedLast(item tomledit.Item, nextPosition *int) tomledit.Item {
	place(&item, nextPosition)
	return item
}

// place is the oracle's `place`.
func place(item *tomledit.Item, nextPosition *int) {
	switch i := (*item).(type) {
	case *tomledit.Item_Table:
		i.F0.SetPosition(*nextPosition)
		*nextPosition++
		for _, child := range i.F0.IterMut() {
			place(child, nextPosition)
		}
	case *tomledit.Item_ArrayOfTables:
		for table := range i.F0.IterMut() {
			table.SetPosition(*nextPosition)
			*nextPosition++
			for _, child := range table.IterMut() {
				place(child, nextPosition)
			}
		}
	}
}

// editInPlaceDocuments are configuration files as people write them: with
// comments, blank lines, dotted keys, inline tables and arrays of tables.
var editInPlaceDocuments = []string{
	"# Mjolnir configuration\nversion = 14\n\n# The theme\ntheme = \"dark\" # inline comment\n\n" +
		"[daemon]\n# where it runs\nremote_command = \"mj\"\nport = 8080\n\n" +
		"[profiles.default]\nharness = \"claude\" # the harness\nmodel = \"opus\"\n\n" +
		"[[targets]]\nname = \"local\"\n\n[[targets]]\nname = \"remote\"\nhost = \"example.com\"\n",
	"version = 12\nname = 'x'\n\n[a]\nb = 1\nc = [1, 2, 3]\n\n[a.d]\ne = true\n\n# trailing comment\n",
	"version = 13 # schema\nlist = [\n  \"one\", # first\n  \"two\",\n]\nmap = { a = 1, b = \"two\" }\n\n[section]\nkey.dotted = 1\nkey.other = 2\n",
	"[z]\na = 1\n\n[y]\nb = 2\n\n[x.w]\nc = 3\n",
	"# only comments\n\n# and blank lines\n",
	"version = 14\n\n[[rules]]\npattern = \"gh pr\"\ndecision = \"allow\"\n\n[[rules]]\npattern = \"gh issue\"\ndecision = \"deny\"\n\n[features]\nmulti_agent = false\n",
	"version = 14\n[mcp_servers.mj_agents]\ncommand = \"mj\"\nargs = [\"mcp\", \"agents\"]\n[mcp_servers.mj_agents.env]\nMJ_SESSION = \"abc\"\n",
}

var editKeys = []string{"a", "b", "new", "theme", "port", "name", "extra", "z", "w", "x"}

// editMutate changes table the way a settings change does: a value changed,
// a key removed or added, a table or an array of tables added, each
// sometimes in a nested table.
func editMutate(random *rand.Rand, table *Table, depth int) {
	for range 1 + random.Intn(3) {
		keys := sortedKeys(table)
		switch random.Intn(7) {
		case 0, 1:
			if len(keys) > 0 {
				key := keys[random.Intn(len(keys))]
				current := table.Get(key).Unwrap()
				if nested, ok := current.(*Value_Table); ok && depth < 3 {
					editMutate(random, &nested.F0, depth+1)
				} else {
					table.Insert(key, randomValue(random, 0))
				}
			}
		case 2:
			if len(keys) > 0 {
				table.Remove(keys[random.Intn(len(keys))])
			}
		case 3:
			table.Insert(editKeys[random.Intn(len(editKeys))], randomValue(random, 0))
		case 4:
			nested := TableNew()
			nested.Insert(editKeys[random.Intn(len(editKeys))], randomValue(random, 0))
			table.Insert(editKeys[random.Intn(len(editKeys))], &Value_Table{F0: nested})
		case 5:
			element := TableNew()
			element.Insert("name", &Value_String{F0: valueStrings[random.Intn(len(valueStrings))]})
			if current, ok := table.Get("targets").Get(); ok {
				if array, ok := current.(*Value_Array); ok {
					array.F0 = append(array.F0, &Value_Table{F0: element})
					continue
				}
			}
			table.Insert("targets", &Value_Array{F0: []Value{&Value_Table{F0: element}}})
		default:
			if version, ok := table.Get("version").Get(); ok {
				if i, ok := version.AsInteger().Get(); ok {
					table.Insert("version", &Value_Integer{F0: i + 1})
				}
			}
		}
	}
}

func TestDiffAgainstRust_edit_in_place(t *testing.T) {
	oracle.Check(t, "toml", "edit_in_place", func(yield func(any)) {
		random := rand.New(rand.NewSource(14))
		for _, existing := range editInPlaceDocuments {
			for range 40 {
				raw, err := TableFromStr(existing)
				if err != nil {
					t.Fatalf("%q does not parse: %v", existing, err)
				}
				// What this build read: the file, maybe without a key it
				// leaves to its default.
				loadedTable := raw.Clone()
				if random.Intn(2) == 0 {
					keys := sortedKeys(&loadedTable)
					if len(keys) > 0 {
						loadedTable.Remove(keys[random.Intn(len(keys))])
					}
				}
				loaded, err := ToStringPretty(&loadedTable)
				if err != nil {
					t.Fatal(err)
				}
				updatedTable := loadedTable.Clone()
				editMutate(random, &updatedTable, 0)
				updated, err := ToStringPretty(&updatedTable)
				if err != nil {
					t.Fatal(err)
				}
				yield(map[string]any{"op": "edit_in_place", "existing": existing, "loaded": loaded, "updated": updated})
			}
		}
		// Inputs that do not parse leave nothing to edit.
		yield(map[string]any{"op": "edit_in_place", "existing": "a = ", "loaded": "", "updated": ""})
		yield(map[string]any{"op": "edit_in_place", "existing": "a = 1\n", "loaded": "a = 1\n", "updated": "a = [\n"})
	}, func(request json.RawMessage) any {
		r := decodeRequest(t, request)
		return oracle.Catch(func() any {
			if text, ok := editInPlace(r.Existing, r.Loaded, r.Updated).Get(); ok {
				return map[string]any{"ok": text}
			}
			return map[string]any{"none": true}
		})
	})
}
