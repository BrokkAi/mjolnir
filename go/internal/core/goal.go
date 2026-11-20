// Port of mj-core/src/goal.rs.
//
// Shared interpretation of the ACP goal and execution extensions.

package core

import (
	"bytes"
	"encoding/json"
	"fmt"
	"sort"
	"strings"

	acp "github.com/BrokkAi/acp-go/schema"
)

const NativeArtifactRoot = "mjolnir-goals"

const ProjectionKey = "mj_goal_state"

// GoalControlAction is an explicit user control; creating an objective
// remains an agent command.
type GoalControlAction string

const (
	GoalControlActionPause  GoalControlAction = "pause"
	GoalControlActionResume GoalControlAction = "resume"
	GoalControlActionClear  GoalControlAction = "clear"
)

// GoalControlActionAll is GoalControlAction::ALL.
var GoalControlActionAll = [3]GoalControlAction{GoalControlActionPause, GoalControlActionResume, GoalControlActionClear}

func (a GoalControlAction) AsStr() string { return string(a) }

// GoalControlActionParse is GoalControlAction::parse.
func GoalControlActionParse(text string) (GoalControlAction, bool) {
	for _, action := range GoalControlActionAll {
		if strings.EqualFold(text, action.AsStr()) {
			return action, true
		}
	}
	return "", false
}

type GoalCapability struct {
	Version       uint32   `json:"version"`
	ControlMethod string   `json:"controlMethod"`
	Actions       []string `json:"actions"`
}

func (c *GoalCapability) Supports(action GoalControlAction) bool {
	if c.Version != 1 || c.ControlMethod != "_session/goal" {
		return false
	}
	for _, value := range c.Actions {
		if value == action.AsStr() {
			return true
		}
	}
	return false
}

const RecoveryID = "session-goal-recovery"

type GoalSnapshot struct {
	Objective     string  `json:"objective"`
	Status        string  `json:"status"`
	CreatedAt     *int64  `json:"createdAt"`
	ControlMethod *string `json:"controlMethod"`
	// Details holds every other field (serde flatten).
	Details map[string]any `json:"-"`
}

// MarshalJSON writes the named fields, then the flattened details in key order.
func (g GoalSnapshot) MarshalJSON() ([]byte, error) {
	type named struct {
		Objective     string  `json:"objective"`
		Status        string  `json:"status"`
		CreatedAt     *int64  `json:"createdAt"`
		ControlMethod *string `json:"controlMethod"`
	}
	head, err := json.Marshal(named{g.Objective, g.Status, g.CreatedAt, g.ControlMethod})
	if err != nil {
		return nil, err
	}
	keys := make([]string, 0, len(g.Details))
	for key := range g.Details {
		switch key {
		case "objective", "status", "createdAt", "controlMethod":
			continue
		}
		keys = append(keys, key)
	}
	sort.Strings(keys)
	var out bytes.Buffer
	out.Write(head[:len(head)-1])
	for _, key := range keys {
		encodedKey, err := json.Marshal(key)
		if err != nil {
			return nil, err
		}
		encodedValue, err := json.Marshal(g.Details[key])
		if err != nil {
			return nil, err
		}
		out.WriteByte(',')
		out.Write(encodedKey)
		out.WriteByte(':')
		out.Write(encodedValue)
	}
	out.WriteByte('}')
	return out.Bytes(), nil
}

func (g *GoalSnapshot) UnmarshalJSON(data []byte) error {
	type named struct {
		Objective     *string `json:"objective"`
		Status        *string `json:"status"`
		CreatedAt     *int64  `json:"createdAt"`
		ControlMethod *string `json:"controlMethod"`
	}
	var head named
	if err := json.Unmarshal(data, &head); err != nil {
		return err
	}
	if head.Objective == nil {
		return fmt.Errorf("missing field `objective`")
	}
	if head.Status == nil {
		return fmt.Errorf("missing field `status`")
	}
	var rest map[string]json.RawMessage
	if err := json.Unmarshal(data, &rest); err != nil {
		return err
	}
	details := map[string]any{}
	for key, raw := range rest {
		switch key {
		case "objective", "status", "createdAt", "controlMethod":
			continue
		}
		decoder := json.NewDecoder(bytes.NewReader(raw))
		decoder.UseNumber()
		var value any
		if err := decoder.Decode(&value); err != nil {
			return err
		}
		details[key] = value
	}
	*g = GoalSnapshot{*head.Objective, *head.Status, head.CreatedAt, head.ControlMethod, details}
	return nil
}

func (g *GoalSnapshot) Active() bool { return g.Status == "active" }

