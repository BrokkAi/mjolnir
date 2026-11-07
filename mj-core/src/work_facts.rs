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

/// One part of the facts: observed, not observable and why, or not read yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub enum Observed<T> {
    Known(T),
    Unavailable {
        reason: String,
    },
    /// Being read: the part has no fixed duration (a Git read of a large
    /// tree), and what was observed so far was recorded without waiting for it.
    Pending,
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
            Self::Unavailable { .. } | Self::Pending => None,
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
    /// Processes no longer running at the check after the stop; zero when
    /// `unverified`. The park may have stopped them or they may have exited
    /// on their own: the check sees only that they are gone.
    #[serde(default)]
    pub gone: u32,
    #[serde(default)]
    pub survived: u32,
    /// The check after the stop could not run, and why. The counts mean
    /// nothing then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unverified: Option<String>,
    /// Mjolnir could not confirm that the park happened (its stop failed, or the
    /// daemon stopped during it), and why. The processes were not checked
    /// afterwards, so the counts mean nothing then either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unconfirmed: Option<String>,
    /// The finish this park follows listed running processes, so its report told
    /// the parent that a later message would say what happened to each. Recorded
    /// when the park begins, from that finish's own facts: a later finish
    /// replaces them while the park's check is still running.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub promised: bool,
    /// The part of `snapshot.shown` that was no longer running after the stop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gone_shown: Vec<RunningProcess>,
    /// The part of `snapshot.shown` that still ran after the stop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub survived_shown: Vec<RunningProcess>,
    /// Processes the finish report listed that were not running when the park
    /// began: they exited before it. The finish report promised the parent an
    /// account of each process it listed, and a process that is in no
    /// snapshot would otherwise go unaccounted for.
    #[serde(default)]
    pub exited_before: u32,
    /// The listed ones among them that the finish report showed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exited_before_shown: Vec<RunningProcess>,
}

impl ParkFacts {
    /// Settle a park from the pids that still ran after the stop. A snapshot
    /// that was not observable settles with nothing counted.
    pub fn settled(snapshot: Observed<ProcessFacts>, survivors: &[u32]) -> Self {
        let mut park = Self {
            pending: false,
            snapshot,
            gone: 0,
            survived: 0,
            unverified: None,
            unconfirmed: None,
            promised: false,
            gone_shown: Vec::new(),
            survived_shown: Vec::new(),
            exited_before: 0,
            exited_before_shown: Vec::new(),
        };
        if let Observed::Known(processes) = &park.snapshot {
            let alive = |pid: u32| survivors.contains(&pid);
            let survived = processes
                .identities
                .iter()
                .filter(|identity| alive(identity.pid))
                .count();
            park.survived = u32::try_from(survived).unwrap_or(u32::MAX);
            park.gone = processes.running.saturating_sub(park.survived);
            let (survived_shown, gone_shown) = processes
                .shown
                .iter()
                .cloned()
                .partition(|process| alive(process.pid));
            park.survived_shown = survived_shown;
            park.gone_shown = gone_shown;
        }
        park
    }

    /// The park as it stands when its stop is about to run: the snapshot, and
    /// no outcome.
    pub fn started(snapshot: Observed<ProcessFacts>) -> Self {
        let mut park = Self::settled(snapshot, &[]);
        park.pending = true;
        park.gone = 0;
        park.survived = 0;
        park.gone_shown.clear();
        park.survived_shown.clear();
        park
    }

    /// Account for the processes the finish report listed against the ones this
    /// park saw: those not in its pre-stop snapshot had exited before the park.
    /// A park whose snapshot is unavailable cannot say, and records nothing.
    #[must_use]
    pub fn with_finish(mut self, finish: &ProcessFacts) -> Self {
        let Observed::Known(snapshot) = &self.snapshot else {
            return self;
        };
        let seen = |identity: &ProcessIdentity| snapshot.identities.contains(identity);
        let exited: Vec<u32> = finish
            .identities
            .iter()
            .filter(|identity| !seen(identity))
            .map(|identity| identity.pid)
            .collect();
        self.exited_before = u32::try_from(exited.len()).unwrap_or(u32::MAX);
        self.exited_before_shown = finish
            .shown
            .iter()
            .filter(|process| exited.contains(&process.pid))
            .cloned()
            .collect();
        self
    }

