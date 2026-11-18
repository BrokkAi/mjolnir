//! One cancellable attachment per session, shared by its display surfaces.

use mj_chat::chat::{ActiveChat, ChatInstance};
use mj_tui::tile_layout::PaneId;
use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use tokio::task::JoinHandle;

pub(super) const ATTACH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
pub(super) struct SessionAttachment {
    selection: Option<String>,
    assignments: BTreeMap<PaneId, PaneInterest>,
    notification_requested: bool,
    generation: u64,
    task: Option<JoinHandle<()>>,
}

enum PaneInterest {
    Requested(mj_tui::PaneAssignment),
    Cancelled(mj_tui::PaneAssignment),
}

impl PaneInterest {
    fn assignment(&self) -> mj_tui::PaneAssignment {
        match self {
            Self::Requested(assignment) | Self::Cancelled(assignment) => *assignment,
        }
    }
}

/// Resources that existed when a lifecycle operation was admitted. The
/// completion can retire these identities, never a subsequent attachment.
pub(super) struct ChatRetirement {
    session_id: String,
    chat: Option<ChatInstance>,
    attachments: BTreeMap<String, u64>,
}

impl ChatRetirement {
    pub(super) fn capture(
        session_id: &str,
        chats: &BTreeMap<String, ActiveChat>,
        attachments: &BTreeMap<String, SessionAttachment>,
    ) -> Self {
        Self {
            session_id: session_id.to_owned(),
            chat: chats.get(session_id).map(ActiveChat::instance),
            attachments: attachments
                .iter()
                .filter_map(|(pane, attachment)| {
                    attachment
                        .identity(session_id)
                        .map(|generation| (pane.clone(), generation))
                })
                .collect(),
        }
    }

    pub(super) fn is_current(
        &self,
        chats: &BTreeMap<String, ActiveChat>,
        attachments: &BTreeMap<String, SessionAttachment>,
    ) -> bool {
        chats.get(&self.session_id).is_none_or(|chat| {
            self.chat
                .as_ref()
                .is_some_and(|instance| chat.is_instance(instance))
        }) && attachments.iter().all(|(pane, attachment)| {
            attachment
                .identity(&self.session_id)
                .is_none_or(|generation| self.attachments.get(pane) == Some(&generation))
        })
    }

    pub(super) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(super) fn retire_attachments(&self, attachments: &mut BTreeMap<String, SessionAttachment>) {
        for (pane, generation) in &self.attachments {
            if let Some(attachment) = attachments.get_mut(pane)
                && attachment.accepts(*generation, Some(&self.session_id))
            {
                attachment.retire(&self.session_id);
            }
        }
    }
}

/// The sessions retained by live conversation or notification interests.
pub(super) fn wanted_chat_sessions(
    dashboard: &mj_tui::DashboardState,
    attachments: &BTreeMap<String, SessionAttachment>,
) -> std::collections::BTreeSet<String> {
    let mut sessions = dashboard
        .pane_sessions()
        .into_iter()
        .filter(|(pane, session)| {
            attachments
                .get(session)
                .is_none_or(|attachment| attachment.pane_can_show(dashboard, *pane))
        })
        .map(|(_, session)| session)
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(session) = dashboard.notification_form_session() {
        sessions.insert(session);
    }
    sessions
}

impl SessionAttachment {
    /// Observe navigation before permitting an old task to join its new
    /// assignment. Work survives reassignment only through another live lease.
    pub(super) fn observe_pane(
        &mut self,
        dashboard: &mj_tui::DashboardState,
        pane: PaneId,
        session: &str,
    ) {
        let Some(assignment) = dashboard.pane_assignment(pane) else {
            return;
        };
        let changed = self
            .assignments
            .get(&pane)
            .is_none_or(|interest| interest.assignment() != assignment);
        if changed && self.pending() && !self.has_surface_interest(dashboard, session) {
            self.cancel();
            self.selection = None;
        }
        self.bind(pane, assignment);
    }

    /// A navigation assignment joins the session's one attachment operation.
    pub(super) fn bind(&mut self, pane: PaneId, assignment: mj_tui::PaneAssignment) {
        if self
            .assignments
            .get(&pane)
            .is_some_and(|interest| interest.assignment() == assignment)
        {
            return;
        }
        self.request_pane(pane, assignment);
    }

    /// An explicit retry can rejoin an assignment that Esc cancelled.
    pub(super) fn request_pane(&mut self, pane: PaneId, assignment: mj_tui::PaneAssignment) {
        let unchanged = matches!(self.assignments.get(&pane), Some(PaneInterest::Requested(current)) if *current == assignment);
        self.assignments
            .insert(pane, PaneInterest::Requested(assignment));
        if !unchanged && !self.pending() {
            self.selection = None;
        }
    }

    /// Observe the notification lease so a newly displayed card can start
    /// work even when its conversation's earlier attempt was cancelled.
    pub(super) fn bind_notification(&mut self, requested: bool) {
        if requested && !self.notification_requested && !self.pending() {
            self.selection = None;
        }
        self.notification_requested = requested;
    }

