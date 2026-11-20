// Port of mj-core/src/config/document.rs.
//
// Writing a configuration into the user's existing file.
//
// A save edits the file in place instead of replacing it with a fresh
// serialization, so the comments, blank lines, and order the user gave it
// survive. Only settings whose value changed are touched.
//
// Rust edits a toml_edit 0.20 Document. Go has no format-preserving TOML
// document, so this file carries the part of one the edit needs: a parser
// that keeps every byte of the file as the decoration of the key, value or
// table it belongs to (tomlEditDocument), the table operations the edit uses,
// and toml_edit's renderer. Values are compared through their decoded form
// (BurntSushi/toml), as Rust compares toml::Table values.

package core

import (
	"errors"
	"fmt"
	"math"
	"reflect"
	"sort"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/BurntSushi/toml"
)

// firstInPlaceVersion is the first version whose file shape this build
// writes in place. Earlier files store fused runtime and host tables that a
// save has to convert, so they are rewritten whole.
const firstInPlaceVersion int64 = 12

// editInPlace is the file existing with the changes from loaded to updated
// applied.
//
// loaded is the configuration as this build read existing, serialized;
// comparing against it rather than against the raw file means a setting the
// file leaves to its default is not written out just because this build
// serializes it. False means the file cannot be edited in place and should be
// written whole.
func editInPlace(existing, loaded, updated string) (string, bool) {
	document, err := parseTomlEditDocument(existing)
	if err != nil {
		return "", false
	}
	version, ok := existingVersion(document)
	if !ok || version < firstInPlaceVersion {
		return "", false
	}
	raw, err := decodeTomlComparable(existing)
	if err != nil {
		return "", false
	}
	before, err := decodeTomlComparable(loaded)
	if err != nil {
		return "", false
	}
	// The canonical load already folded this veto into machine settings.
	// Compare machines against the actual file so an otherwise unchanged save
	// writes those opt-outs before removing the obsolete global section.
	if enabled, ok := tomlLookup(raw, "build_cache", "enabled"); ok && enabled == false {
		if machines, ok := raw["machines"]; ok {
			before["machines"] = machines
		} else {
			delete(before, "machines")
		}
	}
	// The legacy veto was folded during loading. Compare GitHub against the
	// actual file so saving writes the moved switch before removing it.
	if enabled, ok := tomlLookup(raw, "mailbox", "enabled"); ok && enabled == false {
		if _, watched := tomlLookup(raw, "github", "watch", "enabled"); !watched {
			if github, ok := raw["github"]; ok {
				before["github"] = github
			} else {
				delete(before, "github")
			}
		}
	}
	// Compare review against the file so saving replaces the legacy boolean
	// and writes the trigger even when its effective value has not changed.
	if review, ok := raw["review"]; ok {
		before["review"] = review
	}
	// Loading dropped every profile for a removed harness. Compare those
	// against the file so saving removes them.
	if rawProfiles, ok := raw["profiles"].(map[string]any); ok {
		loadedProfiles, isTable := before["profiles"].(map[string]any)
		if _, present := before["profiles"]; !present {
			loadedProfiles, isTable = map[string]any{}, true
			before["profiles"] = loadedProfiles
		}
		if isTable {
			for _, id := range sortedKeys(rawProfiles) {
				if isRemovedHarnessProfile(rawProfiles[id]) {
					loadedProfiles[id] = rawProfiles[id]
				}
			}
		}
	}
	after, err := decodeTomlComparable(updated)
	if err != nil {
		return "", false
	}
	source, err := parseTomlEditDocument(updated)
	if err != nil {
		return "", false
	}
	nextPosition := math.MaxInt / 2
	documentApply(document.root, documentTables{raw: raw, before: before, after: after}, source.root, true, &nextPosition)
	// The version is not a setting and is equal in both serializations, so it
	// is written here: the file now holds what this build writes.
	if updatedVersion, ok := after["version"]; ok {
		wanted, isInteger := updatedVersion.(int64)
		current, hasCurrent := existingVersion(document)
		if hasCurrent != isInteger || current != wanted {
			if entry := document.root.items.get("version"); entry != nil {
				if value, ok := entry.item.(*tomlEditValue); ok {
					value.repr = strconv.FormatInt(wanted, 10)
					value.inline = nil
					value.kind = tomlEditInteger
				}
			}
		}
	}
	// Up to version 14 a container target the user had touched carried the
	// then-current default image in the file. Drop that literal now, so an
	// upgraded build follows the image built for its own version. The diff
	// above cannot remove it on its own: a value equal to the default is
	// absent from both serializations it compares, so the key never looks
	// stale. Only the first save of such a file is affected; once the written
	// version is current, an explicitly chosen image is left alone.
	if version <= legacyDefaultImageLastVersion {
		dropLegacyDefaultImages(document, LegacyDefaultContainerImage)
	}
	// These keys are accepted only for legacy reads and must not survive the
	// comment-preserving path merely because they are absent from both
	// projections.
	if subagents := tomlEditAsTableLike(document.root.get("subagents")); subagents != nil {
		subagents.remove("enabled")
	}
	if profiles := tomlEditAsTableLike(document.root.get("profiles")); profiles != nil {
		for _, id := range profiles.keys() {
			if profile := tomlEditAsTableLike(profiles.get(id)); profile != nil {
				profile.remove("subagents")
			}
		}
	}
	return document.String(), true
}

// dropLegacyDefaultImages removes the historical default `image` from every
// container target in document.
func dropLegacyDefaultImages(document *tomlEditDocument, legacy string) {
	targets := tomlEditAsTableLike(document.root.get("targets"))
	if targets == nil {
		return
	}
	for _, id := range targets.keys() {
		target := tomlEditAsTableLike(targets.get(id))
		if target == nil {
			continue
		}
		if image, ok := tomlEditAsString(target.get("image")); ok && image == legacy {
			target.remove("image")
		}
	}
}

