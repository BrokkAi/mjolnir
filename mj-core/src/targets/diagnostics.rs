//! Diagnostic snapshots only; never use these to decide lifecycle admission.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::CommandSpec;

#[derive(Debug, Clone)]
pub struct BlockingOperationSnapshot {
    pub purpose: String,
    pub program: String,
    pub thread: String,
    pub elapsed_ms: u64,
}

struct ActiveOperation {
    purpose: String,
    program: String,
    thread: String,
    started: Instant,
}

fn operations() -> &'static Mutex<BTreeMap<u64, ActiveOperation>> {
    static OPERATIONS: OnceLock<Mutex<BTreeMap<u64, ActiveOperation>>> = OnceLock::new();
    OPERATIONS.get_or_init(Mutex::default)
}

/// Names a synchronous wait before it starts, including SSH admission and
/// master preparation. Arguments, environment and stdin are never retained.
pub struct BlockingOperation(u64);

impl BlockingOperation {
    pub fn start(purpose: &str, program: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let thread = std::thread::current();
        operations()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id,
                ActiveOperation {
                    purpose: purpose.to_owned(),
                    program: program.to_owned(),
                    thread: format!("{} {:?}", thread.name().unwrap_or("unnamed"), thread.id()),
                    started: Instant::now(),
                },
            );
        Self(id)
    }

    pub(super) fn command(command: &CommandSpec) -> Self {
        Self::start(&command.purpose, &command.program)
    }
}

impl Drop for BlockingOperation {
    fn drop(&mut self) {
        operations()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// Never wait for the registry lock from a stall reporter. `None` means the
/// snapshot was contended, rather than that no work was running.
pub fn active_blocking_operations() -> Option<Vec<BlockingOperationSnapshot>> {
    let operations = match operations().try_lock() {
        Ok(operations) => operations,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return None,
    };
    Some(
        operations
            .values()
            .map(|operation| BlockingOperationSnapshot {
                purpose: operation.purpose.clone(),
                program: operation.program.clone(),
                thread: operation.thread.clone(),
                elapsed_ms: operation.started.elapsed().as_millis() as u64,
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_keep_concurrent_operations_until_each_owner_finishes() {
        let first = BlockingOperation::start("diagnostics-first", "ssh");
        let second = BlockingOperation::start("diagnostics-second", "git");
        let snapshot = || {
            operations()
                .lock()
                .unwrap()
                .values()
                .filter(|operation| operation.purpose.starts_with("diagnostics-"))
                .map(|operation| operation.purpose.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(snapshot(), ["diagnostics-first", "diagnostics-second"]);
        drop(first);
        assert_eq!(snapshot(), ["diagnostics-second"]);
        drop(second);
        assert!(snapshot().is_empty());
    }
}
