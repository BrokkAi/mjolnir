// Port of toml_edit 0.20.2 src/table.rs

package tomledit

import (
	"cmp"
	"iter"
	"sort"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Table is toml_edit::Table: type representing a TOML non-inline table. Its
// zero value is an empty table.
type Table struct {
	// Comments/spaces before and after the header
	decor Decor
	// Whether to hide an empty table
	implicit bool
	// Whether this is a proxy for dotted keys
	dotted bool
	// Used for putting tables back in their original order when serialising.
	//
	// `None` for user created tables (can be overridden with `set_position`)
	docPosition rs.Option[int]
	span        rs.Option[Range]
	items       keyValuePairs
}

/// Constructors
///
/// See also `FromIterator`

// TableNew is Table::new: creates an empty table.
func TableNew() Table {
	return Table{}
}

// TableDefault is the derived `Default for Table`.
func TableDefault() Table {
	return Table{}
}

func tableWithPos(docPosition rs.Option[int]) Table {
	return Table{docPosition: docPosition}
}

func tableWithPairs(items keyValuePairs) Table {
	return Table{items: items}
}

// IntoInlineTable is Table::into_inline_table: convert to an inline table.
func (t *Table) IntoInlineTable() InlineTable {
	for _, kv := range t.items.entries {
		ItemMakeValue(&kv.value)
	}
	inline := inlineTableWithPairs(t.items)
	inline.Fmt()
	return inline
}

/// Formatting

// KeyPathValue is one element of what Table::get_values and
// InlineTable::get_values return: `(Vec<&Key>, &Value)`, the keys leading to
// a value and the value.
type KeyPathValue struct {
	F0 []*Key
	F1 Value
}

// GetValues is Table::get_values: get key/values for values that are
// visually children of this table.
//
// For example, this will return dotted keys.
func (t *Table) GetValues() []KeyPathValue {
	var values []KeyPathValue
	t.appendValues(nil, &values)
	return values
}

func (t *Table) appendValues(parent []*Key, values *[]KeyPathValue) {
	for _, kv := range t.items.entries {
		path := make([]*Key, len(parent), len(parent)+1)
		copy(path, parent)
		path = append(path, &kv.key)
		switch item := kv.value.(type) {
		case *Item_Table:
			if item.F0.IsDotted() {
				item.F0.appendValues(path, values)
			}
		case *Item_Value:
			if table, ok := item.F0.AsInlineTable().Get(); ok {
				if table.IsDotted() {
					table.appendValues(path, values)
				} else {
					*values = append(*values, KeyPathValue{F0: path, F1: item.F0})
				}
			} else {
				*values = append(*values, KeyPathValue{F0: path, F1: item.F0})
			}
		}
	}
}

// Fmt is Table::fmt: auto formats the table.
func (t *Table) Fmt() {
	decorateTable(t)
}

// SortValues is Table::sort_values: sorts Key/Value Pairs of the table.
//
// Doesn't affect subtables or subarrays.
func (t *Table) SortValues() {
	// Assuming standard tables have their doc_position set and this won't negatively impact them
	t.items.sortKeys()
	for _, kv := range t.items.entries {
		if table, ok := kv.value.(*Item_Table); ok && table.F0.IsDotted() {
			table.F0.SortValues()
		}
	}
}

// SortValuesBy is Table::sort_values_by: sort Key/Value Pairs of the table
// using the using the comparison function `compare`.
//
// The comparison function receives two key and value pairs to compare (you
// can sort by keys or values or their combination as needed).
func (t *Table) SortValuesBy(compare func(k1 *Key, v1 Item, k2 *Key, v2 Item) int) {
	t.sortValuesByInternal(compare)
}

func (t *Table) sortValuesByInternal(compare func(k1 *Key, v1 Item, k2 *Key, v2 Item) int) {
	t.items.sortBy(func(val1, val2 *tableKeyValue) int {
		return compare(&val1.key, val1.value, &val2.key, val2.value)
	})

	for _, kv := range t.items.entries {
		if table, ok := kv.value.(*Item_Table); ok && table.F0.IsDotted() {
			table.F0.sortValuesByInternal(compare)
		}
	}
}

// SetImplicit is Table::set_implicit: if a table has no key/value pairs and
// implicit, it will not be displayed.
//
// # Examples
//
//	[target."x86_64/windows.json".dependencies]
//
// In the document above, tables `target` and `target."x86_64/windows.json"`
// are implicit.
//
//	use toml_edit::Document;
//	let mut doc = "[a]\n[a.b]\n".parse::<Document>().expect("invalid toml");
//
//	doc["a"].as_table_mut().unwrap().set_implicit(true);
//	assert_eq!(doc.to_string(), "[a.b]\n");
func (t *Table) SetImplicit(implicit bool) {
	t.implicit = implicit
}

// IsImplicit is Table::is_implicit: if a table has no key/value pairs and
// implicit, it will not be displayed.
func (t *Table) IsImplicit() bool {
	return t.implicit
}

// SetDotted is Table::set_dotted: change this table's dotted status.
func (t *Table) SetDotted(yes bool) {
	t.dotted = yes
}

// IsDotted is Table::is_dotted: check if this is a wrapper for dotted keys,
// rather than a standard table.
func (t *Table) IsDotted() bool {
	return t.dotted
}

// SetPosition is Table::set_position: sets the position of the `Table`
// within the `Document`.
func (t *Table) SetPosition(docPosition int) {
	t.docPosition = rs.Some(docPosition)
}

// Position is Table::position: the position of the `Table` within the
// `Document`.
//
// Returns `None` if the `Table` was created manually (i.e. not via parsing)
// in which case its position is set automatically.  This can be overridden
// with [`Table::set_position`].
func (t *Table) Position() rs.Option[int] {
	return t.docPosition
}

// DecorMut is Table::decor_mut: returns the surrounding whitespace.
func (t *Table) DecorMut() *Decor {
	return &t.decor
}

// Decor is Table::decor: returns the decor associated with a given key of
// the table.
func (t *Table) Decor() *Decor {
	return &t.decor
}

// KeyDecorMut is Table::key_decor_mut: returns the decor associated with a
// given key of the table.
func (t *Table) KeyDecorMut(key string) rs.Option[*Decor] {
	if kv := t.items.get(key); kv != nil {
		return rs.Some(&kv.key.decor)
	}
	return rs.None[*Decor]()
}

// KeyDecor is Table::key_decor: returns the decor associated with a given
// key of the table.
func (t *Table) KeyDecor(key string) rs.Option[*Decor] {
	return t.KeyDecorMut(key)
}

func (t *Table) despan(input string) {
	t.span = rs.None[Range]()
	t.decor.despan(input)
	for _, kv := range t.items.entries {
		kv.key.despan(input)
		kv.value.despan(input)
	}
}

// Iter is Table::iter: an iterator over all key/value pairs, including
// empty.
func (t *Table) Iter() iter.Seq2[string, Item] {
	return func(yield func(string, Item) bool) {
		for _, kv := range t.items.snapshot() {
			if !kv.value.IsNone() {
				if !yield(kv.key.key, kv.value) {
					return
				}
			}
		}
	}
}

// IterMut is Table::iter_mut: an mutable iterator over all key/value pairs,
// including empty. Assigning through a yielded *Item replaces the item in
// the table.
func (t *Table) IterMut() iter.Seq2[KeyMut, *Item] {
	return func(yield func(KeyMut, *Item) bool) {
		for _, kv := range t.items.snapshot() {
			if !kv.value.IsNone() {
				if !yield(kv.key.AsMut(), &kv.value) {
					return
				}
			}
		}
	}
}

// Len is Table::len: the number of non-empty items in the table.
func (t *Table) Len() int {
	count := 0
	for _, kv := range t.items.entries {
		if !kv.value.IsNone() {
			count++
		}
	}
	return count
}

// IsEmpty is Table::is_empty: returns true if the table is empty.
func (t *Table) IsEmpty() bool {
	return t.Len() == 0
}

// Clear is Table::clear: clears the table, removing all key-value pairs.
// Keeps the allocated memory for reuse.
func (t *Table) Clear() {
	t.items.clear()
}

// Entry is Table::entry: gets the given key's corresponding entry in the
// Table for in-place manipulation.
func (t *Table) Entry(key string) Entry {
	// Accept a `&str` rather than an owned type to keep `InternalString`, well, internal
	if kv := t.items.get(key); kv != nil {
		return &Entry_Occupied{F0: OccupiedEntry{items: &t.items, entry: kv}}
	}
	return &Entry_Vacant{F0: VacantEntry{items: &t.items, entryKey: key, key: rs.None[Key]()}}
}

// EntryFormat is Table::entry_format: gets the given key's corresponding
// entry in the Table for in-place manipulation.
func (t *Table) EntryFormat(key *Key) Entry {
	// Accept a `&Key` to be consistent with `entry`
	if kv := t.items.get(key.Get()); kv != nil {
		return &Entry_Occupied{F0: OccupiedEntry{items: &t.items, entry: kv}}
	}
	return &Entry_Vacant{F0: VacantEntry{items: &t.items, entryKey: key.Get(), key: rs.Some(key.Clone())}}
}

// Get is Table::get: an optional reference to an item given the key.
func (t *Table) Get(key string) rs.Option[Item] {
	if kv := t.items.get(key); kv != nil && !kv.value.IsNone() {
		return rs.Some(kv.value)
	}
	return rs.None[Item]()
}

// GetMut is Table::get_mut: an optional mutable reference to an item given
// the key. Assigning through the *Item replaces the item in the table.
func (t *Table) GetMut(key string) rs.Option[*Item] {
	if kv := t.items.get(key); kv != nil && !kv.value.IsNone() {
		return rs.Some(&kv.value)
	}
	return rs.None[*Item]()
}

// GetKeyValue is Table::get_key_value: return references to the key-value
// pair stored for key, if it is present, else None.
func (t *Table) GetKeyValue(key string) rs.Option[rs.Tuple2[*Key, Item]] {
	if kv := t.items.get(key); kv != nil && !kv.value.IsNone() {
		return rs.Some(rs.T2(&kv.key, kv.value))
	}
	return rs.None[rs.Tuple2[*Key, Item]]()
}

// GetKeyValueMut is Table::get_key_value_mut: return mutable references to
// the key-value pair stored for key, if it is present, else None.
func (t *Table) GetKeyValueMut(key string) rs.Option[rs.Tuple2[KeyMut, *Item]] {
	if kv := t.items.get(key); kv != nil && !kv.value.IsNone() {
		return rs.Some(rs.T2(kv.key.AsMut(), &kv.value))
	}
	return rs.None[rs.Tuple2[KeyMut, *Item]]()
}

// ContainsKey is Table::contains_key: returns true if the table contains an
// item with the given key.
func (t *Table) ContainsKey(key string) bool {
	if kv := t.items.get(key); kv != nil {
		return !kv.value.IsNone()
	}
	return false
}

// ContainsTable is Table::contains_table: returns true if the table contains
// a table with the given key.
func (t *Table) ContainsTable(key string) bool {
	if kv := t.items.get(key); kv != nil {
		return kv.value.IsTable()
	}
	return false
}

// ContainsValue is Table::contains_value: returns true if the table contains
// a value with the given key.
func (t *Table) ContainsValue(key string) bool {
	if kv := t.items.get(key); kv != nil {
		return kv.value.IsValue()
	}
	return false
}

// ContainsArrayOfTables is Table::contains_array_of_tables: returns true if
// the table contains an array of tables with the given key.
func (t *Table) ContainsArrayOfTables(key string) bool {
	if kv := t.items.get(key); kv != nil {
		return kv.value.IsArrayOfTables()
	}
	return false
}

// Insert is Table::insert: inserts a key-value pair into the map.
func (t *Table) Insert(key string, item Item) rs.Option[Item] {
	kv := tableKeyValueNew(KeyNew(key), item)
	if old, had := t.items.insert(key, kv); had {
		return rs.Some(old.value)
	}
	return rs.None[Item]()
}

// InsertFormatted is Table::insert_formatted: inserts a key-value pair into
// the map.
func (t *Table) InsertFormatted(key *Key, item Item) rs.Option[Item] {
	kv := tableKeyValueNew(key.Clone(), item)
	if old, had := t.items.insert(key.Get(), kv); had {
		return rs.Some(old.value)
	}
	return rs.None[Item]()
}

// Remove is Table::remove: removes an item given the key.
func (t *Table) Remove(key string) rs.Option[Item] {
	if kv := t.items.shiftRemove(key); kv != nil {
		return rs.Some(kv.value)
	}
	return rs.None[Item]()
}

// RemoveEntry is Table::remove_entry: removes a key from the map, returning
// the stored key and value if the key was previously in the map.
func (t *Table) RemoveEntry(key string) rs.Option[rs.Tuple2[Key, Item]] {
	if kv := t.items.shiftRemove(key); kv != nil {
		return rs.Some(rs.T2(kv.key, kv.value))
	}
	return rs.None[rs.Tuple2[Key, Item]]()
}

// Retain is Table::retain: retains only the elements specified by the `keep`
// predicate.
//
// In other words, remove all pairs `(key, item)` for which
// `keep(&key, &mut item)` returns `false`.
//
// The elements are visited in iteration order.
func (t *Table) Retain(keep func(key string, item *Item) bool) {
	t.items.retain(func(key string, kv *tableKeyValue) bool {
		return keep(key, &kv.value)
	})
}

// Clone is the derived `Clone for Table`.
func (t *Table) Clone() Table {
	cloned := *t
	cloned.items = t.items.clone()
	return cloned
}

// String is `impl Display for Table`.
func (t *Table) String() string {
	var buf strings.Builder
	children := t.GetValues()
	// print table body
	for _, child := range children {
		encodeKeyPath(child.F0, &buf, rs.None[string](), defaultKeyDecorPrefix, defaultKeyDecorSuffix)
		buf.WriteString("=")
		encodeValue(child.F1, &buf, rs.None[string](), defaultValueDecorPrefix, defaultValueDecorSuffix)
		buf.WriteString("\n")
	}
	return buf.String()
}

// FmtDisplay is `impl Display for Table`.
func (t *Table) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(t.String()) }

