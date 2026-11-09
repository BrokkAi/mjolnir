//! Worker-owned observation of what a session's agent left running and of its
//! working tree, answered on the relay connection (`RelayRequest::WorkFacts`).
//!
//! The processes of a session are those that descend from this worker (the
//! harness and everything its tools start) plus any process anywhere that
//! carries the session's `MJ_SESSION_ID`. The second set matters: a process
//! started with `setsid`, or whose launching shell exited, has been handed to
//! PID 1 and no walk of parent links finds it. The controller exports the
//! variable to every harness, and tool shells pass it on.
//!
//! Helpers the harness needs are not work, and they are identified by which
//! processes they are, never by which program they run: the harness's bridge
//! and its helper processes (an MCP server, a language runtime the bridge runs
//! on) are the descendants of this worker that already existed when the
//! harness session was configured, and Mjolnir's own commands are processes of
//! its own executable. A descendant that started after that moment was started
//! by the agent, even when it runs the very `node` the bridge runs on.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use mj_core::work_facts::{
    COMMAND_CHARS, ChangedPath, HeadCommit, Observed, ProcessFacts, ProcessIdentity,
    RunningProcess, SHOWN_PATHS, SHOWN_PROCESSES, WorkFacts, WorktreeFacts,
};

/// Everything the worker knows that the observation needs. Cloned into each
/// relay connection; the timestamp is shared with the relay, which writes it.
#[derive(Clone, Debug)]
pub struct WorkObserver {
    /// Session whose `MJ_SESSION_ID` marks its processes.
    pub session_id: String,
    /// The worker's own process: its descendants are the harness tree.
    pub worker_pid: u32,
    /// The agent's working directory.
    pub directory: PathBuf,
    /// The Git program. Only a test names another.
    pub git: std::ffi::OsString,
    /// Epoch milliseconds at which the harness session was last configured, or
    /// 0 while no configured harness exists.
    pub harness_ready_ms: Arc<AtomicI64>,
    /// The processes this worker has started to collect facts (its Git reads),
    /// whichever request started them, while they run. Shared by every
    /// connection's clone of the observer, so a scan for one request never
    /// reports the subprocess of another as the agent's work.
    pub children: ObserverChildren,
}

/// One recorded subprocess of the observer, removed from the registry when it
/// is dropped (the read ended, or its request went away).
struct RegisteredChild {
    children: ObserverChildren,
    pid: u32,
}

impl Drop for RegisteredChild {
    fn drop(&mut self) {
        // The registry's lock may be held by a scan, and dropping cannot wait.
        let children = self.children.clone();
        let pid = self.pid;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                children.0.lock().await.remove(&pid);
            });
        }
    }
}

/// The registry of the observer's own subprocesses. A scan holds it for the
/// whole scan and a spawn holds it from before the spawn until the new
/// process is recorded, so a scan sees every such process that exists, or none
/// that is not yet recorded.
#[derive(Clone, Debug, Default)]
pub struct ObserverChildren(Arc<tokio::sync::Mutex<std::collections::BTreeSet<u32>>>);

/// One process as the platform reader sees it. Cheap fields only; the text of
/// the command line and the start identity are read for the survivors.
#[derive(Clone, Debug)]
pub(crate) struct ProcRow {
    pub pid: u32,
    pub ppid: u32,
    /// Milliseconds the process has existed, on the host's monotonic clock,
    /// rounded down to the table's resolution.
    pub age_ms: i64,
    pub zombie: bool,
    pub marker: Marker,
}

/// Whether a process's environment carries this session's `MJ_SESSION_ID`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Marker {
    Present,
    #[cfg(any(unix, test))]
    Absent,
    /// The environment could not be read although the process is the same
    /// user's (a process that forbids it, such as one that is not dumpable), so
    /// whether it is this session's cannot be known.
    Unreadable,
}

#[derive(Clone, Debug)]
pub(crate) struct Described {
    /// Raw command line; arguments separated by one space.
    pub command: String,
    pub exe: Option<PathBuf>,
    pub identity: String,
}

pub(crate) struct ScanContext<'a> {
    pub worker_pid: u32,
    /// Age of the harness's configured session in ms; `None` before it exists.
    pub harness_age_ms: Option<i64>,
    /// The resolution of the ages in the table (`ProcessTable::resolution_ms`).
    pub resolution_ms: i64,
    /// Mjolnir's own executable: its commands are never the agent's work.
    pub own_exe: Option<&'a Path>,
    /// The observer's own subprocesses; they and their descendants are never
    /// the agent's work.
    pub observer_children: &'a std::collections::BTreeSet<u32>,
}

