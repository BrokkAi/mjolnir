// Port of mj-worker/src/acp/air_tasks.rs.

package worker

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"strings"
	"time"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// acpJSONMembers splits a JSON object into its raw members, failing as serde
// does on anything that is not an object.
func acpJSONMembers(data []byte) (map[string]json.RawMessage, error) {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return nil, err
	}
	if members == nil {
		return nil, errors.New("invalid type: null, expected a JSON object")
	}
	return members, nil
}

// acpDecodeRequired decodes the member name into target, failing as a serde
// derive does when the member is absent or null.
func acpDecodeRequired(members map[string]json.RawMessage, name string, target any) error {
	raw, ok := members[name]
	if !ok {
		return fmt.Errorf("missing field `%s`", name)
	}
	if bytes.Equal(bytes.TrimSpace(raw), []byte("null")) {
		return fmt.Errorf("invalid type: null for field `%s`", name)
	}
	return json.Unmarshal(raw, target)
}

// acpDecodeRequiredValue decodes the member name as a serde_json::Value: it
// must be present, and may be null.
func acpDecodeRequiredValue(members map[string]json.RawMessage, name string) (any, error) {
	raw, ok := members[name]
	if !ok {
		return nil, fmt.Errorf("missing field `%s`", name)
	}
	return jsonvalue.Parse(raw)
}

// acpSessionValueJSON encodes {"sessionId": sessionID, name: value} as serde
// writes the derived notification structs below.
func acpSessionValueJSON(sessionID schema.SessionId, name string, value any) ([]byte, error) {
	return jsonvalue.Marshal(jsonvalue.Object{{Key: "sessionId", Value: string(sessionID)}, {Key: name, Value: value}})
}

// acpDecodeSessionValue decodes {"sessionId": .., name: ..}, both required.
func acpDecodeSessionValue(data []byte, name string) (schema.SessionId, any, error) {
	members, err := acpJSONMembers(data)
	if err != nil {
		return "", nil, err
	}
	var sessionID schema.SessionId
	if err := acpDecodeRequired(members, "sessionId", &sessionID); err != nil {
		return "", nil, err
	}
	value, err := acpDecodeRequiredValue(members, name)
	return sessionID, value, err
}

// claudeSdkMessageNotificationMethod is ClaudeSdkMessageNotification's
// JSON-RPC method.
const claudeSdkMessageNotificationMethod = "_claude/sdkMessage"

type ClaudeSdkMessageNotification struct {
	SessionID schema.SessionId
	Message   any
}

func (n ClaudeSdkMessageNotification) MarshalJSON() ([]byte, error) {
	return acpSessionValueJSON(n.SessionID, "message", n.Message)
}

func (n *ClaudeSdkMessageNotification) UnmarshalJSON(data []byte) error {
	sessionID, message, err := acpDecodeSessionValue(data, "message")
	if err != nil {
		return err
	}
	*n = ClaudeSdkMessageNotification{SessionID: sessionID, Message: message}
	return nil
}

// grokUsageNotificationMethod is GrokUsageNotification's JSON-RPC method.
const grokUsageNotificationMethod = "_x.ai/session/update"

type GrokUsageNotification struct {
	SessionID schema.SessionId
	Update    any
}

func (n GrokUsageNotification) MarshalJSON() ([]byte, error) {
	return acpSessionValueJSON(n.SessionID, "update", n.Update)
}

func (n *GrokUsageNotification) UnmarshalJSON(data []byte) error {
	sessionID, update, err := acpDecodeSessionValue(data, "update")
	if err != nil {
		return err
	}
	*n = GrokUsageNotification{SessionID: sessionID, Update: update}
	return nil
}

// rawSessionNotificationMethod is RawSessionNotification's JSON-RPC method.
const rawSessionNotificationMethod = "session/update"

// RawSessionNotification is a `session/update` notification with its update
// left undecoded, so the worker can interpret extensions before ACP v1
// decoding.
type RawSessionNotification struct {
	SessionID schema.SessionId
	Update    any
}

func (n RawSessionNotification) MarshalJSON() ([]byte, error) {
	return acpSessionValueJSON(n.SessionID, "update", n.Update)
}

