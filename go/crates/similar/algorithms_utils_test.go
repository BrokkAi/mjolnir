// Port of similar 2.7.0 src/algorithms/utils.rs

package similar

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestAlgorithmsUtils_test_unique(t *testing.T) {
	u := []rs.Tuple2[rune, int]{}
	for _, x := range algorithmsUtilsUnique([]rune{'a', 'b', 'c', 'd', 'd', 'b'}, Range{Start: 0, End: 6}) {
		u = append(u, rs.T2(x.value(), x.originalIndex()))
	}
	assertSliceEq(t, u, []rs.Tuple2[rune, int]{rs.T2('a', 0), rs.T2('c', 2)})
}

func TestAlgorithmsUtils_test_int_hasher(t *testing.T) {
	ih := AlgorithmsIdentifyDistinctNew[uint8](
		[]string{"", "foo", "bar", "baz"},
		Range{Start: 1, End: 4},
		[]string{"", "foo", "blah", "baz"},
		Range{Start: 1, End: 4},
	)
	assertEq(t, ih.OldLookup()[1], 0)
	assertEq(t, ih.OldLookup()[2], 1)
	assertEq(t, ih.OldLookup()[3], 2)
	assertEq(t, ih.NewLookup()[1], 0)
	assertEq(t, ih.NewLookup()[2], 3)
	assertEq(t, ih.NewLookup()[3], 2)
	assertEq(t, ih.OldRange(), Range{Start: 1, End: 4})
	assertEq(t, ih.NewRange(), Range{Start: 1, End: 4})
}

func TestAlgorithmsUtils_test_common_prefix_len(t *testing.T) {
	assertEq(t,
		algorithmsUtilsCommonPrefixLen([]byte(""), Range{Start: 0, End: 0}, []byte(""), Range{Start: 0, End: 0}),
		0,
	)
	assertEq(t,
		algorithmsUtilsCommonPrefixLen([]byte("foobarbaz"), Range{Start: 0, End: 9}, []byte("foobarblah"), Range{Start: 0, End: 10}),
		7,
	)
	assertEq(t,
		algorithmsUtilsCommonPrefixLen([]byte("foobarbaz"), Range{Start: 0, End: 9}, []byte("blablabla"), Range{Start: 0, End: 9}),
		0,
	)
	assertEq(t,
		algorithmsUtilsCommonPrefixLen([]byte("foobarbaz"), Range{Start: 3, End: 9}, []byte("foobarblah"), Range{Start: 3, End: 10}),
		4,
	)
}

func TestAlgorithmsUtils_test_common_suffix_len(t *testing.T) {
	assertEq(t,
		algorithmsUtilsCommonSuffixLen([]byte(""), Range{Start: 0, End: 0}, []byte(""), Range{Start: 0, End: 0}),
		0,
	)
	assertEq(t,
		algorithmsUtilsCommonSuffixLen([]byte("1234"), Range{Start: 0, End: 4}, []byte("X0001234"), Range{Start: 0, End: 8}),
		4,
	)
	assertEq(t,
		algorithmsUtilsCommonSuffixLen([]byte("1234"), Range{Start: 0, End: 4}, []byte("Xxxx"), Range{Start: 0, End: 4}),
		0,
	)
	assertEq(t,
		algorithmsUtilsCommonSuffixLen([]byte("1234"), Range{Start: 2, End: 4}, []byte("01234"), Range{Start: 2, End: 5}),
		2,
	)
}