func (g *GoalSnapshot) SameGoal(other *GoalSnapshot) bool {
	if g.Objective != other.Objective {
		return false
	}
	if g.CreatedAt == nil || other.CreatedAt == nil {
		return g.CreatedAt == nil && other.CreatedAt == nil
	}
	return *g.CreatedAt == *other.CreatedAt
}

type GoalExecution struct {
	Version  uint32  `json:"version"`
	Revision *uint64 `json:"revision"`
	Status   string  `json:"status"`
	TurnID   *string `json:"turnId"`
}

type GoalDecision struct {
	Goal   GoalSnapshot `json:"goal"`
	Resume bool         `json:"resume"`
}

// GoalMeta is the goal entry of an adapter's _meta: goal controls in the
// initialize reply, or a goal snapshot in a session_info_update.
//
// An explicit null is returned as (nil, true).
func GoalMeta(meta map[string]any) (any, bool) {
	if jetbrains, ok := meta["jetbrains"].(map[string]any); ok {
		if air, ok := jetbrains["air"].(map[string]any); ok {
			if goal, ok := air["goal"]; ok {
				return goal, true
			}
		}
	}
	// Read records written by older adapters; current adapters send only AIR.
	goal, ok := meta["goal"]
	return goal, ok
}

type GoalState struct {
	Capability *GoalCapability `json:"capability,omitempty"`
	Known      bool            `json:"known"`
	Snapshot   *GoalSnapshot   `json:"snapshot"`
	Execution  *GoalExecution  `json:"execution"`
	// AnsweredResume is the explicit resume request whose question has
	// already been answered.
	AnsweredResume *string       `json:"answered_resume"`
	PendingResume  *string       `json:"pending_resume"`
	Decision       *GoalDecision `json:"decision"`
}

// GoalStateFromConfiguration is GoalState::from_configuration.
func GoalStateFromConfiguration(configuration map[string]any) (GoalState, error) {
	panic("unported: mj-core/src/goal.rs GoalState::from_configuration")
}

func (s *GoalState) Active() bool { return s.Snapshot != nil && s.Snapshot.Active() }

func (s *GoalState) Running() bool { return s.Execution != nil && s.Execution.Status == "running" }

// LimitReason is why a limited goal stopped, as the adapter reports it:
// usage for the account usage limit, budget for the goal's own token budget.
func (s *GoalState) LimitReason() (string, bool) {
	if s.Snapshot == nil || s.Snapshot.Status != "limited" {
		return "", false
	}
	reason, ok := s.Snapshot.Details["limitReason"].(string)
	return reason, ok
}

// BudgetLimited: the goal spent its own budget, so only the user may
// continue it.
func (s *GoalState) BudgetLimited() bool {
	reason, ok := s.LimitReason()
	return ok && reason == "budget"
}

// ResumableAfterQuota: the account usage limit stopped the goal, and the
// adapter can restart it with an identity check once the limit resets.
func (s *GoalState) ResumableAfterQuota() bool {
	reason, ok := s.LimitReason()
	return ok && reason == "usage" &&
		s.Supports(GoalControlActionResume) &&
		s.Snapshot != nil && s.Snapshot.CreatedAt != nil &&
		s.Snapshot.ControlMethod != nil && *s.Snapshot.ControlMethod == "_session/goal"
}

func (s *GoalState) Synchronized() bool {
	if !s.Known || s.Execution == nil {
		return false
	}
	if s.Snapshot == nil {
		return true
	}
	switch s.Snapshot.Status {
	case "active", "paused", "blocked", "complete", "limited":
		return true
	}
	return false
}

func (s *GoalState) Supports(action GoalControlAction) bool {
	return s.Capability != nil && s.Capability.Supports(action)
}

func (s *GoalState) Restart() {
	s.Capability = nil
	s.Known = false
	s.Execution = nil
}

// Apply applies a session update: missing fields are unchanged; explicit
// null clears a goal. The goal is read with GoalMeta.
func (s *GoalState) Apply(update *acp.SessionUpdate) (bool, error) {
	panic("unported: mj-core/src/goal.rs GoalState::apply")
}

// GoalJournal persists a recovery decision before sending its native
// control action.
type GoalJournal func(acp.SessionUpdate) error

// String is the Debug impl: the journal prints only its name.
func (GoalJournal) String() string { return "GoalJournal" }

type GoalRecoveryContext struct {
	State   GoalState
	Request *string
	Journal GoalJournal
}

func (c *GoalRecoveryContext) Asking() bool {
	if c.State.PendingResume != nil {
		return true
	}
	return c.Request != nil && (c.State.AnsweredResume == nil || *c.State.AnsweredResume != *c.Request)
}

func (c *GoalRecoveryContext) RequestID() (string, bool) {
	if c.State.PendingResume != nil {
		return *c.State.PendingResume, true
	}
	if c.Request != nil {
		return *c.Request, true
	}
	return "", false
}