    /// This outcome as the settlement of `marker`, the park as it was recorded
    /// when it began: the account of the finish it followed travels with it.
    #[must_use]
    pub fn settling(mut self, marker: &ParkFacts) -> Self {
        self.promised = marker.promised;
        self.exited_before = marker.exited_before;
        self.exited_before_shown = marker.exited_before_shown.clone();
        self
    }

    /// A park whose check after the stop could not run.
    pub fn unverified(snapshot: Observed<ProcessFacts>, reason: impl Into<String>) -> Self {
        let mut park = Self::settled(snapshot, &[]);
        park.gone = 0;
        park.gone_shown.clear();
        park.unverified = Some(reason.into());
        park
    }

    /// A park that Mjolnir could not confirm happened. Its processes were not
    /// checked after it, and the message says so.
    pub fn unconfirmed(snapshot: Observed<ProcessFacts>, reason: impl Into<String>) -> Self {
        let mut park = Self::settled(snapshot, &[]);
        park.gone = 0;
        park.gone_shown.clear();
        park.unconfirmed = Some(reason.into());
        park
    }

    /// The number of processes the stop was about, when they were observed.
    fn observed_running(&self) -> u32 {
        self.snapshot
            .known()
            .map_or(0, |processes| processes.running)
    }

    /// Whether the parent hears of this park. It does when processes were no
    /// longer running afterwards, one survived, or what happened to running
    /// processes could not be checked; and always when the finish report
    /// listed running processes (`promised`), which told the parent a later
    /// message would say what happened to each, even if the park could not
    /// observe them.
    pub fn is_news(&self) -> bool {
        !self.pending
            && (self.promised
                || self.gone > 0
                || self.survived > 0
                || self.exited_before > 0
                || ((self.unverified.is_some() || self.unconfirmed.is_some())
                    && self.observed_running() > 0))
    }

