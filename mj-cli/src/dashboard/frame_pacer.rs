//! When the dashboard draws.
//!
//! Applying a background message and drawing a frame are separate decisions.
//! A message only marks the surface out of date; the frame that shows it is
//! drawn on a cadence, so a burst of updates from many working sessions costs
//! one frame per interval rather than one per message. Input is the
//! exception: the frame that shows a key is drawn as soon as the key is
//! handled, before any queued background message is applied.
//!
//! [`FramePacer`] is the one owner of that decision. The loop reports what
//! each wakeup changed and asks the pacer whether to draw now, when the next
//! paced frame is due, and when background feeds may next be taken.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

/// The shortest gap between two frames drawn for background changes.
///
/// Input never waits for it, so it bounds only how often streaming output and
/// other feed updates reach the screen. 16 ms matches a 60 Hz display: a
/// stream still advances every refresh the terminal can show, while a burst
/// of feed messages that arrives within one refresh shares a frame.
pub(crate) const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Paced frames start at least this many frame-build times apart. When a
/// frame costs more than half the interval, as it does for a large surface on
/// a busy host, a fixed interval would leave the loop drawing back to back;
/// spacing frames by their own cost keeps half of the loop free, so a key
/// finds it idle at least half of the time.
const PACED_FRAME_SPACING: u32 = 2;

#[derive(Debug)]
pub(crate) struct FramePacer {
    interval: Duration,
    /// When the next paced frame may start. Frames drawn for input do not
    /// move it, so continuous input cannot hold background updates back.
    next_paced: Option<Instant>,
    /// The visible state changed since the last frame.
    dirty: bool,
    /// The change must be drawn now: input, or a message whose own
    /// bookkeeping assumes the next frame shows it.
    urgent: bool,
}

impl FramePacer {
    pub(crate) fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_paced: None,
            dirty: false,
            urgent: false,
        }
    }

    /// Something visible changed; draw it with the next paced frame.
    pub(crate) fn mark(&mut self) {
        self.dirty = true;
    }

    /// Something visible changed that must be drawn without waiting.
    pub(crate) fn mark_urgent(&mut self) {
        self.dirty = true;
        self.urgent = true;
    }

    /// Whether a change has not reached the screen yet.
    pub(crate) fn pending(&self) -> bool {
        self.dirty
    }

    pub(crate) fn should_draw(&self, now: Instant) -> bool {
        self.dirty && (self.urgent || self.next_paced.is_none_or(|due| now >= due))
    }

    /// Records a frame that started at `started` and finished at `finished`.
    /// Every frame shows the whole current state, so it settles every pending
    /// change; only a paced frame sets when the next one may start.
    pub(crate) fn drew(&mut self, started: Instant, finished: Instant) {
        if !self.urgent {
            let cost = finished.saturating_duration_since(started);
            self.next_paced = Some(started + self.interval.max(cost * PACED_FRAME_SPACING));
        }
        self.dirty = false;
        self.urgent = false;
    }

    /// When background feeds may next be taken, if not now. Holding them until
    /// a paced frame could show their result is what makes a burst share one
    /// wakeup and one frame.
    pub(crate) fn feed_gate(&self, now: Instant) -> Option<Instant> {
        self.next_paced.filter(|due| *due > now)
    }

    /// When a deferred change is due on screen, if one is waiting.
    pub(crate) fn deferred_frame(&self, now: Instant) -> Option<Instant> {
        if !self.dirty || self.urgent {
            return None;
        }
        self.feed_gate(now)
    }
}

/// Holds `future` until `gate`, then awaits it. Nothing is polled before the
/// gate, so a cancel-safe future stays cancel safe.
pub(crate) async fn paced<F: Future>(gate: Option<Instant>, future: F) -> F::Output {
    if let Some(gate) = gate {
        tokio::time::sleep_until(gate).await;
    }
    future.await
}