/// Decide which rows are the agent's work. Pure: the platform readers feed it.
pub(crate) fn select_processes(
    rows: &[ProcRow],
    ctx: &ScanContext<'_>,
    describe: impl Fn(&ProcRow) -> Described,
) -> ProcessFacts {
    use std::collections::{BTreeMap, BTreeSet};
    let mut children = BTreeMap::<u32, Vec<u32>>::new();
    for row in rows {
        children.entry(row.ppid).or_default().push(row.pid);
    }
    let mut tree = BTreeSet::new();
    let mut pending = vec![ctx.worker_pid];
    while let Some(pid) = pending.pop() {
        for &child in children.get(&pid).into_iter().flatten() {
            if tree.insert(child) {
                pending.push(child);
            }
        }
    }
    // The observer's own subprocesses, with everything they started.
    let mut own = ctx.observer_children.clone();
    let mut pending: Vec<u32> = own.iter().copied().collect();
    while let Some(pid) = pending.pop() {
        for &child in children.get(&pid).into_iter().flatten() {
            if own.insert(child) {
                pending.push(child);
            }
        }
    }
    let mut work = Vec::new();
    let mut unattributed = 0_u32;
    for row in rows {
        if row.pid == ctx.worker_pid || row.zombie || own.contains(&row.pid) {
            continue;
        }
        let in_tree = tree.contains(&row.pid);
        if !in_tree && row.marker == Marker::Unreadable {
            // Not the harness's descendant and not readable: it may be work
            // the agent detached, and it is counted rather than left out.
            unattributed = unattributed.saturating_add(1);
            continue;
        }
        if !in_tree && row.marker != Marker::Present {
            continue;
        }
        // A descendant that existed when the harness session was configured is
        // part of the harness. Before that moment nothing is the agent's. A
        // process is taken to be a helper only when its age puts its start
        // before readiness by more than the ages' resolution (whole seconds on
        // macOS): one that may have started on either side of it is reported as
        // work, so uncertainty shows a process and never hides one.
        if in_tree
            && ctx
                .harness_age_ms
                .is_none_or(|age| row.age_ms >= age.saturating_add(ctx.resolution_ms))
        {
            continue;
        }
        let described = describe(row);
        if ctx.own_exe.is_some() && described.exe.as_deref() == ctx.own_exe {
            continue;
        }
        work.push((row, described, !in_tree));
    }
    // Longest running first; the process ID makes the order total.
    work.sort_by_key(|(row, _, _)| (std::cmp::Reverse(row.age_ms), row.pid));
    let running = u32::try_from(work.len()).unwrap_or(u32::MAX);
    let shown: Vec<RunningProcess> = work
        .iter()
        .take(SHOWN_PROCESSES)
        .map(|(row, described, detached)| RunningProcess {
            pid: row.pid,
            elapsed_secs: u64::try_from(row.age_ms / 1000).unwrap_or(0),
            command: command_text(&described.command),
            detached: *detached,
        })
        .collect();
    ProcessFacts {
        unattributed,
        running,
        omitted: running.saturating_sub(u32::try_from(shown.len()).unwrap_or(u32::MAX)),
        shown,
        identities: work
            .iter()
            .map(|(row, described, _)| ProcessIdentity {
                pid: row.pid,
                start: described.identity.clone(),
            })
            .collect(),
    }
}

/// One line, free of terminal controls and secrets, cut for reading.
fn command_text(raw: &str) -> String {
    let clean = mj_core::transcript::sanitize_terminal_text(raw);
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean = mj_core::redact::redact_secrets(&clean);
    if clean.chars().count() <= COMMAND_CHARS {
        return clean;
    }
    let cut: String = clean.chars().take(COMMAND_CHARS).collect();
    format!("{cut}…")
}

#[cfg(target_os = "linux")]
mod linux;
// Compiled for tests everywhere so its parsing is tested here; it is run, and
// built outside tests, on macOS only.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod macos;

/// What a platform reader returns for one scan.
pub(crate) struct ProcessTable {
    pub rows: Vec<ProcRow>,
    pub reader: ProcessReader,
    /// The wall-clock instant the ages in `rows` were measured at. The
    /// harness's age is taken at this instant too: the scan after it can take
    /// as long as it likes without moving the line between a helper and work.
    pub sampled_ms: i64,
    /// The resolution of the ages in `rows`, in milliseconds: the true age of a
    /// process is somewhere within it of the age given. `ps` on macOS reports
    /// whole seconds; Linux gives the clock tick.
    pub resolution_ms: i64,
}

fn scan_processes(observer: &WorkObserver) -> Observed<ProcessFacts> {
    scan_processes_from(observer, |session_id| {
        #[cfg(target_os = "linux")]
        let table = linux::read_table(session_id);
        #[cfg(target_os = "macos")]
        let table = macos::read_table(session_id);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let table: anyhow::Result<ProcessTable> = {
            let _ = session_id;
            Err(anyhow::anyhow!(
                "this platform does not expose process environments"
            ))
        };
        table
    })
}