    pub(super) fn pane_pending(&self, pane: PaneId) -> bool {
        self.pending()
            && matches!(
                self.assignments.get(&pane),
                Some(PaneInterest::Requested(_))
            )
    }

    pub(super) fn release_pane(
        &mut self,
        dashboard: &mj_tui::DashboardState,
        pane: PaneId,
        session: &str,
    ) {
        if let Some(assignment) = dashboard.pane_assignment(pane) {
            self.assignments
                .insert(pane, PaneInterest::Cancelled(assignment));
        }
        if !self.has_surface_interest(dashboard, session) {
            self.cancel();
        }
    }

    /// Access follows this assignment's interest, even if another surface
    /// completed the session's shared attach after this pane cancelled.
    pub(super) fn pane_can_show(&self, dashboard: &mj_tui::DashboardState, pane: PaneId) -> bool {
        self.assignments.get(&pane).is_some_and(|interest| {
            matches!(interest, PaneInterest::Requested(assignment)
                if dashboard.pane_assignment(pane) == Some(*assignment))
        })
    }

    fn has_surface_interest(&self, dashboard: &mj_tui::DashboardState, session: &str) -> bool {
        dashboard.notification_form_session().as_deref() == Some(session)
            || dashboard
                .pane_sessions()
                .into_iter()
                .any(|(pane, held)| held == session && self.pane_can_show(dashboard, pane))
    }

    #[cfg(test)]
    pub(super) fn accepts_pane_result(
        &self,
        generation: u64,
        assignment: mj_tui::PaneAssignment,
        dashboard: &mj_tui::DashboardState,
        pane: PaneId,
        session: &str,
    ) -> bool {
        self.accepts(generation, Some(session))
            && matches!(self.assignments.get(&pane), Some(PaneInterest::Requested(current)) if *current == assignment)
            && dashboard.pane_assignment(pane) == Some(assignment)
            && dashboard.pane_session(pane) == Some(session)
    }

    pub(super) fn accepts_surface_result(
        &self,
        generation: u64,
        dashboard: &mj_tui::DashboardState,
        session: &str,
    ) -> bool {
        self.accepts(generation, Some(session)) && self.has_surface_interest(dashboard, session)
    }

    pub(super) fn pending(&self) -> bool {
        self.task.is_some()
    }

    pub(super) fn finish(&mut self) {
        self.task = None;
    }

    fn identity(&self, session_id: &str) -> Option<u64> {
        (self.selection.as_deref() == Some(session_id)).then_some(self.generation)
    }

    /// Observe selection even when its chat is already warm. Failed/cancelled
    /// opens stay observed so render/feed ticks cannot retry them indefinitely.
    pub(super) fn select(&mut self, session: &str) -> bool {
        if self.selection.as_deref() == Some(session) {
            return false;
        }
        self.selection = Some(session.to_owned());
        true
    }

    pub(super) fn release_unowned_task(
        &mut self,
        dashboard: &mj_tui::DashboardState,
        session: &str,
    ) {
        if self.pending() && !self.has_surface_interest(dashboard, session) {
            self.cancel();
        }
    }

    pub(super) fn select_for_pane(&mut self, pane: PaneId, session: &str) -> bool {
        if !matches!(
            self.assignments.get(&pane),
            Some(PaneInterest::Requested(_))
        ) {
            return false;
        }
        self.select(session)
    }

    pub(super) fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    /// A lifecycle owns the selected conversation temporarily. Unlike a
    /// failed/cancelled open, its completion should allow one fresh attempt.
    pub(super) fn defer(&mut self) {
        if self.selection.take().is_some() || self.task.is_some() {
            self.cancel();
        }
    }

    /// Retiring a warm chat permits one fresh attachment without disturbing
    /// an attachment already underway for another selected session.
    pub(super) fn retire(&mut self, session: &str) {
        if self.selection.as_deref() == Some(session) {
            self.defer();
        }
    }

    pub(super) fn accepts(&self, generation: u64, selection: Option<&str>) -> bool {
        // Input may have changed the row before the event loop starts its new
        // attachment. A queued completion must already respect that choice.
        self.generation == generation && self.selection.as_deref() == selection
    }

    pub(super) fn spawn<T: Send + 'static>(
        &mut self,
        session: &str,
        timeout: Duration,
        work: impl Future<Output = Result<T, String>> + Send + 'static,
        report: impl FnOnce(u64, Result<T, String>) + Send + 'static,
    ) {
        self.cancel();
        self.select(session);
        let generation = self.generation;
        self.task = Some(tokio::spawn(async move {
            let result = tokio::time::timeout(
                timeout,
                tokio_util::task::AbortOnDropHandle::new(tokio::spawn(work)),
            )
            .await;
            let result = match result {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => Err(format!("session opening task failed: {error}")),
                Err(_) => Err(format!(
                    "Session opening did not respond within {} seconds",
                    timeout.as_secs()
                )),
            };
            report(generation, result);
        }));
    }
}

