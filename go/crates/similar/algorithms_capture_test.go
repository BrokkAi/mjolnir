// Port of similar 2.7.0 src/algorithms/capture.rs

package similar

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestAlgorithmsCapture_test_capture_hook_grouping(t *testing.T) {
	rng := []int{}
	for value := 1; value < 100; value++ {
		rng = append(rng, value)
	}
	rngNew := append([]int{}, rng...)
	rngNew[10] = 1000
	rngNew[13] = 1000
	rngNew[16] = 1000
	rngNew[34] = 1000

	d := AlgorithmsReplaceNew(AlgorithmsCaptureNew())
	rs.UnwrapUnit(AlgorithmsDiffSlices(Algorithm_Myers, d, rng, rngNew))

	ops := d.IntoInner().IntoGroupedOps(3)
	tags := []debugNode{}
	for _, group := range ops {
		groupTags := []debugNode{}
		for _, x := range group {
			tag, old, new_ := x.AsTagTuple()
			groupTags = append(groupTags, debugTuple("", debugDiffTag(tag), debugRange(old), debugRange(new_)))
		}
		tags = append(tags, debugList(groupTags))
	}

	snapshotAssert(t, "similar__algorithms__capture__capture_hook_grouping", debugGroupedOps(ops))
	snapshotAssert(t, "similar__algorithms__capture__capture_hook_grouping-2", debugPretty(debugList(tags)))
}