/// A far deadline for a disabled `select!` timer arm, which is still built.
pub(crate) fn never() -> Instant {
    Instant::now() + Duration::from_secs(86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dashboard loop in miniature: input first, then a deferred frame,
    /// then the paced feed. A wakeup that was not input drains the feed, and
    /// the pacer decides whether the next pass draws.
    struct Model {
        pacer: FramePacer,
        feed: tokio::sync::mpsc::UnboundedReceiver<u32>,
        keys: tokio::sync::mpsc::UnboundedReceiver<char>,
        applied: Vec<u32>,
        typed: String,
        /// What each frame showed: how many feed messages were applied, and
        /// the text typed.
        frames: Vec<(usize, String)>,
        wakeups: usize,
    }

    impl Model {
        fn new(
            feed: tokio::sync::mpsc::UnboundedReceiver<u32>,
            keys: tokio::sync::mpsc::UnboundedReceiver<char>,
        ) -> Self {
            Self {
                pacer: FramePacer::new(FRAME_INTERVAL),
                feed,
                keys,
                applied: Vec::new(),
                typed: String::new(),
                frames: Vec::new(),
                wakeups: 0,
            }
        }

        fn frame(&mut self) {
            let now = Instant::now();
            if self.pacer.should_draw(now) {
                self.frames.push((self.applied.len(), self.typed.clone()));
                self.pacer.drew(now, now);
            }
        }

        /// One wakeup; `false` once the feed has closed.
        async fn wake(&mut self) -> bool {
            let now = Instant::now();
            let gate = self.pacer.feed_gate(now);
            let deferred = self.pacer.deferred_frame(now);
            let before = self.applied.len();
            tokio::select! {
                biased;
                Some(key) = self.keys.recv() => {
                    self.wakeups += 1;
                    self.typed.push(key);
                    self.pacer.mark_urgent();
                    return true;
                }
                () = tokio::time::sleep_until(deferred.unwrap_or_else(never)), if deferred.is_some() => {}
                message = paced(gate, self.feed.recv()) => {
                    let Some(message) = message else { return false };
                    self.applied.push(message);
                }
            }
            self.wakeups += 1;
            while let Ok(message) = self.feed.try_recv() {
                self.applied.push(message);
            }
            if self.applied.len() > before {
                self.pacer.mark();
            }
            true
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_feed_updates_shares_paced_frames() {
        let (feed_tx, feed) = tokio::sync::mpsc::unbounded_channel();
        let (_keys_tx, keys) = tokio::sync::mpsc::unbounded_channel();
        let mut model = Model::new(feed, keys);
        // 200 updates, one every millisecond: many more than a 60 Hz
        // display can show.
        let producer = tokio::spawn(async move {
            for message in 0..200 {
                feed_tx.send(message).expect("the model is listening");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        loop {
            model.frame();
            if !model.wake().await {
                break;
            }
        }
        model.frame();
        producer.await.expect("producer");

        assert_eq!(model.applied, (0..200).collect::<Vec<_>>());
        // 200 ms of updates at one frame per interval, plus the first frame
        // and the one that shows the last update.
        let bound = 200 / FRAME_INTERVAL.as_millis() as usize + 2;
        assert!(
            model.frames.len() <= bound,
            "{} frames for 200 updates",
            model.frames.len()
        );
        assert!(model.wakeups <= bound, "{} wakeups", model.wakeups);
        assert_eq!(
            model.frames.last().map(|(applied, _)| *applied),
            Some(200),
            "the last update reached the screen"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_is_drawn_before_the_queued_feed_backlog_is_applied() {
        let (feed_tx, feed) = tokio::sync::mpsc::unbounded_channel();
        let (keys_tx, keys) = tokio::sync::mpsc::unbounded_channel();
        let mut model = Model::new(feed, keys);
        feed_tx.send(0).expect("model");
        assert!(model.wake().await);
        model.frame();
        assert_eq!(model.frames, [(1, String::new())]);
        // A paced frame was just drawn, and a backlog is waiting for the
        // next one when the key arrives.
        for message in 1..100 {
            feed_tx.send(message).expect("model");
        }
        keys_tx.send('k').expect("model");

        assert!(model.wake().await);
        model.frame();
        assert_eq!(
            model.frames.last(),
            Some(&(1, "k".to_owned())),
            "the key's frame came before the backlog was applied"
        );

        assert!(model.wake().await);
        model.frame();
        assert_eq!(model.frames.last(), Some(&(100, "k".to_owned())));
    }

    #[test]
    fn an_expensive_frame_leaves_the_loop_free_for_as_long_as_it_took() {
        let mut pacer = FramePacer::new(FRAME_INTERVAL);
        let start = Instant::now();
        let ms = |ms| start + Duration::from_millis(ms);
        pacer.mark();
        pacer.drew(start, ms(30));
        pacer.mark();
        assert_eq!(pacer.deferred_frame(ms(30)), Some(ms(60)));
        assert_eq!(pacer.feed_gate(ms(30)), Some(ms(60)));
        assert!(!pacer.should_draw(ms(59)));
        assert!(pacer.should_draw(ms(60)));
        // Input does not wait for the spacing.
        pacer.mark_urgent();
        assert!(pacer.should_draw(ms(31)));
    }

    #[test]
    fn input_frames_do_not_hold_back_background_frames() {
        let mut pacer = FramePacer::new(FRAME_INTERVAL);
        let start = Instant::now();
        let mut background_frames = 0;
        // A key every 5 ms for 200 ms, each followed by a background change.
        for step in 0..40 {
            let now = start + Duration::from_millis(5 * step);
            pacer.mark_urgent();
            assert!(pacer.should_draw(now), "a key is drawn at once");
            pacer.drew(now, now);
            pacer.mark();
            if pacer.should_draw(now) {
                pacer.drew(now, now);
                background_frames += 1;
            }
        }
        // One per interval, rounded up to the 5 ms the keys arrive on.
        assert!(background_frames >= 200 / 20, "{background_frames}");
    }
}