// FmtDebug is the derived `Debug for Table`.
func (t *Table) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Table").Field("decor", t.decor).Field("implicit", t.implicit).Field("dotted", t.dotted).
		Field("doc_position", t.docPosition).Field("span", t.span).Field("items", &t.items).Finish()
}

// Extend is `impl Extend<(K, V)> for Table` with K: Into<Key> (a string or a
// Key) and V: Into<Value> (see IntoValue).
func (t *Table) Extend(pairs ...rs.Tuple2[any, any]) {
	for _, pair := range pairs {
		key := intoKey(pair.F0)
		value := tableKeyValueNew(key, &Item_Value{F0: IntoValue(pair.F1)})
		t.items.insert(value.key.Get(), value)
	}
}

// TableFromIter is `impl FromIterator<(K, V)> for Table` with K: Into<Key>
// (a string or a Key) and V: Into<Value> (see IntoValue).
func TableFromIter(pairs ...rs.Tuple2[any, any]) Table {
	table := TableNew()
	table.Extend(pairs...)
	return table
}

// IntoIter is `impl IntoIterator for Table`: the keys and items of the
// table, including empty ones.
func (t *Table) IntoIter() iter.Seq2[InternalString, Item] {
	return func(yield func(InternalString, Item) bool) {
		for _, kv := range t.items.snapshot() {
			if !yield(kv.mapKey, kv.value) {
				return
			}
		}
	}
}

