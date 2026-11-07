//! Facts about a child session's live work that its own worker observed: which
//! processes the agent left running, and the state of its working tree.
//!
//! The worker owns these facts (it owns the harness process tree and the
//! working directory). The daemon asks for them once per finish, records the
//! answer, and every surface (the finish report, the park message and
//! `list_agents`) renders the recorded value through [`WorkFacts::render_lines`],
//! so the surfaces cannot disagree. A part that could not be observed says so
//! with a reason; it is never rendered as zero.

use serde::{Deserialize, Serialize};

// These three limits shape what a reader sees. Counts stay exact whatever they
// are, and no failure is guarded by them.
/// Processes listed in a rendering; the rest are counted as omitted.
pub const SHOWN_PROCESSES: usize = 5;
/// Changed paths listed in a rendering; the rest are counted as omitted.
pub const SHOWN_PATHS: usize = 8;
/// Characters of a command line kept in a rendering.
pub const COMMAND_CHARS: usize = 160;

/// One part of the facts: observed, or not observable and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub enum Observed<T> {
    Known(T),
    Unavailable { reason: String },
}

impl<T> Observed<T> {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }

    pub fn known(&self) -> Option<&T> {
        match self {
            Self::Known(value) => Some(value),
            Self::Unavailable { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkFacts {
    /// Wall-clock time of the observation, for `list_agents` ages. Renderings
    /// of a report never print it, so they stay stable.
    pub observed_at_ms: u64,
    pub processes: Observed<ProcessFacts>,
    pub worktree: Observed<WorktreeFacts>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessFacts {
    /// Exact number of work processes found.
    pub running: u32,
    /// The first [`SHOWN_PROCESSES`], longest running first.
    pub shown: Vec<RunningProcess>,
    /// `running` minus `shown.len()`.
    pub omitted: u32,
    /// Identity of every counted process, for the park's later liveness check.
    pub identities: Vec<ProcessIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningProcess {
    pub pid: u32,
    pub elapsed_secs: u64,
    /// Redacted, terminal-safe command line, cut at [`COMMAND_CHARS`].
    pub command: String,
    /// Not a descendant of the harness: its parent exited and it was handed to
    /// PID 1 or another process outside the harness tree.
    pub detached: bool,
}

/// A process ID together with its start identity: an ID alone can be reused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeFacts {
    pub directory: String,
    pub head: Option<HeadCommit>,
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Exact number of changed paths, tracked or not.
    pub changed: u32,
    pub modified: u32,
    pub untracked: u32,
    /// The first [`SHOWN_PATHS`] changed paths.
    pub shown: Vec<ChangedPath>,
    /// `changed` minus `shown.len()`.
    pub omitted: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadCommit {
    pub sha: String,
    pub subject: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedPath {
    /// One status character as `git status` prints it: `M`, `A`, `D`, `R`,
    /// `?` for untracked, `U` for a conflict.
    pub status: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParkPhase {
    /// The snapshot is durable and the stop may be running or may have run.
    Stopping,
    /// The liveness check ran and `stopped` and `survived` are settled.
    Done,
}

/// What a park did to the processes the child's agent had left running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkFacts {
    pub phase: ParkPhase,
    /// The processes seen just before the stop script ran.
    pub snapshot: Observed<ProcessFacts>,
    /// Counted once `phase` is `Done`.
    #[serde(default)]
    pub stopped: u32,
    #[serde(default)]
    pub survived: u32,
    /// The part of `snapshot.shown` that no longer ran after the stop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stopped_shown: Vec<RunningProcess>,
    /// The part of `snapshot.shown` that still ran after the stop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub survived_shown: Vec<RunningProcess>,
}

impl ParkFacts {
    /// A park that began with the snapshot and has not been checked yet.
    pub fn stopping(snapshot: Observed<ProcessFacts>) -> Self {
        Self {
            phase: ParkPhase::Stopping,
            snapshot,
            stopped: 0,
            survived: 0,
            stopped_shown: Vec::new(),
            survived_shown: Vec::new(),
        }
    }

    /// Settle a park from the pids that still ran after the stop. A snapshot
    /// that was not observable settles with nothing counted.
    pub fn settled(mut self, survivors: &[u32]) -> Self {
        self.phase = ParkPhase::Done;
        if let Observed::Known(processes) = &self.snapshot {
            let alive = |pid: u32| survivors.contains(&pid);
            let survived = processes
                .identities
                .iter()
                .filter(|identity| alive(identity.pid))
                .count();
            self.survived = u32::try_from(survived).unwrap_or(u32::MAX);
            self.stopped = processes.running.saturating_sub(self.survived);
            let (survived_shown, stopped_shown) = processes
                .shown
                .iter()
                .cloned()
                .partition(|process| alive(process.pid));
            self.survived_shown = survived_shown;
            self.stopped_shown = stopped_shown;
        }
        self
    }
}

impl WorkFacts {
    /// Facts when nothing could be observed, with the reason for both parts.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            observed_at_ms: u64::try_from(crate::clock::epoch_millis()).unwrap_or(0),
            processes: Observed::unavailable(reason.clone()),
            worktree: Observed::unavailable(reason),
        }
    }

    /// The observation lines of a report, without the label that says who
    /// observed them. Free of wall-clock times so renderings are stable.
    pub fn render_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        match &self.processes {
            Observed::Known(processes) => {
                lines.extend(render_processes(
                    "Running processes started by the child's agent",
                    processes,
                ));
            }
            Observed::Unavailable { reason } => lines.push(format!(
                "Running processes started by the child's agent: unavailable ({reason})"
            )),
        }
        match &self.worktree {
            Observed::Known(tree) => lines.extend(render_worktree(tree)),
            Observed::Unavailable { reason } => {
                lines.push(format!("Working tree: unavailable ({reason})"));
            }
        }
        lines
    }

    /// Number of running processes, when they were observed.
    pub fn running_processes(&self) -> Option<u32> {
        self.processes.known().map(|processes| processes.running)
    }
}

impl ParkFacts {
    /// Whether the park changed anything a parent needs to hear about.
    pub fn is_news(&self) -> bool {
        self.phase == ParkPhase::Done && (self.stopped > 0 || self.survived > 0)
    }

    pub fn render_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        match (&self.snapshot, self.phase) {
            (Observed::Unavailable { reason }, _) => lines.push(format!(
                "Park: the processes could not be observed before the stop ({reason})"
            )),
            (Observed::Known(_), ParkPhase::Stopping) => {
                lines.push("Park: stopping; the result is not recorded yet".into())
            }
            (Observed::Known(_), ParkPhase::Done) => {
                lines.push(format!(
                    "Park: {} stopped, {} survived",
                    self.stopped, self.survived
                ));
                for process in &self.stopped_shown {
                    lines.push(format!("  stopped: {}", render_process(process)));
                }
                for process in &self.survived_shown {
                    lines.push(format!("  survived: {}", render_process(process)));
                }
            }
        }
        lines
    }
}

/// `4m12s`, `1h02m`, `35s`.
pub fn render_elapsed(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, secs % 3600 / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// `pid 41022, up 4m12s, detached from the harness: command`.
pub fn render_process(process: &RunningProcess) -> String {
    let detached = if process.detached {
        ", detached from the harness"
    } else {
        ""
    };
    format!(
        "pid {}, up {}{detached}: {}",
        process.pid,
        render_elapsed(process.elapsed_secs),
        process.command
    )
}

fn render_processes(label: &str, processes: &ProcessFacts) -> Vec<String> {
    if processes.running == 0 {
        return vec![format!("{label}: none")];
    }
    let mut lines = vec![format!("{label}: {}", processes.running)];
    lines.extend(
        processes
            .shown
            .iter()
            .map(|process| format!("  {}", render_process(process))),
    );
    if processes.omitted > 0 {
        lines.push(format!("  and {} more", processes.omitted));
    }
    lines
}

fn render_worktree(tree: &WorktreeFacts) -> Vec<String> {
    let head = match &tree.head {
        Some(head) => {
            let short: String = head.sha.chars().take(7).collect();
            format!("HEAD {short} {:?}", head.subject)
        }
        None => "no commits yet".to_owned(),
    };
    let branch = match &tree.branch {
        Some(branch) => format!("branch {branch}"),
        None => "detached HEAD".to_owned(),
    };
    let mut lines = vec![format!("Working tree {}: {head}; {branch}", tree.directory)];
    if tree.changed == 0 {
        lines.push("Uncommitted changes: none".into());
        return lines;
    }
    let plural = if tree.changed == 1 { "path" } else { "paths" };
    let mut kinds = Vec::new();
    if tree.modified > 0 {
        kinds.push(format!("{} modified", tree.modified));
    }
    if tree.untracked > 0 {
        kinds.push(format!("{} untracked", tree.untracked));
    }
    let other = tree.changed.saturating_sub(tree.modified + tree.untracked);
    if other > 0 {
        kinds.push(format!("{other} other"));
    }
    lines.push(format!(
        "Uncommitted changes: {} {plural} ({})",
        tree.changed,
        kinds.join(", ")
    ));
    lines.extend(
        tree.shown
            .iter()
            .map(|path| format!("  {} {}", path.status, path.path)),
    );
    if tree.omitted > 0 {
        lines.push(format!("  and {} more", tree.omitted));
    }
    lines
}
