// Port of toml_edit 0.20.2 src/inline_table.rs

package tomledit

import (
	"iter"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// InlineTable is toml_edit::InlineTable: type representing a TOML inline
// table, payload of the `Value::InlineTable` variant. Its zero value is an
// empty inline table.
type InlineTable struct {
	// `preamble` represents whitespaces in an empty table
	preamble RawString
	// Whether to hide an empty table
	implicit bool
	// prefix before `{` and suffix after `}`
	decor Decor
	span  rs.Option[Range]
	// whether this is a proxy for dotted keys
	dotted bool
	items  keyValuePairs
}

/// Constructors
///
/// See also `FromIterator`

// InlineTableNew is InlineTable::new: creates an empty table.
func InlineTableNew() InlineTable {
	return InlineTable{}
}

// InlineTableDefault is the derived `Default for InlineTable`.
func InlineTableDefault() InlineTable {
	return InlineTable{}
}

func inlineTableWithPairs(items keyValuePairs) InlineTable {
	return InlineTable{items: items}
}

// IntoTable is InlineTable::into_table: convert to a table.
func (t *InlineTable) IntoTable() Table {
	table := tableWithPairs(t.items)
	table.Fmt()
	return table
}

/// Formatting

// GetValues is InlineTable::get_values: get key/values for values that are
// visually children of this table.
//
// For example, this will return dotted keys.
func (t *InlineTable) GetValues() []KeyPathValue {
	var values []KeyPathValue
	t.appendValues(nil, &values)
	return values
}

func (t *InlineTable) appendValues(parent []*Key, values *[]KeyPathValue) {
	for _, kv := range t.items.entries {
		path := make([]*Key, len(parent), len(parent)+1)
		copy(path, parent)
		path = append(path, &kv.key)
		item, ok := kv.value.(*Item_Value)
		if !ok {
			continue
		}
		if table, ok := item.F0.(*Value_InlineTable); ok && table.F0.IsDotted() {
			table.F0.appendValues(path, values)
		} else {
			*values = append(*values, KeyPathValue{F0: path, F1: item.F0})
		}
	}
}

// Fmt is InlineTable::fmt: auto formats the table.
func (t *InlineTable) Fmt() {
	decorateInlineTable(t)
}

// SortValues is InlineTable::sort_values: sorts the key/value pairs by key.
func (t *InlineTable) SortValues() {
	// Assuming standard tables have their position set and this won't negatively impact them
	t.items.sortKeys()
	for _, kv := range t.items.entries {
		if table := dottedInlineTable(kv.value); table != nil {
			table.SortValues()
		}
	}
}

// dottedInlineTable is the pattern
// `Item::Value(Value::InlineTable(table)) if table.is_dotted()`.
func dottedInlineTable(item Item) *InlineTable {
	if value, ok := item.(*Item_Value); ok {
		if table, ok := value.F0.(*Value_InlineTable); ok && table.F0.IsDotted() {
			return &table.F0
		}
	}
	return nil
}

// SortValuesBy is InlineTable::sort_values_by: sort Key/Value Pairs of the
// table using the using the comparison function `compare`.
//
// The comparison function receives two key and value pairs to compare (you
// can sort by keys or values or their combination as needed).
func (t *InlineTable) SortValuesBy(compare func(k1 *Key, v1 Value, k2 *Key, v2 Value) int) {
	t.sortValuesByInternal(compare)
}

func (t *InlineTable) sortValuesByInternal(compare func(k1 *Key, v1 Value, k2 *Key, v2 Value) int) {
	t.items.sortBy(func(val1, val2 *tableKeyValue) int {
		v1, ok1 := val1.value.AsValue().Get()
		v2, ok2 := val2.value.AsValue().Get()
		switch {
		case ok1 && ok2:
			return compare(&val1.key, v1, &val2.key, v2)
		case ok1 && !ok2:
			return 1
		case !ok1 && ok2:
			return -1
		default:
			return 0
		}
	})
	for _, kv := range t.items.entries {
		if table := dottedInlineTable(kv.value); table != nil {
			table.sortValuesByInternal(compare)
		}
	}
}

// If a table has no key/value pairs and implicit, it will not be displayed.
func (t *InlineTable) setImplicit(implicit bool) {
	t.implicit = implicit
}

// If a table has no key/value pairs and implicit, it will not be displayed.
func (t *InlineTable) isImplicit() bool {
	return t.implicit
}

// SetDotted is InlineTable::set_dotted: change this table's dotted status.
func (t *InlineTable) SetDotted(yes bool) {
	t.dotted = yes
}

// IsDotted is InlineTable::is_dotted: check if this is a wrapper for dotted
// keys, rather than a standard table.
func (t *InlineTable) IsDotted() bool {
	return t.dotted
}

// DecorMut is InlineTable::decor_mut: returns the surrounding whitespace.
func (t *InlineTable) DecorMut() *Decor {
	return &t.decor
}

// Decor is InlineTable::decor: returns the surrounding whitespace.
func (t *InlineTable) Decor() *Decor {
	return &t.decor
}

// KeyDecorMut is InlineTable::key_decor_mut: returns the decor associated
// with a given key of the table.
func (t *InlineTable) KeyDecorMut(key string) rs.Option[*Decor] {
	if kv := t.items.get(key); kv != nil {
		return rs.Some(&kv.key.decor)
	}
	return rs.None[*Decor]()
}

// KeyDecor is InlineTable::key_decor: returns the decor associated with a
// given key of the table.
func (t *InlineTable) KeyDecor(key string) rs.Option[*Decor] {
	return t.KeyDecorMut(key)
}

// SetPreamble is InlineTable::set_preamble: set whitespace after before
// element (a string or a RawString).
func (t *InlineTable) SetPreamble(preamble any) {
	t.preamble = intoRawString(preamble)
}

// Preamble is InlineTable::preamble: whitespace after before element.
func (t *InlineTable) Preamble() *RawString {
	return &t.preamble
}

func (t *InlineTable) despan(input string) {
	t.span = rs.None[Range]()
	t.decor.despan(input)
	t.preamble.despan(input)
	for _, kv := range t.items.entries {
		kv.key.despan(input)
		kv.value.despan(input)
	}
}

// Iter is InlineTable::iter: an iterator over key/value pairs.
func (t *InlineTable) Iter() iter.Seq2[string, Value] {
	return func(yield func(string, Value) bool) {
		for _, kv := range t.items.snapshot() {
			if value, ok := kv.value.(*Item_Value); ok {
				if !yield(kv.mapKey, value.F0) {
					return
				}
			}
		}
	}
}

// IterMut is InlineTable::iter_mut: an iterator over key/value pairs.
// Assigning through a yielded *Value replaces the value in the table.
func (t *InlineTable) IterMut() iter.Seq2[KeyMut, *Value] {
	return func(yield func(KeyMut, *Value) bool) {
		for _, kv := range t.items.snapshot() {
			if value, ok := kv.value.(*Item_Value); ok {
				if !yield(kv.key.AsMut(), &value.F0) {
					return
				}
			}
		}
	}
}

// Len is InlineTable::len: the number of key/value pairs.
func (t *InlineTable) Len() int {
	count := 0
	for _, kv := range t.items.entries {
		if kv.value.IsValue() {
			count++
		}
	}
	return count
}

// IsEmpty is InlineTable::is_empty: returns true iff the table is empty.
func (t *InlineTable) IsEmpty() bool {
	return t.Len() == 0
}

// Clear is InlineTable::clear: clears the table, removing all key-value
// pairs. Keeps the allocated memory for reuse.
func (t *InlineTable) Clear() {
	t.items.clear()
}

// Entry is InlineTable::entry: gets the given key's corresponding entry in
// the Table for in-place manipulation.
func (t *InlineTable) Entry(key string) InlineEntry {
	if kv := t.items.get(key); kv != nil {
		// Ensure it is a `Value` to simplify `InlineOccupiedEntry`'s code.
		ensureInlineValue(kv)
		return &InlineEntry_Occupied{F0: InlineOccupiedEntry{items: &t.items, entry: kv}}
	}
	return &InlineEntry_Vacant{F0: InlineVacantEntry{items: &t.items, entryKey: key, key: rs.None[Key]()}}
}

func ensureInlineValue(kv *tableKeyValue) {
	scratch := kv.value
	kv.value = &Item_None{}
	value, err := scratch.IntoValue()
	if err != nil {
		// HACK: `Item::None` is a corner case of a corner case, let's just pick a
		// "safe" value
		value = &Value_InlineTable{F0: InlineTableDefault()}
	}
	kv.value = &Item_Value{F0: value}
}

// EntryFormat is InlineTable::entry_format: gets the given key's
// corresponding entry in the Table for in-place manipulation.
func (t *InlineTable) EntryFormat(key *Key) InlineEntry {
	// Accept a `&Key` to be consistent with `entry`
	if kv := t.items.get(key.Get()); kv != nil {
		// Ensure it is a `Value` to simplify `InlineOccupiedEntry`'s code.
		ensureInlineValue(kv)
		return &InlineEntry_Occupied{F0: InlineOccupiedEntry{items: &t.items, entry: kv}}
	}
	return &InlineEntry_Vacant{F0: InlineVacantEntry{items: &t.items, entryKey: key.Get(), key: rs.Some(key.Clone())}}
}

// Get is InlineTable::get: return an optional reference to the value at the
// given the key.
func (t *InlineTable) Get(key string) rs.Option[Value] {
	if kv := t.items.get(key); kv != nil {
		return kv.value.AsValue()
	}
	return rs.None[Value]()
}

// GetMut is InlineTable::get_mut: return an optional mutable reference to
// the value at the given the key.
func (t *InlineTable) GetMut(key string) rs.Option[*Value] {
	if kv := t.items.get(key); kv != nil {
		return kv.value.AsValueMut()
	}
	return rs.None[*Value]()
}

// GetKeyValue is InlineTable::get_key_value: return references to the
// key-value pair stored for key, if it is present, else None.
func (t *InlineTable) GetKeyValue(key string) rs.Option[rs.Tuple2[*Key, Item]] {
	if kv := t.items.get(key); kv != nil && !kv.value.IsNone() {
		return rs.Some(rs.T2(&kv.key, kv.value))
	}
	return rs.None[rs.Tuple2[*Key, Item]]()
}

// GetKeyValueMut is InlineTable::get_key_value_mut: return mutable
// references to the key-value pair stored for key, if it is present, else
// None.
func (t *InlineTable) GetKeyValueMut(key string) rs.Option[rs.Tuple2[KeyMut, *Item]] {
	if kv := t.items.get(key); kv != nil && !kv.value.IsNone() {
		return rs.Some(rs.T2(kv.key.AsMut(), &kv.value))
	}
	return rs.None[rs.Tuple2[KeyMut, *Item]]()
}

// ContainsKey is InlineTable::contains_key: returns true iff the table
// contains given key.
func (t *InlineTable) ContainsKey(key string) bool {
	if kv := t.items.get(key); kv != nil {
		return kv.value.IsValue()
	}
	return false
}

// GetOrInsert is InlineTable::get_or_insert: inserts a key/value pair if the
// table does not contain the key. Returns a mutable reference to the
// corresponding value. The value is `V: Into<Value>` (see IntoValue).
func (t *InlineTable) GetOrInsert(key string, value any) *Value {
	kv := t.items.get(key)
	if kv == nil {
		t.items.insert(key, tableKeyValueNew(KeyNew(key), &Item_Value{F0: IntoValue(value)}))
		kv = t.items.get(key)
	}
	return kv.value.AsValueMut().Expect("non-value type in inline table")
}

// Insert is InlineTable::insert: inserts a key-value pair into the map.
func (t *InlineTable) Insert(key string, value Value) rs.Option[Value] {
	kv := tableKeyValueNew(KeyNew(key), &Item_Value{F0: value})
	if old, had := t.items.insert(key, kv); had {
		if oldValue, err := old.value.IntoValue(); err == nil {
			return rs.Some(oldValue)
		}
	}
	return rs.None[Value]()
}

// InsertFormatted is InlineTable::insert_formatted: inserts a key-value pair
// into the map.
func (t *InlineTable) InsertFormatted(key *Key, value Value) rs.Option[Value] {
	kv := tableKeyValueNew(key.Clone(), &Item_Value{F0: value})
	if old, had := t.items.insert(key.Get(), kv); had && old.value.IsValue() {
		oldValue, _ := old.value.IntoValue()
		return rs.Some(oldValue)
	}
	return rs.None[Value]()
}

// Remove is InlineTable::remove: removes an item given the key.
func (t *InlineTable) Remove(key string) rs.Option[Value] {
	if kv := t.items.shiftRemove(key); kv != nil {
		if value, err := kv.value.IntoValue(); err == nil {
			return rs.Some(value)
		}
	}
	return rs.None[Value]()
}

// RemoveEntry is InlineTable::remove_entry: removes a key from the map,
// returning the stored key and value if the key was previously in the map.
func (t *InlineTable) RemoveEntry(key string) rs.Option[rs.Tuple2[Key, Value]] {
	if kv := t.items.shiftRemove(key); kv != nil {
		if value, err := kv.value.IntoValue(); err == nil {
			return rs.Some(rs.T2(kv.key, value))
		}
	}
	return rs.None[rs.Tuple2[Key, Value]]()
}

// Retain is InlineTable::retain: retains only the elements specified by the
// `keep` predicate.
//
// In other words, remove all pairs `(key, value)` for which
// `keep(&key, &mut value)` returns `false`.
//
// The elements are visited in iteration order.
func (t *InlineTable) Retain(keep func(key string, value *Value) bool) {
	t.items.retain(func(key string, item *tableKeyValue) bool {
		if value, ok := item.value.AsValueMut().Get(); ok {
			return keep(key, value)
		}
		return false
	})
}

// Clone is the derived `Clone for InlineTable`.
func (t *InlineTable) Clone() InlineTable {
	cloned := *t
	cloned.items = t.items.clone()
	return cloned
}

// String is `impl Display for InlineTable`.
func (t *InlineTable) String() string {
	var buf strings.Builder
	encodeInlineTable(t, &buf, rs.None[string](), "", "")
	return buf.String()
}

// FmtDisplay is `impl Display for InlineTable`.
func (t *InlineTable) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(t.String()) }