// keyValuePairs is `IndexMap<InternalString, TableKeyValue>`: the pairs of a
// table in insertion order. Each pair is held by pointer, so a pointer to its
// item stays valid while other pairs come and go.
type keyValuePairs struct {
	entries []*tableKeyValue
	index   map[string]int
}

func (m *keyValuePairs) get(key string) *tableKeyValue {
	if position, ok := m.index[key]; ok {
		return m.entries[position]
	}
	return nil
}

func (m *keyValuePairs) len() int { return len(m.entries) }

// snapshot returns the pairs to iterate over while the map may change under
// the loop body, as callers of a Go iterator are free to do.
func (m *keyValuePairs) snapshot() []*tableKeyValue {
	return append([]*tableKeyValue(nil), m.entries...)
}

// insert is IndexMap::insert: a new key is appended; an existing key keeps
// its position and has its value replaced, and the old value is returned.
func (m *keyValuePairs) insert(key string, kv tableKeyValue) (tableKeyValue, bool) {
	kv.mapKey = key
	if position, ok := m.index[key]; ok {
		old := *m.entries[position]
		m.entries[position] = &kv
		return old, true
	}
	if m.index == nil {
		m.index = map[string]int{}
	}
	m.index[key] = len(m.entries)
	m.entries = append(m.entries, &kv)
	return tableKeyValue{}, false
}

