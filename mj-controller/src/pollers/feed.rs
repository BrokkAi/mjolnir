use super::*;

/// Something a control loop waits on and then drains: one awaited receive for
/// the `select!` arm, and a non-blocking receive for the batch that follows.
pub trait FeedSource {
    type Item;

    /// Cancel-safe: a wait that loses the race must not drop a message.
    fn wait(&mut self) -> impl Future<Output = Option<Self::Item>>;

    fn poll_now(&mut self) -> Option<Self::Item>;
}

impl<T> FeedSource for tokio::sync::mpsc::Receiver<T> {
    type Item = T;

    fn wait(&mut self) -> impl Future<Output = Option<T>> {
        self.recv()
    }

    fn poll_now(&mut self) -> Option<T> {
        self.try_recv().ok()
    }
}

impl<T> FeedSource for tokio::sync::mpsc::UnboundedReceiver<T> {
    type Item = T;

    fn wait(&mut self) -> impl Future<Output = Option<T>> {
        self.recv()
    }

    fn poll_now(&mut self) -> Option<T> {
        self.try_recv().ok()
    }
}

impl<T: Clone> FeedSource for tokio::sync::watch::Receiver<T> {
    type Item = T;

    async fn wait(&mut self) -> Option<T> {
        self.changed().await.ok()?;
        Some(self.borrow_and_update().clone())
    }

    fn poll_now(&mut self) -> Option<T> {
        self.has_changed()
            .ok()
            .filter(|changed| *changed)
            .map(|_| self.borrow_and_update().clone())
    }
}

impl FeedSource for SessionManagerUpdates {
    type Item = SessionManagerUpdate;

    fn wait(&mut self) -> impl Future<Output = Option<SessionManagerUpdate>> {
        self.recv()
    }

    fn poll_now(&mut self) -> Option<SessionManagerUpdate> {
        self.try_recv().ok()
    }
}

impl FeedSource for RecoveryCoordinator {
    type Item = RecoveryResult;

    fn wait(&mut self) -> impl Future<Output = Option<RecoveryResult>> {
        self.result()
    }

    fn poll_now(&mut self) -> Option<RecoveryResult> {
        self.try_result()
    }
}

impl FeedSource for CredentialSyncCoordinator {
    type Item = mj_core::credentials::CredentialSyncResult;

    fn wait(&mut self) -> impl Future<Output = Option<Self::Item>> {
        self.result()
    }

    fn poll_now(&mut self) -> Option<Self::Item> {
        self.try_result()
    }
}

/// One background feed as a control loop uses it.
///
/// The `select!` arm hands the message that woke the loop to [`Feed::accept`],
/// and the drain that follows walks [`Feed::next_ready`] until the feed is
/// empty, so a burst of updates costs one draw. A closed channel reports `None`
/// for ever, which would leave its arm permanently ready; `accept` retires the
/// feed instead, and [`Feed::is_open`] gates the arm.
pub struct Feed<S: FeedSource> {
    pub(super) source: S,
    pub(super) pending: Option<S::Item>,
    pub(super) open: bool,
    /// Whether the drain has taken a message since it was last asked. A loop
    /// that gates a frame on a timer still has to draw when a message rode
    /// along with that timer.
    pub(super) delivered: bool,
}

impl<S: FeedSource> Feed<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            pending: None,
            open: true,
            delivered: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn wait(&mut self) -> impl Future<Output = Option<S::Item>> {
        self.source.wait()
    }

    /// Latches the message that won the select and reports whether one arrived.
    /// Applying it determines whether the visible state needs a redraw.
    pub fn accept(&mut self, message: Option<S::Item>) -> bool {
        match message {
            Some(message) => {
                self.pending = Some(message);
                true
            }
            None => {
                self.open = false;
                false
            }
        }
    }

    /// The next message for the batch drain: the one that won the select
    /// first, then whatever queued behind it.
    pub fn next_ready(&mut self) -> Option<S::Item> {
        let message = self.pending.take().or_else(|| self.source.poll_now());
        self.delivered |= message.is_some();
        message
    }

    /// Whether [`Self::next_ready`] produced a message since the last call.
    pub fn take_delivered(&mut self) -> bool {
        std::mem::take(&mut self.delivered)
    }
}