fn scan_processes_from(
    observer: &WorkObserver,
    read_table: impl FnOnce(&str) -> anyhow::Result<ProcessTable>,
) -> Observed<ProcessFacts> {
    // Held for the whole scan: a Git read that starts meanwhile waits to be
    // recorded, and one already running is in the registry.
    let observer_children = observer.children.0.blocking_lock();
    let own_exe = std::env::current_exe().ok().map(|exe| {
        // A replaced executable reads back with this suffix.
        let text = exe.to_string_lossy();
        PathBuf::from(text.strip_suffix(" (deleted)").unwrap_or(&text))
    });
    let ready_ms = observer.harness_ready_ms.load(Ordering::Acquire);
    let ProcessTable {
        rows,
        reader,
        sampled_ms,
        resolution_ms,
    } = match read_table(&observer.session_id) {
        Ok(table) => table,
        Err(error) => return Observed::unavailable(format!("{error:#}")),
    };
    let ctx = ScanContext {
        worker_pid: observer.worker_pid,
        // The configured moment is wall-clock; ages are monotonic, so convert
        // through the instant the ages were sampled at, not the present.
        harness_age_ms: (ready_ms > 0).then(|| sampled_ms.saturating_sub(ready_ms)),
        resolution_ms,
        own_exe: own_exe.as_deref(),
        observer_children: &observer_children,
    };
    Observed::Known(select_processes(&rows, &ctx, |row| reader.describe(row)))
}

/// Reads the text and identity of a process the scan kept.
pub(crate) struct ProcessReader {
    describe: Box<dyn Fn(&ProcRow) -> Described + Send>,
}

impl ProcessReader {
    #[cfg(any(unix, test))]
    pub(crate) fn new(describe: impl Fn(&ProcRow) -> Described + Send + 'static) -> Self {
        Self {
            describe: Box::new(describe),
        }
    }

    fn describe(&self, row: &ProcRow) -> Described {
        (self.describe)(row)
    }
}

/// What `git status` and `git log` say about `directory`. The two reads run
/// together, so the tree costs the slower of them and not their sum. The log read
/// asks for HEAD with its id (`%H`), and its subject is used only when that id is
/// the one `status` reported: a commit made between the two reads would otherwise
/// give the id of one commit the subject of another, and the subject is then
/// unavailable, with that reason.
async fn read_worktree(
    children: &ObserverChildren,
    git: &std::ffi::OsStr,
    directory: &Path,
) -> Observed<WorktreeFacts> {
    let status = run_git(
        children,
        git,
        directory,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            // Whatever `status.showUntrackedFiles` says, and with untracked
            // directories expanded to their files: a configured `no` would
            // report a tree with new files as clean.
            "--untracked-files=all",
        ],
    );
    let head = run_git(
        children,
        git,
        directory,
        &[
            "log",
            "-1",
            "--format=%H%x00%s",
            "--no-show-signature",
            "HEAD",
            "--",
        ],
    );
    let (status, head) = tokio::join!(status, head);
    let status = match status {
        Ok(output) => output,
        Err(reason) => return Observed::unavailable(reason),
    };
    let mut facts = parse_status(&String::from_utf8_lossy(&status), directory);
    if let Some(commit) = &mut facts.head {
        commit.subject = match head {
            Ok(output) => {
                let text = String::from_utf8_lossy(&output);
                match text.split_once('\0') {
                    // Exactly as Git reported it: redaction and cutting are for the
                    // rendering, which cannot tell a long branch name from a secret.
                    Some((sha, subject)) if sha == commit.sha => {
                        Observed::Known(subject.trim_end_matches('\n').to_owned())
                    }
                    Some(_) => {
                        Observed::unavailable("HEAD moved while the working tree was being read")
                    }
                    None => Observed::unavailable("git log gave no subject"),
                }
            }
            Err(reason) => Observed::unavailable(reason),
        };
    }
    Observed::Known(facts)
}

/// Run Git as a child the observation owns: in a process group of its own,
/// killed and reaped when this future is dropped (the requester disconnected,
/// or the worker is stopping), so a stalled Git never outlives its question and
/// repeated requests cannot pile scans up. While the caller stays connected it
/// waits with no deadline and no limit on the output, because neither has a
/// source: a `git status` takes as long as the tree needs.
async fn run_git(
    children: &ObserverChildren,
    git: &std::ffi::OsStr,
    directory: &Path,
    args: &[&str],
) -> std::result::Result<Vec<u8>, String> {
    let mut command = tokio::process::Command::new(git);
    // `GIT_OPTIONAL_LOCKS=0` keeps this read from taking the index lock a
    // concurrent `git commit` of the agent's would need.
    command
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    // Recorded as the child starts, under the registry's lock, which a scan
    // holds for its whole duration: a scan sees a Git read that exists, or the
    // read waits to be recorded. Removed when the read ends, however it ends,
    // including when this future is dropped on a disconnect.
    let gate = children.0.clone().lock_owned().await;
    let mut registered = None;
    let output = mj_core::subprocess::run_bounded_observed(
        &mut command,
        usize::MAX,
        std::time::Duration::MAX,
        {
            let registered = &mut registered;
            let children = children.clone();
            move |pid| {
                let mut gate = gate;
                if let Some(pid) = pid {
                    gate.insert(pid);
                    *registered = Some(RegisteredChild { children, pid });
                }
            }
        },
    )
    .await;
    let output = match output {
        Ok(output) => output,
        Err(error) => return Err(format!("could not run git: {error:#}")),
    };
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.lines().next().unwrap_or("").trim();
    Err(if detail.is_empty() {
        format!("git failed with {}", output.status)
    } else {
        detail.to_owned()
    })
}

