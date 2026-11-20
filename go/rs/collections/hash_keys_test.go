package collections

// How HashMap and HashSet tell keys apart when Go's == would not do it as
// Rust's Eq does: keys that hold pointers or interfaces are compared by
// what they hold, keys with a hand-written Hash by their HashKey.

import (
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
