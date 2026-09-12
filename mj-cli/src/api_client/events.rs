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
}

impl EventDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<ApiEvent>> {
        let mut events = Vec::new();
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
            event: mj_controller::database::ApiEventData::Error {
                message: "é".repeat(70000),
                command_id: None,
            },
        };
        let frame = format!(
            ": heartbeat\r\n\r\nid: 3\r\nevent: error\r\ndata: {}\r\n\r\n",
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
        let body = r#"{"seq":2,"session_id":"s","recorded_at_ms":1,"type":"error","data":{"message":"failed","command_id":null}}"#;
        assert!(
            EventDecoder::default()
                .push(format!("id: 1\nevent: error\ndata: {body}\n\n").as_bytes())
                .is_err()
        );
        let error = EventDecoder::default()
            .push(b"event: stream_error\ndata: reconnect\n\n")
            .unwrap_err();
        assert!(error.to_string().contains("reconnect"));
    }
}