// FmtDebug is the derived `Debug for InlineTable`.
func (t *InlineTable) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("InlineTable").Field("preamble", t.preamble).Field("implicit", t.implicit).
		Field("decor", t.decor).Field("span", t.span).Field("dotted", t.dotted).Field("items", &t.items).Finish()
}

// Extend is `impl Extend<(K, V)> for InlineTable` with K: Into<Key> (a
// string or a Key) and V: Into<Value> (see IntoValue).
func (t *InlineTable) Extend(pairs ...rs.Tuple2[any, any]) {
	for _, pair := range pairs {
		key := intoKey(pair.F0)
		value := tableKeyValueNew(key, &Item_Value{F0: IntoValue(pair.F1)})
		t.items.insert(value.key.Get(), value)
	}
}

// InlineTableFromIter is `impl FromIterator<(K, V)> for InlineTable` with
// K: Into<Key> (a string or a Key) and V: Into<Value> (see IntoValue).
func InlineTableFromIter(pairs ...rs.Tuple2[any, any]) InlineTable {
	table := InlineTableNew()
	table.Extend(pairs...)
	return table
}

// IntoIter is `impl IntoIterator for InlineTable`: the keys and values of
// the table.
func (t *InlineTable) IntoIter() iter.Seq2[InternalString, Value] {
	return func(yield func(InternalString, Value) bool) {
		for _, kv := range t.items.snapshot() {
			if value, ok := kv.value.(*Item_Value); ok {
				if !yield(kv.mapKey, value.F0) {
					return
				}
			}
		}
	}
}