// shiftRemove is IndexMap::shift_remove: removes the pair and shifts the
// ones after it down, keeping their order.
func (m *keyValuePairs) shiftRemove(key string) *tableKeyValue {
	position, ok := m.index[key]
	if !ok {
		return nil
	}
	removed := m.entries[position]
	m.entries = append(m.entries[:position:position], m.entries[position+1:]...)
	m.reindex()
	return removed
}

func (m *keyValuePairs) reindex() {
	m.index = make(map[string]int, len(m.entries))
	for position, kv := range m.entries {
		m.index[kv.mapKey] = position
	}
}

func (m *keyValuePairs) clear() {
	m.entries = nil
	m.index = nil
}

// retain is IndexMap::retain.
func (m *keyValuePairs) retain(keep func(key string, kv *tableKeyValue) bool) {
	kept := make([]*tableKeyValue, 0, len(m.entries))
	for _, kv := range m.entries {
		if keep(kv.mapKey, kv) {
			kept = append(kept, kv)
		}
	}
	m.entries = kept
	m.reindex()
}

// sortKeys is IndexMap::sort_keys, a stable sort.
func (m *keyValuePairs) sortKeys() {
	entries := m.snapshot()
	sort.SliceStable(entries, func(i, j int) bool {
		return cmp.Compare(entries[i].mapKey, entries[j].mapKey) < 0
	})
	m.entries = entries
	m.reindex()
}

