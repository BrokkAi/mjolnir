// Port of the tests in mj-worker/src/acp/native_agents.rs.

package worker

import (
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestAvailabilityInventoryRequiresExplicitEvidenceAndUniqueIdentities(t *testing.T) {
	agent := jsonvalue.Object{
		{Key: "session_id", Value: "child"},
		{Key: "availability", Value: "available"},
		{Key: "stable_id", Value: "stable"},
	}
	if _, err := decodeInventory(jsonvalue.Object{{Key: "agents", Value: []any{agent}}}); err == nil {
		t.Fatal("an inventory without `complete` must be refused")
	}
	if _, err := decodeInventory(jsonvalue.Object{
		{Key: "agents", Value: []any{agent, agent}},
		{Key: "complete", Value: true},
	}); err == nil {
		t.Fatal("an inventory with duplicate identities must be refused")
	}
	inventory, err := decodeInventory(jsonvalue.Object{
		{Key: "agents", Value: []any{agent}},
		{Key: "complete", Value: false},
	})
	if err != nil {
		t.Fatal(err)
	}
	if inventory.Complete {
		t.Fatal("the inventory says it is incomplete")
	}
	if stable := inventory.Agents[0].StableID; stable == nil || *stable != "stable" {
		t.Fatalf("stable id = %v", stable)
	}
}

func TestRoutesNestedChildrenWithoutConsumingParentOutput(t *testing.T) {
	var router NativeAgentRouter
	spawn := func(id string) jsonvalue.Object {
		return jsonvalue.Object{
			{Key: "sessionUpdate", Value: "subagent_spawned"},
			{Key: "subagentSessionId", Value: id},
			{Key: "name", Value: "review"},
			{Key: "task", Value: "review source"},
			{Key: "capabilities", Value: jsonvalue.Object{}},
		}
	}
	routed, err := router.Route("root", spawn("child"))
	if err != nil {
		t.Fatal(err)
	}
	if event, ok := routed.(RoutedEvent); !ok {
		t.Fatalf("routed = %#v", routed)
	} else if spawned, ok := event.Value.(core.NativeAgentEventSpawned); !ok || spawned.ParentSessionID != nil {
		t.Fatalf("event = %#v", event.Value)
	}
	routed, err = router.Route("child", spawn("grandchild"))
	if err != nil {
		t.Fatal(err)
	}
	if event, ok := routed.(RoutedEvent); !ok {
		t.Fatalf("routed = %#v", routed)
	} else if spawned, ok := event.Value.(core.NativeAgentEventSpawned); !ok || spawned.ParentSessionID == nil || *spawned.ParentSessionID != "child" {
		t.Fatalf("event = %#v", event.Value)
	}
	message := jsonvalue.Object{
		{Key: "sessionUpdate", Value: "agent_message_chunk"},
		{Key: "content", Value: jsonvalue.Object{{Key: "type", Value: "text"}, {Key: "text", Value: "hello"}}},
	}
	routed, err = router.Route("root", message)
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := routed.(RoutedParent); !ok {
		t.Fatalf("routed = %#v", routed)
	}
	routed, err = router.Route("grandchild", message)
	if err != nil {
		t.Fatal(err)
	}
	if event, ok := routed.(RoutedEvent); !ok {
		t.Fatalf("routed = %#v", routed)
	} else if update, ok := event.Value.(core.NativeAgentEventUpdate); !ok || update.SessionID != "grandchild" {
		t.Fatalf("event = %#v", event.Value)
	}
}