func existingVersion(document *tomlEditDocument) (int64, bool) {
	entry := document.root.items.get("version")
	if entry == nil {
		return 0, false
	}
	value, ok := entry.item.(*tomlEditValue)
	if !ok || value.kind != tomlEditInteger {
		return 0, false
	}
	decoded, ok := value.decoded()
	if !ok {
		return 0, false
	}
	integer, ok := decoded.(int64)
	return integer, ok
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
	raw, before, after := tables.raw, tables.before, tables.after
	var stale []string
	for _, key := range target.keys() {
		if _, kept := after[key]; kept {
			continue
		}
		if _, read := before[key]; topLevel || before != nil && read {
			stale = append(stale, key)
		}
	}
	for _, key := range stale {
		target.remove(key)
	}
	for _, key := range sortedKeys(after) {
		value := after[key]
		previous, hadPrevious := before[key]
		spelled, wasSpelled := raw[key]
		// Unchanged, and either spelled as this build writes it or left to its
		// default. A value the file spells another way, such as a renamed
		// choice, is rewritten in the current spelling.
		if hadPrevious && reflect.DeepEqual(previous, value) && (!wasSpelled || reflect.DeepEqual(spelled, value)) {
			continue
		}
		replacement := source.get(key)
		if replacement == nil {
			continue
		}
		existing := target.get(key)
		if existing == nil {
			target.insert(key, placedLast(cloneTomlEditItem(replacement), nextPosition))
			continue
		}
		valueTable, valueIsTable := value.(map[string]any)
		table := tomlEditAsTableLike(existing)
		sourceTable := tomlEditAsTableLike(replacement)
		if valueIsTable && table != nil && sourceTable != nil {
			spelledTable, _ := spelled.(map[string]any)
			previousTable, _ := previous.(map[string]any)
			documentApply(table, documentTables{raw: spelledTable, before: previousTable, after: valueTable}, sourceTable, false, nextPosition)
			continue
		}
		if current, ok := existing.(*tomlEditValue); ok {
			// Keep the comment beside the value and its spacing.
			converted := tomlEditIntoValue(cloneTomlEditItem(replacement))
			converted.decor = current.decor
			target.replace(key, converted)
			continue
		}
		target.replace(key, placedLast(cloneTomlEditItem(replacement), nextPosition))
	}
}

// placedLast puts a new table after every table the file already has, in the
// order the serialization gives its parts.
func placedLast(item tomlEditItem, nextPosition *int) tomlEditItem {
	var place func(item tomlEditItem)
	placeTable := func(table *tomlEditTable) {
		position := *nextPosition
		table.position = &position
		*nextPosition++
		for _, key := range table.items.order {
			place(table.items.byKey[key].item)
		}
	}
	place = func(item tomlEditItem) {
		switch item := item.(type) {
		case *tomlEditTable:
			placeTable(item)
		case *tomlEditArrayOfTables:
			for _, table := range item.tables {
				placeTable(table)
			}
		}
	}
	place(item)
	return item
}

// decodeTomlComparable decodes a TOML document for comparison, with every
// array in one Go shape so values Rust's toml::Value calls equal are
// reflect.DeepEqual.
func decodeTomlComparable(text string) (map[string]any, error) {
	var document map[string]any
	if _, err := toml.Decode(text, &document); err != nil {
		return nil, err
	}
	if document == nil {
		return map[string]any{}, nil
	}
	return tomlComparable(document).(map[string]any), nil
}

func tomlComparable(value any) any {
	switch value := value.(type) {
	case map[string]any:
		for key, child := range value {
			value[key] = tomlComparable(child)
		}
		return value
	case []map[string]any:
		array := make([]any, len(value))
		for i, child := range value {
			array[i] = tomlComparable(child)
		}
		return array
	case []any:
		for i, child := range value {
			value[i] = tomlComparable(child)
		}
		return value
	}
	return value
}

// tomlLookup follows keys through nested decoded tables.
func tomlLookup(table map[string]any, keys ...string) (any, bool) {
	var value any = table
	for _, key := range keys {
		current, ok := value.(map[string]any)
		if !ok {
			return nil, false
		}
		value, ok = current[key]
		if !ok {
			return nil, false
		}
	}
	return value, true
}

// ---------------------------------------------------------------------------
// The document model (toml_edit's Document, Table, InlineTable, Item, Key).

// tomlEditDocument is a parsed TOML file that keeps its comments, spacing and
// order (toml_edit::Document).
type tomlEditDocument struct {
	root     *tomlEditTable
	trailing string
}

// tomlEditTableLike is a table of a tomlEditDocument, standard or inline
// (toml_edit::TableLike).
type tomlEditTableLike interface {
	// keys is the table's keys in file order.
	keys() []string
	get(key string) tomlEditItem
	remove(key string)
	// insert adds or replaces key with a fresh key and item.
	insert(key string, item tomlEditItem)
	// replace sets the item of an existing key, keeping the key as written.
	replace(key string, item tomlEditItem)
}

// tomlEditItem is one value, table or array of tables in a tomlEditDocument
// (toml_edit::Item).
type tomlEditItem interface {
	isTomlEditItem()
}

// tomlEditDecor is the text around a key, value or table header. Nil means
// none was written, and the renderer supplies its default.
type tomlEditDecor struct {
	prefix *string
	suffix *string
}

func (d tomlEditDecor) prefixOr(fallback string) string {
	if d.prefix != nil {
		return *d.prefix
	}
	return fallback
}

func (d tomlEditDecor) suffixOr(fallback string) string {
	if d.suffix != nil {
		return *d.suffix
	}
	return fallback
}

