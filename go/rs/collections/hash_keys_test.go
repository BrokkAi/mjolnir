package collections

// How HashMap and HashSet tell keys apart when Go's == would not do it as
// Rust's Eq does: keys that hold pointers, interfaces or slices are compared
// by what they hold, keys with their own Hash and Eq by their HashKey.

import (
	"iter"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// requestStream has the shape of SessionRequestStream in mj-session-runtime
// (an enum whose variants carry data, with a derived Hash and Eq), which
// Mjolnir keys a HashMap with.
type requestStream interface{ isRequestStream() }

type requestStream_Primary struct{ F0 string }

type requestStream_Reviewer struct {
	F0 string
	F1 rs.Option[string]
}

func (*requestStream_Primary) isRequestStream()  {}
func (*requestStream_Reviewer) isRequestStream() {}

var _ = rs.RegisterEnum[requestStream]("SessionRequestStream",
	rs.Variant[requestStream_Primary]("Primary", rs.ShapeTuple),
	rs.Variant[requestStream_Reviewer]("Reviewer", rs.ShapeTuple),
)

// Each variant value is a separate pointer, so Go's == alone would keep one
// entry for each of them.
func TestHashMapEnumKeysCompareByValue(t *testing.T) {
	var budgets HashMap[requestStream, int]
	budgets.Insert(&requestStream_Primary{F0: "s1"}, 1)
	budgets.Insert(&requestStream_Reviewer{F0: "s1", F1: rs.None[string]()}, 2)
	budgets.Insert(&requestStream_Reviewer{F0: "s1", F1: rs.Some("r")}, 3)

	expectEq(t, budgets.Insert(&requestStream_Primary{F0: "s1"}, 10), rs.Some(1))
	expectEq(t, budgets.Len(), 3)
	*budgets.Entry(&requestStream_Reviewer{F0: "s1", F1: rs.Some("r")}).OrInsert(0) += 30
	expectEq(t, *budgets.Index(&requestStream_Reviewer{F0: "s1", F1: rs.Some("r")}), 33)
	expect(t, !budgets.ContainsKey(&requestStream_Primary{F0: "s2"}))
	expect(t, !budgets.ContainsKey(&requestStream_Reviewer{F0: "s1", F1: rs.Some("x")}))
	expectEq(t, budgets.Remove(&requestStream_Reviewer{F0: "s1", F1: rs.None[string]()}), rs.Some(2))
	expectEq(t, budgets.Len(), 2)

	// Removing the entry just yielded is allowed while iterating.
	budgets.Retain(func(key requestStream, value *int) bool { return rs.Is[*requestStream_Primary](key) })
	expectEq(t, budgets.Len(), 1)
	expectEq(t, collect(budgets.Keys()), []requestStream{&requestStream_Primary{F0: "s1"}})

	var seen HashSet[requestStream]
	expect(t, seen.Insert(&requestStream_Primary{F0: "a"}))
	expect(t, !seen.Insert(&requestStream_Primary{F0: "a"}))
	replacement := &requestStream_Primary{F0: "a"}
	replaced := seen.Replace(replacement)
	expect(t, replaced.IsSome())
	stored, _ := seen.Get(&requestStream_Primary{F0: "a"})
	expect(t, stored == requestStream(replacement), "replace stores the new element")
	expectEq(t, collect(seen.Drain()), []requestStream{replacement})
	expect(t, seen.IsEmpty())
}

// A Box<T> or Arc<T> key is a pointer in Go; Rust compares what it points
// at.
func TestHashMapPointerKeysCompareByPointee(t *testing.T) {
	var m HashMap[*string, int]
	first, second := "key", "key"
	m.Insert(&first, 1)
	expectEq(t, m.Insert(&second, 2), rs.Some(1))
	expectEq(t, m.Len(), 1)

	type holder struct {
		name  string
		boxed rs.Option[*int]
	}
	one, alsoOne := 1, 1
	var h HashSet[holder]
	expect(t, h.Insert(holder{name: "x", boxed: rs.Some(&one)}))
	expect(t, !h.Insert(holder{name: "x", boxed: rs.Some(&alsoOne)}))
	expect(t, h.Insert(holder{name: "x", boxed: rs.None[*int]()}))
	expectEq(t, h.Len(), 2)
}

// caseless has a hand-written Hash and Eq: names that differ in case are
// one key.
type caseless struct{ name string }

func (c caseless) HashKey() string { return strings.ToLower(c.name) }

func (c caseless) Equal(other caseless) bool { return strings.EqualFold(c.name, other.name) }

func TestHashMapHashKeyDecidesEquality(t *testing.T) {
	var m HashMap[caseless, int]
	m.Insert(caseless{"Alpha"}, 1)
	expectEq(t, m.Insert(caseless{"ALPHA"}, 2), rs.Some(1))
	key, value, ok := m.GetKeyValue(caseless{"alpha"})
	expect(t, ok)
	// The stored key stays the first one, as Rust's insert keeps it.
	expectEq(t, key.name, "Alpha")
	expectEq(t, *value, 2)
}

// pathKey has the shape of rspath.Key: a path held as a string, with its
// own Hash, Eq and Ord that look at its components, as Rust's PathBuf does,
// so that "a//b" and "a/b/" are the path "a/b". (Only repeated and
// trailing separators are handled here.)
type pathKey string

func (p pathKey) components() []string {
	var parts []string
	for _, part := range strings.Split(string(p), "/") {
		if part != "" {
			parts = append(parts, part)
		}
	}
	return parts
}

func (p pathKey) HashKey() string { return strings.Join(p.components(), "\x00") }

func (p pathKey) Equal(other pathKey) bool { return p.HashKey() == other.HashKey() }

func (p pathKey) Compare(other pathKey) int {
	return slices.Compare(p.components(), other.components())
}

func keyStrings[K ~string](seq iter.Seq[K]) []string {
	texts := []string{}
	for key := range seq {
		texts = append(texts, string(key))
	}
	return texts
}

// rs.Eq would call Equal, so the tests that the first key is kept compare
// the strings.
func TestHashMapKeysWithOwnHashAndEq(t *testing.T) {
	var m HashMap[pathKey, int]
	m.Insert("a//b", 1)
	expectEq(t, m.Insert("a/b", 2), rs.Some(1))
	expectEq(t, m.Len(), 1)
	key, value, ok := m.GetKeyValue("a/b/")
	expect(t, ok)
	expectEq(t, string(key), "a//b")
	expectEq(t, *value, 2)
	expectEq(t, keyStrings(m.Keys()), []string{"a//b"})
	*m.Entry("a/b").OrInsert(0) += 10
	expectEq(t, *m.Index("a///b"), 12)
	expectEq(t, m.Remove("a/b"), rs.Some(12))
	expect(t, m.IsEmpty())

	var s HashSet[pathKey]
	expect(t, s.Insert("x/y"))
	expect(t, !s.Insert("x//y/"))
	got, _ := s.Get("x/y/")
	expectEq(t, string(got), "x/y")
	replaced := s.Replace("x//y")
	expectEq(t, string(replaced.Unwrap()), "x/y")
	got, _ = s.Get("x/y")
	expectEq(t, string(got), "x//y")
	expectEq(t, keyStrings(s.Iter()), []string{"x//y"})

	// The B-tree collections order the same keys by Compare: by
	// component, so "a/b" comes before "a-b", where the strings order the
	// other way, and "a//b" is the key "a/b".
	var b BTreeMap[pathKey, int]
	b.Insert("a-b", 1)
	b.Insert("a/b", 2)
	expectEq(t, b.Insert("a//b", 3), rs.Some(2))
	expectEq(t, keyStrings(b.Keys()), []string{"a/b", "a-b"})
	expectEq(t, *b.Index("a/b/"), 3)
}

// Keys that Go cannot compare: Rust's Vec, held as a slice.
func TestHashMapSliceKeys(t *testing.T) {
	// IdSets::intern in mj-transcript/src/github_history.rs, over a
	// HashMap<Vec<u32>, u32>.
	var index HashMap[[]uint32, uint32]
	var sets [][]uint32
	intern := func(set []uint32) uint32 {
		if i, ok := index.Get(set); ok {
			return *i
		}
		i := uint32(len(sets))
		index.Insert(rs.Clone(set), i)
		sets = append(sets, set)
		return i
	}
	expectEq(t, intern([]uint32{1, 2, 3}), 0)
	expectEq(t, intern([]uint32{1, 2}), 1)
	// Another slice with the same elements is the same key.
	expectEq(t, intern([]uint32{1, 2, 3}), 0)
	expectEq(t, intern([]uint32{3, 2, 1}), 2)
	// An empty Vec, however the slice holds it.
	expectEq(t, intern([]uint32{}), 3)
	expectEq(t, intern(nil), 3)
	expectEq(t, index.Len(), 4)
	expectEq(t, index.Remove([]uint32{1, 2}), rs.Some(uint32(1)))
	expect(t, !index.ContainsKey([]uint32{1, 2}))
	expectEq(t, collectPairs(index.IterSorted()), []rs.Tuple2[[]uint32, uint32]{
		rs.T2([]uint32{}, uint32(3)), rs.T2([]uint32{1, 2, 3}, uint32(0)), rs.T2([]uint32{3, 2, 1}, uint32(2)),
	})

	// A struct holding a slice is compared field by field, the slice by its
	// elements.
	type window struct {
		ids    []uint32
		length uint16
	}
	var seen HashSet[window]
	expect(t, seen.Insert(window{ids: []uint32{7}, length: 1}))
	expect(t, !seen.Insert(window{ids: []uint32{7}, length: 1}))
	expect(t, seen.Insert(window{ids: []uint32{7}, length: 2}))
	expect(t, seen.Insert(window{ids: []uint32{7, 8}, length: 1}))
	expectEq(t, seen.Len(), 3)
	expect(t, seen.Contains(window{ids: []uint32{7, 8}, length: 1}))
}
