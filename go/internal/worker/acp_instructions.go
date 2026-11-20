// Port of mj-worker/src/acp/instructions.rs.

// ACP instruction composition and system-prompt delivery.

package worker

import (
	"context"
	"fmt"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// InstructionRole is core's standing session role, which the worker's acp
// module re-exports.
type InstructionRole = core.InstructionRole

func compose(ctx context.Context, spec *LaunchSpec, mcpAvailable bool, events chan<- core.RuntimeEvent) (string, error) {
	panic("unported: mj-worker/src/acp/instructions.rs compose")
}

// deliver decides how the composed instructions reach the harness: nil when
// staging delivers them or there are none, or when the adapter has no
// channel for them (which is reported).
func deliver(ctx context.Context, spec *LaunchSpec, capabilities *schema.AgentCapabilities, adapterVersion *string, text string, events chan<- core.RuntimeEvent) (SystemPromptAppend, error) {
	panic("unported: mj-worker/src/acp/instructions.rs deliver")
}

func unsupportedChannel(ctx context.Context, spec *LaunchSpec, events chan<- core.RuntimeEvent, message string) error {
	if spec.InstructionRole != nil {
		switch *spec.InstructionRole {
		case core.InstructionRoleBriefWriter:
			return fmt.Errorf("Cannot start the relation brief writer: %s", message)
		case core.InstructionRoleReviewer:
			return fmt.Errorf("Cannot start the automated reviewer: %s", message)
		}
	}
	return emitRuntimeEvent(ctx, events, core.RuntimeEventWarning{Message: message})
}