type tomlEditKey struct {
	name string
	// repr is the key as written; nil renders the default spelling.
	repr  *string
	decor tomlEditDecor
}

func newTomlEditKey(name string) tomlEditKey { return tomlEditKey{name: name} }

func (k tomlEditKey) display() string {
	if k.repr != nil {
		return *k.repr
	}
	if k.name != "" && strings.IndexFunc(k.name, func(r rune) bool { return !isBareKeyRune(r) }) < 0 {
		return k.name
	}
	return tomlBasicString(k.name, false)
}

type tomlEditValueKind int

const (
	tomlEditOther tomlEditValueKind = iota
	tomlEditString
	tomlEditInteger
	tomlEditArray
	tomlEditInline
)

// tomlEditValue is a value with its decoration. Scalars and arrays keep the
// text they were written as; an inline table keeps its entries, because a
// save edits inside it.
type tomlEditValue struct {
	kind   tomlEditValueKind
	repr   string
	inline *tomlEditInlineTable
	decor  tomlEditDecor
}

// decoded is the value as TOML reads it.
func (v *tomlEditValue) decoded() (any, bool) {
	if v.inline != nil {
		return nil, false
	}
	var holder map[string]any
	if _, err := toml.Decode("v = "+v.repr, &holder); err != nil {
		return nil, false
	}
	value, ok := holder["v"]
	return value, ok
}

type tomlEditInlineTable struct {
	preamble string
	items    tomlEditEntries
	dotted   bool
}

type tomlEditTable struct {
	items    tomlEditEntries
	decor    tomlEditDecor
	position *int
	implicit bool
	dotted   bool
}

type tomlEditArrayOfTables struct {
	tables []*tomlEditTable
}

func (*tomlEditValue) isTomlEditItem()         {}
func (*tomlEditTable) isTomlEditItem()         {}
func (*tomlEditArrayOfTables) isTomlEditItem() {}

type tomlEditEntry struct {
	key  tomlEditKey
	item tomlEditItem
}

// tomlEditEntries is an insertion-ordered map, as toml_edit's IndexMap.
type tomlEditEntries struct {
	order []string
	byKey map[string]*tomlEditEntry
}

func (e *tomlEditEntries) get(name string) *tomlEditEntry { return e.byKey[name] }

func (e *tomlEditEntries) set(entry *tomlEditEntry) {
	if e.byKey == nil {
		e.byKey = map[string]*tomlEditEntry{}
	}
	if _, exists := e.byKey[entry.key.name]; !exists {
		e.order = append(e.order, entry.key.name)
	}
	e.byKey[entry.key.name] = entry
}

func (e *tomlEditEntries) remove(name string) *tomlEditEntry {
	entry, ok := e.byKey[name]
	if !ok {
		return nil
	}
	delete(e.byKey, name)
	for i, key := range e.order {
		if key == name {
			e.order = append(e.order[:i:i], e.order[i+1:]...)
			break
		}
	}
	return entry
}

func (e *tomlEditEntries) keys() []string { return append([]string(nil), e.order...) }

func (t *tomlEditTable) keys() []string { return t.items.keys() }

func (t *tomlEditTable) get(key string) tomlEditItem {
	if entry := t.items.get(key); entry != nil {
		return entry.item
	}
	return nil
}

func (t *tomlEditTable) remove(key string) { t.items.remove(key) }

func (t *tomlEditTable) insert(key string, item tomlEditItem) {
	t.items.set(&tomlEditEntry{key: newTomlEditKey(key), item: item})
}

func (t *tomlEditTable) replace(key string, item tomlEditItem) {
	if entry := t.items.get(key); entry != nil {
		entry.item = item
	}
}

func (t *tomlEditInlineTable) keys() []string { return t.items.keys() }

func (t *tomlEditInlineTable) get(key string) tomlEditItem {
	if entry := t.items.get(key); entry != nil {
		return entry.item
	}
	return nil
}

func (t *tomlEditInlineTable) remove(key string) { t.items.remove(key) }

func (t *tomlEditInlineTable) insert(key string, item tomlEditItem) {
	t.items.set(&tomlEditEntry{key: newTomlEditKey(key), item: tomlEditIntoValue(item)})
}

func (t *tomlEditInlineTable) replace(key string, item tomlEditItem) {
	if entry := t.items.get(key); entry != nil {
		entry.item = tomlEditIntoValue(item)
	}
}

// tomlEditAsTableLike is Item::as_table_like: a standard table, or a value
// holding an inline table.
func tomlEditAsTableLike(item tomlEditItem) tomlEditTableLike {
	switch item := item.(type) {
	case *tomlEditTable:
		return item
	case *tomlEditValue:
		if item.inline != nil {
			return item.inline
		}
	}
	return nil
}

// tomlEditAsString is Item::as_str.
func tomlEditAsString(item tomlEditItem) (string, bool) {
	value, ok := item.(*tomlEditValue)
	if !ok || value.kind != tomlEditString {
		return "", false
	}
	decoded, ok := value.decoded()
	if !ok {
		return "", false
	}
	text, ok := decoded.(string)
	return text, ok
}

func cloneTomlEditTable(table *tomlEditTable) *tomlEditTable {
	copied := *table
	copied.items = cloneTomlEditEntries(table.items)
	if table.position != nil {
		position := *table.position
		copied.position = &position
	}
	return &copied
}

func cloneTomlEditEntries(entries tomlEditEntries) tomlEditEntries {
	copied := tomlEditEntries{order: append([]string(nil), entries.order...)}
	if entries.byKey != nil {
		copied.byKey = make(map[string]*tomlEditEntry, len(entries.byKey))
		for name, entry := range entries.byKey {
			copied.byKey[name] = &tomlEditEntry{key: entry.key, item: cloneTomlEditItem(entry.item)}
		}
	}
	return copied
}

