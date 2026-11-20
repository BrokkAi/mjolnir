// Port of mj-transcript/src/projection/api_events.rs.

package transcript

import "github.com/BrokkAi/mjolnir/go/internal/core"

// apiEventsDerive is api_events::derive: it retains transitions before the
// database page coalesces its final projection.
func apiEventsDerive(current *core.MaterializedSession, event *core.RelayEvent, mutation *core.MaterializedSessionMutation) []core.ApiEventData {
	panic("unported: mj-transcript/src/projection/api_events.rs derive")
}
