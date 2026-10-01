//! Bounded SSE decoding, independent of network chunk boundaries.
use anyhow::{Result, bail, ensure};
use mj_controller::database::ApiEvent;

#[derive(Default)]
pub(crate) struct EventDecoder {
    line: Vec<u8>,
    data: String,
    id: Option<u64>,
    kind: String,
    frame_bytes: usize,
    handoff: Option<u64>,
}

impl EventDecoder {
    /// The cursor to resume from once the daemon announced that an upgrade is
    /// replacing it. Nothing after that announcement is decoded.
    pub(crate) fn handoff(&self) -> Option<u64> {
        self.handoff
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<ApiEvent>> {
        let mut events = Vec::new();
        if self.handoff.is_some() {
            return Ok(events);
        }
        for &byte in bytes {
            self.frame_bytes += 1;
            ensure!(
                self.frame_bytes <= 8 * 1024 * 1024,
                "API event exceeds 8 MiB"
            );
            if byte != b'\n' {
                self.line.push(byte);
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let line = std::str::from_utf8(&line)?.trim_end_matches('\r');
            if line.is_empty() {
                if self.kind == "stream_error" {
                    bail!("{}", self.data.trim_end());
                }
                if self.kind == mj_controller::server::api::DAEMON_HANDOFF_CODE {
                    let Some(cursor) = self.id else {
                        bail!("the daemon handoff announcement names no event ID");
                    };
                    self.handoff = Some(cursor);
                    return Ok(events);
                }
                if !self.data.is_empty() {
                    let event: ApiEvent = serde_json::from_str(&self.data)?;
                    ensure!(
                        self.id == Some(event.seq),
                        "SSE ID does not match API event sequence"
                    );
                    ensure!(
                        self.kind == event.event.kind(),
                        "SSE type does not match API event type"
                    );
                    events.push(event);
                }
                self.data.clear();
                self.id = None;
                self.kind.clear();
                self.frame_bytes = 0;
            } else if !line.starts_with(':') {
                let (field, value) = line.split_once(':').unwrap_or((line, ""));
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "data" => {
                        self.data.push_str(value);
                        self.data.push('\n');
                    }
                    "id" => self.id = Some(value.parse()?),
                    "event" => value.clone_into(&mut self.kind),
                    _ => {}
                }
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_decoder_handles_unicode_large_frames_and_arbitrary_chunks() {
        let event = ApiEvent {
            seq: 3,
            session_id: "session-1".into(),
            recorded_at_ms: 1,
            event: mj_controller::database::ApiEventData::SessionFault {
                reason: mj_core::event_outcome::OutcomeReason::StartupFailed,
                message: "é".repeat(70000),
                command_id: None,
            },
        };
        let frame = format!(
            ": heartbeat\r\n\r\nid: 3\r\nevent: session_fault\r\ndata: {}\r\n\r\n",
            serde_json::to_string(&event).unwrap()
        );
        let mut decoder = EventDecoder::default();
        let mut decoded = Vec::new();
        for chunk in frame.as_bytes().chunks(317) {
            decoded.extend(decoder.push(chunk).unwrap());
        }
        assert_eq!(decoded, [event]);
    }

    #[test]
    fn event_decoder_rejects_conflicting_identity_and_stream_errors() {
        let body = r#"{"seq":2,"session_id":"s","recorded_at_ms":1,"type":"session_fault","data":{"reason":"startup_failed","message":"failed","command_id":null}}"#;
        assert!(
            EventDecoder::default()
                .push(format!("id: 1\nevent: session_fault\ndata: {body}\n\n").as_bytes())
                .is_err()
        );
        let error = EventDecoder::default()
            .push(b"event: stream_error\ndata: reconnect\n\n")
            .unwrap_err();
        assert!(error.to_string().contains("reconnect"));
    }

    #[test]
    fn a_handoff_announcement_keeps_earlier_events_and_names_the_resume_cursor() {
        let body = r#"{"seq":7,"session_id":"s","recorded_at_ms":1,"type":"session_fault","data":{"reason":"startup_failed","message":"failed","command_id":null}}"#;
        let stream = format!(
            "id: 7\nevent: session_fault\ndata: {body}\n\n\
             id: 9\nevent: daemon_handoff\ndata: the daemon is being replaced\n\n\
             id: 10\nevent: session_fault\ndata: not decoded\n\n"
        );
        let mut decoder = EventDecoder::default();
        let events = decoder.push(stream.as_bytes()).unwrap();
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            [7]
        );
        assert_eq!(decoder.handoff(), Some(9));
        assert!(decoder.push(b"id: 11\n\n").unwrap().is_empty());
    }
}