func cloneTomlEditItem(item tomlEditItem) tomlEditItem {
	switch item := item.(type) {
	case *tomlEditValue:
		copied := *item
		if item.inline != nil {
			inline := *item.inline
			inline.items = cloneTomlEditEntries(item.inline.items)
			copied.inline = &inline
		}
		return &copied
	case *tomlEditTable:
		return cloneTomlEditTable(item)
	case *tomlEditArrayOfTables:
		copied := &tomlEditArrayOfTables{tables: make([]*tomlEditTable, len(item.tables))}
		for i, table := range item.tables {
			copied.tables[i] = cloneTomlEditTable(table)
		}
		return copied
	}
	panic(fmt.Sprintf("unknown TOML item %T", item))
}

// tomlEditIntoValue is Item::into_value: a table becomes an inline table and
// an array of tables an array of inline tables, both laid out on one line.
func tomlEditIntoValue(item tomlEditItem) *tomlEditValue {
	switch item := item.(type) {
	case *tomlEditValue:
		return item
	case *tomlEditTable:
		return &tomlEditValue{kind: tomlEditInline, inline: tomlEditIntoInlineTable(item)}
	case *tomlEditArrayOfTables:
		var text strings.Builder
		text.WriteByte('[')
		for i, table := range item.tables {
			if i > 0 {
				text.WriteString(", ")
			}
			text.WriteString(renderTomlEditInline(tomlEditIntoInlineTable(table)))
		}
		text.WriteByte(']')
		return &tomlEditValue{kind: tomlEditArray, repr: text.String()}
	}
	panic(fmt.Sprintf("unknown TOML item %T", item))
}

// tomlEditIntoInlineTable is Table::into_inline_table: every entry becomes a
// value, with its key and value decoration reset to the inline defaults.
func tomlEditIntoInlineTable(table *tomlEditTable) *tomlEditInlineTable {
	inline := &tomlEditInlineTable{}
	for _, name := range table.items.order {
		entry := table.items.byKey[name]
		value := tomlEditIntoValue(entry.item)
		value.decor = tomlEditDecor{}
		key := entry.key
		key.decor = tomlEditDecor{}
		inline.items.set(&tomlEditEntry{key: key, item: value})
	}
	return inline
}

// ---------------------------------------------------------------------------
// Rendering (toml_edit's encode.rs).

const (
	tomlDefaultKeyPrefix         = ""
	tomlDefaultKeySuffix         = " "
	tomlDefaultTablePrefix       = "\n"
	tomlDefaultValuePrefix       = " "
	tomlDefaultInlineKeyPrefix   = " "
	tomlDefaultInlineKeySuffix   = " "
	tomlDefaultTrailingSuffix    = " "
	tomlDefaultNonTrailingSuffix = ""
)

// tomlEditKeyValue is one value with the key path that names it from its
// visual parent table, as toml_edit's get_values returns it.
type tomlEditKeyValue struct {
	path  []tomlEditKey
	value *tomlEditValue
}

func tomlEditValues(entries tomlEditEntries, parent []tomlEditKey, values []tomlEditKeyValue) []tomlEditKeyValue {
	for _, name := range entries.order {
		entry := entries.byKey[name]
		path := append(append([]tomlEditKey(nil), parent...), entry.key)
		switch item := entry.item.(type) {
		case *tomlEditTable:
			if item.dotted {
				values = tomlEditValues(item.items, path, values)
			}
		case *tomlEditValue:
			if item.inline != nil && item.inline.dotted {
				values = tomlEditValues(item.inline.items, path, values)
			} else {
				values = append(values, tomlEditKeyValue{path: path, value: item})
			}
		}
	}
	return values
}

func renderTomlEditPath(text *strings.Builder, path []tomlEditKey, prefix, suffix string) {
	for i, key := range path {
		keyPrefix, keySuffix := "", ""
		if i == 0 {
			keyPrefix = prefix
		}
		if i == len(path)-1 {
			keySuffix = suffix
		}
		if i > 0 {
			text.WriteByte('.')
		}
		text.WriteString(key.decor.prefixOr(keyPrefix))
		text.WriteString(key.display())
		text.WriteString(key.decor.suffixOr(keySuffix))
	}
}

func renderTomlEditValue(text *strings.Builder, value *tomlEditValue, prefix, suffix string) {
	text.WriteString(value.decor.prefixOr(prefix))
	if value.inline != nil {
		text.WriteString(renderTomlEditInline(value.inline))
	} else {
		text.WriteString(value.repr)
	}
	text.WriteString(value.decor.suffixOr(suffix))
}

func renderTomlEditInline(table *tomlEditInlineTable) string {
	var text strings.Builder
	text.WriteByte('{')
	text.WriteString(table.preamble)
	values := tomlEditValues(table.items, nil, nil)
	for i, entry := range values {
		if i > 0 {
			text.WriteByte(',')
		}
		suffix := tomlDefaultNonTrailingSuffix
		if i == len(values)-1 {
			suffix = tomlDefaultTrailingSuffix
		}
		renderTomlEditPath(&text, entry.path, tomlDefaultInlineKeyPrefix, tomlDefaultInlineKeySuffix)
		text.WriteByte('=')
		renderTomlEditValue(&text, entry.value, tomlDefaultValuePrefix, suffix)
	}
	text.WriteByte('}')
	return text.String()
}

