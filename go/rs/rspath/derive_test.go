package rspath

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Product code compares components with rs.Eq and rs.Cmp, which read the
// registration of the enums and the Equal and Compare methods. These tests
// hold that route to the comparisons the iterators themselves use.

func derivedSamples() []Component {
	prefix := func(path string) Component {
		return Windows.Components(path).Next().Unwrap()
	}
	return []Component{
		prefix(`\\?\pictures\kittens`), prefix(`\\?\UNC\server\other`), prefix(`\\?\UNC\server\share`),
		prefix(`\\?\c:\`), prefix(`\\?\D:\`), prefix(`\\.\BrainInterface`), prefix(`\\server\share`),
		prefix(`\\server\zzz`), prefix(`C:\Users`), prefix(`c:\Users`), prefix(`D:`),
		&Component_RootDir{}, &Component_CurDir{}, &Component_ParentDir{},
		&Component_Normal{F0: ""}, &Component_Normal{F0: "a"}, &Component_Normal{F0: "a\xff"},
		&Component_Normal{F0: "b"}, &Component_Normal{F0: "é"},
	}
}

func TestComponentEqAndCmpThroughRs(t *testing.T) {
	samples := derivedSamples()
	for _, a := range samples {
		for _, b := range samples {
			if got, want := rs.Eq(a, b), componentEq(a, b); got != want {
				t.Errorf("rs.Eq(%s, %s) = %v, the derived PartialEq says %v",
					Windows.componentDebugString(a), Windows.componentDebugString(b), got, want)
			}
			if got, want := rs.Cmp(a, b), componentCmp(a, b); got != want {
				t.Errorf("rs.Cmp(%s, %s) = %d, the derived Ord says %d",
					Windows.componentDebugString(a), Windows.componentDebugString(b), got, want)
			}
		}
	}
}

func TestComponentOrderIsDeclarationOrder(t *testing.T) {
	// The samples are listed in ascending order, equal neighbours aside
	// (`C:` and `c:` are the same prefix).
	samples := derivedSamples()
	for index := 1; index < len(samples); index++ {
		previous, current := samples[index-1], samples[index]
		if order := ComponentCmp(previous, current); order > 0 {
			t.Errorf("%s sorts after %s", Windows.componentDebugString(previous), Windows.componentDebugString(current))
		}
	}
	upper := Windows.Components(`C:\Users`).Next().Unwrap()
	lower := Windows.Components(`c:\Users`).Next().Unwrap()
	if !ComponentEq(upper, lower) || !rs.Eq(upper, lower) {
		t.Errorf("the prefixes `C:` and `c:` are not equal")
	}
	if upper.AsOsStr() != "C:" || lower.AsOsStr() != "c:" {
		t.Errorf("the prefixes do not keep their spelling: %q, %q", upper.AsOsStr(), lower.AsOsStr())
	}
}

func TestPathComponentsEqAndCmpThroughRs(t *testing.T) {
	for _, F := range bothFlavors {
		for _, pair := range [][2]string{
			{"a/b", "a//b/"}, {"a/b", "a/c"}, {"/a", "a"}, {"./a", "a"}, {"", ""}, {"a/b/c", "a/b"}, {"x", "y/.."},
		} {
			left, right := F.Components(pair[0]), F.Components(pair[1])
			if got, want := rs.Eq(left, right), F.Eq(pair[0], pair[1]); got != want {
				t.Errorf("%v: rs.Eq of the components of %q and %q = %v, the paths say %v", F, pair[0], pair[1], got, want)
			}
			if got, want := rs.Cmp(left, right), F.Cmp(pair[0], pair[1]); got != want {
				t.Errorf("%v: rs.Cmp of the components of %q and %q = %d, the paths say %d", F, pair[0], pair[1], got, want)
			}
		}
	}
}

func TestCloneOfComponentsIsIndependent(t *testing.T) {
	original := Unix.Components("/a/b/c")
	original.Next()
	clone := rs.Clone(original)
	clone.Next()
	clone.NextBack()
	if got := original.AsPath(); got != "a/b/c" {
		t.Errorf("advancing a clone moved the original: %q", got)
	}
	if got := clone.AsPath(); got != "b" {
		t.Errorf("the clone is at %q", got)
	}
	if got := original.Clone().AsPath(); got != "a/b/c" {
		t.Errorf("Clone() is at %q", got)
	}
}

func TestDerivedDebugThroughTheFormatter(t *testing.T) {
	if !formatterReady() {
		t.Skip("rs.Format is not ported yet; the derived Debug text is checked through DebugString in the oracle tests")
	}
	for _, component := range derivedSamples() {
		if got, want := rs.DebugString(component), Windows.componentDebugString(component); got != want {
			t.Errorf("rs.DebugString = %s, expected %s", got, want)
		}
	}
	if got := rs.DebugString(&StripPrefixError{F0: rs.Unit{}}); got != "StripPrefixError(())" {
		t.Errorf("rs.DebugString(StripPrefixError) = %s", got)
	}
	if got := rs.DebugString(&NormalizeError{}); got != "NormalizeError" {
		t.Errorf("rs.DebugString(NormalizeError) = %s", got)
	}
	if got := rs.ToString(&StripPrefixError{F0: rs.Unit{}}); got != "prefix not found" {
		t.Errorf("rs.ToString(StripPrefixError) = %s", got)
	}
	if got := rs.Format("{:?}", Unix.Debug("a\xffb")); got != `"a\xFFb"` {
		t.Errorf("rs.Format of Debug(path) = %s", got)
	}
}