// sortBy is IndexMap::sort_by, a stable sort.
func (m *keyValuePairs) sortBy(compare func(a, b *tableKeyValue) int) {
	entries := m.snapshot()
	sort.SliceStable(entries, func(i, j int) bool {
		return compare(entries[i], entries[j]) < 0
	})
	m.entries = entries
	m.reindex()
}

func (m *keyValuePairs) clone() keyValuePairs {
	cloned := keyValuePairs{entries: make([]*tableKeyValue, len(m.entries))}
	for position, kv := range m.entries {
		cloned.entries[position] = &tableKeyValue{mapKey: kv.mapKey, key: kv.key, value: kv.value.Clone()}
	}
	cloned.reindex()
	return cloned
}

// FmtDebug is IndexMap's Debug: the pairs as a map.
func (m *keyValuePairs) FmtDebug(f *rs.Formatter) error {
	debugMap := f.DebugMap()
	for _, kv := range m.entries {
		debugMap.Entry(kv.mapKey, kv)
	}
	return debugMap.Finish()
}

func decorateTable(table *Table) {
	for _, kv := range table.items.entries {
		value, ok := kv.value.(*Item_Value)
		if !ok {
			continue
		}
		kv.key.decor.Clear()
		value.F0.DecorMut().Clear()
	}
}

// `key1 = value1`
const (
	defaultKeyDecorPrefix = ""
	defaultKeyDecorSuffix = " "
)

const (
	defaultTableDecorPrefix = "\n"
	defaultTableDecorSuffix = ""
)

const (
	defaultKeyPathDecorPrefix = ""
	defaultKeyPathDecorSuffix = ""
)

type tableKeyValue struct {
	// mapKey is the IndexMap key the pair is stored under.
	mapKey InternalString
	key    Key
	value  Item
}

func tableKeyValueNew(key Key, value Item) tableKeyValue {
	return tableKeyValue{key: key, value: value}
}