// String renders the document as toml_edit's Display does: the root's values,
// then every table in position order.
func (d *tomlEditDocument) String() string {
	type visited struct {
		position int
		table    *tomlEditTable
		path     []tomlEditKey
		isArray  bool
	}
	var tables []visited
	lastPosition := 0
	var visit func(table *tomlEditTable, path []tomlEditKey, isArray bool)
	visit = func(table *tomlEditTable, path []tomlEditKey, isArray bool) {
		if !table.dotted {
			if table.position != nil {
				lastPosition = *table.position
			}
			tables = append(tables, visited{lastPosition, table, append([]tomlEditKey(nil), path...), isArray})
		}
		for _, name := range table.items.order {
			entry := table.items.byKey[name]
			switch item := entry.item.(type) {
			case *tomlEditTable:
				key := entry.key
				if item.dotted {
					// May have newlines and generally isn't written for
					// standard tables.
					key.decor = tomlEditDecor{}
				}
				visit(item, append(append([]tomlEditKey(nil), path...), key), false)
			case *tomlEditArrayOfTables:
				for _, child := range item.tables {
					visit(child, append(append([]tomlEditKey(nil), path...), entry.key), true)
				}
			}
		}
	}
	visit(d.root, nil, false)
	sort.SliceStable(tables, func(i, j int) bool { return tables[i].position < tables[j].position })

	var text strings.Builder
	firstTable := true
	for _, entry := range tables {
		table := entry.table
		children := tomlEditValues(table.items, nil, nil)
		// An implicit table with no values of its own has no header.
		visibleStandardTable := !(table.implicit && len(children) == 0)
		header := func(open, close string) {
			prefix := tomlDefaultTablePrefix
			if firstTable {
				firstTable = false
				prefix = ""
			}
			text.WriteString(table.decor.prefixOr(prefix))
			text.WriteString(open)
			renderTomlEditPath(&text, entry.path, "", "")
			text.WriteString(close)
			text.WriteString(table.decor.suffixOr(""))
			text.WriteByte('\n')
		}
		switch {
		case len(entry.path) == 0:
			if len(children) > 0 {
				firstTable = false
			}
		case entry.isArray:
			header("[[", "]]")
		case visibleStandardTable:
			header("[", "]")
		}
		for _, child := range children {
			renderTomlEditPath(&text, child.path, tomlDefaultKeyPrefix, tomlDefaultKeySuffix)
			text.WriteByte('=')
			renderTomlEditValue(&text, child.value, tomlDefaultValuePrefix, "")
			text.WriteByte('\n')
		}
	}
	text.WriteString(d.trailing)
	return text.String()
}

func isBareKeyRune(r rune) bool {
	return r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z' || r >= '0' && r <= '9' || r == '-' || r == '_'
}

// ---------------------------------------------------------------------------
// Parsing (toml_edit's parser and ParseState).

var errTomlEditSyntax = errors.New("not a TOML document this editor can keep")

type tomlEditParser struct {
	text string
	pos  int
	// trailing is the whitespace and comments not yet claimed by a key or a
	// header, as a span of text; trailingStart < 0 means none.
	trailingStart, trailingEnd int

	document        *tomlEditDocument
	currentPosition int
	current         *tomlEditTable
	currentIsArray  bool
	currentPath     []tomlEditKey
}

func parseTomlEditDocument(text string) (*tomlEditDocument, error) {
	p := &tomlEditParser{
		text:          strings.TrimPrefix(text, "\uFEFF"),
		trailingStart: -1,
		document:      &tomlEditDocument{root: &tomlEditTable{}},
		current:       &tomlEditTable{},
	}
	if err := p.parse(); err != nil {
		return nil, err
	}
	return p.document, nil
}

func (p *tomlEditParser) parse() error {
	p.parseWhitespace()
	for p.pos < len(p.text) {
		switch p.text[p.pos] {
		case '#':
			start := p.pos
			p.skipComment()
			p.onTrivia(start, p.pos)
		case '[':
			if err := p.parseHeader(); err != nil {
				return err
			}
		case '\n', '\r':
			start := p.pos
			if !p.skipNewline() {
				return errTomlEditSyntax
			}
			p.onTrivia(start, p.pos)
		default:
			if err := p.parseKeyValue(); err != nil {
				return err
			}
		}
		p.parseWhitespace()
	}
	if err := p.finalizeTable(); err != nil {
		return err
	}
	p.document.trailing = p.takeTrailing()
	return nil
}

func (p *tomlEditParser) onTrivia(start, end int) {
	if p.trailingStart < 0 {
		p.trailingStart = start
	}
	p.trailingEnd = end
}

func (p *tomlEditParser) takeTrailing() string {
	if p.trailingStart < 0 {
		return ""
	}
	text := p.text[p.trailingStart:p.trailingEnd]
	p.trailingStart = -1
	return text
}

func (p *tomlEditParser) parseWhitespace() {
	start := p.pos
	p.skipSpaces()
	p.onTrivia(start, p.pos)
}

func (p *tomlEditParser) skipSpaces() {
	for p.pos < len(p.text) && (p.text[p.pos] == ' ' || p.text[p.pos] == '\t') {
		p.pos++
	}
}

func (p *tomlEditParser) skipComment() {
	for p.pos < len(p.text) && p.text[p.pos] != '\n' && p.text[p.pos] != '\r' {
		p.pos++
	}
}

func (p *tomlEditParser) skipNewline() bool {
	switch {
	case strings.HasPrefix(p.text[p.pos:], "\r\n"):
		p.pos += 2
	case p.pos < len(p.text) && p.text[p.pos] == '\n':
		p.pos++
	default:
		return false
	}
	return true
}

// lineTrailing reads the whitespace and comment ending a line, then the line
// ending, returning the whitespace and comment.
func (p *tomlEditParser) lineTrailing() (string, error) {
	start := p.pos
	p.skipSpaces()
	if p.pos < len(p.text) && p.text[p.pos] == '#' {
		p.skipComment()
	}
	trailing := p.text[start:p.pos]
	if p.pos < len(p.text) && !p.skipNewline() {
		return "", errTomlEditSyntax
	}
	return trailing, nil
}

