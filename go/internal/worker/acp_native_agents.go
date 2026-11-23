// Port of mj-worker/src/acp/native_agents.rs.

// Interpret the negotiated native child-session extension before ACP v1
// decoding.

package worker

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"time"

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
	params := jsonvalue.Object{{Key: "sessionId", Value: string(sessionID)}}
	answered, drop := acpStartCall[json.RawMessage](ctx, connection, "_session/subagents/availability", params)
	defer drop()
	timer := time.NewTimer(5 * time.Second)
	defer timer.Stop()
	var inventory Inventory
	var failure error
	select {
	case response := <-answered:
		if response.err != nil {
			failure = response.err
		} else if value, err := jsonvalue.Parse(response.value); err != nil {
			failure = err
		} else {
			inventory, failure = decodeInventory(value)
		}
	case <-timer.C:
		failure = errors.New("availability query timed out")
	case <-ctx.Done():
		return errRelayEventCoordinatorStopped
	}
	var event core.RuntimeEvent
	if failure == nil {
		event = core.RuntimeEventNativeAgent{Event: core.NativeAgentEventAvailability{
			Reports: inventory.Agents, Complete: inventory.Complete,
		}}
	} else {
		event = core.RuntimeEventWarning{Message: "Subagent availability remains unknown: " + failure.Error()}
	}
	return emitRuntimeEvent(ctx, events, event)
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
	kind, _ := jsonGetString(update, "sessionUpdate")
	if kind != nil && (*kind == "subagent_spawned" || *kind == "subagent_state_update") {
		encoded, err := jsonvalue.Marshal(update)
		var event Lifecycle
		if err == nil {
			event, err = unmarshalLifecycle(encoded)
		}
		if err != nil {
			return nil, fmt.Errorf("decode native agent lifecycle: %w", err)
		}
		switch event := event.(type) {
		case LifecycleSpawned:
			if event.SubagentSessionID == "" || event.SubagentSessionID == addressed {
				return nil, errors.New("native agent has an empty or self-referential identity")
			}
			var parentSessionID *string
			if r.IsChild(addressed) {
				parent := addressed
				parentSessionID = &parent
			}
			if r.children == nil {
				r.children = map[string]struct{}{}
			}
			r.children[event.SubagentSessionID] = struct{}{}
			return RoutedEvent{Value: core.NativeAgentEventSpawned{
				SessionID:       event.SubagentSessionID,
				ParentSessionID: parentSessionID,
				Name:            event.Name,
				Task:            event.Task,
				Capabilities:    event.Capabilities,
			}}, nil
		case LifecycleState:
			if !r.IsChild(event.SubagentSessionID) {
				return nil, errors.New("native agent state precedes its spawn")
			}
			return RoutedEvent{Value: core.NativeAgentEventState{SessionID: event.SubagentSessionID, State: event.State}}, nil
		}
	}
	if r.IsChild(addressed) {
		encoded, err := jsonvalue.Marshal(update)
		var decoded schema.SessionUpdate
		if err == nil {
			err = json.Unmarshal(encoded, &decoded)
		}
		if err != nil {
			return nil, fmt.Errorf("decode native child update: %w", err)
		}
		return RoutedEvent{Value: core.NativeAgentEventUpdate{SessionID: addressed, Update: decoded}}, nil
	}
	return RoutedParent{}, nil
}
