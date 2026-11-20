package rspath

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// registerProbe has path fields of each shape the `path` option applies to,
// as the declarations generator writes them.
type registerProbe struct {
	P     string            `rs:"p,path"`
	Maybe rs.Option[string] `rs:"maybe,path"`
	List  []string          `rs:"list,path"`
}

func TestRsEqAndCmpComparePathFieldsByComponent(t *testing.T) {
	// Byte order puts `a/b` after `a-b`; Rust's component order puts it
	// before, since the component `a` is a prefix of `a-b`.
	if "a/b" < "a-b" {
		t.Fatal("the bytes of the samples no longer disagree with their components")
	}
	cases := []struct {
		name  string
		a, b  registerProbe
		equal bool
		order int
	}{
		{"a//b is a/b", registerProbe{P: "a//b"}, registerProbe{P: "a/b"}, true, 0},
		{"a/./b is a/b", registerProbe{P: "a/./b"}, registerProbe{P: "a/b"}, true, 0},
		{"a/b sorts before a-b", registerProbe{P: "a/b"}, registerProbe{P: "a-b"}, false, -1},
		{"Some(a//b) is Some(a/b)", registerProbe{Maybe: rs.Some("a//b")}, registerProbe{Maybe: rs.Some("a/b")}, true, 0},
		{"Some(a/b) sorts before Some(a-b)", registerProbe{Maybe: rs.Some("a/b")}, registerProbe{Maybe: rs.Some("a-b")}, false, -1},
		{"None sorts before Some", registerProbe{}, registerProbe{Maybe: rs.Some("")}, false, -1},
		{"[a//b] is [a/b]", registerProbe{List: []string{"a//b", "c"}}, registerProbe{List: []string{"a/b", "c"}}, true, 0},
		{"[a/b] sorts before [a-b]", registerProbe{List: []string{"a/b"}}, registerProbe{List: []string{"a-b"}}, false, -1},
	}
	for _, c := range cases {
		if got := rs.Eq(c.a, c.b); got != c.equal {
			t.Errorf("%s: rs.Eq = %v", c.name, got)
		}
		if got, swapped := rs.Cmp(c.a, c.b), rs.Cmp(c.b, c.a); got != c.order || swapped != -c.order {
			t.Errorf("%s: rs.Cmp = %d, swapped %d, want %d", c.name, got, swapped, c.order)
		}
		if got := rs.PartialCmp(c.a, c.b); got != rs.Some(rs.Ordering(c.order)) {
			t.Errorf("%s: rs.PartialCmp = %+v", c.name, got)
		}
	}
}
