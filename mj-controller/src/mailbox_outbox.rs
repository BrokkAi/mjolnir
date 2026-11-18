//! The mailbox outbox's wake signal, delivery outcomes and command ids. The
//! daemon's delivery owner is `daemon::mailbox_delivery`.

use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use tokio::sync::Notify;

const TRUSTED_PARENT_MESSAGE_PROTOCOL_ERROR: &str = "This queued parent message reached an older mj worker that cannot receive structured mailbox events; it was not delivered.";
const SESSION_MESSAGE_PROTOCOL_ERROR: &str = "This queued session message reached an older mj worker that cannot receive its sender information; it was not delivered.";

pub(crate) fn outbox_notify() -> &'static Notify {
    static NOTIFY: OnceLock<Notify> = OnceLock::new();
    NOTIFY.get_or_init(Notify::new)
}

/// Wake the delivery owner after a producer commits new outbox rows.
pub(crate) fn notify_mailbox_outbox_changed() {
    outbox_notify().notify_one();
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MailboxEventDelivery {
    /// Not delivered yet; the text says what delivery waits for.
    Pending(String),
    Accepted(u64),
    Discarded,
    Failed(String),
}

/// Read the worker protocol only from the protocol version published in the
/// session manager's current view. Request admission and outbox delivery use
/// this same fact.
pub(crate) fn published_worker_relay_protocol(
    view: &mj_client::session::ManagedSessionView,
) -> Option<u32> {
    view.snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.operational.relay_protocol_version)
}

pub(crate) fn trusted_parent_message_protocol_error(protocol: Option<u32>) -> Option<&'static str> {
    protocol
        .filter(|version| *version < mj_core::relay::RELAY_STRUCTURED_MAILBOX_PROTOCOL)
        .map(|_| TRUSTED_PARENT_MESSAGE_PROTOCOL_ERROR)
}

pub(crate) fn session_message_protocol_error(protocol: Option<u32>) -> Option<&'static str> {
    protocol
        .filter(|version| *version < mj_core::relay::RELAY_SESSION_MESSAGE_PROTOCOL)
        .map(|_| SESSION_MESSAGE_PROTOCOL_ERROR)
}

pub(crate) fn mailbox_command_id(event_key: &str) -> String {
    let digest = Sha256::digest(event_key.as_bytes());
    let mut command_id = String::with_capacity("mailbox-".len() + digest.len() * 2);
    command_id.push_str("mailbox-");
    for byte in digest {
        use std::fmt::Write as _;
        write!(command_id, "{byte:02x}").expect("writing to String cannot fail");
    }
    command_id
}