// FmtDebug is the derived `Debug for TableKeyValue`.
func (kv *tableKeyValue) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("TableKeyValue").Field("key", &kv.key).Field("value", kv.value).Finish()
}

// TableLike is toml_edit::TableLike: this trait represents either a `Table`,
// or an `InlineTable`.
//
// *Table implements it. An InlineTable gives different results as a
// TableLike than through its own methods of the same names (its own `get`
// and `iter` hand out values, not items), so it implements the trait through
// the view that Item.AsTableLike, Item.AsTableLikeMut and
// InlineTable.AsTableLike return.
type TableLike interface {
	// Iter is TableLike::iter: an iterator over key/value pairs.
	Iter() iter.Seq2[string, Item]
	// IterMut is TableLike::iter_mut: an mutable iterator over all key/value
	// pairs, including empty.
	IterMut() iter.Seq2[KeyMut, *Item]
	// Len is TableLike::len: the number of nonempty items.
	Len() int
	// IsEmpty is TableLike::is_empty: returns true if the table is empty.
	IsEmpty() bool
	// Clear is TableLike::clear: clears the table, removing all key-value
	// pairs. Keeps the allocated memory for reuse.
	Clear()
	// Entry is TableLike::entry: gets the given key's corresponding entry in
	// the Table for in-place manipulation.
	Entry(key string) Entry
	// EntryFormat is TableLike::entry_format: gets the given key's
	// corresponding entry in the Table for in-place manipulation.
	EntryFormat(key *Key) Entry
	// Get is TableLike::get: an optional reference to an item given the key.
	Get(key string) rs.Option[Item]
	// GetMut is TableLike::get_mut: an optional mutable reference to an item
	// given the key.
	GetMut(key string) rs.Option[*Item]
	// GetKeyValue is TableLike::get_key_value: return references to the
	// key-value pair stored for key, if it is present, else None.
	GetKeyValue(key string) rs.Option[rs.Tuple2[*Key, Item]]
	// GetKeyValueMut is TableLike::get_key_value_mut: return mutable
	// references to the key-value pair stored for key, if it is present, else
	// None.
	GetKeyValueMut(key string) rs.Option[rs.Tuple2[KeyMut, *Item]]
	// ContainsKey is TableLike::contains_key: returns true if the table
	// contains an item with the given key.
	ContainsKey(key string) bool
	// Insert is TableLike::insert: inserts a key-value pair into the map.
	Insert(key string, value Item) rs.Option[Item]
	// Remove is TableLike::remove: removes an item given the key.
	Remove(key string) rs.Option[Item]

	// GetValues is TableLike::get_values: get key/values for values that are
	// visually children of this table.
	//
	// For example, this will return dotted keys.
	GetValues() []KeyPathValue

	// Fmt is TableLike::fmt: auto formats the table.
	Fmt()
	// SortValues is TableLike::sort_values: sorts Key/Value Pairs of the
	// table.
	//
	// Doesn't affect subtables or subarrays.
	SortValues()
	// SetDotted is TableLike::set_dotted: change this table's dotted status.
	SetDotted(yes bool)
	// IsDotted is TableLike::is_dotted: check if this is a wrapper for dotted
	// keys, rather than a standard table.
	IsDotted() bool

	// KeyDecorMut is TableLike::key_decor_mut: returns the decor associated
	// with a given key of the table.
	KeyDecorMut(key string) rs.Option[*Decor]
	// KeyDecor is TableLike::key_decor: returns the decor associated with a
	// given key of the table.
	KeyDecor(key string) rs.Option[*Decor]
}

var _ TableLike = (*Table)(nil)

// Entry is toml_edit::Entry: a view into a single location in a map, which
// may be vacant or occupied. Its variants are *Entry_Occupied and
// *Entry_Vacant.
type Entry interface {
	isEntry()
	// Key is Entry::key: returns the entry key.
	//
	//	use toml_edit::Table;
	//
	//	let mut map = Table::new();
	//
	//	assert_eq!("hello", map.entry("hello").key());
	Key() string
	// OrInsert is Entry::or_insert: ensures a value is in the entry by
	// inserting the default if empty, and returns a mutable reference to the
	// value in the entry.
	OrInsert(fallback Item) *Item
	// OrInsertWith is Entry::or_insert_with: ensures a value is in the entry
	// by inserting the result of the default function if empty, and returns a
	// mutable reference to the value in the entry.
	OrInsertWith(fallback func() Item) *Item
}

