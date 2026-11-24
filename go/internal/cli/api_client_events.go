// Port of mj-cli/src/api_client/events.rs.
//
// Bounded SSE decoding, independent of network chunk boundaries.

package cli

import "github.com/BrokkAi/mjolnir/go/internal/core"

// eventFrameLimit is the most bytes one SSE frame may carry (8 MiB).
const eventFrameLimit = 8 * 1024 * 1024

// eventDecoder decodes the `GET /events` stream into API events (Rust
// `EventDecoder`). The zero value is ready to use.
type eventDecoder struct {
	line       []byte
	data       []byte
	id         *uint64
	kind       string
	frameBytes int
	// handoffCursor is set once the daemon announced that an upgrade is
	// replacing it.
	handoffCursor *uint64
}

// handoff is the cursor to resume from once the daemon announced that an
// upgrade is replacing it. Nothing after that announcement is decoded.
func (d *eventDecoder) handoff() (uint64, bool) {
	if d.handoffCursor == nil {
		return 0, false
	}
	return *d.handoffCursor, true
}

// push decodes bytes, returning the events whose frames they completed.
func (d *eventDecoder) push(bytes []byte) ([]core.ApiEvent, error) {
	panic("unported: mj-cli/src/api_client/events.rs EventDecoder::push")
}
