// Port of the tests in mj-core/src/worker_protocol.rs.

package core

import (
	"strings"
	"testing"
)

func decodeKnownRelayRequest(t *testing.T, frame string) *RelayRequestEnvelope {
	t.Helper()
	known, ok := DecodeRelayRequest([]byte(frame)).(DecodedRelayRequestKnown)
	if !ok {
		t.Fatalf("expected known relay request: %#v", DecodeRelayRequest([]byte(frame)))
	}
	return known.Envelope
}

func assertInvalidRelayRequest(t *testing.T, frame string) {
	t.Helper()
	if decoded, ok := DecodeRelayRequest([]byte(frame)).(DecodedRelayRequestInvalid); !ok {
		t.Fatalf("expected an invalid relay request, got %#v for %s", DecodeRelayRequest([]byte(frame)), frame)
	} else if decoded.Message == "" {
		t.Fatalf("an invalid relay request needs a message: %s", frame)
	}
}

func TestRelayV1DecodesAttachWithoutAcceptingWorkerMethods(t *testing.T) {
	envelope := decodeKnownRelayRequest(t, `{"request_id":"r1","protocol_version":1,"request":{"method":"attach","params":{"after_ordinal":7,"after_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}}`)
	if attach, ok := envelope.Request.(RelayRequestAttach); !ok || attach.AfterOrdinal != 7 {
		t.Fatalf("expected attach after ordinal 7, got %#v", envelope.Request)
	}

	legacy := `{"request_id":"r2","protocol_version":1,"request":{"method":"subscribe","params":{"after_seq":0}}}`
	if unknown, ok := DecodeRelayRequest([]byte(legacy)).(DecodedRelayRequestUnknown); !ok || unknown.Method != "subscribe" {
		t.Fatalf("expected unknown subscribe, got %#v", DecodeRelayRequest([]byte(legacy)))
	}

	assertInvalidRelayRequest(t, `{"request_id":"r3","protocol_version":1,"request":{"method":"attach","params":{"controller_store_id":"retired-store","after_ordinal":7,"after_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}}`)
}

func TestMalformedRelayMethodIsStructurallyInvalid(t *testing.T) {
	assertInvalidRelayRequest(t, `{"request_id":"r1","protocol_version":1,"request":{"method":"acknowledge","params":{}}}`)
	assertInvalidRelayRequest(t, `{"request_id":"r2","protocol_version":33,"request":{"method":"ack_mailbox","params":{}}}`)
}

func TestCredentialMethodsDecodeOnTheRelayV1FloorWithoutAPath(t *testing.T) {
	envelope := decodeKnownRelayRequest(t, `{"request_id":"r1","protocol_version":1,"request":{"method":"credential_state"}}`)
	if envelope.ProtocolVersion != 1 {
		t.Fatalf("protocol version = %d", envelope.ProtocolVersion)
	}
	if envelope.Request != (RelayRequestCredentialState{}) {
		t.Fatalf("expected credential_state, got %#v", envelope.Request)
	}

	envelope = decodeKnownRelayRequest(t, `{"request_id":"r2","protocol_version":1,"request":{"method":"install_credentials","params":{"data":"e30="}}}`)
	if envelope.Request != (RelayRequestInstallCredentials{Data: "e30="}) {
		t.Fatalf("expected install_credentials, got %#v", envelope.Request)
	}

	assertInvalidRelayRequest(t, `{"request_id":"r3","protocol_version":1,"request":{"method":"read_credentials","params":{"path":"/tmp/stolen"}}}`)
}

func TestSkillsMethodsDecodeOnTheRelayV1FloorWithoutAPath(t *testing.T) {
	envelope := decodeKnownRelayRequest(t, `{"request_id":"r1","protocol_version":1,"request":{"method":"skills_state"}}`)
	if envelope.Request != (RelayRequestSkillsState{}) {
		t.Fatalf("expected skills_state, got %#v", envelope.Request)
	}

	envelope = decodeKnownRelayRequest(t, `{"request_id":"r2","protocol_version":1,"request":{"method":"install_skills","params":{"data":"SEVMU0tJTDE="}}}`)
	if envelope.Request != (RelayRequestInstallSkills{Data: "SEVMU0tJTDE="}) {
		t.Fatalf("expected install_skills, got %#v", envelope.Request)
	}

	assertInvalidRelayRequest(t, `{"request_id":"r3","protocol_version":1,"request":{"method":"install_skills","params":{"data":"e30=","path":"/tmp/planted"}}}`)
}

func TestRelayV1RejectsUnknownEnvelopeAndNestedRangeFields(t *testing.T) {
	assertInvalidRelayRequest(t, `{"request_id":"r1","protocol_version":1,"controller_store_id":"retired-store","request":{"method":"status"}}`)
	assertInvalidRelayRequest(t, `{"request_id":"r2","protocol_version":1,"request":{"method":"hello","params":{"controller_version":"1.0.0","supported":{"min":1,"max":1,"preferred":1}}}}`)
}

// Hard-won: 0df43902b8: bad compact and elicitation parameters were reported as unsupported methods.
func TestAServedMethodWithBadParametersIsInvalidNotUnsupported(t *testing.T) {
	// Connection-served methods are as much this relay's own as the
	// journaled ones: bad parameters must not read as "no such method".
	for _, frame := range []string{
		`{"request_id":"r1","protocol_version":2,"request":{"method":"compact","params":{}}}`,
		`{"request_id":"r2","protocol_version":2,"request":{"method":"respond_elicitation","params":{}}}`,
		`{"request_id":"r3","protocol_version":33,"request":{"method":"drain_mailbox","params":{}}}`,
	} {
		assertInvalidRelayRequest(t, frame)
	}
}

func TestAFrameThatIsNotJSONStaysAnswerable(t *testing.T) {
	// The daemon answers this instead of dropping the connection, so the
	// reply has to carry a protocol version the controller accepts.
	invalid, ok := DecodeRelayRequest([]byte("not json at all")).(DecodedRelayRequestInvalid)
	if !ok {
		t.Fatal("a non-JSON frame must decode as invalid")
	}
	if invalid.RequestID != "invalid-request" {
		t.Fatalf("request id = %q", invalid.RequestID)
	}
	if invalid.ProtocolVersion != RelayProtocolVersion {
		t.Fatalf("protocol version = %d", invalid.ProtocolVersion)
	}
	if !strings.Contains(invalid.Message, "invalid relay request envelope") {
		t.Fatal(invalid.Message)
	}
}