impl Drop for SessionAttachment {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{mpsc, oneshot};

    #[tokio::test]
    async fn a_transition_retires_an_attach_and_rearms_the_same_selection_once() {
        let mut attachment = SessionAttachment::default();
        let (tx, rx) = oneshot::channel();
        attachment.spawn(
            "moving",
            ATTACH_TIMEOUT,
            async { Ok(()) },
            move |generation, _| {
                tx.send(generation).unwrap();
            },
        );
        let generation = rx.await.unwrap();
        attachment.defer();
        attachment.defer();
        assert!(!attachment.accepts(generation, Some("moving")));
        assert!(attachment.select("moving"));
        assert!(!attachment.select("moving"));
    }

    #[test]
    fn retiring_a_warm_chat_rearms_only_its_own_selection() {
        let mut attachment = SessionAttachment::default();
        assert!(attachment.select("created"));
        attachment.retire("another-workspace");
        assert!(!attachment.select("created"));
        attachment.retire("created");
        assert!(attachment.select("created"));
        assert!(!attachment.select("created"));
    }

    #[tokio::test]
    async fn failed_open_does_not_retry_on_background_wakeups_but_can_be_retried_explicitly() {
        let mut attachment = SessionAttachment::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(attachment.select("muse"));
        attachment.spawn(
            "muse",
            ATTACH_TIMEOUT,
            async { Err::<(), _>("not managed".into()) },
            move |generation, result| {
                tx.send((generation, result)).unwrap();
            },
        );
        let (failed_generation, result) = rx.recv().await.unwrap();
        assert!(result.is_err());
        for _ in 0..1024 {
            assert!(!attachment.select("muse"));
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        attachment.spawn(
            "muse",
            ATTACH_TIMEOUT,
            async { Ok(()) },
            move |generation, result| {
                tx.send((generation, result)).unwrap();
            },
        );
        let (generation, result) = rx.recv().await.unwrap();
        assert!(result.is_ok());
        assert!(attachment.accepts(generation, Some("muse")));
        // A selection event can precede the next call to select/spawn.
        assert!(!attachment.accepts(generation, Some("codex")));
        assert!(!attachment.accepts(failed_generation, Some("muse")));
    }

    #[tokio::test]
    async fn switching_sessions_aborts_hung_open_and_opens_the_new_selection() {
        let mut attachment = SessionAttachment::default();
        let (started, started_rx) = oneshot::channel();
        let (held, mut released) = mpsc::channel::<()>(1);
        attachment.spawn(
            "muse",
            ATTACH_TIMEOUT,
            async move {
                let _held = held;
                started.send(()).unwrap();
                std::future::pending::<Result<(), String>>().await
            },
            |_, _| panic!("cancelled attempt must not report"),
        );
        started_rx.await.unwrap();
        let (tx, rx) = oneshot::channel();
        assert!(attachment.select("codex"));
        attachment.spawn(
            "codex",
            ATTACH_TIMEOUT,
            async { Ok(()) },
            move |generation, result| {
                tx.send((generation, result)).unwrap();
            },
        );
        let (generation, result) = tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .unwrap()
            .unwrap();
        assert!(attachment.accepts(generation, Some("codex")));
        assert!(result.is_ok());
        assert!(
            tokio::time::timeout(Duration::from_secs(1), released.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn timeout_drops_hung_work_and_cancel_does_not_restart_the_same_selection() {
        let mut attachment = SessionAttachment::default();
        let (tx, rx) = oneshot::channel();
        let (held, mut released) = mpsc::channel::<()>(1);
        attachment.spawn(
            "muse",
            Duration::from_millis(20),
            async move {
                let _held = held;
                std::future::pending::<Result<(), String>>().await
            },
            move |generation, result| {
                tx.send((generation, result)).unwrap();
            },
        );
        let (generation, result) = tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().contains("did not respond"));
        assert!(
            tokio::time::timeout(Duration::from_secs(1), released.recv())
                .await
                .unwrap()
                .is_none()
        );
        attachment.cancel();
        assert!(!attachment.accepts(generation, Some("muse")));
        assert!(!attachment.select("muse"));
        assert!(attachment.select("codex"));
        assert!(attachment.select("muse"));
    }

    #[tokio::test]
    async fn panicked_open_reports_failure_instead_of_leaving_the_spinner_running() {
        let mut attachment = SessionAttachment::default();
        let (tx, rx) = oneshot::channel();
        attachment.spawn(
            "muse",
            ATTACH_TIMEOUT,
            async {
                panic!("fake preparation panic");
                #[allow(unreachable_code)]
                Ok(())
            },
            move |_, result| {
                tx.send(result).unwrap();
            },
        );
        assert!(rx.await.unwrap().unwrap_err().contains("task failed"));
    }
}