    /// The lines of a `list_agents` entry.
    pub fn render_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.pending {
            return vec!["Park: stopping; the result is not recorded yet".into()];
        }
        if let Some(reason) = &self.unconfirmed {
            lines.push(format!(
                "Park: not confirmed ({reason}); the processes listed earlier were not checked"
            ));
            return lines;
        }
        match (&self.snapshot, &self.unverified) {
            (Observed::Unavailable { reason }, _) => lines.push(format!(
                "Park: the processes could not be observed before the stop ({reason})"
            )),
            (Observed::Pending, _) => {
                lines.push("Park: the processes were not read before the stop".into());
            }
            (Observed::Known(processes), _) if processes.running == 0 => {
                lines.push("Park: no processes were running".into());
            }
            (Observed::Known(processes), Some(reason)) => lines.push(format!(
                "Park: what happened to {} processes could not be checked ({reason})",
                processes.running
            )),
            (Observed::Known(_), None) => {
                lines.push(format!(
                    "Park: {} no longer running, {} survived",
                    self.gone, self.survived
                ));
            }
        }
        lines.extend(process_lines(Some("gone"), &self.gone_shown, self.gone));
        lines.extend(process_lines(
            Some("survived"),
            &self.survived_shown,
            self.survived,
        ));
        if self.exited_before > 0 {
            lines.push(format!(
                "  {} listed in the finish report had exited before the park",
                self.exited_before
            ));
        }
        lines
    }

    /// The body of the message that tells a parent what a park did. It says
    /// only what the check saw: a process that is gone may have been stopped by
    /// the park or may have exited on its own.
    pub fn message_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let plural = |count: u32| if count == 1 { "process" } else { "processes" };
        if let Some(reason) = &self.unconfirmed {
            lines.push(format!(
                "Mjolnir could not confirm the park of this child: {reason}. The processes listed in the finish report were not checked after it, so whether they still run is unknown; check on them yourself if they matter."
            ));
            if let Observed::Known(processes) = &self.snapshot {
                lines.extend(
                    processes
                        .shown
                        .iter()
                        .map(|process| format!("  {}", render_process(process))),
                );
            }
            return lines;
        }
        match (&self.snapshot, &self.unverified) {
            (Observed::Pending, _) => {
                lines.push(
                    "The processes listed in the finish report were not read when the park began. Parking stops the harness and the processes under it; a detached process may keep running, so check on them yourself if they matter."
                        .into(),
                );
                return lines;
            }
            (Observed::Unavailable { reason }, _) => {
                lines.push(format!(
                    "The processes listed in the finish report could not be checked when the park began: {reason}. Parking stops the harness and the processes under it; a detached process may keep running, so check on them yourself if they matter."
                ));
                return lines;
            }
            (Observed::Known(processes), _) if processes.running == 0 => {
                lines.push(
                    "None of the processes listed in the finish report was still running when the park began; they had exited before it:"
                        .into(),
                );
                lines.extend(process_lines(
                    None,
                    &self.exited_before_shown,
                    self.exited_before,
                ));
                return lines;
            }
            (Observed::Known(processes), Some(reason)) => {
                lines.push(format!(
                    "The park ran, but what happened to the {} processes the child's agent had left running could not be checked: {reason}.",
                    processes.running
                ));
                lines.extend(
                    processes
                        .shown
                        .iter()
                        .map(|process| format!("  {}", render_process(process))),
                );
            }
            (Observed::Known(_), None) => {}
        }
        if self.gone > 0 {
            lines.push(format!(
                "{} {} the child's agent had left running {} no longer running after the park. Mjolnir cannot tell whether the park stopped {} or {} exited on {} own:",
                self.gone,
                plural(self.gone),
                if self.gone == 1 { "was" } else { "were" },
                if self.gone == 1 { "it" } else { "them" },
                if self.gone == 1 { "it" } else { "they" },
                if self.gone == 1 { "its" } else { "their" },
            ));
            lines.extend(process_lines(None, &self.gone_shown, self.gone));
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
        if self.exited_before > 0 {
            lines.push(format!(
                "{} {} the finish report listed had already exited before the park began:",
                self.exited_before,
                plural(self.exited_before)
            ));
            lines.extend(process_lines(
                None,
                &self.exited_before_shown,
                self.exited_before,
            ));
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
            Observed::Pending => lines
                .push("Running processes started by the child's agent: still being read".into()),
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
        Observed::Pending => vec![
            "Working tree: still being read when this was written; a later message from Mjolnir gives it, and list_agents shows it"
                .into(),
        ],
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
    let mut lines = if processes.running == 0 {
        vec![format!("{label}: none")]
    } else {
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
            format!("HEAD {short} {:?}", display_text(&head.subject))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_that_is_gone_is_never_said_to_have_been_stopped_by_the_park() {
        let process = |pid: u32| RunningProcess {
            pid,
            elapsed_secs: 3,
            command: "cargo build".into(),
            detached: false,
        };
        let snapshot = Observed::Known(ProcessFacts {
            unattributed: 0,
            running: 2,
            shown: vec![process(10), process(11)],
            omitted: 0,
            identities: vec![
                ProcessIdentity {
                    pid: 10,
                    start: "boot:10".into(),
                },
                ProcessIdentity {
                    pid: 11,
                    start: "boot:11".into(),
                },
            ],
        });
        let park = ParkFacts::settled(snapshot, &[11]);
        let text = park.message_lines().join("\n") + &park.render_lines().join("\n");
        assert!(!text.contains("Parking stopped"), "{text}");
        assert!(!text.contains("stopped by"), "{text}");
        assert!(text.contains("no longer running"), "{text}");
        assert!(
            text.contains("cannot tell whether the park stopped"),
            "{text}"
        );
        assert_eq!((park.gone, park.survived), (1, 1));
    }
}
