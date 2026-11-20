// Port of similar 2.7.0 src/common.rs

package similar

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestCommon_test_non_string_iter_change(t *testing.T) {
	old := []int{1, 2, 3}
	new_ := []int{1, 2, 4}
	ops := CaptureDiffSlices(Algorithm_Myers, old, new_)
	changes := []rs.Tuple2[ChangeTag, int]{}
	for _, x := range ops {
		for x := range DiffOpIterChanges(x, old, new_).All() {
			changes = append(changes, rs.T2(x.Tag(), x.Value()))
		}
	}

	assertSliceEq(t,
		changes,
		[]rs.Tuple2[ChangeTag, int]{
			rs.T2(ChangeTag_Equal, 1),
			rs.T2(ChangeTag_Equal, 2),
			rs.T2(ChangeTag_Delete, 3),
			rs.T2(ChangeTag_Insert, 4),
		},
	)
}
