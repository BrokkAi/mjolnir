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

/// What a park did to the processes the child's agent had left running. It is
/// recorded in one write with the park and its notification, so it exists only
/// for a park whose stop script ran to its check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkFacts {
    /// The stop was begun and its outcome is not recorded. The counts mean
    /// nothing until a daemon settles the park: this is the marker a daemon
    /// that died between the stop and its record finds at startup.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
    /// The processes seen just before the stop script ran.
    pub snapshot: Observed<ProcessFacts>,
    /// Counted from the check after the stop; zero when `unverified`.
    #[serde(default)]
    pub stopped: u32,
    #[serde(default)]
    pub survived: u32,
    /// The check after the stop could not run, and why. The counts mean
    /// nothing then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unverified: Option<String>,
    /// The part of `snapshot.shown` that no longer ran after the stop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stopped_shown: Vec<RunningProcess>,
    /// The part of `snapshot.shown` that still ran after the stop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub survived_shown: Vec<RunningProcess>,
}

impl ParkFacts {
    /// Settle a park from the pids that still ran after the stop. A snapshot
    /// that was not observable settles with nothing counted.
    pub fn settled(snapshot: Observed<ProcessFacts>, survivors: &[u32]) -> Self {
        let mut park = Self {
            pending: false,
            snapshot,
            stopped: 0,
            survived: 0,
            unverified: None,
            stopped_shown: Vec::new(),
            survived_shown: Vec::new(),
        };
        if let Observed::Known(processes) = &park.snapshot {
            let alive = |pid: u32| survivors.contains(&pid);
            let survived = processes
                .identities
                .iter()
                .filter(|identity| alive(identity.pid))
                .count();
            park.survived = u32::try_from(survived).unwrap_or(u32::MAX);
            park.stopped = processes.running.saturating_sub(park.survived);
            let (survived_shown, stopped_shown) = processes
                .shown
                .iter()
                .cloned()
                .partition(|process| alive(process.pid));
            park.survived_shown = survived_shown;
            park.stopped_shown = stopped_shown;
        }
        park
    }

    /// The park as it stands when its stop is about to run: the snapshot, and
    /// no outcome.
    pub fn started(snapshot: Observed<ProcessFacts>) -> Self {
        let mut park = Self::settled(snapshot, &[]);
        park.pending = true;
        park.stopped = 0;
        park.survived = 0;
        park.stopped_shown.clear();
        park.survived_shown.clear();
        park
    }

    /// A park whose check after the stop could not run.
    pub fn unverified(snapshot: Observed<ProcessFacts>, reason: impl Into<String>) -> Self {
        let mut park = Self::settled(snapshot, &[]);
        park.stopped = 0;
        park.stopped_shown.clear();
        park.unverified = Some(reason.into());
        park
    }

    /// The number of processes the stop was about, when they were observed.
    fn observed_running(&self) -> u32 {
        self.snapshot
            .known()
            .map_or(0, |processes| processes.running)
    }

    /// Whether the park changed anything a parent needs to hear about: it
    /// stopped a process, one survived, or what happened to processes that
    /// were running could not be checked.
    pub fn is_news(&self) -> bool {
        !self.pending
            && (self.stopped > 0
                || self.survived > 0
                || (self.unverified.is_some() && self.observed_running() > 0))
    }

    /// The lines of a `list_agents` entry.
    pub fn render_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.pending {
            return vec!["Park: stopping; the result is not recorded yet".into()];
        }
        match (&self.snapshot, &self.unverified) {
            (Observed::Unavailable { reason }, _) => lines.push(format!(
                "Park: the processes could not be observed before the stop ({reason})"
            )),
            (Observed::Known(processes), _) if processes.running == 0 => {
                lines.push("Park: no processes were running".into());
            }
            (Observed::Known(processes), Some(reason)) => lines.push(format!(
                "Park: what happened to {} processes could not be checked ({reason})",
                processes.running
            )),
            (Observed::Known(_), None) => {
                lines.push(format!(
                    "Park: {} stopped, {} survived",
                    self.stopped, self.survived
                ));
            }
        }
        lines.extend(process_lines(
            Some("stopped"),
            &self.stopped_shown,
            self.stopped,
        ));
        lines.extend(process_lines(
            Some("survived"),
            &self.survived_shown,
            self.survived,
        ));
        lines
    }

    /// The body of the message that tells a parent what a park did.
    pub fn message_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let plural = |count: u32| if count == 1 { "process" } else { "processes" };
        if let Some(reason) = &self.unverified {
            lines.push(format!(
                "The park ran, but what happened to the {} processes the child's agent had left running could not be checked: {reason}.",
                self.observed_running()
            ));
            lines.extend(self.snapshot.known().into_iter().flat_map(|processes| {
                processes
                    .shown
                    .iter()
                    .map(|process| format!("  {}", render_process(process)))
            }));
        }
        if self.stopped > 0 {
            lines.push(format!(
                "Parking stopped {} {} the child's agent had left running:",
                self.stopped,
                plural(self.stopped)
            ));
            lines.extend(process_lines(None, &self.stopped_shown, self.stopped));
        }
        if self.survived > 0 {
            lines.push(format!(
                "{} {} survived the park and {} still running:",
                self.survived,
                plural(self.survived),
                if self.survived == 1 { "is" } else { "are" }
            ));
            lines.extend(process_lines(None, &self.survived_shown, self.survived));
        }
        lines
    }
}

/// Listed processes, each labelled with `verb` when there is one, and the
/// count of those not listed.
fn process_lines(verb: Option<&str>, shown: &[RunningProcess], count: u32) -> Vec<String> {
    let label = verb.map(|verb| format!("{verb}: ")).unwrap_or_default();
    let mut lines: Vec<String> = shown
        .iter()
        .map(|process| format!("  {label}{}", render_process(process)))
        .collect();
    let omitted = count.saturating_sub(u32::try_from(shown.len()).unwrap_or(u32::MAX));
    if omitted > 0 {
        lines.push(format!("  and {omitted} more"));
    }
    lines
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
