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
    /// Processes of the same user outside the harness's tree whose environment
    /// could not be read, so that whether they carry this session's marker, and
    /// are work the agent detached, is unknown. They are not in `running`.
    #[serde(default)]
    pub unattributed: u32,
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
    /// The commit's subject, or why it is unavailable: it is read beside the
    /// status and is the subject of this commit only if Git still says HEAD is it.
    pub subject: Observed<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedPath {
    /// One status character as `git status` prints it: `M`, `A`, `D`, `R`,
    /// `?` for untracked, `U` for a conflict.
    pub status: String,
    pub path: String,
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
            Observed::Known(processes) => lines.extend(render_processes(processes)),
            Observed::Unavailable { reason } => {
                lines.push(format!("Running processes: unavailable ({reason})"));
            }
        }
        lines.extend(render_worktree_part(&self.worktree));
        lines
    }

    /// Number of running processes, when they were observed.
    pub fn running_processes(&self) -> Option<u32> {
        self.processes.known().map(|processes| processes.running)
    }
}

/// The working-tree lines of a report, or the reason they are missing.
pub fn render_worktree_part(worktree: &Observed<WorktreeFacts>) -> Vec<String> {
    match worktree {
        Observed::Known(tree) => render_worktree(tree),
        Observed::Unavailable { reason } => vec![format!("Working tree: unavailable ({reason})")],
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

/// What the process scan looks at. A process outside it (a job started with
/// `env -i setsid` whose launching shell has exited, say) is not seen, so the
/// lines say what was searched and an empty result says none were found there,
/// not that none are running.
const PROCESS_SCOPE: &str = "in the worker's process tree or carrying this session's marker";

fn render_processes(processes: &ProcessFacts) -> Vec<String> {
    let mut lines = if processes.running == 0 {
        vec![format!("No running processes found {PROCESS_SCOPE}")]
    } else {
        let mut lines = vec![format!(
            "Running processes found {PROCESS_SCOPE}: {}",
            processes.running
        )];
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
    };
    if processes.unattributed > 0 {
        lines.push(format!(
            "  {} other processes of this user could not be attributed to this session (their environment is unreadable)",
            processes.unattributed
        ));
    }
    lines
}

/// Text from the machine as a reader sees it: terminal controls removed and a
/// long value cut. Paths, branch names and subjects are stored exactly as Git
/// reported them; only this rendering changes them.
fn display_text(text: &str) -> String {
    let clean = crate::transcript::sanitize_terminal_text(text);
    let clean = clean.replace(['\n', '\t'], " ");
    if clean.chars().count() <= COMMAND_CHARS {
        return clean;
    }
    let cut: String = clean.chars().take(COMMAND_CHARS).collect();
    format!("{cut}…")
}

fn render_worktree(tree: &WorktreeFacts) -> Vec<String> {
    let head = match &tree.head {
        Some(head) => {
            let short: String = head.sha.chars().take(7).collect();
            match &head.subject {
                Observed::Known(subject) => format!("HEAD {short} {:?}", display_text(subject)),
                Observed::Unavailable { reason } => {
                    format!(
                        "HEAD {short} (subject unavailable: {})",
                        display_text(reason)
                    )
                }
            }
        }
        None => "no commits yet".to_owned(),
    };
    let branch = match &tree.branch {
        Some(branch) => format!("branch {}", display_text(branch)),
        None => "detached HEAD".to_owned(),
    };
    let mut lines = vec![format!(
        "Working tree {}: {head}; {branch}",
        display_text(&tree.directory)
    )];
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
            .map(|path| format!("  {} {}", path.status, display_text(&path.path))),
    );
    if tree.omitted > 0 {
        lines.push(format!("  and {} more", tree.omitted));
    }
    lines
}
