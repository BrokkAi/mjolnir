// Port of mj-client/src/settings/discovery.rs.

// Choices a draft offers only once they are discovered: the models and
// efforts its profiles advertise, from the daemon's capability catalog.

package client

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// EffortsPending is set on a task's discovered choices while its selected
// model's efforts are still being discovered.
const EffortsPending = "efforts_pending"

// FillSubagentOptions fills the sub-agent model and effort choices of draft
// from capabilities. A task whose profiles are not all discovered yet is
// left unset, which the page reports as still discovering. Returns what
// discovery could not reach; a draft that is not a configuration yet is
// left as it is.
func FillSubagentOptions(draft *any, capabilities *core.ProfileCapabilitiesSnapshot) (*string, error) {
	panic("unported: mj-client/src/settings/discovery.rs fill_subagent_options")
}

// FillReviewerOptions fills reviewer choices from the same profile catalog
// as subagent choices.
func FillReviewerOptions(draft *any, capabilities *core.ProfileCapabilitiesSnapshot) error {
	panic("unported: mj-client/src/settings/discovery.rs fill_reviewer_options")
}
