// Port of mj-worker/src/acp/instructions.rs.

// ACP instruction composition and system-prompt delivery.

package worker

import (
	"context"
	"fmt"
	"path/filepath"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// InstructionRole is core's standing session role, which the worker's acp
// module re-exports.
type InstructionRole = core.InstructionRole

func compose(ctx context.Context, spec *LaunchSpec, mcpAvailable bool, events chan<- core.RuntimeEvent) (string, error) {
	if core.DeliveredByStaging(spec.Harness) {
		return "", nil
	}
	index := ""
	indexFailed := false
	if spec.Harness != core.HarnessKindClaude && spec.ProjectMemory != nil {
		memory := spec.ProjectMemory
		root := memory.Root
		contents, err := fileReadOffRuntime(ctx, "project-memory-read", func() (*string, error) {
			store := core.NewProjectMemoryStore(root)
			return store.StartupIndex()
		})
		if err != nil {
			index, indexFailed = err.Error(), true
			if err := emitRuntimeEvent(ctx, events, core.RuntimeEventWarning{Message: fmt.Sprintf(
				"Project memory index %s unavailable: %s", filepath.Join(memory.Root, core.MemoryIndex), index)}); err != nil {
				return "", err
			}
		} else if contents != nil {
			index = *contents
		}
	}
	facts := core.InstructionContext{
		Harness:         spec.Harness,
		Memory:          spec.ProjectMemory,
		HistoryTools:    mcpAvailable && spec.ProjectMemory != nil && spec.ProjectMemory.HistorySocket != nil,
		InstructionRole: spec.InstructionRole,
	}
	if socket := spec.SubagentMCPSocket; socket != nil && mcpAvailable &&
		(spec.Harness != core.HarnessKindClaude || socket.ProfileRegistration) {
		facts.Agents = &core.AgentTools{
			Role:                 socket.Role,
			ReviewTools:          socket.ReviewTools,
			OrchestratorsEnabled: socket.OrchestratorsEnabled,
		}
	}
	if directory, ok := spec.Environment[core.ChildReportDirEnv]; ok {
		facts.ReportDirectory = &directory
	}
	return core.Compose(&facts, index, indexFailed), nil
}

// deliver decides how the composed instructions reach the harness: nil when
// staging delivers them or there are none, or when the adapter has no
// channel for them (which is reported).
func deliver(ctx context.Context, spec *LaunchSpec, capabilities *schema.AgentCapabilities, adapterVersion *string, text string, events chan<- core.RuntimeEvent) (SystemPromptAppend, error) {
	if core.DeliveredByStaging(spec.Harness) || text == "" {
		return nil, nil
	}
	if capabilities.SessionCapabilities != nil && capabilities.SessionCapabilities.SystemPrompt != nil {
		return SystemPromptAppendStandard{Value: schema.SystemPrompt{
			Mode:    schema.SystemPromptModeAppend,
			Content: []schema.ContentBlock{{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: text}}},
		}}, nil
	}
	if spec.Harness == core.HarnessKindAntigravity &&
		jsonPath(capabilities.Meta, "antigravity", "systemPrompt", "append") == true {
		return SystemPromptAppendAntigravity{Value: text}, nil
	}
	version := "(unknown version)"
	if adapterVersion != nil {
		version = *adapterVersion
	}
	message := fmt.Sprintf("%s adapter %s does not advertise a persistent instruction channel. Mjolnir's standing instructions and project memory could not be delivered. Updating the adapter fixes this.",
		spec.Harness.DisplayName(), version)
	return nil, unsupportedChannel(ctx, spec, events, message)
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