// parseKeyPath reads dotted keys, each with the whitespace around it.
func (p *tomlEditParser) parseKeyPath() ([]tomlEditKey, int, error) {
	var path []tomlEditKey
	firstPrefixEnd := -1
	for {
		prefixStart := p.pos
		p.skipSpaces()
		prefix := p.text[prefixStart:p.pos]
		if firstPrefixEnd < 0 {
			firstPrefixEnd = p.pos
		}
		reprStart := p.pos
		name, err := p.parseSimpleKey()
		if err != nil {
			return nil, 0, err
		}
		repr := p.text[reprStart:p.pos]
		suffixStart := p.pos
		p.skipSpaces()
		suffix := p.text[suffixStart:p.pos]
		path = append(path, tomlEditKey{name: name, repr: &repr, decor: tomlEditDecor{prefix: &prefix, suffix: &suffix}})
		if p.pos < len(p.text) && p.text[p.pos] == '.' {
			p.pos++
			continue
		}
		return path, firstPrefixEnd, nil
	}
}

func (p *tomlEditParser) parseSimpleKey() (string, error) {
	if p.pos >= len(p.text) {
		return "", errTomlEditSyntax
	}
	switch p.text[p.pos] {
	case '"':
		end, err := p.basicStringEnd(p.pos)
		if err != nil {
			return "", err
		}
		name, err := tomlUnescapeBasic(p.text[p.pos+1 : end-1])
		p.pos = end
		return name, err
	case '\'':
		end := strings.IndexByte(p.text[p.pos+1:], '\'')
		if end < 0 {
			return "", errTomlEditSyntax
		}
		name := p.text[p.pos+1 : p.pos+1+end]
		p.pos += end + 2
		return name, nil
	}
	start := p.pos
	for p.pos < len(p.text) && isBareKeyRune(rune(p.text[p.pos])) {
		p.pos++
	}
	if p.pos == start {
		return "", errTomlEditSyntax
	}
	return p.text[start:p.pos], nil
}

func (p *tomlEditParser) parseKeyValue() error {
	path, firstPrefixEnd, err := p.parseKeyPath()
	if err != nil {
		return err
	}
	if p.pos >= len(p.text) || p.text[p.pos] != '=' {
		return errTomlEditSyntax
	}
	p.pos++
	prefixStart := p.pos
	p.skipSpaces()
	valuePrefix := p.text[prefixStart:p.pos]
	value, err := p.parseValue()
	if err != nil {
		return err
	}
	valueSuffix, err := p.lineTrailing()
	if err != nil {
		return err
	}
	value.decor = tomlEditDecor{prefix: &valuePrefix, suffix: &valueSuffix}

	// The whitespace and comments before the line belong to its first key.
	if p.trailingStart >= 0 {
		prefix := p.text[p.trailingStart:firstPrefixEnd]
		path[0].decor.prefix = &prefix
		p.trailingStart = -1
	}
	key := path[len(path)-1]
	path = path[:len(path)-1]
	table, err := p.descendPath(p.current, path, true)
	if err != nil {
		return err
	}
	// "Likewise, using dotted keys to redefine tables already defined in
	// [table] form is not allowed"
	if table.dotted == (len(path) == 0) {
		return errTomlEditSyntax
	}
	if table.items.get(key.name) != nil {
		return errTomlEditSyntax
	}
	table.items.set(&tomlEditEntry{key: key, item: value})
	return nil
}

func (p *tomlEditParser) parseHeader() error {
	array := strings.HasPrefix(p.text[p.pos:], "[[")
	if array {
		p.pos += 2
	} else {
		p.pos++
	}
	path, _, err := p.parseKeyPath()
	if err != nil {
		return err
	}
	closing := "]"
	if array {
		closing = "]]"
	}
	if !strings.HasPrefix(p.text[p.pos:], closing) {
		return errTomlEditSyntax
	}
	p.pos += len(closing)
	suffix, err := p.lineTrailing()
	if err != nil {
		return err
	}
	if err := p.finalizeTable(); err != nil {
		return err
	}
	prefix := p.takeTrailing()
	decor := tomlEditDecor{prefix: &prefix, suffix: &suffix}
	if array {
		return p.startArrayTable(path, decor)
	}
	return p.startTable(path, decor)
}

func (p *tomlEditParser) descendPath(table *tomlEditTable, path []tomlEditKey, dotted bool) (*tomlEditTable, error) {
	for _, key := range path {
		entry := table.items.get(key.name)
		if entry == nil {
			created := &tomlEditTable{implicit: true, dotted: dotted}
			table.items.set(&tomlEditEntry{key: key, item: created})
			table = created
			continue
		}
		switch item := entry.item.(type) {
		case *tomlEditValue:
			return nil, errTomlEditSyntax
		case *tomlEditArrayOfTables:
			if len(item.tables) == 0 {
				return nil, errTomlEditSyntax
			}
			table = item.tables[len(item.tables)-1]
		case *tomlEditTable:
			// Since tables cannot be defined more than once, redefining such
			// tables using a [table] header is not allowed. Likewise, using
			// dotted keys to redefine tables already defined in [table] form
			// is not allowed.
			if dotted && !item.implicit {
				return nil, errTomlEditSyntax
			}
			table = item
		}
	}
	return table, nil
}

func (p *tomlEditParser) startTable(path []tomlEditKey, decor tomlEditDecor) error {
	parent, err := p.descendPath(p.document.root, path[:len(path)-1], false)
	if err != nil {
		return err
	}
	key := path[len(path)-1]
	// Keep any child tables an implicit table already holds.
	if entry := parent.items.remove(key.name); entry != nil {
		table, ok := entry.item.(*tomlEditTable)
		if !ok || !table.implicit || table.dotted {
			return errTomlEditSyntax
		}
		p.current = table
	}
	p.currentPosition++
	position := p.currentPosition
	p.current.decor = decor
	p.current.implicit = false
	p.current.dotted = false
	p.current.position = &position
	p.currentIsArray = false
	p.currentPath = path
	return nil
}

