// Port of mj-core/src/worker_protocol.rs.
//
// Stable parsing helpers for the relay-v1 JSON boundary.
//
// This module owns tolerant method-name parsing so extending the relay does
// not turn an unknown request into a disconnected proxy.

package core

import (
	"encoding/json"
	"fmt"
	"reflect"
)

// DecodedRelayRequest is the outcome of decoding one relay request frame.
type DecodedRelayRequest interface {
	isDecodedRelayRequest()
}

type DecodedRelayRequestKnown struct {
	Envelope *RelayRequestEnvelope
}

type DecodedRelayRequestUnknown struct {
	RequestID       string
	ProtocolVersion uint32
	Method          string
}

type DecodedRelayRequestInvalid struct {
	RequestID       string
	ProtocolVersion uint32
	Message         string
}

func (DecodedRelayRequestKnown) isDecodedRelayRequest()   {}
func (DecodedRelayRequestUnknown) isDecodedRelayRequest() {}
func (DecodedRelayRequestInvalid) isDecodedRelayRequest() {}

// DecodeRelayRequest decodes one frame on the incompatible relay-v1 boundary.
//
// Every outcome is answerable: an unknown method stays a structured protocol
// error rather than being mistaken for transport loss, and a frame that is
// not even JSON becomes an Invalid response instead of an error that would
// drop the connection. Requests whose envelope cannot be read at all are
// answered on this relay's own protocol version, because no version the peer
// asked for could be recovered from the frame.
func DecodeRelayRequest(data []byte) DecodedRelayRequest {
	var identity map[string]json.RawMessage
	_ = json.Unmarshal(data, &identity)
	requestID := "invalid-request"
	if raw, ok := identity["request_id"]; ok {
		var text string
		if json.Unmarshal(raw, &text) == nil {
			requestID = text
		}
	}
	protocolVersion := RelayProtocolVersion
	if raw, ok := identity["protocol_version"]; ok {
		var version uint32
		if json.Unmarshal(raw, &version) == nil {
			protocolVersion = version
		}
	}
	var raw rawRequestEnvelope
	if err := json.Unmarshal(data, &raw); err != nil {
		return DecodedRelayRequestInvalid{
			RequestID:       requestID,
			ProtocolVersion: protocolVersion,
			Message:         fmt.Sprintf("invalid relay request envelope: %v", err),
		}
	}
	var fields map[string]json.RawMessage
	_ = json.Unmarshal(raw.Request, &fields)
	method := ""
	if rawMethod, ok := fields["method"]; ok {
		_ = json.Unmarshal(rawMethod, &method)
	}
	request, err := UnmarshalRelayRequestJSON(raw.Request)
	switch {
	case err == nil:
		return DecodedRelayRequestKnown{Envelope: &RelayRequestEnvelope{
			RequestID:       raw.RequestID,
			ProtocolVersion: raw.ProtocolVersion,
			Request:         request,
		}}
	case method != "" && !isServedRelayMethod(method):
		return DecodedRelayRequestUnknown{RequestID: raw.RequestID, ProtocolVersion: raw.ProtocolVersion, Method: method}
	}
	return DecodedRelayRequestInvalid{
		RequestID:       raw.RequestID,
		ProtocolVersion: raw.ProtocolVersion,
		Message:         fmt.Sprintf("invalid %s relay request: %v", rustDebugString(method), err),
	}
}

// isServedRelayMethod reports every method this relay serves, by the name
// RelayRequestMethodName gives it. A method missing here would be answered
// "unsupported" when its own parameters are what is malformed, so a new
// RelayRequest variant must be added here too.
func isServedRelayMethod(method string) bool {
	switch method {
	case "hello", "attach", "acknowledge", "submit", "status", "drain_mailbox", "ack_mailbox",
		"credential_state", "read_credentials", "install_credentials", "skills_state", "install_skills",
		"history_query", "history_requests", "complete_history_request", "compact", "respond_elicitation":
		return true
	}
	return false
}

// rawRequestEnvelope refuses unknown fields.
type rawRequestEnvelope struct {
	RequestID       string          `json:"request_id"`
	ProtocolVersion uint32          `json:"protocol_version"`
	Request         json.RawMessage `json:"request"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (e *rawRequestEnvelope) UnmarshalJSON(data []byte) error {
	type plain rawRequestEnvelope
	if err := relayRejectUnknownFields(data, reflect.TypeFor[plain](), ""); err != nil {
		return err
	}
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	for _, name := range []string{"request_id", "protocol_version", "request"} {
		if _, ok := fields[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	return json.Unmarshal(data, (*plain)(e))
}
