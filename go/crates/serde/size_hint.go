// Port of serde_core 1.0.229 src/private/size_hint.rs

package serde

import "github.com/BrokkAi/mjolnir/go/rs"

// sizeHintCautious is `size_hint::cautious::<Element>(hint)`: how many
// elements to reserve room for before reading a sequence or a map, given what
// the input claims and the size of one element. It never reserves more than
// one megabyte on the input's word.
func sizeHintCautious(hint rs.Option[int], elementSize uintptr) int {
	const maxPreallocBytes = 1024 * 1024
	if elementSize == 0 {
		return 0
	}
	return min(hint.UnwrapOr(0), maxPreallocBytes/int(elementSize))
}
