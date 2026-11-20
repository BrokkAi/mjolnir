// Port of mj-worker/src/acp/air_tasks.rs.

package worker

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
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
	panic("unported: mj-worker/src/acp/air_tasks.rs air_task_update")
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
	panic("unported: mj-worker/src/acp/air_tasks.rs stop_background_task")
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