func decorateInlineTable(table *InlineTable) {
	for _, kv := range table.items.entries {
		value, ok := kv.value.(*Item_Value)
		if !ok {
			continue
		}
		kv.key.decor.Clear()
		value.F0.DecorMut().Clear()
	}
}

// AsTableLike returns the table as a TableLike: `self as &mut dyn
// TableLike`. See TableLike for why this is a view and not the table itself.
func (t *InlineTable) AsTableLike() TableLike {
	return t.asTableLike()
}

func (t *InlineTable) asTableLike() *inlineTableLike {
	return &inlineTableLike{table: t}
}

// inlineTableLike is `impl TableLike for InlineTable`.
type inlineTableLike struct {
	table *InlineTable
}

var _ TableLike = (*inlineTableLike)(nil)

func (t *inlineTableLike) Iter() iter.Seq2[string, Item] {
	return func(yield func(string, Item) bool) {
		for _, kv := range t.table.items.snapshot() {
			if !yield(kv.mapKey, kv.value) {
				return
			}
		}
	}
}

func (t *inlineTableLike) IterMut() iter.Seq2[KeyMut, *Item] {
	return func(yield func(KeyMut, *Item) bool) {
		for _, kv := range t.table.items.snapshot() {
			if !yield(kv.key.AsMut(), &kv.value) {
				return
			}
		}
	}
}