/// Parse `git status --porcelain=v2 -z --branch`: header records beginning
/// `# `, then NUL-separated change records. A rename record is followed by one
/// more field, the original path.
fn parse_status(text: &str, directory: &Path) -> WorktreeFacts {
    let mut head_sha = None;
    let mut branch = None;
    let (mut changed, mut modified, mut untracked) = (0_u32, 0_u32, 0_u32);
    let mut shown = Vec::new();
    let mut fields = text.split('\0');
    while let Some(field) = fields.next() {
        if let Some(header) = field.strip_prefix("# ") {
            match header.split_once(' ') {
                Some(("branch.oid", oid)) if oid != "(initial)" => head_sha = Some(oid.to_owned()),
                Some(("branch.head", name)) if name != "(detached)" => {
                    branch = Some(name.to_owned());
                }
                _ => {}
            }
            continue;
        }
        let (status, path) = match field.split_at_checked(2) {
            Some(("? ", path)) => ("?".to_owned(), path),
            // `1 XY sub mH mI mW hH hI path`, `2 XY ... score path` and
            // `u XY ... path` put the path after the eighth, ninth and tenth
            // space-separated fields; paths may contain spaces.
            Some(("1 ", rest)) => change_record(rest, 8),
            Some(("2 ", rest)) => {
                // The original path of a rename is its own field.
                fields.next();
                change_record(rest, 9)
            }
            Some(("u ", rest)) => change_record(rest, 10),
            _ => continue,
        };
        changed = changed.saturating_add(1);
        match status.as_str() {
            "?" => untracked = untracked.saturating_add(1),
            "M" => modified = modified.saturating_add(1),
            _ => {}
        }
        if shown.len() < SHOWN_PATHS {
            shown.push(ChangedPath {
                status,
                // Exactly as Git reported it; the rendering sanitizes and cuts.
                path: path.to_owned(),
            });
        }
    }
    let shown_count = u32::try_from(shown.len()).unwrap_or(u32::MAX);
    WorktreeFacts {
        directory: directory.display().to_string(),
        head: head_sha.map(|sha| HeadCommit {
            sha,
            subject: Observed::unavailable("the subject was not read"),
        }),
        branch,
        changed,
        modified,
        untracked,
        omitted: changed.saturating_sub(shown_count),
        shown,
    }
}

/// The status letter and path of a `1`, `2` or `u` record whose text after the
/// record type is `rest`; `leading` fields precede the path.
fn change_record(rest: &str, leading: usize) -> (String, &str) {
    let mut parts = rest.splitn(leading, ' ');
    let xy = parts.next().unwrap_or("..");
    let path = parts.last().unwrap_or("");
    let mut letters = xy.chars();
    let (index, tree) = (letters.next().unwrap_or('.'), letters.next().unwrap_or('.'));
    let letter = if index == 'U' || tree == 'U' {
        'U'
    } else if index != '.' {
        index
    } else {
        tree
    };
    (letter.to_string(), path)
}

/// What a sub-agent's worker observes of its agent's work when a turn ends: the
/// processes under the worker or carrying the session's marker, and the working
/// tree. The two are read together. A scan never reports the observer's own Git
/// children (they are registered as they start). The Git reads have no fixed
/// duration; the caller that waits for them can cancel by dropping this future,
/// which kills them.
pub async fn capture(observer: &WorkObserver) -> WorkFacts {
    let scan = {
        let observer = observer.clone();
        async move {
            tokio::task::spawn_blocking(move || scan_processes(&observer))
                .await
                .unwrap_or_else(|error| {
                    Observed::unavailable(format!("process scan failed: {error}"))
                })
        }
    };
    // Awaited here, not handed to a blocking thread: the Git children belong to
    // this future and are killed when it is dropped.
    capture_from(
        scan,
        read_worktree(&observer.children, &observer.git, &observer.directory),
    )
    .await
}

