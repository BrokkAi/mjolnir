//! Reporting for a poll loop that waits on a busy lock, so a long wait names
//! the lock, the waiter and the holder instead of staying silent.

use std::time::{Duration, Instant};

/// A wait this short is ordinary contention and is not reported.
const FIRST_REPORT: Duration = Duration::from_secs(5);
/// A wait that keeps going repeats its line at this interval.
const REPEAT_REPORT: Duration = Duration::from_secs(60);

/// One wait on one lock. Create it when the first attempt finds the lock
/// busy, call [`blocked`](Self::blocked) on every failed attempt, and end it
/// with [`acquired`](Self::acquired). Dropping it unfinished (cancellation or
/// an error) ends the wait as "gave up".
pub struct LockWait {
    subject: &'static str,
    started: Instant,
    next_report: Duration,
    reports: u32,
    /// Captured at the first report so the closing line names the same lock.
    named: Option<(String, String)>,
}

impl LockWait {
    /// `subject` completes the sentence "waiting for ..." in the log lines.
    pub fn new(subject: &'static str) -> Self {
        Self {
            subject,
            started: Instant::now(),
            next_report: FIRST_REPORT,
            reports: 0,
            named: None,
        }
    }

    /// Whether a line is due after `elapsed`, and when the next one is.
    fn due(&mut self, elapsed: Duration) -> bool {
        if elapsed < self.next_report {
            return false;
        }
        self.next_report = elapsed + REPEAT_REPORT;
        true
    }

    /// An attempt found the lock busy. `holder` is only evaluated when a line
    /// is due.
    pub fn blocked(&mut self, lock: &str, waiter: &str, holder: impl FnOnce() -> String) {
        let elapsed = self.started.elapsed();
        if !self.due(elapsed) {
            return;
        }
        let holder = holder();
        let waited_seconds = elapsed.as_secs();
        if self.reports == 0 {
            tracing::info!(lock, waiter, %holder, waited_seconds, "waiting for {}", self.subject);
        } else {
            tracing::info!(lock, waiter, %holder, waited_seconds, "still waiting for {}", self.subject);
        }
        self.reports += 1;
        self.named = Some((lock.to_owned(), waiter.to_owned()));
    }

    /// The lock was taken.
    pub fn acquired(mut self) {
        self.finish("acquired");
    }

    fn finish(&mut self, outcome: &'static str) {
        if let Some((lock, waiter)) = self.named.take() {
            tracing::info!(
                lock = %lock,
                waiter = %waiter,
                waited_seconds = self.started.elapsed().as_secs(),
                outcome,
                "finished waiting for {}",
                self.subject
            );
        }
    }
}

impl Drop for LockWait {
    fn drop(&mut self) {
        self.finish("gave up");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_after_five_seconds_then_once_a_minute() {
        let mut wait = LockWait::new("a test lock");
        let due: Vec<u64> = (0..=190)
            .filter(|second| wait.due(Duration::from_secs(*second)))
            .collect();
        assert_eq!(due, [5, 65, 125, 185]);
    }
}