// Len is the trait's default method: the number of nonempty items.
func (t *inlineTableLike) Len() int {
	count := 0
	for _, kv := range t.table.items.entries {
		if !kv.value.IsNone() {
			count++
		}
	}
	return count
}

// IsEmpty is the trait's default method.
func (t *inlineTableLike) IsEmpty() bool {
	return t.Len() == 0
}

func (t *inlineTableLike) Clear() {
	t.table.Clear()
}

func (t *inlineTableLike) Entry(key string) Entry {
	// Accept a `&str` rather than an owned type to keep `InternalString`, well, internal
	if kv := t.table.items.get(key); kv != nil {
		return &Entry_Occupied{F0: OccupiedEntry{items: &t.table.items, entry: kv}}
	}
	return &Entry_Vacant{F0: VacantEntry{items: &t.table.items, entryKey: key, key: rs.None[Key]()}}
}

func (t *inlineTableLike) EntryFormat(key *Key) Entry {
	// Accept a `&Key` to be consistent with `entry`
	if kv := t.table.items.get(key.Get()); kv != nil {
		return &Entry_Occupied{F0: OccupiedEntry{items: &t.table.items, entry: kv}}
	}
	return &Entry_Vacant{F0: VacantEntry{items: &t.table.items, entryKey: key.Get(), key: rs.Some(key.Clone())}}
}