func (n *RawSessionNotification) UnmarshalJSON(data []byte) error {
	sessionID, update, err := acpDecodeSessionValue(data, "update")
	if err != nil {
		return err
	}
	*n = RawSessionNotification{SessionID: sessionID, Update: update}
	return nil
}

// airTaskUpdate decodes AIR tasks independently of the harness and owning
// session. handled is false when update is not an AIR async task update;
// when it is, the change is nil if there is nothing to apply. The error is
// what made an AIR update malformed.
func airTaskUpdate(update any) (change core.AirTaskUpdate, handled bool, err error) {
	kindValue, _ := jsonGetString(update, "sessionUpdate")
	if kindValue == nil {
		return nil, false, nil
	}
	kind := *kindValue
	if !strings.HasPrefix(kind, "async_task_") {
		return nil, false, nil
	}
	taskID := func() (string, error) {
		id, _ := jsonGetString(update, "asyncTaskId")
		if id == nil || strings.TrimSpace(*id) == "" {
			return "", fmt.Errorf("%s requires a non-empty asyncTaskId", kind)
		}
		return *id, nil
	}
	if taskType, _ := jsonGetString(update, "taskType"); kind == "async_task_spawned" && taskType != nil && *taskType == "turn" {
		id, err := taskID()
		if err != nil {
			return nil, true, err
		}
		return core.AirTaskUpdateTurnSpawned{TaskID: id, Replayed: false}, true, nil
	}
	id, err := taskID()
	if err != nil {
		return nil, true, err
	}
	description, _ := jsonGetString(update, "description")
	if description != nil && strings.TrimSpace(*description) == "" {
		description = nil
	}
	if description == nil {
		description, _ = jsonGetString(update, "name")
	}
	var canStop *bool
	if object, ok := update.(jsonvalue.Object); ok {
		if value, ok := object.Get("canStop"); ok {
			if flag, ok := value.(bool); ok {
				canStop = &flag
			}
		}
	}
	switch kind {
	case "async_task_spawned":
		if description == nil {
			return nil, true, errors.New("async_task_spawned requires name or description")
		}
		if canStop == nil {
			return nil, true, errors.New("async_task_spawned requires canStop")
		}
		return core.AirTaskUpdateSpawned{TaskID: id, Description: *description, CanStop: *canStop}, true, nil
	case "async_task_progress":
		return core.AirTaskUpdateChanged{TaskID: id, Description: description, CanStop: canStop}, true, nil
	case "async_task_state_update":
		state, _ := jsonGetString(update, "state")
		switch {
		case state != nil && (*state == "running" || *state == "paused"):
			return core.AirTaskUpdateChanged{TaskID: id, Description: description, CanStop: canStop}, true, nil
		case state != nil && (*state == "completed" || *state == "failed" || *state == "stopped"):
			return core.AirTaskUpdateFinished{TaskID: id, Settled: *state != "stopped", Failed: *state == "failed"}, true, nil
		}
		stateDebug := "None"
		if state != nil {
			stateDebug = "Some(" + sessionConfigDebugString(*state) + ")"
		}
		return nil, true, fmt.Errorf("unknown AIR async task state %s", stateDebug)
	}
	return nil, true, fmt.Errorf("unknown AIR async task update %s", sessionConfigDebugString(kind))
}

// airAsyncTaskStopMethod is AirAsyncTaskStopRequest's JSON-RPC method; its
// response is AirAsyncTaskStopResponse.
const airAsyncTaskStopMethod = "_session/async_task/stop"

type AirAsyncTaskStopRequest struct {
	SessionID   schema.SessionId `json:"sessionId"`
	AsyncTaskID string           `json:"asyncTaskId"`
}

func (r *AirAsyncTaskStopRequest) UnmarshalJSON(data []byte) error {
	members, err := acpJSONMembers(data)
	if err != nil {
		return err
	}
	var request AirAsyncTaskStopRequest
	if err := acpDecodeRequired(members, "sessionId", &request.SessionID); err != nil {
		return err
	}
	if err := acpDecodeRequired(members, "asyncTaskId", &request.AsyncTaskID); err != nil {
		return err
	}
	*r = request
	return nil
}

type AirAsyncTaskStopResponse struct {
	Stopped bool `json:"stopped"`
}

