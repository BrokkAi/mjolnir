//! Per-session receipt writes. Optimistic acknowledgement is distinct from
//! durability, and one slow session must not hold up another session's save.

use mj_core::state::SessionRecord;
use std::collections::BTreeMap;

pub(super) fn preserve_read_positions(
    incoming: &mut BTreeMap<String, SessionRecord>,
    previous: &BTreeMap<String, SessionRecord>,
) {
    for (id, session) in incoming {
        if let Some(previous) = previous.get(id) {
            session.viewed_through_event_ordinal = session
                .viewed_through_event_ordinal
                .max(previous.viewed_through_event_ordinal);
        }
    }
}

#[derive(Default)]
pub(super) struct ReadReceipts {
    sessions: BTreeMap<String, Receipt>,
    pub failures: BTreeMap<String, String>,
}

#[derive(Default)]
struct Receipt {
    desired: u64,
    saved: u64,
    in_flight: Option<u64>,
    failures: u8,
}

impl Receipt {
    fn start(&mut self) -> Option<u64> {
        if self.in_flight.is_some() || self.desired <= self.saved || self.failures >= 2 {
            return None;
        }
        self.in_flight = Some(self.desired);
        self.in_flight
    }
}

impl ReadReceipts {
    pub fn acknowledge(&mut self, session_id: &str, through: u64) -> Option<u64> {
        let receipt = self.sessions.entry(session_id.to_owned()).or_default();
        if through > receipt.desired {
            receipt.desired = through;
            receipt.failures = 0;
        }
        receipt.start()
    }

    pub fn complete(&mut self, session_id: &str, result: Result<u64, String>) -> Option<u64> {
        let receipt = self.sessions.get_mut(session_id)?;
        let sent = receipt.in_flight.take()?;
        match result {
            Ok(saved) if saved >= sent => {
                receipt.saved = receipt.saved.max(saved);
                receipt.failures = 0;
                self.failures.remove(session_id);
            }
            result => {
                receipt.failures += 1;
                let error = match result {
                    Err(error) => error,
                    Ok(saved) => format!("save acknowledged only event {saved}, expected {sent}"),
                };
                self.failures.insert(session_id.to_owned(), error);
            }
        }
        receipt.start()
    }

    pub fn is_saving(&self) -> bool {
        self.sessions
            .values()
            .any(|receipt| receipt.in_flight.is_some())
    }

    pub fn retry_failed(&mut self) -> Vec<(String, u64)> {
        self.sessions
            .iter_mut()
            .filter_map(|(id, receipt)| {
                if receipt.in_flight.is_none() && receipt.desired > receipt.saved {
                    receipt.failures = 1;
                    receipt.start().map(|through| (id.clone(), through))
                } else {
                    None
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_sessions_save_concurrently_and_updates_coalesce() {
        let mut receipts = ReadReceipts::default();
        assert_eq!(receipts.acknowledge("a", 3), Some(3));
        assert_eq!(receipts.acknowledge("b", 8), Some(8));
        assert_eq!(receipts.acknowledge("a", 5), None);
        assert_eq!(receipts.acknowledge("a", 4), None);
        assert_eq!(receipts.complete("a", Ok(3)), Some(5));
        assert_eq!(receipts.complete("b", Ok(8)), None);
        assert!(receipts.is_saving());
        assert_eq!(receipts.complete("a", Ok(5)), None);
        assert!(!receipts.is_saving());
        assert_eq!(receipts.acknowledge("a", 4), None);
    }

    #[test]
    fn failed_saves_retry_boundedly_and_remain_available_for_shutdown() {
        let mut receipts = ReadReceipts::default();
        assert_eq!(receipts.acknowledge("a", 3), Some(3));
        assert_eq!(receipts.complete("a", Err("offline".into())), Some(3));
        assert_eq!(receipts.complete("a", Err("offline".into())), None);
        assert!(!receipts.is_saving());
        assert_eq!(receipts.failures["a"], "offline");
        assert_eq!(receipts.retry_failed(), vec![("a".into(), 3)]);
        assert!(receipts.is_saving());
        assert_eq!(receipts.complete("a", Ok(3)), None);
        assert!(receipts.failures.is_empty());
    }
}
