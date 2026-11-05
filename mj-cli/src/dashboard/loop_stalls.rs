//! Logs a dashboard loop pass that kept input waiting.
//!
//! A pass runs from the moment the loop's `select!` returns to the moment the
//! next one starts waiting; input cannot be read in between. Polling the arm
//! futures inside the `select!` runs on the same thread and blocks input too,
//! so the CPU spent there counts as well. On Linux the thread's scheduler
//! statistics split a slow pass into time on a CPU and time spent runnable
//! but waiting for one, which is the difference between work the loop did and
//! a host too busy to run it.

use std::time::{Duration, Instant};

/// A pass this long, or this much CPU inside one `select!`, is a freeze a
/// person can see.
const STALL: Duration = Duration::from_millis(100);
/// At most one stall line per interval; a host under heavy load can stall
/// every pass.
const LOG_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
pub(super) enum Phase {
    /// The body of the `select!` arm that woke the loop.
    Arm,
    DrainFeeds,
    Persistence,
    Actions,
    Housekeeping,
    Draw,
    Notifications,
}

const PHASES: usize = 7;

pub(super) struct Meter {
    sched: Option<Schedstat>,
    arm: &'static str,
    phase: Phase,
    phase_started: Instant,
    phases: [Duration; PHASES],
    pass_started: Instant,
    pass_sample: Option<Sample>,
    select_sample: Option<Sample>,
    /// CPU the thread used inside the `select!` that started this pass.
    select_cpu: Option<Duration>,
    limiter: Limiter,
}

impl Meter {
    /// Opens the scheduler statistics of the calling thread, so it has to
    /// be built on the thread that runs the loop. The loop runs inside
    /// `block_on` on the main thread and never moves.
    pub(super) fn new() -> Self {
        let sched = Schedstat::open();
        let now = Instant::now();
        let pass_sample = sched.as_ref().and_then(Schedstat::read);
        Self {
            sched,
            arm: "start",
            phase: Phase::Housekeeping,
            phase_started: now,
            phases: [Duration::ZERO; PHASES],
            pass_started: now,
            pass_sample,
            select_sample: None,
            select_cpu: None,
            limiter: Limiter::default(),
        }
    }

    /// Charges the time since the last switch to the current phase and moves
    /// on to `phase`.
    pub(super) fn phase(&mut self, phase: Phase) {
        self.charge();
        self.phase = phase;
    }

    fn charge(&mut self) -> Instant {
        let now = Instant::now();
        self.phases[self.phase as usize] += now - self.phase_started;
        self.phase_started = now;
        now
    }

    /// The loop is about to wait: this ends the pass, which is logged when
    /// it kept input waiting.
    pub(super) fn select_started(&mut self) {
        let now = self.charge();
        let sample = self.sched.as_ref().and_then(Schedstat::read);
        let total = now - self.pass_started;
        let pass = sample
            .zip(self.pass_sample)
            .map(|(end, start)| end.since(start));
        if total >= STALL || self.select_cpu.is_some_and(|cpu| cpu >= STALL) {
            self.report(now, total, pass);
        }
        self.select_sample = sample;
    }

    /// The `select!` returned through the arm named `arm`; this starts the
    /// next pass.
    pub(super) fn woke(&mut self, arm: &'static str) {
        let now = Instant::now();
        let sample = self.sched.as_ref().and_then(Schedstat::read);
        self.select_cpu = sample
            .zip(self.select_sample)
            .map(|(end, start)| end.since(start).cpu);
        self.arm = arm;
        self.phase = Phase::Arm;
        self.phase_started = now;
        self.phases = [Duration::ZERO; PHASES];
        self.pass_started = now;
        self.pass_sample = sample;
    }

    /// The pass handed the terminal to another program on purpose, so the
    /// time it waited is not a freeze.
    pub(super) fn forget_pass(&mut self) {
        let now = Instant::now();
        self.phase_started = now;
        self.phases = [Duration::ZERO; PHASES];
        self.pass_started = now;
        self.pass_sample = self.sched.as_ref().and_then(Schedstat::read);
        self.select_cpu = None;
    }

