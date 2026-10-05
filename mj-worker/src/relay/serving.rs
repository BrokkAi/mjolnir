use super::*;
use std::io::{BufRead, Write};
pub fn serve_relay_json_lines(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    relay: &mut DurableRelay,
) -> Result<()> {
    while let Some(request) = read_relay_frame(reader)? {
        let response = relay.handle(request);
        write_relay_frame(writer, &response)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::relay::test_support::*;

    #[test]
    fn relay_has_a_hard_protocol_v1_floor() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let response = relay.handle(RelayRequestEnvelope {
            request_id: "hello-old".into(),
            protocol_version: 0,
            request: RelayRequest::Hello {
                controller_version: "old".into(),
                supported: RelayVersionRange { min: 0, max: 0 },
            },
        });
        assert!(matches!(
            response.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::IncompatibleProtocol,
                    retryable: false,
                    ..
                }
            }
        ));
    }

    // Hard-won: 0a278358: protocol 2 only prevented a new controller from attaching to a live v1 worker.
    #[test]
    fn current_range_overlaps_protocol_v1() {
        let v1 = RelayVersionRange { min: 1, max: 1 };
        let v2 = RelayVersionRange { min: 2, max: 2 };
        assert_eq!(RelayVersionRange::CURRENT.negotiate(v1), Some(1));
        assert_eq!(v1.negotiate(RelayVersionRange::CURRENT), Some(1));
        assert_eq!(
            RelayVersionRange::CURRENT.negotiate(RelayVersionRange::CURRENT),
            Some(RELAY_PROTOCOL_VERSION)
        );
        assert_eq!(v1.negotiate(v2), None);
        assert!(RelayVersionRange::CURRENT.contains(1));
        assert!(RelayVersionRange::CURRENT.contains(2));
        assert!(RelayVersionRange::CURRENT.contains(3));
        assert!(RelayVersionRange::CURRENT.contains(4));
        assert!(!RelayVersionRange::CURRENT.contains(0));
        assert!(!RelayVersionRange::CURRENT.contains(RELAY_PROTOCOL_VERSION + 1));
        assert!(RelayRequest::Status.supported_at(1));
        assert_eq!(RelayCommand::Cancel.minimum_protocol(), 1);
        assert!(
            !RelayRequest::RespondElicitation {
                elicitation_id: String::new(),
                response: mj_core::elicitation::ElicitationResponse::Cancel,
            }
            .supported_at(1)
        );
        let cancel_turn = RelayRequest::Submit {
            command_id: "cancel-turn".into(),
            command: RelayCommand::CancelTurn,
        };
        assert_eq!(cancel_turn.minimum_protocol(), 7);
        assert!(!cancel_turn.supported_at(6));
        assert!(cancel_turn.supported_at(RELAY_PROTOCOL_VERSION));
        let stop_background = RelayRequest::StopBackgroundTask {
            background_task_id: "terminal:task-1".into(),
        };
        assert_eq!(stop_background.minimum_protocol(), 9);
        assert!(!stop_background.supported_at(8));
        assert!(stop_background.supported_at(RELAY_PROTOCOL_VERSION));
    }

    #[test]
    fn hello_refuses_readers_that_cannot_preserve_provider_details() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let response = relay.handle(RelayRequestEnvelope {
            request_id: "hello-v1".into(),
            protocol_version: 1,
            request: RelayRequest::Hello {
                controller_version: "old".into(),
                supported: RelayVersionRange { min: 1, max: 1 },
            },
        });
        assert_eq!(response.protocol_version, 1);
        assert!(matches!(
            response.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::IncompatibleProtocol,
                    ..
                }
            }
        ));
    }

    /// A worker built before the field existed answers hello without it. That
    /// hello must still parse, reporting no build.
    #[test]
    fn a_hello_without_a_worker_build_parses() {
        let payload: RelayResponsePayload = serde_json::from_value(serde_json::json!({
            "type": "hello",
            "data": {
                "negotiated": 1,
                "relay_version": "2.0.0",
                "session_id": SESSION,
            },
        }))
        .expect("an old worker's hello must still parse");
        assert!(matches!(
            payload,
            RelayResponsePayload::Hello {
                worker_build: None,
                ..
            }
        ));
    }

    /// A worker serves only the current protocol. An older controller is
    /// refused even for a request every protocol has had.
    #[test]
    fn a_request_below_the_current_protocol_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let response = relay.handle(RelayRequestEnvelope {
            request_id: "status-old".into(),
            protocol_version: RELAY_PROTOCOL_VERSION - 1,
            request: RelayRequest::Status,
        });
        assert!(matches!(
            response.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::IncompatibleProtocol,
                    ..
                }
            }
        ));
    }

    #[test]
    fn protocol_v1_cannot_respond_to_elicitation_on_the_durable_relay() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let response = relay.handle(RelayRequestEnvelope {
            request_id: "elicit-v1".into(),
            protocol_version: 1,
            request: RelayRequest::RespondElicitation {
                elicitation_id: "form-1".into(),
                response: mj_core::elicitation::ElicitationResponse::Cancel,
            },
        });
        assert!(matches!(
            response.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::IncompatibleProtocol,
                    retryable: false,
                    ..
                }
            }
        ));
    }
    #[test]
    fn goal_control_requires_protocol_eleven_and_old_readers_cannot_attach() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let response = relay.handle(RelayRequestEnvelope {
            request_id: "old-goal-control".into(),
            protocol_version: 10,
            request: RelayRequest::Submit {
                command_id: "clear".into(),
                command: RelayCommand::GoalControl {
                    action: mj_core::goal::GoalControlAction::Clear,
                },
            },
        });
        assert!(matches!(
            response.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::IncompatibleProtocol,
                    ..
                }
            }
        ));
        let response = relay.handle(RelayRequestEnvelope {
            request_id: "old-reader".into(),
            protocol_version: 10,
            request: RelayRequest::Hello {
                controller_version: "old".into(),
                supported: RelayVersionRange { min: 1, max: 10 },
            },
        });
        assert!(matches!(
            response.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::IncompatibleProtocol,
                    ..
                }
            }
        ));
    }
}
