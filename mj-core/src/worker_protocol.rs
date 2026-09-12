//! Stable parsing helpers for the relay-v1 JSON boundary.
//!
//! This module owns tolerant method-name parsing so extending the relay does
//! not turn an unknown request into a disconnected proxy.

use serde::Deserialize;
use serde_json::Value;

use crate::relay::{RELAY_PROTOCOL_VERSION, RelayRequest, RelayRequestEnvelope};

#[derive(Debug)]
pub enum DecodedRelayRequest {
    Known(RelayRequestEnvelope),
    Unknown {
        request_id: String,
        protocol_version: u32,
        method: String,
    },
    Invalid {
        request_id: String,
        protocol_version: u32,
        message: String,
    },
}

/// Decode one frame on the incompatible relay-v1 boundary.
///
/// Every outcome is answerable: an unknown method stays a structured protocol
/// error rather than being mistaken for transport loss, and a frame that is
/// not even JSON becomes an `Invalid` response instead of an error that would
/// drop the connection. Requests whose envelope cannot be read at all are
/// answered on this relay's own protocol version, because no version the peer
/// asked for could be recovered from the frame.
pub fn decode_relay_request(bytes: &[u8]) -> DecodedRelayRequest {
    let identity = serde_json::from_slice::<Value>(bytes).unwrap_or_default();
    let request_id = identity
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("invalid-request")
        .to_owned();
    let protocol_version = identity
        .get("protocol_version")
        .and_then(Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .unwrap_or(RELAY_PROTOCOL_VERSION);
    let raw: RawRequestEnvelope = match serde_json::from_slice(bytes) {
        Ok(raw) => raw,
        Err(error) => {
            return DecodedRelayRequest::Invalid {
                request_id,
                protocol_version,
                message: format!("invalid relay request envelope: {error}"),
            };
        }
    };
    let method = raw
        .request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    match serde_json::from_value::<RelayRequest>(raw.request) {
        Ok(request) => DecodedRelayRequest::Known(RelayRequestEnvelope {
            request_id: raw.request_id,
            protocol_version: raw.protocol_version,
            request,
        }),
        Err(_error) if !method.is_empty() && !is_served_relay_method(&method) => {
            DecodedRelayRequest::Unknown {
                request_id: raw.request_id,
                protocol_version: raw.protocol_version,
                method,
            }
        }
        Err(error) => DecodedRelayRequest::Invalid {
            request_id: raw.request_id,
            protocol_version: raw.protocol_version,
            message: format!("invalid {method:?} relay request: {error}"),
        },
    }
}

/// Every method this relay serves, by the name
/// [`RelayRequest::method_name`] gives it. A method missing here would be
/// answered "unsupported" when its own parameters are what is malformed, so a
/// new `RelayRequest` variant must be added here too.
fn is_served_relay_method(method: &str) -> bool {
    matches!(
        method,
        "hello"
            | "attach"
            | "acknowledge"
            | "submit"
            | "status"
            | "credential_state"
            | "read_credentials"
            | "install_credentials"
            | "skills_state"
            | "install_skills"
            | "compact"
            | "respond_elicitation"
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequestEnvelope {
    request_id: String,
    protocol_version: u32,
    request: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_v1_decodes_attach_without_accepting_worker_methods() {
        let request = br#"{"request_id":"r1","protocol_version":1,"request":{"method":"attach","params":{"after_ordinal":7,"after_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}}"#;
        let DecodedRelayRequest::Known(envelope) = decode_relay_request(request) else {
            panic!("expected known relay request");
        };
        assert!(matches!(
            envelope.request,
            RelayRequest::Attach {
                after_ordinal: 7,
                ..
            }
        ));

        let legacy = br#"{"request_id":"r2","protocol_version":1,"request":{"method":"subscribe","params":{"after_seq":0}}}"#;
        assert!(matches!(
            decode_relay_request(legacy),
            DecodedRelayRequest::Unknown { ref method, .. } if method == "subscribe"
        ));

        let old_attach = br#"{"request_id":"r3","protocol_version":1,"request":{"method":"attach","params":{"controller_store_id":"retired-store","after_ordinal":7,"after_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}}"#;
        assert!(matches!(
            decode_relay_request(old_attach),
            DecodedRelayRequest::Invalid { .. }
        ));
    }

    #[test]
    fn malformed_relay_method_is_structurally_invalid() {
        let request = br#"{"request_id":"r1","protocol_version":1,"request":{"method":"acknowledge","params":{}}}"#;
        assert!(matches!(
            decode_relay_request(request),
            DecodedRelayRequest::Invalid { .. }
        ));
    }

    #[test]
    fn credential_methods_decode_on_the_relay_v1_floor_without_a_path() {
        let state =
            br#"{"request_id":"r1","protocol_version":1,"request":{"method":"credential_state"}}"#;
        let DecodedRelayRequest::Known(envelope) = decode_relay_request(state) else {
            panic!("credential_state should decode");
        };
        assert_eq!(envelope.protocol_version, 1);
        assert_eq!(envelope.request, RelayRequest::CredentialState);

        let install = br#"{"request_id":"r2","protocol_version":1,"request":{"method":"install_credentials","params":{"data":"e30="}}}"#;
        let DecodedRelayRequest::Known(envelope) = decode_relay_request(install) else {
            panic!("install_credentials should decode");
        };
        assert_eq!(
            envelope.request,
            RelayRequest::InstallCredentials {
                data: "e30=".into()
            }
        );

        let caller_selected_path = br#"{"request_id":"r3","protocol_version":1,"request":{"method":"read_credentials","params":{"path":"/tmp/stolen"}}}"#;
        assert!(matches!(
            decode_relay_request(caller_selected_path),
            DecodedRelayRequest::Invalid { .. }
        ));
    }

    #[test]
    fn skills_methods_decode_on_the_relay_v1_floor_without_a_path() {
        let state =
            br#"{"request_id":"r1","protocol_version":1,"request":{"method":"skills_state"}}"#;
        let DecodedRelayRequest::Known(envelope) = decode_relay_request(state) else {
            panic!("skills_state should decode");
        };
        assert_eq!(envelope.request, RelayRequest::SkillsState);

        let install = br#"{"request_id":"r2","protocol_version":1,"request":{"method":"install_skills","params":{"data":"SEVMU0tJTDE="}}}"#;
        let DecodedRelayRequest::Known(envelope) = decode_relay_request(install) else {
            panic!("install_skills should decode");
        };
        assert_eq!(
            envelope.request,
            RelayRequest::InstallSkills {
                data: "SEVMU0tJTDE=".into()
            }
        );

        let caller_selected_path = br#"{"request_id":"r3","protocol_version":1,"request":{"method":"install_skills","params":{"data":"e30=","path":"/tmp/planted"}}}"#;
        assert!(matches!(
            decode_relay_request(caller_selected_path),
            DecodedRelayRequest::Invalid { .. }
        ));
    }

    #[test]
    fn relay_v1_rejects_unknown_envelope_and_nested_range_fields() {
        let retired_top_level = br#"{"request_id":"r1","protocol_version":1,"controller_store_id":"retired-store","request":{"method":"status"}}"#;
        assert!(matches!(
            decode_relay_request(retired_top_level),
            DecodedRelayRequest::Invalid { .. }
        ));

        let nested = br#"{"request_id":"r2","protocol_version":1,"request":{"method":"hello","params":{"controller_version":"1.0.0","supported":{"min":1,"max":1,"preferred":1}}}}"#;
        assert!(matches!(
            decode_relay_request(nested),
            DecodedRelayRequest::Invalid { .. }
        ));
    }

    #[test]
    fn a_served_method_with_bad_parameters_is_invalid_not_unsupported() {
        // Connection-served methods are as much this relay's own as the
        // journaled ones: bad parameters must not read as "no such method".
        for frame in [
            br#"{"request_id":"r1","protocol_version":2,"request":{"method":"compact","params":{}}}"#.as_slice(),
            br#"{"request_id":"r2","protocol_version":2,"request":{"method":"respond_elicitation","params":{}}}"#.as_slice(),
        ] {
            match decode_relay_request(frame) {
                DecodedRelayRequest::Invalid { .. } => {}
                other => panic!("expected an invalid-parameter decode, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_frame_that_is_not_json_stays_answerable() {
        // The daemon answers this instead of dropping the connection, so the
        // reply has to carry a protocol version the controller accepts.
        let DecodedRelayRequest::Invalid {
            request_id,
            protocol_version,
            message,
        } = decode_relay_request(b"not json at all")
        else {
            panic!("a non-JSON frame must decode as invalid");
        };
        assert_eq!(request_id, "invalid-request");
        assert_eq!(protocol_version, RELAY_PROTOCOL_VERSION);
        assert!(
            message.contains("invalid relay request envelope"),
            "{message}"
        );
    }
}