    fn report(&mut self, now: Instant, total: Duration, pass: Option<Sample>) {
        let Some(suppressed) = self.limiter.admit(now, total) else {
            return;
        };
        let ms = |duration: Duration| duration.as_millis() as u64;
        let phase = |phase: Phase| ms(self.phases[phase as usize]);
        tracing::warn!(
            arm = self.arm,
            total_ms = ms(total),
            cpu_ms = pass.map(|pass| ms(pass.cpu)),
            runqueue_ms = pass.map(|pass| ms(pass.runqueue)),
            select_cpu_ms = self.select_cpu.map(ms),
            arm_ms = phase(Phase::Arm),
            drain_feeds_ms = phase(Phase::DrainFeeds),
            persistence_ms = phase(Phase::Persistence),
            actions_ms = phase(Phase::Actions),
            housekeeping_ms = phase(Phase::Housekeeping),
            draw_ms = phase(Phase::Draw),
            notifications_ms = phase(Phase::Notifications),
            suppressed = suppressed.count,
            suppressed_max_ms = ms(suppressed.max),
            "the dashboard loop was busy"
        );
    }
}

/// Holds stall lines to one per [`LOG_INTERVAL`] and counts the ones it held.
#[derive(Default)]
struct Limiter {
    last: Option<Instant>,
    held: Suppressed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Suppressed {
    count: u64,
    max: Duration,
}

impl Limiter {
    /// Whether a stall at `now` is logged; when it is, what was held since
    /// the last line.
    fn admit(&mut self, now: Instant, total: Duration) -> Option<Suppressed> {
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < LOG_INTERVAL)
        {
            self.held.count += 1;
            self.held.max = self.held.max.max(total);
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.held))
    }
}

#[derive(Clone, Copy)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Sample {
    cpu: Duration,
    runqueue: Duration,
}

impl Sample {
    fn since(self, start: Self) -> Self {
        Self {
            cpu: self.cpu.saturating_sub(start.cpu),
            runqueue: self.runqueue.saturating_sub(start.runqueue),
        }
    }
}

/// The calling thread's `/proc/thread-self/schedstat`, kept open so that a
/// sample is one read.
#[cfg(target_os = "linux")]
struct Schedstat(std::fs::File);

#[cfg(target_os = "linux")]
impl Schedstat {
    fn open() -> Option<Self> {
        match std::fs::File::open("/proc/thread-self/schedstat") {
            Ok(file) => Some(Self(file)),
            Err(error) => {
                tracing::debug!(%error, "dashboard loop scheduler statistics are unavailable");
                None
            }
        }
    }

    fn read(&self) -> Option<Sample> {
        use std::os::unix::fs::FileExt;
        // Three decimal u64 counters: CPU ns, run-queue wait ns, timeslices.
        let mut bytes = [0u8; 80];
        let read = self.0.read_at(&mut bytes, 0).ok()?;
        parse_schedstat(&bytes[..read])
    }
}

#[cfg(not(target_os = "linux"))]
struct Schedstat;

#[cfg(not(target_os = "linux"))]
impl Schedstat {
    fn open() -> Option<Self> {
        None
    }

    fn read(&self) -> Option<Sample> {
        None
    }
}

#[cfg(target_os = "linux")]
fn parse_schedstat(bytes: &[u8]) -> Option<Sample> {
    let mut fields = std::str::from_utf8(bytes).ok()?.split_ascii_whitespace();
    let mut next = || fields.next()?.parse().ok().map(Duration::from_nanos);
    Some(Sample {
        cpu: next()?,
        runqueue: next()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalls_inside_the_interval_are_held_and_reported_with_the_next_line() {
        let mut limiter = Limiter::default();
        let start = Instant::now();
        let ms = Duration::from_millis;
        assert_eq!(limiter.admit(start, ms(150)), Some(Suppressed::default()));
        assert_eq!(limiter.admit(start + ms(200), ms(400)), None);
        assert_eq!(limiter.admit(start + ms(999), ms(120)), None);
        assert_eq!(
            limiter.admit(start + ms(1_000), ms(110)),
            Some(Suppressed {
                count: 2,
                max: ms(400)
            })
        );
        // The interval runs from the last line, not from the first stall.
        assert_eq!(limiter.admit(start + ms(1_500), ms(100)), None);
        assert_eq!(
            limiter.admit(start + ms(2_000), ms(100)),
            Some(Suppressed {
                count: 1,
                max: ms(100)
            })
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_kernel_reports_this_threads_cpu_and_reads_again_from_the_start() {
        let sched = Schedstat::open().expect("open /proc/thread-self/schedstat");
        let first = sched.read().expect("parse schedstat");
        // Spin on CPU time, not wall time: a loaded host can leave this
        // thread waiting for most of any wall-clock window.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut second = first;
        while second.since(first).cpu < Duration::from_millis(5) && Instant::now() < deadline {
            std::hint::black_box(0u64);
            second = sched.read().expect("parse schedstat again");
        }
        assert!(second.since(first).cpu >= Duration::from_millis(5));
        assert!(second.runqueue >= first.runqueue);
    }
}
