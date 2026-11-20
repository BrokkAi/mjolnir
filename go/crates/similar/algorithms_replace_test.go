// Port of similar 2.7.0 src/algorithms/replace.rs

package similar

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestAlgorithmsReplace_test_mayers_replace(t *testing.T) {
	a := []string{
		">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>\n",
		"a\n",
		"b\n",
		"c\n",
		"================================\n",
		"d\n",
		"e\n",
		"f\n",
		"<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<\n",
	}
	b := []string{
		">>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>\n",
		"x\n",
		"b\n",
		"c\n",
		"================================\n",
		"y\n",
		"e\n",
		"f\n",
		"<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<\n",
	}

	d := AlgorithmsReplaceNew(AlgorithmsCaptureNew())
	rs.UnwrapUnit(AlgorithmsDiffSlices(Algorithm_Myers, d, a, b))

	assertEq(t, snapshotNormalize(debugOps(d.IntoInner().Ops())), snapshotNormalize(`
[
    Equal {
        old_index: 0,
        new_index: 0,
        len: 1,
    },
    Replace {
        old_index: 1,
        old_len: 1,
        new_index: 1,
        new_len: 1,
    },
    Equal {
        old_index: 2,
        new_index: 2,
        len: 3,
    },
    Replace {
        old_index: 5,
        old_len: 1,
        new_index: 5,
        new_len: 1,
    },
    Equal {
        old_index: 6,
        new_index: 6,
        len: 3,
    },
]
`))
}

func TestAlgorithmsReplace_test_replace(t *testing.T) {
	a := []int{0, 1, 2, 3, 4}
	b := []int{0, 1, 2, 7, 8, 9}

	d := AlgorithmsReplaceNew(AlgorithmsCaptureNew())
	rs.UnwrapUnit(AlgorithmsDiffSlices(Algorithm_Myers, d, a, b))
	assertEq(t, snapshotNormalize(debugOps(d.IntoInner().Ops())), snapshotNormalize(`
[
    Equal {
        old_index: 0,
        new_index: 0,
        len: 3,
    },
    Replace {
        old_index: 3,
        old_len: 2,
        new_index: 3,
        new_len: 3,
    },
]
`))
}