func (t *inlineTableLike) Get(key string) rs.Option[Item] {
	if kv := t.table.items.get(key); kv != nil {
		return rs.Some(kv.value)
	}
	return rs.None[Item]()
}

func (t *inlineTableLike) GetMut(key string) rs.Option[*Item] {
	if kv := t.table.items.get(key); kv != nil {
		return rs.Some(&kv.value)
	}
	return rs.None[*Item]()
}

func (t *inlineTableLike) GetKeyValue(key string) rs.Option[rs.Tuple2[*Key, Item]] {
	return t.table.GetKeyValue(key)
}

func (t *inlineTableLike) GetKeyValueMut(key string) rs.Option[rs.Tuple2[KeyMut, *Item]] {
	return t.table.GetKeyValueMut(key)
}

func (t *inlineTableLike) ContainsKey(key string) bool {
	return t.table.ContainsKey(key)
}

func (t *inlineTableLike) Insert(key string, value Item) rs.Option[Item] {
	inner, err := value.IntoValue()
	if err != nil {
		// value.into_value().unwrap()
		rs.PanicAt(1, "called `Result::unwrap()` on an `Err` value: "+rs.DebugString(err.(*ItemError).Item))
	}
	if old, ok := t.table.Insert(key, inner).Get(); ok {
		return rs.Some[Item](&Item_Value{F0: old})
	}
	return rs.None[Item]()
}

func (t *inlineTableLike) Remove(key string) rs.Option[Item] {
	if old, ok := t.table.Remove(key).Get(); ok {
		return rs.Some[Item](&Item_Value{F0: old})
	}
	return rs.None[Item]()
}

func (t *inlineTableLike) GetValues() []KeyPathValue {
	return t.table.GetValues()
}

func (t *inlineTableLike) Fmt() {
	t.table.Fmt()
}

func (t *inlineTableLike) SortValues() {
	t.table.SortValues()
}

func (t *inlineTableLike) SetDotted(yes bool) {
	t.table.SetDotted(yes)
}

func (t *inlineTableLike) IsDotted() bool {
	return t.table.IsDotted()
}

func (t *inlineTableLike) KeyDecorMut(key string) rs.Option[*Decor] {
	return t.table.KeyDecorMut(key)
}

func (t *inlineTableLike) KeyDecor(key string) rs.Option[*Decor] {
	return t.table.KeyDecor(key)
}

// `{ key1 = value1, ... }`
const (
	defaultInlineKeyDecorPrefix = " "
	defaultInlineKeyDecorSuffix = " "
)

// InlineEntry is toml_edit::InlineEntry: a view into a single location in a
// map, which may be vacant or occupied. Its variants are
// *InlineEntry_Occupied and *InlineEntry_Vacant.
type InlineEntry interface {
	isInlineEntry()
	// Key is InlineEntry::key: returns the entry key.
	//
	//	use toml_edit::Table;
	//
	//	let mut map = Table::new();
	//
	//	assert_eq!("hello", map.entry("hello").key());
	Key() string
	// OrInsert is InlineEntry::or_insert: ensures a value is in the entry by
	// inserting the default if empty, and returns a mutable reference to the
	// value in the entry.
	OrInsert(fallback Value) *Value
	// OrInsertWith is InlineEntry::or_insert_with: ensures a value is in the
	// entry by inserting the result of the default function if empty, and
	// returns a mutable reference to the value in the entry.
	OrInsertWith(fallback func() Value) *Value
}