// Entry_Occupied is Entry::Occupied: an occupied Entry.
type Entry_Occupied struct{ F0 OccupiedEntry }

// Entry_Vacant is Entry::Vacant: a vacant Entry.
type Entry_Vacant struct{ F0 VacantEntry }

func (*Entry_Occupied) isEntry() {}
func (*Entry_Vacant) isEntry()   {}

// Key is Entry::key.
func (e *Entry_Occupied) Key() string { return e.F0.Key() }

// Key is Entry::key.
func (e *Entry_Vacant) Key() string { return e.F0.Key() }

// OrInsert is Entry::or_insert.
func (e *Entry_Occupied) OrInsert(Item) *Item { return e.F0.IntoMut() }

// OrInsert is Entry::or_insert.
func (e *Entry_Vacant) OrInsert(fallback Item) *Item { return e.F0.Insert(fallback) }

// OrInsertWith is Entry::or_insert_with.
func (e *Entry_Occupied) OrInsertWith(func() Item) *Item { return e.F0.IntoMut() }

// OrInsertWith is Entry::or_insert_with.
func (e *Entry_Vacant) OrInsertWith(fallback func() Item) *Item { return e.F0.Insert(fallback()) }

// OccupiedEntry is toml_edit::OccupiedEntry: a view into a single occupied
// location in a `IndexMap`.
type OccupiedEntry struct {
	items *keyValuePairs
	entry *tableKeyValue
}

// Key is OccupiedEntry::key: gets a reference to the entry key.
//
//	use toml_edit::Table;
//
//	let mut map = Table::new();
//
//	assert_eq!("foo", map.entry("foo").key());
func (e *OccupiedEntry) Key() string {
	return e.entry.mapKey
}

// KeyMut is OccupiedEntry::key_mut: gets a mutable reference to the entry
// key.
func (e *OccupiedEntry) KeyMut() KeyMut {
	return e.entry.key.AsMut()
}

// Get is OccupiedEntry::get: gets a reference to the value in the entry.
func (e *OccupiedEntry) Get() Item {
	return e.entry.value
}

// GetMut is OccupiedEntry::get_mut: gets a mutable reference to the value in
// the entry.
func (e *OccupiedEntry) GetMut() *Item {
	return &e.entry.value
}

// IntoMut is OccupiedEntry::into_mut: converts the OccupiedEntry into a
// mutable reference to the value in the entry with a lifetime bound to the
// map itself.
func (e *OccupiedEntry) IntoMut() *Item {
	return &e.entry.value
}

// Insert is OccupiedEntry::insert: sets the value of the entry, and returns
// the entry's old value.
func (e *OccupiedEntry) Insert(value Item) Item {
	old := e.entry.value
	e.entry.value = value
	return old
}

// Remove is OccupiedEntry::remove: takes the value out of the entry, and
// returns it.
func (e *OccupiedEntry) Remove() Item {
	return e.items.shiftRemove(e.entry.mapKey).value
}

// VacantEntry is toml_edit::VacantEntry: a view into a single empty location
// in a `IndexMap`.
type VacantEntry struct {
	items    *keyValuePairs
	entryKey InternalString
	key      rs.Option[Key]
}

// Key is VacantEntry::key: gets a reference to the entry key.
//
//	use toml_edit::Table;
//
//	let mut map = Table::new();
//
//	assert_eq!("foo", map.entry("foo").key());
func (e *VacantEntry) Key() string {
	return e.entryKey
}

// Insert is VacantEntry::insert: sets the value of the entry with the
// VacantEntry's key, and returns a mutable reference to it.
func (e *VacantEntry) Insert(value Item) *Item {
	key := e.key.UnwrapOrElse(func() Key { return KeyNew(e.entryKey) })
	e.items.insert(e.entryKey, tableKeyValueNew(key, value))
	return &e.items.get(e.entryKey).value
}