func (r *AirAsyncTaskStopResponse) UnmarshalJSON(data []byte) error {
	members, err := acpJSONMembers(data)
	if err != nil {
		return err
	}
	var response AirAsyncTaskStopResponse
	if err := acpDecodeRequired(members, "stopped", &response.Stopped); err != nil {
		return err
	}
	*r = response
	return nil
}

const backgroundTaskStopTimeout = 5 * time.Second

// stopBackgroundTask stops target; the error is the refusal the relay shows.
func stopBackgroundTask(ctx context.Context, connection *acp.Connection, sessionID schema.SessionId, terminals *TerminalRegistry, target core.BackgroundTaskStopTarget) error {
	switch target := target.(type) {
	case core.BackgroundTaskStopTargetNativeAgent:
		request := NativeAgentCancelRequest{SessionID: sessionID, SubagentSessionID: target.SessionID}
		answered, drop := acpStartCall[NativeAgentCancelResponse](ctx, connection, nativeAgentCancelMethod, request)
		defer drop()
		timer := time.NewTimer(backgroundTaskStopTimeout)
		defer timer.Stop()
		select {
		case response := <-answered:
			switch {
			case response.err != nil:
				return errors.New("cancel native agent: " + response.err.Error())
			case response.value.Cancelled:
				return nil
			}
			return errors.New("native agent is no longer running")
		case <-timer.C:
			return errors.New("timed out cancelling native agent")
		}
	case core.BackgroundTaskStopTargetHostedTerminal:
		if terminals.Kill(target.TerminalID) {
			return nil
		}
		return errors.New("background task is no longer running")
	case core.BackgroundTaskStopTargetAirAsyncTask:
		request := AirAsyncTaskStopRequest{SessionID: schema.SessionId(target.SessionID), AsyncTaskID: target.TaskID}
		answered, drop := acpStartCall[AirAsyncTaskStopResponse](ctx, connection, airAsyncTaskStopMethod, request)
		defer drop()
		timer := time.NewTimer(backgroundTaskStopTimeout)
		defer timer.Stop()
		select {
		case response := <-answered:
			switch {
			case response.err != nil:
				return errors.New("stop AIR background task: " + response.err.Error())
			case response.value.Stopped:
				return nil
			}
			return errors.New("background task is no longer stoppable")
		case <-timer.C:
			return errors.New("timed out stopping AIR background task")
		}
	}
	panic(fmt.Sprintf("unknown background task stop target %T", target))
}

// resolveBackgroundTaskStop stops target and answers resolved, a oneshot
// channel with room for the one result.
func resolveBackgroundTaskStop(ctx context.Context, connection *acp.Connection, sessionID schema.SessionId, terminals *TerminalRegistry, target core.BackgroundTaskStopTarget, resolved chan<- error) {
	result := stopBackgroundTask(ctx, connection, sessionID, terminals, target)
	select {
	case resolved <- result:
	default:
		slog.Debug("background task stop receiver was already closed",
			"session_id", string(sessionID), "operation", "stop_background_task")
	}
}

// nativeAgentCancelMethod is NativeAgentCancelRequest's JSON-RPC method; its
// response is NativeAgentCancelResponse.
const nativeAgentCancelMethod = "_session/subagent/cancel"

type NativeAgentCancelRequest struct {
	SessionID         schema.SessionId `json:"sessionId"`
	SubagentSessionID string           `json:"subagentSessionId"`
}

func (r *NativeAgentCancelRequest) UnmarshalJSON(data []byte) error {
	members, err := acpJSONMembers(data)
	if err != nil {
		return err
	}
	var request NativeAgentCancelRequest
	if err := acpDecodeRequired(members, "sessionId", &request.SessionID); err != nil {
		return err
	}
	if err := acpDecodeRequired(members, "subagentSessionId", &request.SubagentSessionID); err != nil {
		return err
	}
	*r = request
	return nil
}

type NativeAgentCancelResponse struct {
	Cancelled bool `json:"cancelled"`
}

func (r *NativeAgentCancelResponse) UnmarshalJSON(data []byte) error {
	members, err := acpJSONMembers(data)
	if err != nil {
		return err
	}
	var response NativeAgentCancelResponse
	if err := acpDecodeRequired(members, "cancelled", &response.Cancelled); err != nil {
		return err
	}
	*r = response
	return nil
}
