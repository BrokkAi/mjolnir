//! The two ends of a migration stream: the sender runs on the old host, the
//! receiver on the new one. See `protocol` for the frames they exchange.

mod fsops;
mod receiver;
mod sender;

pub use receiver::{PublicationHook, PublicationStep, ReceiverConfig, StartDaemon, run_receiver};
pub use sender::{CutoverHooks, Phase1, Sender, SenderOutcome, SenderPlan, run_migration_sender};

#[cfg(all(test, unix))]
mod tests;
