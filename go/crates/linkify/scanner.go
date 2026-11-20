// Port of linkify 0.11.0 src/scanner.rs

package linkify

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

// scannerRange is the `Range<usize>` a scanner returns: the byte offsets of
// the link it found.
type scannerRange struct {
	start int
	end   int
}

type scannerScanner interface {
	scan(s string, triggerIndex int) rs.Option[scannerRange]
}