func (p *tomlEditParser) startArrayTable(path []tomlEditKey, decor tomlEditDecor) error {
	parent, err := p.descendPath(p.document.root, path[:len(path)-1], false)
	if err != nil {
		return err
	}
	key := path[len(path)-1]
	entry := parent.items.get(key.name)
	if entry == nil {
		parent.items.set(&tomlEditEntry{key: key, item: &tomlEditArrayOfTables{}})
	} else if _, ok := entry.item.(*tomlEditArrayOfTables); !ok {
		return errTomlEditSyntax
	}
	p.currentPosition++
	position := p.currentPosition
	p.current.decor = decor
	p.current.implicit = false
	p.current.dotted = false
	p.current.position = &position
	p.currentIsArray = true
	p.currentPath = path
	return nil
}

func (p *tomlEditParser) finalizeTable() error {
	table := p.current
	path := p.currentPath
	p.current = &tomlEditTable{}
	p.currentPath = nil
	if len(path) == 0 {
		p.document.root = table
		return nil
	}
	parent, err := p.descendPath(p.document.root, path[:len(path)-1], false)
	if err != nil {
		return err
	}
	key := path[len(path)-1]
	entry := parent.items.get(key.name)
	if p.currentIsArray {
		if entry == nil {
			entry = &tomlEditEntry{key: key, item: &tomlEditArrayOfTables{}}
			parent.items.set(entry)
		}
		array, ok := entry.item.(*tomlEditArrayOfTables)
		if !ok {
			return errTomlEditSyntax
		}
		array.tables = append(array.tables, table)
		return nil
	}
	if entry != nil {
		// [a.b.c] preceded [a.b].
		existing, ok := entry.item.(*tomlEditTable)
		if !ok || !existing.implicit {
			return errTomlEditSyntax
		}
		entry.item = table
		return nil
	}
	parent.items.set(&tomlEditEntry{key: key, item: table})
	return nil
}

// parseValue reads one value. Scalars and arrays keep their text; an inline
// table is read into entries.
func (p *tomlEditParser) parseValue() (*tomlEditValue, error) {
	if p.pos >= len(p.text) {
		return nil, errTomlEditSyntax
	}
	start := p.pos
	switch p.text[p.pos] {
	case '"':
		end, err := p.basicStringEnd(p.pos)
		if err != nil {
			return nil, err
		}
		p.pos = end
		return &tomlEditValue{kind: tomlEditString, repr: p.text[start:end]}, nil
	case '\'':
		end, err := p.literalStringEnd(p.pos)
		if err != nil {
			return nil, err
		}
		p.pos = end
		return &tomlEditValue{kind: tomlEditString, repr: p.text[start:end]}, nil
	case '[':
		if err := p.skipArray(); err != nil {
			return nil, err
		}
		return &tomlEditValue{kind: tomlEditArray, repr: p.text[start:p.pos]}, nil
	case '{':
		inline, err := p.parseInlineTable()
		if err != nil {
			return nil, err
		}
		return &tomlEditValue{kind: tomlEditInline, inline: inline}, nil
	}
	end := p.scalarEnd(p.pos)
	if end == start {
		return nil, errTomlEditSyntax
	}
	p.pos = end
	repr := p.text[start:end]
	kind := tomlEditOther
	if tomlIntegerRepr(repr) {
		kind = tomlEditInteger
	}
	return &tomlEditValue{kind: kind, repr: repr}, nil
}

// tomlIntegerRepr is whether a scalar is written as an integer.
func tomlIntegerRepr(repr string) bool {
	body := strings.TrimLeft(repr, "+-")
	if strings.HasPrefix(body, "0x") || strings.HasPrefix(body, "0o") || strings.HasPrefix(body, "0b") {
		return true
	}
	if body == "" {
		return false
	}
	for _, r := range body {
		if !(r >= '0' && r <= '9' || r == '_') {
			return false
		}
	}
	return true
}

// scalarEnd is where a number, boolean or date-time starting at start ends.
// A date and time may be separated by one space.
func (p *tomlEditParser) scalarEnd(start int) int {
	end := start
	for end < len(p.text) && !strings.ContainsRune(" \t\r\n,]}#", rune(p.text[end])) {
		end++
	}
	if end-start == 10 && end+3 < len(p.text) && p.text[end] == ' ' && p.text[start+4] == '-' &&
		isASCIIDigit(p.text[end+1]) && isASCIIDigit(p.text[end+2]) && p.text[end+3] == ':' {
		end++
		for end < len(p.text) && !strings.ContainsRune(" \t\r\n,]}#", rune(p.text[end])) {
			end++
		}
	}
	return end
}

func isASCIIDigit(b byte) bool { return b >= '0' && b <= '9' }

// basicStringEnd is the index just past the basic string (single or
// multi-line) starting at start.
func (p *tomlEditParser) basicStringEnd(start int) (int, error) {
	if strings.HasPrefix(p.text[start:], `"""`) {
		i := start + 3
		for i < len(p.text) {
			switch {
			case p.text[i] == '\\':
				i += 2
			case strings.HasPrefix(p.text[i:], `"""`):
				end := i + 3
				// Up to two quotes before the delimiter belong to the string.
				for extra := 0; extra < 2 && end < len(p.text) && p.text[end] == '"'; extra++ {
					end++
				}
				return end, nil
			default:
				i++
			}
		}
		return 0, errTomlEditSyntax
	}
	for i := start + 1; i < len(p.text); i++ {
		switch p.text[i] {
		case '\\':
			i++
		case '"':
			return i + 1, nil
		case '\n':
			return 0, errTomlEditSyntax
		}
	}
	return 0, errTomlEditSyntax
}

