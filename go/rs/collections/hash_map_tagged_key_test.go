package collections

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// A key whose field is compared through a registered tag comparer (as a
// path field is through rspath's "path") must not be keyed by Go's ==:
// two spellings rs.Eq calls equal are one key, as in Rust.
func init() {
	rs.RegisterTagComparer("collections_test_fold",
		func(a, b string) int { return strings.Compare(strings.ToLower(a), strings.ToLower(b)) },
		func(a, b string) bool { return strings.EqualFold(a, b) })
}

type taggedKey struct {
	Name string `rs:"name,collections_test_fold"`
	N    int    `rs:"n"`
}

func TestHashMapKeysWithATagComparedFieldUseRsEq(t *testing.T) {
	var m HashMap[taggedKey, int]
	m.Insert(taggedKey{Name: "Alpha", N: 1}, 1)
	if old := m.Insert(taggedKey{Name: "ALPHA", N: 1}, 2); !old.IsSome() || old.Unwrap() != 1 {
		t.Fatalf("a key rs.Eq calls equal was stored twice: Insert returned %v", old)
	}
	if m.Len() != 1 {
		t.Fatalf("Len = %d, want 1", m.Len())
	}
	if value, ok := m.Get(taggedKey{Name: "alpha", N: 1}); !ok || *value != 2 {
		t.Fatalf("Get by another spelling = %v, %v; want 2, true", value, ok)
	}
	m.Insert(taggedKey{Name: "alpha", N: 2}, 3)
	if m.Len() != 2 {
		t.Fatalf("a key differing in an untagged field merged: Len = %d, want 2", m.Len())
	}

	var set HashSet[taggedKey]
	set.Insert(taggedKey{Name: "Beta"})
	if set.Insert(taggedKey{Name: "beta"}) {
		t.Fatal("HashSet.Insert added a key rs.Eq calls equal to one present")
	}
}