/// The two observations of a capture, run together: the scan of the processes
/// (blocking work, on the blocking pool) beside the Git reads. The time added to
/// a turn's end is the slower of the two, not their sum.
async fn capture_from(
    processes: impl std::future::Future<Output = Observed<ProcessFacts>>,
    worktree: impl std::future::Future<Output = Observed<WorktreeFacts>>,
) -> WorkFacts {
    let (processes, worktree) = tokio::join!(processes, worktree);
    WorkFacts {
        observed_at_ms: u64::try_from(mj_core::clock::epoch_millis()).unwrap_or(0),
        processes,
        worktree,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKER: u32 = 100;

    fn row(pid: u32, ppid: u32, age_ms: i64, marked: bool) -> ProcRow {
        ProcRow {
            pid,
            ppid,
            age_ms,
            zombie: false,
            marker: if marked {
                Marker::Present
            } else {
                Marker::Absent
            },
        }
    }

    fn describe(row: &ProcRow) -> Described {
        let (command, exe) = match row.pid {
            // The harness's own helpers, present when it was configured.
            101..=104 => ("hel worker acp-supervisor".to_owned(), "/bin/hel"),
            105 => ("node bridge.js".to_owned(), "/usr/bin/node"),
            106 => (
                "claude --output-format stream-json".to_owned(),
                "/usr/bin/claude",
            ),
            // The agent's own `node`: the very program the bridge runs on.
            107 => ("node server.js".to_owned(), "/usr/bin/node"),
            _ => (format!("cargo test --job {}", row.pid), "/usr/bin/cargo"),
        };
        Described {
            command,
            exe: Some(exe.into()),
            identity: format!("boot:{}", row.pid),
        }
    }

    // A busy host: 300 unrelated processes, the harness helpers started before
    // the session was configured, and the agent's own work: a still-parented
    // build, the agent's own `node`, and a `setsid` process that has been
    // handed to PID 1.
    fn busy_table() -> Vec<ProcRow> {
        let mut rows: Vec<ProcRow> = (1000..1300)
            .map(|pid| row(pid, 1, 500_000, false))
            .collect();
        rows.push(row(WORKER, 1, 900_000, false));
        for pid in 101..=104 {
            rows.push(row(pid, WORKER, 800_000, true));
        }
        rows.push(row(105, 101, 800_000, true));
        rows.push(row(106, 105, 800_000, true));
        rows.push(row(107, 105, 30_000, true));
        for pid in 200..210 {
            rows.push(row(pid, 105, 10_000 + i64::from(pid), true));
        }
        rows.push(row(300, 1, 20_000, true));
        rows
    }

    static NO_CHILDREN: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();

    fn context(own: &Path) -> ScanContext<'_> {
        ScanContext {
            worker_pid: WORKER,
            harness_age_ms: Some(700_000),
            resolution_ms: 0,
            own_exe: Some(own),
            observer_children: &NO_CHILDREN,
        }
    }

    #[test]
    fn processes_exclude_helpers_and_keep_exact_counts_beyond_what_is_shown() {
        let facts = select_processes(&busy_table(), &context(Path::new("/bin/hel")), describe);
        // The agent's `node` is reported beside ten builds and the reparented
        // process; the bridge, which runs the same `node`, is not, nor is any
        // other helper or unrelated process among the 300.
        assert_eq!(facts.running, 12);
        assert_eq!(facts.identities.len(), 12);
        assert_eq!(facts.shown.len(), SHOWN_PROCESSES);
        assert_eq!(facts.omitted, 7);
        // Longest running first: the node is 30 s old, the detached process
        // 20 s, the builds 10 s; only the detached one is marked so.
        assert_eq!(facts.shown[0].pid, 107);
        assert_eq!(facts.shown[0].command, "node server.js");
        assert_eq!(facts.shown[1].pid, 300);
        assert!(facts.shown[1].detached);
        assert!(
            facts
                .shown
                .iter()
                .filter(|process| process.detached)
                .count()
                == 1
        );
        assert!(
            facts
                .identities
                .iter()
                .all(|identity| identity.start == format!("boot:{}", identity.pid))
        );
        assert!(!facts.identities.iter().any(|identity| identity.pid == 105));
    }

    #[test]
    fn nothing_is_agent_work_before_the_harness_is_configured() {
        let mut context = context(Path::new("/bin/hel"));
        context.harness_age_ms = None;
        let facts = select_processes(&busy_table(), &context, describe);
        // Only the process outside the harness tree remains.
        assert_eq!(facts.running, 1);
        assert_eq!(facts.shown[0].pid, 300);
    }

    #[test]
    fn long_commands_are_redacted_and_cut() {
        let secret = "ghp_".to_owned() + &"a1B2".repeat(12);
        let text = command_text(&format!(
            "curl -H token:{secret}\n  --data {}",
            "word ".repeat(100)
        ));
        assert!(!text.contains(&secret), "{text}");
        assert!(!text.contains('\n'));
        assert_eq!(text.chars().count(), COMMAND_CHARS + 1);
        assert!(text.ends_with('…'));
    }

    fn git(directory: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    // Git writes more than a pipe holds when thousands of paths are dirty, so
    // a collector that writes before it reads, or that stops at the first
    // buffer, shows up here.
    #[tokio::test]
    async fn worktree_counts_thousands_of_dirty_paths_exactly_and_shows_eight() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path();
        git(directory, &["init", "-q", "-b", "task"]);
        std::fs::write(directory.join("tracked.txt"), "one\n").unwrap();
        git(directory, &["add", "tracked.txt"]);
        git(
            directory,
            &["commit", "-qm", "Fix parser, with unicode: naïve"],
        );
        std::fs::write(directory.join("tracked.txt"), "two\n").unwrap();
        for index in 0..5000 {
            std::fs::write(
                directory.join(format!("untracked-file-with-a-long-name-{index:05}.txt")),
                "",
            )
            .unwrap();
        }
        let Observed::Known(facts) =
            read_worktree(&ObserverChildren::default(), "git".as_ref(), directory).await
        else {
            panic!("worktree unavailable");
        };
        assert_eq!(facts.changed, 5001);
        assert_eq!(facts.modified, 1);
        assert_eq!(facts.untracked, 5000);
        assert_eq!(facts.shown.len(), SHOWN_PATHS);
        assert_eq!(facts.omitted, 5001 - 8);
        assert_eq!(facts.branch.as_deref(), Some("task"));
        let head = facts.head.unwrap();
        assert_eq!(head.sha.len(), 40);
        assert_eq!(
            head.subject,
            Observed::Known("Fix parser, with unicode: naïve".into())
        );
    }

    #[tokio::test]
    async fn a_directory_that_is_not_a_repository_is_unavailable_not_clean() {
        let temp = tempfile::tempdir().unwrap();
        match read_worktree(&ObserverChildren::default(), "git".as_ref(), temp.path()).await {
            Observed::Unavailable { reason } => {
                assert!(reason.contains("not a git repository"), "{reason}")
            }
            other => panic!("reported {other:?} for a directory outside any repository"),
        }
    }

    #[tokio::test]
    async fn renames_and_paths_with_spaces_are_one_change_each() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path();
        git(directory, &["init", "-q"]);
        std::fs::write(
            directory.join("old name.txt"),
            "content that is long enough to be a rename\n",
        )
        .unwrap();
        git(directory, &["add", "."]);
        git(directory, &["commit", "-qm", "first"]);
        git(directory, &["mv", "old name.txt", "new name.txt"]);
        let Observed::Known(facts) =
            read_worktree(&ObserverChildren::default(), "git".as_ref(), directory).await
        else {
            panic!("worktree unavailable");
        };
        assert_eq!(facts.changed, 1);
        assert_eq!(facts.shown[0].status, "R");
        assert_eq!(facts.shown[0].path, "new name.txt");
    }

    // The case issue 1350 turned on: the agent's process has no parent in the
    // harness tree. A real process started with `setsid` from a shell that has
    // exited is found through its environment alone.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_setsid_orphan_is_found_by_its_session_marker_and_identified_like_the_park_script() {
        let session = format!("work-facts-test-{}", std::process::id());
        let launched = std::process::Command::new("sh")
            .args(["-c", "setsid sleep 61 >/dev/null 2>&1 &"])
            .env("MJ_SESSION_ID", &session)
            .status()
            .unwrap();
        assert!(launched.success());
        let observer = WorkObserver {
            session_id: session,
            worker_pid: std::process::id(),
            directory: std::env::temp_dir(),
            git: "git".into(),
            harness_ready_ms: Arc::new(AtomicI64::new(0)),
            children: ObserverChildren::default(),
        };
        let mut found = None;
        for _ in 0..100 {
            if let Observed::Known(facts) = scan_processes(&observer)
                && facts.running == 1
            {
                found = Some(facts);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let facts = found.expect("the orphan was not found");
        let process = &facts.shown[0];
        let pid = process.pid;
        let identity = mj_core::subprocess::process_birth_identity(pid).unwrap();
        // Stop it before asserting so a failure leaves nothing behind.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        assert!(process.detached);
        assert_eq!(process.command, "sleep 61");
        assert_eq!(facts.identities[0].start, identity);
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_observers_own_git_is_not_reported_as_the_agents_work() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let slow_git = temp.path().join("fake-git-for-the-observer-test");
        std::fs::write(&slow_git, "#!/bin/sh\nsleep 1\nexit 1\n").unwrap();
        std::fs::set_permissions(&slow_git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let observer = WorkObserver {
            session_id: format!("observer-git-{}", std::process::id()),
            // This test process stands in for the worker, so a `git` the
            // collector starts is one of its descendants, as in a real worker,
            // and it started after the harness was configured.
            worker_pid: std::process::id(),
            directory: temp.path().to_path_buf(),
            git: slow_git.clone().into(),
            harness_ready_ms: Arc::new(AtomicI64::new(mj_core::clock::epoch_millis() - 1)),
            children: ObserverChildren::default(),
        };
        let facts = capture(&observer).await;
        let Observed::Known(processes) = &facts.processes else {
            panic!("{:?}", facts.processes);
        };
        let reported = processes
            .shown
            .iter()
            .filter(|process| process.command.contains("fake-git-for-the-observer-test"))
            .count();
        assert_eq!(reported, 0, "{processes:?}");
        // The Git read ran, and failed, and says so; it is not an empty tree.
        assert!(facts.worktree.known().is_none(), "{:?}", facts.worktree);
    }

    /// Process ages and the harness's age are measured at one instant. A helper
    /// that started just before the harness was configured stays a helper
    /// however long the rest of the scan takes; the slow scan used to age the
    /// harness past the helper and report it as the agent's work.
    #[test]
    fn a_slow_scan_does_not_turn_a_helper_into_the_agents_work() {
        let worker = 4_000_000;
        let observer = WorkObserver {
            session_id: "one-sampling-instant".into(),
            worker_pid: worker,
            directory: PathBuf::from("/nonexistent"),
            git: "/nonexistent/git-must-not-run".into(),
            harness_ready_ms: Arc::new(AtomicI64::new(0)),
            children: ObserverChildren::default(),
        };
        let sampled_ms = mj_core::clock::epoch_millis();
        observer
            .harness_ready_ms
            .store(sampled_ms - 1_000, Ordering::Release);
        let row = |pid: u32, age_ms: i64| ProcRow {
            pid,
            ppid: worker,
            age_ms,
            zombie: false,
            marker: Marker::Absent,
        };
        let facts = scan_processes_from(&observer, |_| {
            // 11 started 50 ms before the harness was configured, 12 after.
            let rows = vec![row(11, 1_050), row(12, 500)];
            // The rest of the scan: reading what the survivors are.
            std::thread::sleep(std::time::Duration::from_millis(300));
            Ok(ProcessTable {
                rows,
                reader: ProcessReader::new(|row| Described {
                    command: format!("process {}", row.pid),
                    exe: None,
                    identity: format!("boot:{}", row.pid),
                }),
                sampled_ms,
                resolution_ms: 0,
            })
        });
        let Observed::Known(processes) = facts else {
            panic!("{facts:?}");
        };
        let pids: Vec<u32> = processes.shown.iter().map(|process| process.pid).collect();
        assert_eq!(pids, [12], "{processes:?}");
    }

    #[tokio::test]
    async fn untracked_directories_are_expanded_whatever_the_repository_configures() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path();
        git(directory, &["init", "-q"]);
        git(directory, &["config", "status.showUntrackedFiles", "no"]);
        std::fs::create_dir(directory.join("newdir")).unwrap();
        std::fs::write(directory.join("newdir/a.txt"), "a\n").unwrap();
        std::fs::write(directory.join("newdir/b.txt"), "b\n").unwrap();
        let Observed::Known(facts) =
            read_worktree(&ObserverChildren::default(), "git".as_ref(), directory).await
        else {
            panic!("worktree unavailable");
        };
        assert_eq!((facts.changed, facts.untracked), (2, 2));
        let paths: Vec<_> = facts.shown.iter().map(|path| path.path.as_str()).collect();
        assert_eq!(paths, ["newdir/a.txt", "newdir/b.txt"]);
    }

    #[test]
    fn a_process_whose_environment_cannot_be_read_is_counted_not_omitted() {
        let mut rows = busy_table();
        rows.push(ProcRow {
            marker: Marker::Unreadable,
            ..row(400, 1, 15_000, false)
        });
        // One inside the harness's tree needs no marker to be attributed.
        rows.push(ProcRow {
            marker: Marker::Unreadable,
            ..row(401, 105, 15_000, false)
        });
        let facts = select_processes(&rows, &context(Path::new("/bin/hel")), describe);
        assert_eq!(facts.unattributed, 1);
        assert_eq!(facts.running, 13);
        assert!(!facts.identities.iter().any(|identity| identity.pid == 400));
    }

    // A same-user process that forbids reading its environment (a process that
    // is not dumpable), found by the real reader.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_linux_reader_tells_unreadable_from_absent() {
        let mut child = std::process::Command::new("python3")
            .args([
                "-I",
                "-c",
                "import ctypes, time; ctypes.CDLL(None).prctl(4, 0); time.sleep(30)",
            ])
            .env("MJ_SESSION_ID", "unreadable-test")
            .spawn()
            .expect("start a process that is not dumpable");
        let wanted = b"MJ_SESSION_ID=unreadable-test";
        let mut seen = Marker::Absent;
        for _ in 0..100 {
            seen = linux::session_marker(child.id(), wanted);
            if seen == Marker::Unreadable {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let own = linux::session_marker(std::process::id(), b"MJ_NO_SUCH_VARIABLE=1");
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(seen, Marker::Unreadable);
        assert_eq!(own, Marker::Absent);
    }
    #[tokio::test]
    async fn a_real_long_path_is_stored_exactly_as_git_reported_it() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path();
        git(directory, &["init", "-q"]);
        let long = "src/a-very-long-ordinary-file-name-with-more-than-twenty-four-characters.rs";
        let spaced = "docs/notes  with   spaces.md";
        std::fs::create_dir_all(directory.join("src")).unwrap();
        std::fs::create_dir_all(directory.join("docs")).unwrap();
        std::fs::write(directory.join(long), "").unwrap();
        std::fs::write(directory.join(spaced), "").unwrap();
        let Observed::Known(facts) =
            read_worktree(&ObserverChildren::default(), "git".as_ref(), directory).await
        else {
            panic!("worktree unavailable");
        };
        let paths: Vec<_> = facts.shown.iter().map(|path| path.path.as_str()).collect();
        assert_eq!(paths, [spaced, long]);
    }

    /// The scan and the Git reads of a capture overlap: each waits on a barrier
    /// that only the other side releases, so the capture finishes only if both
    /// run at once. (The outer deadline only turns a regression into a failure
    /// instead of a hang.)
    #[tokio::test]
    async fn a_capture_runs_the_scan_and_the_tree_read_together() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let scan = {
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                Observed::unavailable("a fake scan")
            }
        };
        let tree = async move {
            barrier.wait().await;
            Observed::unavailable("a fake tree read")
        };
        let facts =
            tokio::time::timeout(std::time::Duration::from_secs(30), capture_from(scan, tree))
                .await
                .expect("the two reads did not run together");
        assert!(facts.processes.known().is_none() && facts.worktree.known().is_none());
    }

    /// A fake `git` for the tree read: `status` and `log` each announce that
    /// they started and then wait for the other to have started, so the reads
    /// can only finish if they run together.
    #[cfg(unix)]
    fn rendezvous_git(directory: &Path, status_sha: &str, log_sha: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let git = directory.join("rendezvous-git");
        std::fs::write(
            &git,
            format!(
                r#"#!/bin/sh
dir="$2"; command="$3"
wait_for() {{
  i=0
  while [ ! -f "$dir/$1" ]; do
    i=$((i+1)); [ "$i" -gt 1000 ] && exit 3
    sleep 0.01
  done
}}
case "$command" in
  status) touch "$dir/status.started"; wait_for log.started
          printf '# branch.oid {status_sha}\000# branch.head main\000' ;;
  log) touch "$dir/log.started"; wait_for status.started
       printf '{log_sha}\000The subject\n' ;;
  *) exit 2 ;;
