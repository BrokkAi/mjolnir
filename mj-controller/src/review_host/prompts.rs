use super::*;

/// How long an idle reviewing role waits before reading its journal again. An
/// attach answers immediately even when nothing has been journaled, so without
/// this a review with several roles would spin on empty pages.
pub(super) const ROLE_POLL_IDLE_INTERVAL: Duration = Duration::from_millis(200);

/// Why a review could not start. Every variant is something a person can act
/// on, which is why they carry their own sentences rather than a code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRefusal(pub String);

impl std::fmt::Display for StartRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The message every surface gives for a prompt held by an open review.
pub const PROMPT_HELD_MESSAGE: &str =
    "a review of the last turn is open; forward, dismiss or cancel it first";

/// Sessions whose prompts an unresolved review is holding. A hold may admit
/// exactly one command for the matching review's corrective handoff; all
/// ordinary prompts, including controller-authored notices, remain refused.
///
/// This is the authoritative lock, and it is in memory on purpose: the process
/// that owns the review owns the lock, so a lock can never outlive the review
/// that set it. The shipped design kept it in a database row written by the
/// terminal, which is how a killed terminal could hold a session's prompts for
/// ever.
pub(super) static PROMPT_LOCK: LazyLock<Mutex<BTreeMap<String, PromptHold>>> =
    LazyLock::new(Mutex::default);

#[derive(Debug, Default)]
pub(super) struct PromptHold {
    pub(super) delivery_epoch: Option<u64>,
    pub(super) delivery_command_id: Option<String>,
}

/// Fresh reviewer conversations need an identity that is unique across all
/// roles, reviews, and controller restarts. A slot-local counter makes an
/// extended review's next supervisor collide with a previous supervisor, so
/// use a random nonce.
pub(crate) fn next_review_generation() -> Result<u64, String> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| format!("generate reviewer generation: {error}"))?;
    let generation = u64::from_le_bytes(random);
    if generation == 0 {
        return Err("generate reviewer generation: random nonce was zero".to_owned());
    }
    Ok(generation)
}

/// Whether a prompt for `session_id` must be refused, and why.
#[must_use]
pub fn prompt_refusal(session_id: &str) -> Option<&'static str> {
    PROMPT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(session_id)
        .then_some(PROMPT_HELD_MESSAGE)
}

pub(super) fn hold_prompts(session_id: &str) {
    PROMPT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id.to_owned(), PromptHold::default());
}

pub(super) fn release_prompts(session_id: &str) {
    PROMPT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(session_id);
}

/// Grants the actor one narrowly scoped exception to the prompt hold. The
/// grant is tied to both the review epoch and command identity so a delayed
/// request from an older review cannot enter a later one.
pub(super) fn admit_review_delivery(
    session_id: &str,
    epoch: u64,
    command_id: &str,
) -> Option<ReviewDeliveryAdmission> {
    let mut locks = PROMPT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let hold = locks.get_mut(session_id)?;
    match (hold.delivery_epoch, hold.delivery_command_id.as_deref()) {
        (Some(existing_epoch), Some(existing_command))
            if existing_epoch != epoch || existing_command != command_id =>
        {
            None
        }
        _ => {
            hold.delivery_epoch = Some(epoch);
            hold.delivery_command_id = Some(command_id.to_owned());
            Some(ReviewDeliveryAdmission::new(
                session_id.to_owned(),
                epoch,
                command_id.to_owned(),
            ))
        }
    }
}

/// Called by the session actor before it bypasses the normal prompt refusal.
/// This check is deliberately kept in the host-owned hold registry so an
/// arbitrary caller cannot turn a generic prompt into an internal delivery.
pub(crate) fn review_delivery_admitted(
    session_id: &str,
    admission: &ReviewDeliveryAdmission,
) -> bool {
    let locks = PROMPT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.get(session_id).is_some_and(|hold| {
        admission.session_id() == session_id
            && hold.delivery_epoch == Some(admission.epoch())
            && hold.delivery_command_id.as_deref() == Some(admission.command_id())
    })
}

/// Where the host reads the arming configuration. The daemon reloads
/// `config.toml` every 500 ms already, so this closure just reads whatever it
/// last installed.
pub type ReviewConfigSource = Arc<dyn Fn() -> ReviewConfig + Send + Sync>;
