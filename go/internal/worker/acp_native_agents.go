// Port of mj-worker/src/acp/native_agents.rs.

// Interpret the negotiated native child-session extension before ACP v1
// decoding.

package worker

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Inventory is the adapter's answer to an availability query.
type Inventory struct {
	Agents   []core.NativeAgentAvailabilityReport `json:"agents"`
	Complete bool                                 `json:"complete"`
}

// UnmarshalJSON decodes as serde's derive does: both members are required.
func (i *Inventory) UnmarshalJSON(data []byte) error {
	members, err := acpJSONMembers(data)
	if err != nil {
		return err
	}
	var inventory Inventory
	if err := acpDecodeRequired(members, "agents", &inventory.Agents); err != nil {
		return err
	}
	if err := acpDecodeRequired(members, "complete", &inventory.Complete); err != nil {
		return err
	}
	*i = inventory
	return nil
}

func decodeInventory(value any) (Inventory, error) {
	data, err := jsonvalue.Marshal(value)
	if err != nil {
		return Inventory{}, err
	}
	var inventory Inventory
	if err := json.Unmarshal(data, &inventory); err != nil {
		return Inventory{}, err
	}
	identities := map[string]bool{}
	for _, agent := range inventory.Agents {
		if agent.SessionID == "" || identities[agent.SessionID] {
			return Inventory{}, errors.New("availability inventory has empty or duplicate identities")
		}
		identities[agent.SessionID] = true
		if agent.StableID != nil && *agent.StableID == "" {
			return Inventory{}, errors.New("availability inventory has an empty stable identity")
		}
	}
	return inventory, nil
}

// refreshAvailability is only called after the adapter explicitly
// negotiates this read-only extension.
func refreshAvailability(ctx context.Context, connection *acp.Connection, sessionID schema.SessionId, events chan<- core.RuntimeEvent) error {
	panic("unported: mj-worker/src/acp/native_agents.rs refresh_availability")
}

// NativeAgentRouter routes notifications addressed to native child sessions.
// The zero value has no children.
type NativeAgentRouter struct {
	children map[string]struct{}
}

// Lifecycle is a native agent lifecycle notification, internally tagged by
// "sessionUpdate".
type Lifecycle interface {
	isLifecycle()
}

type LifecycleSpawned struct {
	SubagentSessionID string                       `json:"subagentSessionId"`
	Name              string                       `json:"name"`
	Task              string                       `json:"task"`
	Capabilities      core.NativeAgentCapabilities `json:"capabilities"`
}

type LifecycleState struct {
	SubagentSessionID string                `json:"subagentSessionId"`
	State             core.NativeAgentState `json:"state"`
}

func (LifecycleSpawned) isLifecycle() {}
func (LifecycleState) isLifecycle()   {}

// unmarshalLifecycle decodes a Lifecycle as serde's internally tagged derive
// does: the tag picks the variant, its fields are required except
// `capabilities` (default), and other members are ignored.
func unmarshalLifecycle(data []byte) (Lifecycle, error) {
	members, err := acpJSONMembers(data)
	if err != nil {
		return nil, err
	}
	var tag string
	if err := acpDecodeRequired(members, "sessionUpdate", &tag); err != nil {
		return nil, err
	}
	switch tag {
	case "subagent_spawned":
		var spawned LifecycleSpawned
		if err := acpDecodeRequired(members, "subagentSessionId", &spawned.SubagentSessionID); err != nil {
			return nil, err
		}
		if err := acpDecodeRequired(members, "name", &spawned.Name); err != nil {
			return nil, err
		}
		if err := acpDecodeRequired(members, "task", &spawned.Task); err != nil {
			return nil, err
		}
		if raw, ok := members["capabilities"]; ok {
			if err := json.Unmarshal(raw, &spawned.Capabilities); err != nil {
				return nil, err
			}
		}
		return spawned, nil
	case "subagent_state_update":
		var state LifecycleState
		if err := acpDecodeRequired(members, "subagentSessionId", &state.SubagentSessionID); err != nil {
			return nil, err
		}
		if err := acpDecodeRequired(members, "state", &state.State); err != nil {
			return nil, err
		}
		return state, nil
	}
	return nil, fmt.Errorf("unknown variant `%s`, expected `subagent_spawned` or `subagent_state_update`", tag)
}

// Routed is what the router did with a notification.
type Routed interface {
	isRouted()
}

// RoutedEvent is a native agent event for the relay.
type RoutedEvent struct {
	Value core.NativeAgentEvent
}

// RoutedParent is not a native agent notification; the caller handles it as
// parent output.
type RoutedParent struct{}

func (RoutedEvent) isRouted()  {}
func (RoutedParent) isRouted() {}

func (r *NativeAgentRouter) IsChild(id string) bool {
	_, ok := r.children[id]
	return ok
}

func (r *NativeAgentRouter) Route(addressed string, update any) (Routed, error) {
	panic("unported: mj-worker/src/acp/native_agents.rs NativeAgentRouter::route")
}