esac
"#
            ),
        )
        .unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        git
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_status_and_the_subject_reads_run_together_and_agree_on_the_commit() {
        let temp = tempfile::tempdir().unwrap();
        let sha = "1".repeat(40);
        let git = rendezvous_git(temp.path(), &sha, &sha);
        let Observed::Known(tree) =
            read_worktree(&ObserverChildren::default(), git.as_os_str(), temp.path()).await
        else {
            panic!("the status and the log did not run together");
        };
        assert_eq!(
            tree.head.unwrap().subject,
            Observed::Known("The subject".into())
        );
    }

    /// A commit made between the two reads would give the id of one commit the
    /// subject of another: the subject is then unavailable, with that reason, and
    /// the commit id from `status` stays.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_subject_read_for_a_different_commit_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let git = rendezvous_git(temp.path(), &"1".repeat(40), &"2".repeat(40));
        let Observed::Known(tree) =
            read_worktree(&ObserverChildren::default(), git.as_os_str(), temp.path()).await
        else {
            panic!("the tree was unavailable");
        };
        let head = tree.head.unwrap();
        assert_eq!(head.sha, "1".repeat(40));
        assert!(
            matches!(
                &head.subject,
                Observed::Unavailable { reason } if reason.contains("HEAD moved")
            ),
            "{:?}",
            head.subject
        );
    }

    // `ps etime` is whole seconds while the harness's readiness age is in
    // milliseconds, so a process that started near readiness reads younger or
    // older than it is. Uncertainty reports the process: one that started within
    // the resolution of readiness counts as the agent's work, and only one that
    // started clearly before it is a helper.
    #[test]
    fn a_process_near_readiness_counts_as_work_at_whole_second_resolution() {
        let worker = 4_000_000;
        let observer = WorkObserver {
            session_id: "whole-second-ages".into(),
            worker_pid: worker,
            directory: PathBuf::from("/nonexistent"),
            git: "/nonexistent/git-must-not-run".into(),
            harness_ready_ms: Arc::new(AtomicI64::new(0)),
            children: ObserverChildren::default(),
        };
        let sampled_ms = mj_core::clock::epoch_millis();
        observer
            .harness_ready_ms
            .store(sampled_ms - 10_000, Ordering::Release);
        let row = |pid: u32, age_ms: i64| ProcRow {
            pid,
            ppid: worker,
            age_ms,
            zombie: false,
            marker: Marker::Absent,
        };
        let facts = scan_processes_from(&observer, |_| {
            Ok(ProcessTable {
                // Readiness was 10 s ago. 11 started 3 s before it, 12 started
                // 0.5 s after it (9.5 s old, shown as 9 s), 13 started 0.3 s
                // before it (10.3 s old, shown as 10 s).
                rows: vec![row(11, 13_000), row(12, 9_000), row(13, 10_000)],
                reader: ProcessReader::new(|row| Described {
                    command: format!("process {}", row.pid),
                    exe: None,
                    identity: format!("boot:{}", row.pid),
                }),
                sampled_ms,
                resolution_ms: 1_000,
            })
        });
        let Observed::Known(processes) = facts else {
            panic!("{facts:?}");
        };
        let mut pids: Vec<u32> = processes.shown.iter().map(|process| process.pid).collect();
        pids.sort_unstable();
        assert_eq!(pids, [12, 13], "{processes:?}");
    }
}
