// Port of mj-core/src/goal.rs.
//
// Shared interpretation of the ACP goal and execution extensions.

package core

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"sort"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
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
func GoalMeta(meta jsonvalue.Object) (any, bool) {
	if jetbrains, ok := meta.Get("jetbrains"); ok {
		if jetbrains, ok := jetbrains.(jsonvalue.Object); ok {
			if air, ok := jetbrains.Get("air"); ok {
				if air, ok := air.(jsonvalue.Object); ok {
					if goal, ok := air.Get("goal"); ok {
						return goal, true
					}
				}
			}
		}
	}
	// Read records written by older adapters; current adapters send only AIR.
	return meta.Get("goal")
}

// goalDecode is serde_json::from_value into out. A null decodes only into
// an Option (a pointer out of a pointer); into a struct it fails as serde
// fails.
func goalDecode(value any, out any, nullable bool) error {
	if value == nil && !nullable {
		return errors.New("invalid type: null, expected a struct")
	}
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		return err
	}
	return json.Unmarshal(encoded, out)
}

// goalRequireFields fails as serde fails when a required field is absent.
func goalRequireFields(data []byte, fields ...string) error {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return err
	}
	if members == nil {
		return nil
	}
	for _, field := range fields {
		if _, ok := members[field]; !ok {
			return fmt.Errorf("missing field `%s`", field)
		}
	}
	return nil
}

func (c *GoalCapability) UnmarshalJSON(data []byte) error {
	if err := goalRequireFields(data, "version", "controlMethod", "actions"); err != nil {
		return err
	}
	type plain GoalCapability
	return json.Unmarshal(data, (*plain)(c))
}

func (e *GoalExecution) UnmarshalJSON(data []byte) error {
	if err := goalRequireFields(data, "version", "status"); err != nil {
		return err
	}
	type plain GoalExecution
	return json.Unmarshal(data, (*plain)(e))
}

func (d *GoalDecision) UnmarshalJSON(data []byte) error {
	if err := goalRequireFields(data, "goal", "resume"); err != nil {
		return err
	}
	type plain GoalDecision
	return json.Unmarshal(data, (*plain)(d))
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
	value, ok := configuration[ProjectionKey]
	if !ok {
		return GoalState{}, nil
	}
	if value == nil {
		return GoalState{}, errors.New("decode projected goal state: invalid type: null, expected struct GoalState")
	}
	encoded, err := json.Marshal(value)
	if err != nil {
		return GoalState{}, fmt.Errorf("decode projected goal state: %w", err)
	}
	var state GoalState
	if err := json.Unmarshal(encoded, &state); err != nil {
		return GoalState{}, fmt.Errorf("decode projected goal state: %w", err)
	}
	return state, nil
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
	if update.Kind != acp.SessionUpdateKindSessionInfoUpdate || update.SessionInfoUpdate == nil {
		return false, nil
	}
	meta := update.SessionInfoUpdate.Meta
	if meta == nil {
		return false, nil
	}
	changed := false
	if value, ok := meta.Get("mjGoalCapability"); ok {
		var capability *GoalCapability
		if err := goalDecode(value, &capability, true); err != nil {
			return false, fmt.Errorf("decode goal capability: %w", err)
		}
		s.Capability = capability
		changed = true
	}
	if goal, ok := GoalMeta(meta); ok {
		wasActive := s.Active()
		var snapshot *GoalSnapshot
		if err := goalDecode(goal, &snapshot, true); err != nil {
			return false, fmt.Errorf("decode ACP goal state: %w", err)
		}
		s.Snapshot = snapshot
		// A later pause or replacement supersedes an unacknowledged resume.
		if decision := s.Decision; decision != nil {
			current := s.Snapshot
			stillCurrent := current != nil && current.SameGoal(&decision.Goal) &&
				(current.Status == "active" || current.Status == "paused")
			laterPause := decision.Resume && current != nil && current.Status == "paused" &&
				(wasActive || func() bool {
					newer, ok := goalDetailI64(current.Details, "updatedAt")
					older, okOld := goalDetailI64(decision.Goal.Details, "updatedAt")
					return ok && okOld && newer > older
				}())
			if !stillCurrent || laterPause {
				s.Decision = nil
			}
		}
		s.Known = true
		changed = true
	}
	if value, ok := meta.Get("execution"); ok {
		var execution GoalExecution
		if err := goalDecode(value, &execution, false); err != nil {
			return false, fmt.Errorf("decode ACP execution state: %w", err)
		}
		if execution.Version != 1 || (execution.Status != "running" && execution.Status != "idle") {
			return false, errors.New("unsupported ACP execution state")
		}
		if s.Execution != nil && s.Execution.Revision != nil && execution.Revision != nil &&
			*execution.Revision < *s.Execution.Revision {
			return changed, nil
		}
		// Ignore an old completion that arrived after a newer native turn.
		var currentTurn *string
		if s.Execution != nil {
			currentTurn = s.Execution.TurnID
		}
		if execution.Status == "running" || !s.Running() || currentTurn == nil ||
			execution.TurnID == nil || *currentTurn == *execution.TurnID {
			s.Execution = &execution
		}
		changed = true
	}
	if value, ok := meta.Get("mjGoalResumePending"); ok {
		if request, ok := value.(string); ok {
			s.PendingResume = &request
			changed = true
		}
	}
	if value, ok := meta.Get("mjGoalResumeAnswered"); ok {
		if request, ok := value.(string); ok {
			s.AnsweredResume = &request
			s.PendingResume = nil
			changed = true
		}
	}
	if value, ok := meta.Get("mjGoalDecision"); ok {
		var decision *GoalDecision
		if err := goalDecode(value, &decision, true); err != nil {
			return false, fmt.Errorf("decode goal decision: %w", err)
		}
		s.Decision = decision
		changed = true
	}
	return changed, nil
}

// goalDetailI64 is details.get(key).and_then(Value::as_i64).
func goalDetailI64(details map[string]any, key string) (int64, bool) {
	switch value := details[key].(type) {
	case json.Number:
		number, err := value.Int64()
		return number, err == nil
	case int64:
		return value, true
	case uint64:
		if value <= 1<<63-1 {
			return int64(value), true
		}
	}
	return 0, false
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
