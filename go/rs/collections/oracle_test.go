package collections

// The Go side of the oracle module go/port/oracle/src/std_collections.rs:
// each script of operations that the Rust collections ran is run on the
// port, and what every step returns and leaves behind must be the same.

import (
	"encoding/binary"
	"encoding/json"
	"fmt"
	"iter"
	"slices"
	"strconv"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// oracleFnv is FNV-1a, 64 bits: the digest of a collection's content.
type oracleFnv uint64

func newOracleFnv() *oracleFnv {
	digest := oracleFnv(0xcbf29ce484222325)
	return &digest
}

func (d *oracleFnv) write(bytes []byte) {
	for _, b := range bytes {
		*d ^= oracleFnv(b)
		*d *= 0x100000001b3
	}
}

func (d *oracleFnv) writeInt(value int64) {
	d.write(binary.LittleEndian.AppendUint64(nil, uint64(value)))
}

func (d *oracleFnv) text() string {
	return fmt.Sprintf("%016x", uint64(*d))
}

// oracleItem is what the scripts need to know about a key or element type.
type oracleItem[T any] struct {
	parse func(json.RawMessage) T
	feed  func(*oracleFnv, T)
	// class is a number in 0..modulus that the predicates of retain and
	// extract_if are built on.
	class    func(T, int64) int64
	inverted func() (T, T)
	add      func(*T, int64)
	number   func(T) int64
}

func euclid(value, modulus int64) int64 {
	rest := value % modulus
	if rest < 0 {
		rest += modulus
	}
	return rest
}

var oracleInt = oracleItem[int64]{
	parse:    func(raw json.RawMessage) int64 { return decodeJSON[int64](raw) },
	feed:     func(digest *oracleFnv, value int64) { digest.writeInt(value) },
	class:    euclid,
	inverted: func() (int64, int64) { return 5, 3 },
	add:      func(value *int64, delta int64) { *value += delta },
	number:   func(value int64) int64 { return value },
}

var oracleByte = oracleItem[uint8]{
	parse:    func(raw json.RawMessage) uint8 { return uint8(decodeJSON[int64](raw)) },
	feed:     func(digest *oracleFnv, value uint8) { digest.writeInt(int64(value)) },
	class:    func(value uint8, modulus int64) int64 { return euclid(int64(value), modulus) },
	inverted: func() (uint8, uint8) { return 5, 3 },
	add:      func(value *uint8, delta int64) { *value = uint8(int64(*value) + delta) },
	number:   func(value uint8) int64 { return int64(value) },
}

var oracleStr = oracleItem[string]{
	parse: func(raw json.RawMessage) string { return decodeJSON[string](raw) },
	feed: func(digest *oracleFnv, value string) {
		digest.writeInt(int64(len(value)))
		digest.write([]byte(value))
	},
	class: func(value string, modulus int64) int64 {
		sum := int64(0)
		for index := 0; index < len(value); index++ {
			sum += int64(value[index])
		}
		return euclid(sum+int64(len(value)), modulus)
	},
	inverted: func() (string, string) { return "m", "c" },
	add:      func(value *string, delta int64) {},
	number:   func(value string) int64 { return int64(len(value)) },
}

func decodeJSON[T any](raw json.RawMessage) T {
	var value T
	if err := json.Unmarshal(raw, &value); err != nil {
		panic(fmt.Sprintf("oracle argument %s: %v", raw, err))
	}
	return value
}

func argInt(args []json.RawMessage, index int) int64 { return decodeJSON[int64](args[index]) }

func argIndex(args []json.RawMessage, index int) int { return int(decodeJSON[int64](args[index])) }

func argBound[T any](item oracleItem[T], args []json.RawMessage, index int) Bound[T] {
	parts := decodeJSON[[]json.RawMessage](args[index])
	switch decodeJSON[string](parts[0]) {
	case "i":
		return Included(item.parse(parts[1]))
	case "e":
		return Excluded(item.parse(parts[1]))
	default:
		return Unbounded[T]()
	}
}

func argIndexBound(args []json.RawMessage, index int) Bound[int] {
	parts := decodeJSON[[]json.RawMessage](args[index])
	switch decodeJSON[string](parts[0]) {
	case "i":
		return Included(int(decodeJSON[int64](parts[1])))
	case "e":
		return Excluded(int(decodeJSON[int64](parts[1])))
	default:
		return Unbounded[int]()
	}
}

func argPairs[K any](item oracleItem[K], args []json.RawMessage, index int) []rs.Tuple2[K, int64] {
	raw := decodeJSON[[][]json.RawMessage](args[index])
	pairs := make([]rs.Tuple2[K, int64], 0, len(raw))
	for _, pair := range raw {
		pairs = append(pairs, rs.T2(item.parse(pair[0]), decodeJSON[int64](pair[1])))
	}
	return pairs
}

func pairSeq[K any](pairs []rs.Tuple2[K, int64]) iter.Seq2[K, int64] {
	return func(yield func(K, int64) bool) {
		for _, pair := range pairs {
			if !yield(pair.F0, pair.F1) {
				return
			}
		}
	}
}

func argItems[T any](item oracleItem[T], args []json.RawMessage, index int) []T {
	raw := decodeJSON[[]json.RawMessage](args[index])
	elements := make([]T, 0, len(raw))
	for _, element := range raw {
		elements = append(elements, item.parse(element))
	}
	return elements
}

// The JSON forms of what a step returns. A list is never nil, so that an
// empty one is written `[]` as Rust writes it.

func jsonPair[K any](key K, value int64) any { return []any{key, value} }

func jsonOptionalPair[K any](key K, value *int64, ok bool) any {
	if !ok {
		return nil
	}
	return jsonPair(key, *value)
}

func jsonOptionalTuple[K any](option rs.Option[rs.Tuple2[K, int64]]) any {
	if pair, ok := option.Get(); ok {
		return jsonPair(pair.F0, pair.F1)
	}
	return nil
}

func jsonOption[T any](option rs.Option[T]) any {
	if value, ok := option.Get(); ok {
		return value
	}
	return nil
}

func jsonOptionalItem[T any](value T, ok bool) any {
	if !ok {
		return nil
	}
	return value
}

func jsonPointer[T any](value *T, ok bool) any {
	if !ok {
		return nil
	}
	return *value
}

func jsonPairList[K any](seq iter.Seq2[K, *int64]) []any {
	list := []any{}
	for key, value := range seq {
		list = append(list, jsonPair(key, *value))
	}
	return list
}

func jsonList[T any](seq iter.Seq[T]) []any {
	list := []any{}
	for element := range seq {
		list = append(list, element)
	}
	return list
}

func jsonPointerList[T any](seq iter.Seq[*T]) []any {
	list := []any{}
	for element := range seq {
		list = append(list, *element)
	}
	return list
}

func jsonSlice[T any](elements []T) []any {
	list := make([]any, 0, len(elements))
	for _, element := range elements {
		list = append(list, element)
	}
	return list
}

func sign(order int) int {
	switch {
	case order < 0:
		return -1
	case order > 0:
		return 1
	default:
		return 0
	}
}

// panics reports whether f panics.
func panics(f func()) (panicked bool) {
	defer func() {
		if recover() != nil {
			panicked = true
		}
	}()
	f()
	return false
}

// runScript runs the steps of a request, catching the panic of each step as
// the Rust side does, and returns the reply.
func runScript(t *testing.T, request json.RawMessage, step func(name string, args []json.RawMessage) any, state func(full bool) any) any {
	t.Helper()
	var decoded struct {
		State string              `json:"state"`
		Steps [][]json.RawMessage `json:"steps"`
	}
	if err := json.Unmarshal(request, &decoded); err != nil {
		t.Fatalf("oracle request %s: %v", request, err)
	}
	results := make([]any, 0, len(decoded.Steps))
	for _, entry := range decoded.Steps {
		name := decodeJSON[string](entry[0])
		returned := oracle.Catch(func() any { return step(name, entry[1:]) })
		results = append(results, []any{returned, state(decoded.State == "full")})
	}
	return map[string]any{"steps": results}
}

type unknownStep string

func mapState[K any](item oracleItem[K], seq iter.Seq2[K, *int64], length int, full bool) any {
	if full {
		return jsonPairList(seq)
	}
	digest := newOracleFnv()
	for key, value := range seq {
		item.feed(digest, key)
		digest.writeInt(*value)
	}
	return []any{length, digest.text()}
}

func setState[T any](item oracleItem[T], seq iter.Seq[T], length int, full bool) any {
	if full {
		return jsonList(seq)
	}
	digest := newOracleFnv()
	for element := range seq {
		item.feed(digest, element)
	}
	return []any{length, digest.text()}
}

func mapProbe[K any](item oracleItem[K], m *BTreeMap[K, int64]) bool {
	high, low := item.inverted()
	return panics(func() {
		for range m.Range(Included(high), Excluded(low)) {
		}
	})
}

func setProbe[T any](item oracleItem[T], s *BTreeSet[T]) bool {
	high, low := item.inverted()
	return panics(func() {
		for range s.Range(Included(high), Excluded(low)) {
		}
	})
}

func identity[T any](value T) T { return value }

func btreeMapStep[K any](item oracleItem[K], m *BTreeMap[K, int64], name string, args []json.RawMessage) any {
	key := func(index int) K { return item.parse(args[index]) }
	derived := func(k K) int64 { return item.class(k, 1000) }
	switch name {
	case "insert":
		return jsonOption(m.Insert(key(0), argInt(args, 1)))
	case "remove":
		return jsonOption(m.Remove(key(0)))
	case "remove_entry":
		return jsonOptionalTuple(m.RemoveEntry(key(0)))
	case "get":
		return jsonPointer(m.Get(key(0)))
	case "get_key_value":
		return jsonOptionalPair(m.GetKeyValue(key(0)))
	case "contains_key":
		return m.ContainsKey(key(0))
	case "get_mut_add":
		value, ok := m.GetMut(key(0))
		if !ok {
			return nil
		}
		*value += argInt(args, 1)
		return *value
	case "index":
		return *m.Index(key(0))
	case "first_key_value":
		return jsonOptionalPair(m.FirstKeyValue())
	case "last_key_value":
		return jsonOptionalPair(m.LastKeyValue())
	case "pop_first":
		return jsonOptionalTuple(m.PopFirst())
	case "pop_last":
		return jsonOptionalTuple(m.PopLast())
	case "first_entry_add", "last_entry_add":
		option := m.FirstEntry()
		if name == "last_entry_add" {
			option = m.LastEntry()
		}
		entry, ok := option.Get()
		if !ok {
			return nil
		}
		*entry.GetMut() += argInt(args, 0)
		return jsonPair(entry.Key(), *entry.Get())
	case "first_entry_remove":
		entry, ok := m.FirstEntry().Get()
		if !ok {
			return nil
		}
		return jsonPair(entry.RemoveEntry())
	case "last_entry_remove":
		entry, ok := m.LastEntry().Get()
		if !ok {
			return nil
		}
		return entry.Remove()
	case "entry_or_insert":
		value := m.Entry(key(0)).OrInsert(argInt(args, 1))
		*value++
		return *value
	case "entry_or_insert_with":
		called := false
		value := m.Entry(key(0)).OrInsertWith(func() int64 {
			called = true
			return argInt(args, 1)
		})
		*value++
		return []any{*value, called}
	case "entry_or_insert_with_key":
		return *m.Entry(key(0)).OrInsertWithKey(derived)
	case "entry_or_default":
		value := m.Entry(key(0)).OrDefault()
		*value++
		return *value
	case "entry_and_modify":
		delta := argInt(args, 1)
		return *m.Entry(key(0)).AndModify(func(value *int64) { *value += delta }).OrInsert(argInt(args, 2))
	case "entry_key":
		entry := m.Entry(key(0))
		return []any{entry.Key(), rs.Is[*BTreeMapEntry_Occupied[K, int64]](entry)}
	case "entry_insert_entry":
		entry := m.Entry(key(0)).InsertEntry(argInt(args, 1))
		return jsonPair(entry.Key(), *entry.Get())
	case "entry_match":
		mode := decodeJSON[string](args[1])
		value := argInt(args, 2)
		switch entry := m.Entry(key(0)).(type) {
		case *BTreeMapEntry_Occupied[K, int64]:
			switch mode {
			case "get":
				return []any{"o", entry.F0.Key(), *entry.F0.Get()}
			case "insert":
				return []any{"o", entry.F0.Insert(value)}
			case "remove":
				return []any{"o", entry.F0.Remove()}
			case "remove_entry":
				removedKey, removedValue := entry.F0.RemoveEntry()
				return []any{"o", removedKey, removedValue}
			case "into_mut":
				slot := entry.F0.IntoMut()
				*slot += value
				return []any{"o", *slot}
			}
		case *BTreeMapEntry_Vacant[K, int64]:
			switch mode {
			case "get", "remove", "remove_entry":
				return []any{"v", entry.F0.Key()}
			case "insert":
				return []any{"v", *entry.F0.Insert(value)}
			case "into_mut":
				return []any{"v", entry.F0.IntoKey()}
			}
		default:
			panic(rs.Unreachable(entry))
		}
	case "range":
		return jsonPairList(m.Range(argBound(item, args, 0), argBound(item, args, 1)))
	case "range_rev":
		return jsonPairList(m.RangeRev(argBound(item, args, 0), argBound(item, args, 1)))
	case "range_mut_add":
		delta := argInt(args, 2)
		count := 0
		for _, value := range m.RangeMut(argBound(item, args, 0), argBound(item, args, 1)) {
			*value += delta
			count++
		}
		return count
	case "retain":
		modulus, rest := argInt(args, 0), argInt(args, 1)
		m.Retain(func(k K, _ *int64) bool { return item.class(k, modulus) != rest })
		return nil
	case "retain_mut":
		modulus, rest, delta := argInt(args, 0), argInt(args, 1), argInt(args, 2)
		m.Retain(func(k K, value *int64) bool {
			*value += delta
			return item.class(k, modulus) != rest
		})
		return nil
	case "extract_if":
		modulus, rest := argInt(args, 2), argInt(args, 3)
		take := -1
		if string(args[4]) != "null" {
			take = argIndex(args, 4)
		}
		extracted := []any{}
		if take != 0 {
			pred := func(k K, value *int64) bool {
				*value++
				return item.class(k, modulus) == rest
			}
			for k, value := range m.ExtractIf(argBound(item, args, 0), argBound(item, args, 1), pred) {
				extracted = append(extracted, jsonPair(k, value))
				if len(extracted) == take {
					break
				}
			}
		} else {
			// Rust looks the bounds up even when nothing is taken.
			argBound(item, args, 0)
			argBound(item, args, 1)
		}
		return extracted
	case "split_off":
		other := m.SplitOff(key(0))
		other.check(nil)
		return []any{jsonPairList(other.Iter()), mapProbe(item, &other)}
	case "append":
		other := BTreeMapFromIter(pairSeq(argPairs(item, args, 0)))
		m.Append(&other)
		return []any{other.Len(), mapProbe(item, &other)}
	case "extend":
		m.Extend(pairSeq(argPairs(item, args, 0)))
		return nil
	case "from_iter":
		*m = BTreeMapFromIter(pairSeq(argPairs(item, args, 0)))
		return nil
	case "clear":
		m.Clear()
		return nil
	case "clone":
		*m = m.cloneWith(identity[K], identity[int64])
		return nil
	case "take":
		taken := *m
		*m = BTreeMapNew[K, int64]()
		return taken.Len()
	case "iter_rev":
		return jsonPairList(m.IterRev())
	case "keys":
		return jsonList(m.Keys())
	case "keys_rev":
		return jsonList(m.KeysRev())
	case "values":
		return jsonPointerList(m.Values())
	case "values_rev":
		return jsonPointerList(m.ValuesRev())
	case "values_mut_add":
		delta := argInt(args, 0)
		for value := range m.ValuesMut() {
			*value += delta
		}
		return nil
	case "len":
		return []any{m.Len(), m.IsEmpty()}
	case "cmp":
		other := BTreeMapFromIter(pairSeq(argPairs(item, args, 0)))
		return []any{m.Equal(&other), sign(m.Compare(&other))}
	case "probe":
		return mapProbe(item, m)
	}
	panic(unknownStep(name))
}

func runBTreeMap[K any](t *testing.T, item oracleItem[K], request json.RawMessage) any {
	var m BTreeMap[K, int64]
	return runScript(t, request, func(name string, args []json.RawMessage) any {
		returned := btreeMapStep(item, &m, name, args)
		m.check(t)
		return returned
	}, func(full bool) any { return mapState(item, m.Iter(), m.Len(), full) })
}

func btreeSetStep[T any](item oracleItem[T], s *BTreeSet[T], name string, args []json.RawMessage) any {
	element := func(index int) T { return item.parse(args[index]) }
	other := func(index int) *BTreeSet[T] {
		set := BTreeSetFromIter(slices.Values(argItems(item, args, index)))
		return &set
	}
	// The operators clone every element with rs.Clone; the scripts run what
	// is behind them.
	operator := func(seq iter.Seq[T]) any {
		result := btreeSetFromSortedIter(seq)
		result.m.check(nil)
		return []any{jsonList(result.Iter()), setProbe(item, &result)}
	}
	switch name {
	case "insert":
		return s.Insert(element(0))
	case "replace":
		return jsonOption(s.Replace(element(0)))
	case "remove":
		return s.Remove(element(0))
	case "take":
		return jsonOption(s.Take(element(0)))
	case "contains":
		return s.Contains(element(0))
	case "get":
		return jsonOptionalItem(s.Get(element(0)))
	case "first":
		return jsonOptionalItem(s.First())
	case "last":
		return jsonOptionalItem(s.Last())
	case "pop_first":
		return jsonOption(s.PopFirst())
	case "pop_last":
		return jsonOption(s.PopLast())
	case "range":
		return jsonList(s.Range(argBound(item, args, 0), argBound(item, args, 1)))
	case "range_rev":
		return jsonList(s.RangeRev(argBound(item, args, 0), argBound(item, args, 1)))
	case "retain":
		modulus, rest := argInt(args, 0), argInt(args, 1)
		s.Retain(func(value T) bool { return item.class(value, modulus) != rest })
		return nil
	case "extract_if":
		modulus, rest := argInt(args, 2), argInt(args, 3)
		take := -1
		if string(args[4]) != "null" {
			take = argIndex(args, 4)
		}
		extracted := []any{}
		if take != 0 {
			pred := func(value T) bool { return item.class(value, modulus) == rest }
			for value := range s.ExtractIf(argBound(item, args, 0), argBound(item, args, 1), pred) {
				extracted = append(extracted, value)
				if len(extracted) == take {
					break
				}
			}
		}
		return extracted
	case "split_off":
		split := s.SplitOff(element(0))
		split.m.check(nil)
		return []any{jsonList(split.Iter()), setProbe(item, &split)}
	case "append":
		appended := other(0)
		s.Append(appended)
		return []any{appended.Len(), setProbe(item, appended)}
	case "extend":
		s.Extend(slices.Values(argItems(item, args, 0)))
		return nil
	case "from_iter":
		*s = *other(0)
		return nil
	case "clear":
		s.Clear()
		return nil
	case "clone":
		*s = s.cloneWith(identity[T])
		return nil
	case "iter_rev":
		return jsonList(s.IterRev())
	case "len":
		return []any{s.Len(), s.IsEmpty()}
	case "cmp":
		compared := other(0)
		return []any{s.Equal(compared), sign(s.Compare(compared))}
	case "probe":
		return setProbe(item, s)
	case "union":
		return jsonList(s.Union(other(0)))
	case "intersection":
		return jsonList(s.Intersection(other(0)))
	case "difference":
		return jsonList(s.Difference(other(0)))
	case "symmetric_difference":
		return jsonList(s.SymmetricDifference(other(0)))
	case "is_subset":
		return s.IsSubset(other(0))
	case "is_superset":
		return s.IsSuperset(other(0))
	case "is_disjoint":
		return s.IsDisjoint(other(0))
	case "bitor":
		return operator(s.Union(other(0)))
	case "bitand":
		return operator(s.Intersection(other(0)))
	case "bitxor":
		return operator(s.SymmetricDifference(other(0)))
	case "sub":
		return operator(s.Difference(other(0)))
	}
	panic(unknownStep(name))
}

func runBTreeSet[T any](t *testing.T, item oracleItem[T], request json.RawMessage) any {
	var s BTreeSet[T]
	return runScript(t, request, func(name string, args []json.RawMessage) any {
		returned := btreeSetStep(item, &s, name, args)
		s.m.check(t)
		return returned
	}, func(full bool) any { return setState(item, s.Iter(), s.Len(), full) })
}

// sortedPairs is the content of a hash map in key order, which is how the
// Rust side reports a collection that has no order of its own.
func sortedPairs[K any](seq iter.Seq2[K, int64]) []any {
	var pairs []rs.Tuple2[K, int64]
	for key, value := range seq {
		pairs = append(pairs, rs.T2(key, value))
	}
	slices.SortFunc(pairs, func(a, b rs.Tuple2[K, int64]) int { return rs.Cmp(a.F0, b.F0) })
	list := []any{}
	for _, pair := range pairs {
		list = append(list, jsonPair(pair.F0, pair.F1))
	}
	return list
}

func sortedItems[T any](seq iter.Seq[T]) []any {
	elements := slices.Collect(seq)
	slices.SortFunc(elements, rs.Cmp[T])
	return jsonSlice(elements)
}

func hashMapStep[K comparable](item oracleItem[K], m *HashMap[K, int64], name string, args []json.RawMessage) any {
	key := func(index int) K { return item.parse(args[index]) }
	derived := func(k K) int64 { return item.class(k, 1000) }
	switch name {
	case "insert":
		return jsonOption(m.Insert(key(0), argInt(args, 1)))
	case "remove":
		return jsonOption(m.Remove(key(0)))
	case "remove_entry":
		return jsonOptionalTuple(m.RemoveEntry(key(0)))
	case "get":
		return jsonPointer(m.Get(key(0)))
	case "get_key_value":
		return jsonOptionalPair(m.GetKeyValue(key(0)))
	case "contains_key":
		return m.ContainsKey(key(0))
	case "get_mut_add":
		value, ok := m.GetMut(key(0))
		if !ok {
			return nil
		}
		*value += argInt(args, 1)
		return *value
	case "index":
		return *m.Index(key(0))
	case "entry_or_insert":
		value := m.Entry(key(0)).OrInsert(argInt(args, 1))
		*value++
		return *value
	case "entry_or_insert_with":
		called := false
		value := m.Entry(key(0)).OrInsertWith(func() int64 {
			called = true
			return argInt(args, 1)
		})
		*value++
		return []any{*value, called}
	case "entry_or_insert_with_key":
		return *m.Entry(key(0)).OrInsertWithKey(derived)
	case "entry_or_default":
		value := m.Entry(key(0)).OrDefault()
		*value++
		return *value
	case "entry_and_modify":
		delta := argInt(args, 1)
		return *m.Entry(key(0)).AndModify(func(value *int64) { *value += delta }).OrInsert(argInt(args, 2))
	case "entry_key":
		entry := m.Entry(key(0))
		return []any{entry.Key(), rs.Is[*HashMapEntry_Occupied[K, int64]](entry)}
	case "entry_insert_entry":
		entry := m.Entry(key(0)).InsertEntry(argInt(args, 1))
		return jsonPair(entry.Key(), *entry.Get())
	case "entry_match":
		mode := decodeJSON[string](args[1])
		value := argInt(args, 2)
		switch entry := m.Entry(key(0)).(type) {
		case *HashMapEntry_Occupied[K, int64]:
			switch mode {
			case "get":
				return []any{"o", entry.F0.Key(), *entry.F0.Get()}
			case "insert":
				return []any{"o", entry.F0.Insert(value)}
			case "remove":
				return []any{"o", entry.F0.Remove()}
			case "remove_entry":
				removedKey, removedValue := entry.F0.RemoveEntry()
				return []any{"o", removedKey, removedValue}
			case "into_mut":
				slot := entry.F0.IntoMut()
				*slot += value
				return []any{"o", *slot}
			}
		case *HashMapEntry_Vacant[K, int64]:
			switch mode {
			case "get", "remove", "remove_entry":
				return []any{"v", entry.F0.Key()}
			case "insert":
				return []any{"v", *entry.F0.Insert(value)}
			case "into_mut":
				return []any{"v", entry.F0.IntoKey()}
			}
		default:
			panic(rs.Unreachable(entry))
		}
	case "retain":
		modulus, rest := argInt(args, 0), argInt(args, 1)
		m.Retain(func(k K, _ *int64) bool { return item.class(k, modulus) != rest })
		return nil
	case "retain_mut":
		modulus, rest, delta := argInt(args, 0), argInt(args, 1), argInt(args, 2)
		m.Retain(func(k K, value *int64) bool {
			*value += delta
			return item.class(k, modulus) != rest
		})
		return nil
	case "extract_if":
		modulus, rest := argInt(args, 0), argInt(args, 1)
		return sortedPairs(m.ExtractIf(func(k K, _ *int64) bool { return item.class(k, modulus) == rest }))
	case "drain":
		return sortedPairs(m.Drain())
	case "extend":
		m.Extend(pairSeq(argPairs(item, args, 0)))
		return nil
	case "from_iter":
		*m = HashMapFromIter(pairSeq(argPairs(item, args, 0)))
		return nil
	case "with_capacity":
		*m = HashMapWithCapacity[K, int64](argIndex(args, 0))
		return nil
	case "clear":
		m.Clear()
		return nil
	case "clone":
		*m = m.cloneWith(identity[K], identity[int64])
		return nil
	case "keys":
		return sortedItems(m.Keys())
	case "values":
		values := []int64{}
		for value := range m.Values() {
			values = append(values, *value)
		}
		slices.Sort(values)
		return values
	case "values_mut_add":
		delta := argInt(args, 0)
		for value := range m.ValuesMut() {
			*value += delta
		}
		return nil
	case "iter_mut_add":
		delta := argInt(args, 0)
		for k, value := range m.IterMut() {
			*value += delta + item.class(k, 3)
		}
		return nil
	case "len":
		return []any{m.Len(), m.IsEmpty()}
	case "eq":
		other := HashMapFromIter(pairSeq(argPairs(item, args, 0)))
		return m.Equal(&other)
	}
	panic(unknownStep(name))
}

func runHashMap[K comparable](t *testing.T, item oracleItem[K], request json.RawMessage) any {
	var m HashMap[K, int64]
	return runScript(t, request, func(name string, args []json.RawMessage) any {
		return hashMapStep(item, &m, name, args)
	}, func(full bool) any { return mapState(item, m.IterSorted(), m.Len(), full) })
}

func hashSetStep[T comparable](item oracleItem[T], s *HashSet[T], name string, args []json.RawMessage) any {
	element := func(index int) T { return item.parse(args[index]) }
	other := func(index int) *HashSet[T] {
		set := HashSetFromIter(slices.Values(argItems(item, args, index)))
		return &set
	}
	switch name {
	case "insert":
		return s.Insert(element(0))
	case "replace":
		return jsonOption(s.Replace(element(0)))
	case "remove":
		return s.Remove(element(0))
	case "take":
		return jsonOption(s.Take(element(0)))
	case "contains":
		return s.Contains(element(0))
	case "get":
		return jsonOptionalItem(s.Get(element(0)))
	case "retain":
		modulus, rest := argInt(args, 0), argInt(args, 1)
		s.Retain(func(value T) bool { return item.class(value, modulus) != rest })
		return nil
	case "extract_if":
		modulus, rest := argInt(args, 0), argInt(args, 1)
		return sortedItems(s.ExtractIf(func(value T) bool { return item.class(value, modulus) == rest }))
	case "drain":
		return sortedItems(s.Drain())
	case "extend":
		s.Extend(slices.Values(argItems(item, args, 0)))
		return nil
	case "from_iter":
		*s = *other(0)
		return nil
	case "clear":
		s.Clear()
		return nil
	case "clone":
		*s = s.cloneWith(identity[T])
		return nil
	case "len":
		return []any{s.Len(), s.IsEmpty()}
	case "eq":
		return s.Equal(other(0))
	case "union", "bitor":
		return sortedItems(s.Union(other(0)))
	case "intersection", "bitand":
		return sortedItems(s.Intersection(other(0)))
	case "difference", "sub":
		return sortedItems(s.Difference(other(0)))
	case "symmetric_difference", "bitxor":
		return sortedItems(s.SymmetricDifference(other(0)))
	case "is_subset":
		return s.IsSubset(other(0))
	case "is_superset":
		return s.IsSuperset(other(0))
	case "is_disjoint":
		return s.IsDisjoint(other(0))
	}
	panic(unknownStep(name))
}

func runHashSet[T comparable](t *testing.T, item oracleItem[T], request json.RawMessage) any {
	var s HashSet[T]
	return runScript(t, request, func(name string, args []json.RawMessage) any {
		return hashSetStep(item, &s, name, args)
	}, func(full bool) any { return setState(item, s.IterSorted(), s.Len(), full) })
}

// vecDequeState is the two slices of the deque and its capacity: the port
// keeps the same buffer layout as Rust, and this is how that is checked.
func vecDequeState[T any](item oracleItem[T], d *VecDeque[T], full bool) any {
	front, back := d.AsSlices()
	if full {
		return []any{jsonSlice(front), jsonSlice(back), d.Capacity()}
	}
	digest := newOracleFnv()
	for element := range d.Iter() {
		item.feed(digest, *element)
	}
	return []any{len(front), len(back), d.Capacity(), digest.text()}
}

// rotatedDeque is `rotated` of the Rust side: a deque with the given
// elements whose storage wraps around after rotation of them.
func rotatedDeque[T any](elements []T, rotation int) VecDeque[T] {
	deque := VecDequeWithCapacity[T](len(elements))
	split := 0
	if len(elements) != 0 {
		split = rotation % len(elements)
	}
	for _, element := range elements[split:] {
		deque.PushBack(element)
	}
	for index := split - 1; index >= 0; index-- {
		deque.PushFront(elements[index])
	}
	return deque
}

func vecDequeStep[T any](item oracleItem[T], d *VecDeque[T], name string, args []json.RawMessage) any {
	element := func(index int) T { return item.parse(args[index]) }
	found := func(at int, ok bool) any { return []any{at, ok} }
	switch name {
	case "push_back":
		d.PushBack(element(0))
		return nil
	case "push_front":
		d.PushFront(element(0))
		return nil
	case "push_back_mut_add":
		slot := d.PushBackMut(element(0))
		item.add(slot, argInt(args, 1))
		return *slot
	case "push_front_mut_add":
		slot := d.PushFrontMut(element(0))
		item.add(slot, argInt(args, 1))
		return *slot
	case "pop_back":
		return jsonOption(d.PopBack())
	case "pop_front":
		return jsonOption(d.PopFront())
	case "pop_back_if":
		limit := argInt(args, 0)
		return jsonOption(d.PopBackIf(func(value *T) bool { return item.number(*value) < limit }))
	case "pop_front_if":
		limit := argInt(args, 0)
		return jsonOption(d.PopFrontIf(func(value *T) bool { return item.number(*value) < limit }))
	case "insert":
		d.Insert(argIndex(args, 0), element(1))
		return nil
	case "insert_mut_add":
		slot := d.InsertMut(argIndex(args, 0), element(1))
		item.add(slot, argInt(args, 2))
		return *slot
	case "remove":
		return jsonOption(d.Remove(argIndex(args, 0)))
	case "swap":
		d.Swap(argIndex(args, 0), argIndex(args, 1))
		return nil
	case "swap_remove_back":
		return jsonOption(d.SwapRemoveBack(argIndex(args, 0)))
	case "swap_remove_front":
		return jsonOption(d.SwapRemoveFront(argIndex(args, 0)))
	case "truncate":
		d.Truncate(argIndex(args, 0))
		return nil
	case "retain_back":
		d.RetainBack(argIndex(args, 0))
		return nil
	case "clear":
		d.Clear()
		return nil
	case "reserve":
		d.Reserve(argIndex(args, 0))
		return nil
	case "reserve_exact":
		d.ReserveExact(argIndex(args, 0))
		return nil
	case "shrink_to_fit":
		d.ShrinkToFit()
		return nil
	case "shrink_to":
		d.ShrinkTo(argIndex(args, 0))
		return nil
	case "rotate_left":
		d.RotateLeft(argIndex(args, 0))
		return nil
	case "rotate_right":
		d.RotateRight(argIndex(args, 0))
		return nil
	case "make_contiguous":
		return jsonSlice(d.MakeContiguous())
	case "drain":
		return jsonList(d.Drain(argIndexBound(args, 0), argIndexBound(args, 1)))
	case "drain_rev":
		return jsonList(d.DrainRev(argIndexBound(args, 0), argIndexBound(args, 1)))
	case "split_off":
		other := d.SplitOff(argIndex(args, 0))
		other.check(nil)
		return vecDequeState(item, &other, true)
	case "append":
		other := rotatedDeque(argItems(item, args, 0), argIndex(args, 1))
		d.Append(&other)
		other.check(nil)
		return vecDequeState(item, &other, true)
	case "extend":
		d.Extend(slices.Values(argItems(item, args, 0)))
		return nil
	case "resize":
		d.resize(argIndex(args, 0), element(1), identity[T])
		return nil
	case "resize_with":
		next := element(1)
		d.ResizeWith(argIndex(args, 0), func() T {
			value := next
			item.add(&next, 1)
			return value
		})
		return nil
	case "retain":
		modulus, rest := argInt(args, 0), argInt(args, 1)
		d.Retain(func(value *T) bool { return item.class(*value, modulus) != rest })
		return nil
	case "retain_mut":
		modulus, rest, delta := argInt(args, 0), argInt(args, 1), argInt(args, 2)
		d.RetainMut(func(value *T) bool {
			keep := item.class(*value, modulus) != rest
			item.add(value, delta)
			return keep
		})
		return nil
	case "range":
		return jsonPointerList(d.Range(argIndexBound(args, 0), argIndexBound(args, 1)))
	case "range_rev":
		return jsonPointerList(d.RangeRev(argIndexBound(args, 0), argIndexBound(args, 1)))
	case "range_mut_add":
		delta := argInt(args, 2)
		count := 0
		for value := range d.RangeMut(argIndexBound(args, 0), argIndexBound(args, 1)) {
			item.add(value, delta)
			count++
		}
		return count
	case "get":
		return jsonPointer(d.Get(argIndex(args, 0)))
	case "get_mut_add":
		value, ok := d.GetMut(argIndex(args, 0))
		if !ok {
			return nil
		}
		item.add(value, argInt(args, 1))
		return *value
	case "front":
		return jsonPointer(d.Front())
	case "back":
		return jsonPointer(d.Back())
	case "front_mut_add", "back_mut_add":
		value, ok := d.FrontMut()
		if name == "back_mut_add" {
			value, ok = d.BackMut()
		}
		if !ok {
			return nil
		}
		item.add(value, argInt(args, 0))
		return *value
	case "index":
		return *d.Index(argIndex(args, 0))
	case "index_mut_add":
		slot := d.IndexMut(argIndex(args, 0))
		item.add(slot, argInt(args, 1))
		return *slot
	case "contains":
		return d.Contains(element(0))
	case "binary_search":
		return found(d.BinarySearch(element(0)))
	case "binary_search_by_key":
		return found(VecDequeBinarySearchByKey(d, argInt(args, 0), func(value *T) int64 { return item.number(*value) / 2 }))
	case "partition_point":
		limit := argInt(args, 0)
		return d.PartitionPoint(func(value *T) bool { return item.number(*value) < limit })
	case "iter":
		return jsonPointerList(d.Iter())
	case "iter_rev":
		return jsonPointerList(d.IterRev())
	case "iter_mut_add":
		delta := argInt(args, 0)
		for value := range d.IterMut() {
			item.add(value, delta)
		}
		return nil
	case "len":
		return []any{d.Len(), d.IsEmpty()}
	case "eq":
		return d.EqualSlice(argItems(item, args, 0))
	case "eq_deque":
		other := rotatedDeque(argItems(item, args, 0), argIndex(args, 1))
		return d.Equal(&other)
	case "cmp":
		other := rotatedDeque(argItems(item, args, 0), argIndex(args, 1))
		return sign(d.Compare(&other))
	case "from":
		*d = VecDequeFrom(argItems(item, args, 0)...)
		d.ShrinkToFit()
		return nil
	case "from_iter":
		*d = VecDequeFromIter(slices.Values(argItems(item, args, 0)))
		return nil
	case "with_capacity":
		*d = VecDequeWithCapacity[T](argIndex(args, 0))
		return nil
	case "new":
		*d = VecDequeNew[T]()
		return nil
	case "clone":
		*d = d.cloneWith(identity[T])
		return nil
	case "into_vec":
		taken := *d
		*d = VecDequeNew[T]()
		return jsonSlice(VecFromVecDeque(taken))
	}
	panic(unknownStep(name))
}

func runVecDeque[T any](t *testing.T, item oracleItem[T], request json.RawMessage) any {
	var d VecDeque[T]
	return runScript(t, request, func(name string, args []json.RawMessage) any {
		returned := vecDequeStep(item, &d, name, args)
		d.check(t)
		return returned
	}, func(full bool) any { return vecDequeState(item, &d, full) })
}

func oracleAnswer(t *testing.T) func(request json.RawMessage) any {
	return func(request json.RawMessage) any {
		var header struct {
			Op   string `json:"op"`
			Key  string `json:"key"`
			Elem string `json:"elem"`
		}
		if err := json.Unmarshal(request, &header); err != nil {
			t.Fatalf("oracle request %s: %v", request, err)
		}
		defer func() {
			if recovered := recover(); recovered != nil {
				if name, unknown := recovered.(unknownStep); unknown {
					t.Fatalf("the Go side has no step %q", string(name))
				}
				panic(recovered)
			}
		}()
		switch header.Op + "/" + header.Key + header.Elem {
		case "btree_map/int":
			return runBTreeMap(t, oracleInt, request)
		case "btree_map/str":
			return runBTreeMap(t, oracleStr, request)
		case "btree_set/int":
			return runBTreeSet(t, oracleInt, request)
		case "btree_set/str":
			return runBTreeSet(t, oracleStr, request)
		case "hash_map/int":
			return runHashMap(t, oracleInt, request)
		case "hash_map/str":
			return runHashMap(t, oracleStr, request)
		case "hash_set/int":
			return runHashSet(t, oracleInt, request)
		case "hash_set/str":
			return runHashSet(t, oracleStr, request)
		case "vec_deque/i64":
			return runVecDeque(t, oracleInt, request)
		case "vec_deque/u8":
			return runVecDeque(t, oracleByte, request)
		case "sip/":
			return runSip(t, request)
		case "hash_value/":
			return hashValues()
		}
		t.Fatalf("the Go side has no answer for %s", request)
		return nil
	}
}

func runSip(t *testing.T, request json.RawMessage) any {
	var decoded struct {
		Writes [][]json.RawMessage `json:"writes"`
	}
	if err := json.Unmarshal(request, &decoded); err != nil {
		t.Fatalf("oracle request %s: %v", request, err)
	}
	hasher := HashMapDefaultHasherNew()
	finishes := []string{strconv.FormatUint(hasher.Finish(), 10)}
	for _, write := range decoded.Writes {
		value := write[1]
		switch kind := decodeJSON[string](write[0]); kind {
		case "bytes":
			raw := decodeJSON[[]int](value)
			bytes := make([]byte, len(raw))
			for index, b := range raw {
				bytes[index] = byte(b)
			}
			hasher.Write(bytes)
		case "str":
			hasher.WriteStr(decodeJSON[string](value))
		case "u8":
			hasher.WriteU8(uint8(decodeJSON[uint64](value)))
		case "u16":
			hasher.WriteU16(uint16(decodeJSON[uint64](value)))
		case "u32":
			hasher.WriteU32(uint32(decodeJSON[uint64](value)))
		case "u64":
			parsed, err := strconv.ParseUint(decodeJSON[string](value), 10, 64)
			if err != nil {
				t.Fatal(err)
			}
			hasher.WriteU64(parsed)
		case "usize":
			hasher.WriteUsize(int(decodeJSON[int64](value)))
		case "i8":
			hasher.WriteI8(int8(decodeJSON[int64](value)))
		case "i16":
			hasher.WriteI16(int16(decodeJSON[int64](value)))
		case "i32":
			hasher.WriteI32(int32(decodeJSON[int64](value)))
		case "i64":
			hasher.WriteI64(decodeJSON[int64](value))
		case "isize":
			hasher.WriteIsize(int(decodeJSON[int64](value)))
		case "length_prefix":
			hasher.WriteLengthPrefix(int(decodeJSON[int64](value)))
		default:
			t.Fatalf("the Go side has no write %q", kind)
		}
		finishes = append(finishes, strconv.FormatUint(hasher.Finish(), 10))
	}
	return map[string]any{"finish": finishes}
}

// hashValues rebuilds the hashes of the values the Rust side hashes with
// their Hash impls, from the hasher's write methods, the way the
// documentation of HashMapDefaultHasher says a port of `value.hash(..)` is
// written.
func hashValues() any {
	hashed := func(feed func(h *HashMapDefaultHasher)) string {
		hasher := HashMapDefaultHasherNew()
		feed(&hasher)
		return strconv.FormatUint(hasher.Finish(), 10)
	}
	chars := func(h *HashMapDefaultHasher, text string) {
		runes := []rune(text)
		h.WriteLengthPrefix(len(runes))
		for _, r := range runes {
			h.WriteU32(uint32(r))
		}
	}
	return map[string]string{
		"str":    hashed(func(h *HashMapDefaultHasher) { h.WriteStr("session-1") }),
		"string": hashed(func(h *HashMapDefaultHasher) { h.WriteStr("session-1") }),
		"chars":  hashed(func(h *HashMapDefaultHasher) { chars(h, "héllo, wörld") }),
		"tuple": hashed(func(h *HashMapDefaultHasher) {
			h.WriteStr("a")
			h.WriteU8(7)
			h.WriteI64(-3)
			h.WriteU8(1)
		}),
		"option_some": hashed(func(h *HashMapDefaultHasher) {
			h.WriteIsize(1)
			h.WriteU32(9)
		}),
		"option_none":  hashed(func(h *HashMapDefaultHasher) { h.WriteIsize(0) }),
		"unit_variant": hashed(func(h *HashMapDefaultHasher) { h.WriteIsize(0) }),
		"tuple_variant": hashed(func(h *HashMapDefaultHasher) {
			h.WriteIsize(1)
			h.WriteU32(5)
			h.WriteStr("x")
		}),
		"named_variant": hashed(func(h *HashMapDefaultHasher) {
			h.WriteIsize(2)
			h.WriteU64(1 << 40)
			h.WriteU8(1)
		}),
		"record": hashed(func(h *HashMapDefaultHasher) {
			h.WriteStr("n")
			h.WriteU16(513)
			chars(h, "aß")
			h.WriteIsize(1)
			h.WriteU32(1)
			h.WriteStr("")
			h.WriteIsize(1)
			h.WriteI32(-1)
		}),
		"bytes": hashed(func(h *HashMapDefaultHasher) {
			h.WriteLengthPrefix(3)
			h.Write([]byte{1, 2, 3})
		}),
		"u16s": hashed(func(h *HashMapDefaultHasher) {
			h.WriteLengthPrefix(3)
			h.WriteU16(1)
			h.WriteU16(2)
			h.WriteU16(3)
		}),
		"usize": hashed(func(h *HashMapDefaultHasher) { h.WriteUsize(77) }),
		"char":  hashed(func(h *HashMapDefaultHasher) { h.WriteU32('é') }),
		"bool":  hashed(func(h *HashMapDefaultHasher) { h.WriteU8(0) }),
		"unit":  hashed(func(h *HashMapDefaultHasher) {}),
		"btree_map": hashed(func(h *HashMapDefaultHasher) {
			h.WriteLengthPrefix(2)
			h.WriteU8(1)
			h.WriteStr("a")
			h.WriteU8(2)
			h.WriteStr("b")
		}),
		"vec_deque": hashed(func(h *HashMapDefaultHasher) {
			h.WriteLengthPrefix(2)
			h.WriteU32(3)
			h.WriteU32(4)
		}),
	}
}
