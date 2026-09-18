//! Refusals: failures whose reason is safe to send to whoever asked.
//!
//! Most failures name profile homes, project paths, SSH hosts or container
//! locators, so they stay on the controller and the caller gets a generic
//! answer plus a reference into the daemon log. A refusal is different: it is a
//! precondition the caller can fix, and the sentence that says so is written
//! for that caller.
//!
//! The reason travels as a marker on the error chain, the way
//! `WorkerRestartLeftNoWorker` does, so intermediate `context` layers do not
//! hide it:
//!
//! ```ignore
//! source.with_context(|| Refusal::precondition("create a workspace first"))?;
//! ```
//!
//! Anything that is not marked stays internal. That is the safe default: a new
//! failure site leaks nothing until someone writes a sentence for it.

/// Why a request was refused, in words meant for the person who made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    kind: RefusalKind,
    message: String,
}

/// Which kind of refusal this is. The two map onto the answers the HTTP
/// surfaces already give: a state the caller must change first, and a request
/// naming something the controller cannot use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalKind {
    /// Something must be set up or finished before this can run.
    Precondition,
    /// The request itself names something unusable.
    Unusable,
}

impl Refusal {
    /// A state the caller has to change before the request can run.
    pub fn precondition(message: impl Into<String>) -> Self {
        Self {
            kind: RefusalKind::Precondition,
            message: message.into(),
        }
    }

    /// A request that names something the controller cannot use.
    pub fn unusable(message: impl Into<String>) -> Self {
        Self {
            kind: RefusalKind::Unusable,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> RefusalKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The refusal this error carries, if any.
    ///
    /// The marker is carried by the error, not by its text, and `anyhow`'s
    /// downcast walks every context layer, so added context does not hide it.
    #[must_use]
    pub fn of(error: &anyhow::Error) -> Option<Self> {
        error.downcast_ref::<Self>().cloned()
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Refusal {}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, anyhow};

    #[test]
    fn a_refusal_survives_added_context() {
        let error = Err::<(), _>(anyhow!("relay path /home/someone/profile is missing"))
            .context(Refusal::precondition("create a workspace first"))
            .context("start a phone session")
            .unwrap_err();
        let refusal = Refusal::of(&error).expect("the refusal is still on the chain");
        assert_eq!(refusal.message(), "create a workspace first");
        assert_eq!(refusal.kind(), RefusalKind::Precondition);
    }

    #[test]
    fn an_unmarked_error_carries_no_refusal() {
        let error = anyhow!("ssh host build-07 refused the connection")
            .context("provision the session target");
        assert_eq!(Refusal::of(&error), None);
    }

    #[test]
    fn a_bailed_refusal_is_found_too() {
        let error = anyhow::Error::new(Refusal::unusable("no target named laptop is configured"))
            .context("refresh capacity");
        let refusal = Refusal::of(&error).expect("the refusal is on the chain");
        assert_eq!(refusal.kind(), RefusalKind::Unusable);
    }
}