func (p *tomlEditParser) literalStringEnd(start int) (int, error) {
	if strings.HasPrefix(p.text[start:], "'''") {
		index := strings.Index(p.text[start+3:], "'''")
		if index < 0 {
			return 0, errTomlEditSyntax
		}
		end := start + 3 + index + 3
		for extra := 0; extra < 2 && end < len(p.text) && p.text[end] == '\''; extra++ {
			end++
		}
		return end, nil
	}
	for i := start + 1; i < len(p.text); i++ {
		switch p.text[i] {
		case '\'':
			return i + 1, nil
		case '\n':
			return 0, errTomlEditSyntax
		}
	}
	return 0, errTomlEditSyntax
}

// skipArrayTrivia passes whitespace, newlines and comments inside an array.
func (p *tomlEditParser) skipArrayTrivia() {
	for p.pos < len(p.text) {
		switch p.text[p.pos] {
		case ' ', '\t', '\n', '\r':
			p.pos++
		case '#':
			p.skipComment()
		default:
			return
		}
	}
}

func (p *tomlEditParser) skipArray() error {
	p.pos++ // [
	for {
		p.skipArrayTrivia()
		if p.pos >= len(p.text) {
			return errTomlEditSyntax
		}
		if p.text[p.pos] == ']' {
			p.pos++
			return nil
		}
		if _, err := p.parseValue(); err != nil {
			return err
		}
		p.skipArrayTrivia()
		if p.pos >= len(p.text) {
			return errTomlEditSyntax
		}
		switch p.text[p.pos] {
		case ',':
			p.pos++
		case ']':
			p.pos++
			return nil
		default:
			return errTomlEditSyntax
		}
	}
}

func (p *tomlEditParser) parseInlineTable() (*tomlEditInlineTable, error) {
	p.pos++ // {
	type pair struct {
		path []tomlEditKey
		key  tomlEditKey
		item *tomlEditValue
	}
	var pairs []pair
	readPair := func() error {
		path, _, err := p.parseKeyPath()
		if err != nil {
			return err
		}
		if p.pos >= len(p.text) || p.text[p.pos] != '=' {
			return errTomlEditSyntax
		}
		p.pos++
		prefixStart := p.pos
		p.skipSpaces()
		prefix := p.text[prefixStart:p.pos]
		value, err := p.parseValue()
		if err != nil {
			return err
		}
		suffixStart := p.pos
		p.skipSpaces()
		suffix := p.text[suffixStart:p.pos]
		value.decor = tomlEditDecor{prefix: &prefix, suffix: &suffix}
		pairs = append(pairs, pair{path: path[:len(path)-1], key: path[len(path)-1], item: value})
		return nil
	}
	// A key starts after optional whitespace; an empty table goes straight
	// to the closing brace.
	lookahead := p.pos
	for lookahead < len(p.text) && (p.text[lookahead] == ' ' || p.text[lookahead] == '\t') {
		lookahead++
	}
	if lookahead < len(p.text) && p.text[lookahead] != '}' {
		if err := readPair(); err != nil {
			return nil, err
		}
		for p.pos < len(p.text) && p.text[p.pos] == ',' {
			p.pos++
			if err := readPair(); err != nil {
				return nil, err
			}
		}
	}
	preambleStart := p.pos
	p.skipSpaces()
	if p.pos >= len(p.text) || p.text[p.pos] != '}' {
		return nil, errTomlEditSyntax
	}
	table := &tomlEditInlineTable{preamble: p.text[preambleStart:p.pos]}
	p.pos++
	for _, pair := range pairs {
		target := table
		for _, key := range pair.path {
			entry := target.items.get(key.name)
			if entry == nil {
				child := &tomlEditInlineTable{dotted: true}
				target.items.set(&tomlEditEntry{key: key, item: &tomlEditValue{kind: tomlEditInline, inline: child}})
				target = child
				continue
			}
			value, ok := entry.item.(*tomlEditValue)
			if !ok || value.inline == nil || !value.inline.dotted {
				return nil, errTomlEditSyntax
			}
			target = value.inline
		}
		if target.items.get(pair.key.name) != nil {
			return nil, errTomlEditSyntax
		}
		target.items.set(&tomlEditEntry{key: pair.key, item: pair.item})
	}
	return table, nil
}

// tomlUnescapeBasic decodes the escapes of a basic string's body.
func tomlUnescapeBasic(body string) (string, error) {
	if !strings.Contains(body, `\`) {
		return body, nil
	}
	var text strings.Builder
	for i := 0; i < len(body); i++ {
		if body[i] != '\\' {
			text.WriteByte(body[i])
			continue
		}
		i++
		if i >= len(body) {
			return "", errTomlEditSyntax
		}
		switch body[i] {
		case 'b':
			text.WriteByte('\b')
		case 't':
			text.WriteByte('\t')
		case 'n':
			text.WriteByte('\n')
		case 'f':
			text.WriteByte('\f')
		case 'r':
			text.WriteByte('\r')
		case 'e':
			text.WriteByte(0x1b)
		case '"':
			text.WriteByte('"')
		case '\\':
			text.WriteByte('\\')
		case 'x', 'u', 'U':
			width := map[byte]int{'x': 2, 'u': 4, 'U': 8}[body[i]]
			if i+1+width > len(body) {
				return "", errTomlEditSyntax
			}
			code, err := strconv.ParseUint(body[i+1:i+1+width], 16, 32)
			if err != nil || !utf8.ValidRune(rune(code)) {
				return "", errTomlEditSyntax
			}
			text.WriteRune(rune(code))
			i += width
		default:
			return "", errTomlEditSyntax
		}
	}
	return text.String(), nil
}
