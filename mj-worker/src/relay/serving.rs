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
    fn hello_from_protocol_v1_controller_negotiates_v1() {
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
        match response.body {
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Hello { negotiated, .. },
            } => assert_eq!(negotiated, 1),
            other => panic!("expected a v1 hello, got {other:?}"),
        }
    }

    /// The controller decides whether to replace a worker from what hello
    /// reports, so hello has to carry the build and a worker that was never
    /// told one has to say so rather than guess.
    #[test]
    fn hello_reports_the_worker_build_when_the_worker_knows_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let hello = |relay: &mut DurableRelay| {
            let response = relay.handle(RelayRequestEnvelope {
                request_id: "hello-build".into(),
                protocol_version: RELAY_PROTOCOL_VERSION,
                request: RelayRequest::Hello {
                    controller_version: "current".into(),
                    supported: RelayVersionRange::CURRENT,
                },
            });
            match response.body {
                RelayResponseBody::Ok {
                    payload: RelayResponsePayload::Hello { worker_build, .. },
                } => worker_build,
                other => panic!("expected a hello, got {other:?}"),
            }
        };
        assert_eq!(hello(&mut relay), None);

        relay.set_worker_build(Some("a".repeat(64)));
        assert_eq!(hello(&mut relay), Some("a".repeat(64)));
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

    #[test]
    fn protocol_v1_status_is_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let response = relay.handle(RelayRequestEnvelope {
            request_id: "status-v1".into(),
            protocol_version: 1,
            request: RelayRequest::Status,
        });
        assert_eq!(response.protocol_version, 1);
        assert!(matches!(
            response.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Status(_)
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
}
