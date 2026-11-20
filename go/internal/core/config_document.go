// Port of mj-core/src/config/document.rs.
//
// Writing a configuration into the user's existing file.
//
// A save edits the file in place instead of replacing it with a fresh
// serialization, so the comments, blank lines, and order the user gave it
// survive. Only settings whose value changed are touched.
//
// Rust edits a toml_edit::Document. BurntSushi/toml has no format-preserving
// document, so the Go port needs its own: tomlEditDocument and
// tomlEditTableLike stand for it here, and the bodies phase decides its
// implementation (the ExecPlan defers this choice to config saving).

package core

// firstInPlaceVersion is the first version whose file shape this build
// writes in place. Earlier files store fused runtime and host tables that a
// save has to convert, so they are rewritten whole.
const firstInPlaceVersion int64 = 12

// tomlEditDocument is a parsed TOML file that keeps its comments, spacing and
// order (toml_edit::Document).
type tomlEditDocument struct {
	root tomlEditTableLike
}

// tomlEditTableLike is a table of a tomlEditDocument, standard or inline
// (toml_edit::TableLike).
type tomlEditTableLike interface {
	// keys is the table's keys in file order.
	keys() []string
	get(key string) (tomlEditItem, bool)
	remove(key string)
	insert(key string, item tomlEditItem)
}

// tomlEditItem is one value, table or array of tables in a tomlEditDocument
// (toml_edit::Item).
type tomlEditItem interface {
	isTomlEditItem()
}

// editInPlace is the file existing with the changes from loaded to updated
// applied.
//
// loaded is the configuration as this build read existing, serialized;
// comparing against it rather than against the raw file means a setting the
// file leaves to its default is not written out just because this build
// serializes it. False means the file cannot be edited in place and should be
// written whole.
func editInPlace(existing, loaded, updated string) (string, bool) {
	panic("unported: mj-core/src/config/document.rs edit_in_place")
}

// dropLegacyDefaultImages removes the historical default `image` from every
// container target in document.
func dropLegacyDefaultImages(document *tomlEditDocument, legacy string) {
	panic("unported: mj-core/src/config/document.rs drop_legacy_default_images")
}

func existingVersion(document *tomlEditDocument) (int64, bool) {
	panic("unported: mj-core/src/config/document.rs existing_version")
}

// documentTables is one table three ways: as the file spells it, as this
// build read it, and as it is to be saved. Tables are decoded TOML
// (map[string]any); nil means absent.
type documentTables struct {
	raw    map[string]any
	before map[string]any
	after  map[string]any
}

// documentApply brings target from before to after, leaving every key whose
// value did not change exactly as the file wrote it.
//
// A key the save drops is removed when this build read it (the user cleared
// it), and at the top level whatever this build no longer writes, such as an
// obsolete section. A default the user wrote out stays.
func documentApply(target tomlEditTableLike, tables documentTables, source tomlEditTableLike, topLevel bool, nextPosition *int) {
	panic("unported: mj-core/src/config/document.rs apply")
}

// placedLast puts a new table after every table the file already has, in the
// order the serialization gives its parts.
func placedLast(item tomlEditItem, nextPosition *int) tomlEditItem {
	panic("unported: mj-core/src/config/document.rs placed_last")
}