// InlineEntry_Occupied is InlineEntry::Occupied: an occupied Entry.
type InlineEntry_Occupied struct{ F0 InlineOccupiedEntry }

// InlineEntry_Vacant is InlineEntry::Vacant: a vacant Entry.
type InlineEntry_Vacant struct{ F0 InlineVacantEntry }

func (*InlineEntry_Occupied) isInlineEntry() {}
func (*InlineEntry_Vacant) isInlineEntry()   {}

// Key is InlineEntry::key.
func (e *InlineEntry_Occupied) Key() string { return e.F0.Key() }

// Key is InlineEntry::key.
func (e *InlineEntry_Vacant) Key() string { return e.F0.Key() }

// OrInsert is InlineEntry::or_insert.
func (e *InlineEntry_Occupied) OrInsert(Value) *Value { return e.F0.IntoMut() }

// OrInsert is InlineEntry::or_insert.
func (e *InlineEntry_Vacant) OrInsert(fallback Value) *Value { return e.F0.Insert(fallback) }

// OrInsertWith is InlineEntry::or_insert_with.
func (e *InlineEntry_Occupied) OrInsertWith(func() Value) *Value { return e.F0.IntoMut() }

// OrInsertWith is InlineEntry::or_insert_with.
func (e *InlineEntry_Vacant) OrInsertWith(fallback func() Value) *Value {
	return e.F0.Insert(fallback())
}

// InlineOccupiedEntry is toml_edit::InlineOccupiedEntry: a view into a
// single occupied location in a `IndexMap`.
type InlineOccupiedEntry struct {
	items *keyValuePairs
	entry *tableKeyValue
}

// Key is InlineOccupiedEntry::key: gets a reference to the entry key.
//
//	use toml_edit::Table;
//
//	let mut map = Table::new();
//
//	assert_eq!("foo", map.entry("foo").key());
func (e *InlineOccupiedEntry) Key() string {
	return e.entry.mapKey
}

// KeyMut is InlineOccupiedEntry::key_mut: gets a mutable reference to the
// entry key.
func (e *InlineOccupiedEntry) KeyMut() KeyMut {
	return e.entry.key.AsMut()
}

// Get is InlineOccupiedEntry::get: gets a reference to the value in the
// entry.
func (e *InlineOccupiedEntry) Get() Value {
	return e.entry.value.AsValue().Unwrap()
}

// GetMut is InlineOccupiedEntry::get_mut: gets a mutable reference to the
// value in the entry.
func (e *InlineOccupiedEntry) GetMut() *Value {
	return e.entry.value.AsValueMut().Unwrap()
}

// IntoMut is InlineOccupiedEntry::into_mut: converts the OccupiedEntry into
// a mutable reference to the value in the entry with a lifetime bound to the
// map itself.
func (e *InlineOccupiedEntry) IntoMut() *Value {
	return e.entry.value.AsValueMut().Unwrap()
}

// Insert is InlineOccupiedEntry::insert: sets the value of the entry, and
// returns the entry's old value.
func (e *InlineOccupiedEntry) Insert(value Value) Value {
	old := e.entry.value
	e.entry.value = &Item_Value{F0: value}
	return rs.Unwrap(old.IntoValue())
}

// Remove is InlineOccupiedEntry::remove: takes the value out of the entry,
// and returns it.
func (e *InlineOccupiedEntry) Remove() Value {
	return rs.Unwrap(e.items.shiftRemove(e.entry.mapKey).value.IntoValue())
}

// InlineVacantEntry is toml_edit::InlineVacantEntry: a view into a single
// empty location in a `IndexMap`.
type InlineVacantEntry struct {
	items    *keyValuePairs
	entryKey InternalString
	key      rs.Option[Key]
}

// Key is InlineVacantEntry::key: gets a reference to the entry key.
//
//	use toml_edit::Table;
//
//	let mut map = Table::new();
//
//	assert_eq!("foo", map.entry("foo").key());
func (e *InlineVacantEntry) Key() string {
	return e.entryKey
}

// Insert is InlineVacantEntry::insert: sets the value of the entry with the
// VacantEntry's key, and returns a mutable reference to it.
func (e *InlineVacantEntry) Insert(value Value) *Value {
	key := e.key.UnwrapOrElse(func() Key { return KeyNew(e.entryKey) })
	e.items.insert(e.entryKey, tableKeyValueNew(key, &Item_Value{F0: value}))
	return e.items.get(e.entryKey).value.AsValueMut().Unwrap()
}
