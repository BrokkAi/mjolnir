//! Adopting a native coding-agent session as a stopped Mjolnir session.
//!
//! One implementation serves every harness: the `mj import <harness>`
//! subcommands differ only in which agent home they read and what they call the
//! session file, and the dashboard's background import runs the same steps with
//! progress reporting and cancellation.

use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use clap::{ArgGroup, Args, Subcommand};
use mj_core::config::{Config, HarnessKind, sessions_dir};
use mj_core::state::{SessionRecord, State};

use mj_controller::controller::Controller;
use mj_controller::import::{
    BundleResolution, ClaudeSessionSelection, ClaudeTranscript, ImportArchiveProgress,
    ImportControl, ImportedClaudeSession, NativeImportRequest, NativeScanCache, SessionEditTargets,
    import_native_session, import_safety_issues, locate_native_session, read_native_transcript,
    resolve_bundle, scan_native_sessions, session_edit_targets,
};
use mj_tui::{ImportProfileOption, ImportSessionOption};

const IMPORT_CANCELLED_MESSAGE: &str = "Import cancelled; no Mjolnir files were changed.";
const DIRTY_IMPORT_WARNING: &str =
    "These Git roots are dirty; Mjolnir will archive their complete current state:";
const IMPORT_RUNTIME_CONTEXT: &str = "import persistence requires the Mjolnir async runtime";

#[derive(Debug, Args)]
pub(crate) struct ImportArgs {
    #[command(subcommand)]
    command: ImportCommand,
}

#[derive(Debug, Subcommand)]
enum ImportCommand {
    /// Import a session created by vanilla Claude Code.
    Claude(NativeImportArgs),
    /// Import a session created by vanilla Codex.
    Codex(NativeImportArgs),
    /// Import a session created by vanilla Kimi Code.
    Kimi(NativeImportArgs),
    /// Import a session created by vanilla Grok Build.
    Grok(NativeImportArgs),
    /// Import a session created by Muse Code.
    Muse(NativeImportArgs),
}

/// The arguments every `mj import <harness>` subcommand takes. The harness is
/// the subcommand name, so the remaining options are shared.
#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("import-session-selection")
        .required(true)
        .args(["session", "latest"])
))]
pub(crate) struct NativeImportArgs {
    /// Native session UUID to import.
    #[arg(long)]
    session: Option<String>,
    /// Import the most recently modified session.
    #[arg(long)]
    latest: bool,
    /// Profile whose home holds the session. Needed only when several
    /// enabled profiles run this harness.
    #[arg(long)]
    profile: Option<String>,
    /// Existing configured bundle to associate with the imported session.
    #[arg(long)]
    bundle: Option<String>,
    /// Title displayed in Mjolnir's dashboard.
    #[arg(long)]
    title: Option<String>,
    /// Proceed after acknowledging dirty detected Git roots.
    #[arg(long = "allow-dirty", visible_alias = "allow-dirty-local")]
    allow_dirty_local: bool,
    /// Proceed after acknowledging edited non-Git or scratch directories will be omitted.
    #[arg(long)]
    allow_omitted_non_git: bool,
    #[command(flatten)]
    pub(crate) workspace: crate::WorkspaceName,
}

impl ImportArgs {
    /// The workspace named with the command.
    pub(crate) fn workspace(&self) -> &crate::WorkspaceName {
        match &self.command {
            ImportCommand::Claude(args)
            | ImportCommand::Codex(args)
            | ImportCommand::Kimi(args)
            | ImportCommand::Grok(args)
            | ImportCommand::Muse(args) => &args.workspace,
        }
    }
}

impl ImportCommand {
    /// The harness the subcommand names, and the arguments it took.
    fn split(self) -> (HarnessKind, NativeImportArgs) {
        match self {
            ImportCommand::Claude(args) => (HarnessKind::Claude, args),
            ImportCommand::Codex(args) => (HarnessKind::Codex, args),
            ImportCommand::Kimi(args) => (HarnessKind::Kimi, args),
            ImportCommand::Grok(args) => (HarnessKind::Grok, args),
            ImportCommand::Muse(args) => (HarnessKind::Muse, args),
        }
    }
}

pub(crate) fn import(args: ImportArgs, workspace_id: &str) -> Result<()> {
    let (harness, args) = args.command.split();
    let source = import_source(&Config::load()?, harness, args.profile.as_deref())?;
    import_native(harness, args, &source, workspace_id).map(|_| ())
}

/// Where one import reads its session from.
#[derive(Debug, PartialEq, Eq)]
struct ImportSource {
    /// The configured profile whose home this is, when one is.
    profile_id: Option<String>,
    home: PathBuf,
}

/// Where `mj import <harness>` reads sessions: the home of the named profile,
/// else of the one enabled profile that runs this harness, else the harness's
/// own stock home when no profile runs it.
///
/// It used to read the stock home whatever was configured, so on a machine
/// where a profile keeps its sessions elsewhere, `--latest` picked a session
/// that belonged to some other `mj` instance or to the user's own harness
/// (F-18).
fn import_source(
    config: &Config,
    harness: HarnessKind,
    requested: Option<&str>,
) -> Result<ImportSource> {
    if let Some(profile_id) = requested {
        let profile = config
            .profiles
            .get(profile_id)
            .with_context(|| format!("unknown profile {profile_id:?}"))?;
        ensure!(profile.enabled, "profile {profile_id:?} is disabled");
        ensure!(
            profile.kind == harness,
            "profile {profile_id:?} runs {}, not {}",
            profile.kind.display_name(),
            harness.display_name()
        );
        return Ok(ImportSource {
            profile_id: Some(profile_id.to_owned()),
            home: profile.home.clone(),
        });
    }
    let candidates: Vec<(&String, &mj_core::config::HarnessProfile)> = config
        .profiles
        .iter()
        .filter(|(_, profile)| profile.enabled && profile.kind == harness)
        .collect();
    match candidates.as_slice() {
        [] => Ok(ImportSource {
            profile_id: None,
            home: harness_config_home(harness)?,
        }),
        [(profile_id, profile)] => Ok(ImportSource {
            profile_id: Some((*profile_id).clone()),
            home: profile.home.clone(),
        }),
        several => bail!(
            "several profiles run {}; name the one whose home holds the session with --profile: {}",
            harness.display_name(),
            several
                .iter()
                .map(|(profile_id, _)| profile_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Adopt one named native session, answering with the Mjolnir session id the
/// import produced, or `None` when the person declined the safety prompt.
///
/// This is `mj import <harness> --session <id>` without the subcommand, for
/// `mj resume --wiki` continuing a row another tool wrote. It reads the same
/// stock harness home, so a session the harness itself no longer keeps there
/// cannot be imported this way.
pub(crate) fn import_named_native_session(
    harness: HarnessKind,
    native_session_id: String,
    workspace_id: &str,
) -> Result<Option<String>> {
    let source = ImportSource {
        profile_id: None,
        home: harness_config_home(harness)?,
    };
    import_native(
        harness,
        NativeImportArgs {
            session: Some(native_session_id),
            latest: false,
            profile: None,
            bundle: None,
            title: None,
            allow_dirty_local: false,
            allow_omitted_non_git: false,
            workspace: crate::WorkspaceName::default(),
        },
        &source,
        workspace_id,
    )
}

/// How the CLI names a harness while it reports what it selected.
const fn import_label(harness: HarnessKind) -> &'static str {
    match harness {
        HarnessKind::Claude => "Claude",
        HarnessKind::Codex => "Codex",
        HarnessKind::Kimi => "Kimi",
        HarnessKind::Grok => "Grok Build",
        HarnessKind::Muse => "Muse Code",
    }
}

/// Where a harness keeps the sessions Mjolnir may read. Never modified.
fn harness_config_home(harness: HarnessKind) -> Result<PathBuf> {
    mj_controller::import::harness_config_home(harness)
}

/// Adopt one native session from the command line. Every harness takes these
/// same steps; only the locator and the importer bound in [`LocatedImport`]
/// differ.
fn import_native(
    harness: HarnessKind,
    args: NativeImportArgs,
    source: &ImportSource,
    workspace_id: &str,
) -> Result<Option<String>> {
    let home = source.home.clone();
    let selection = match args.session {
        Some(session) => ClaudeSessionSelection::NativeSessionId(session),
        None => ClaudeSessionSelection::Latest,
    };
    let located = locate_native_session(harness, &home, &selection)?;
    // Printed before anything is read or written, so a `--latest` that picked
    // the wrong session is seen before it is imported.
    println!(
        "Selected {} session {} at {}{}",
        import_label(harness),
        located.native_session_id,
        located.source_path.display(),
        match &source.profile_id {
            Some(profile_id) => format!(" (profile {profile_id})"),
            None => " (no profile runs this harness; read its default home)".to_owned(),
        }
    );
    let transcript = read_native_transcript(harness, &located.source_path)?;
    println!("Original cwd: {}", transcript.cwd.display());

    let mut state = mj_controller::database::load_state()?;
    state.validate()?;
    let targets = session_edit_targets(&transcript, &home)?;
    if !confirm_import_safety(&targets, args.allow_dirty_local, args.allow_omitted_non_git)? {
        println!("{IMPORT_CANCELLED_MESSAGE}");
        return Ok(None);
    }
    // Resolve and persist a synthesized bundle while holding the config lock;
    // the archive scan then runs outside that lock so other settings do not
    // wait behind a large import.
    let (config, bundle_id) = Config::update(|config| {
        resolve_import_bundle(config, &transcript, &targets, args.bundle.as_deref())
    })?;
    let imported = import_native_session(
        &config,
        &mut state,
        NativeImportRequest {
            harness,
            harness_home: &home,
            native_session_id: &located.native_session_id,
            source_path: &located.source_path,
            transcript: &transcript,
            bundle_id: &bundle_id,
            profile_id: source.profile_id.as_deref(),
            title: args.title.as_deref(),
            archive_directory: &sessions_dir(),
        },
        None,
    )?;
    let session = state
        .sessions
        .get_mut(&imported.session_id)
        .context("import did not add its session to controller state")?;
    session.workspace_id = workspace_id.to_owned();
    persist_imported_session(session)?;
    println!("{}", import_success_message(&imported));
    Ok(Some(imported.session_id))
}

fn import_success_message(imported: &ImportedClaudeSession) -> String {
    format!(
        "Imported {} as Mjolnir session {} (bundle {}, archive {})",
        imported.native_session_id,
        imported.session_id,
        imported.bundle_id,
        imported.archive_path.display()
    )
}

fn resolve_import_bundle(
    config: &mut Config,
    transcript: &ClaudeTranscript,
    targets: &SessionEditTargets,
    requested_bundle: Option<&str>,
) -> Result<String> {
    match resolve_bundle(config, &transcript.cwd, targets, requested_bundle)? {
        BundleResolution::Existing(bundle_id) => Ok(bundle_id),
        BundleResolution::Synthesized { id, bundle } => {
            config.bundles.insert(id.clone(), bundle);
            Ok(id)
        }
    }
}

fn confirm_import_safety(
    targets: &SessionEditTargets,
    allow_dirty: bool,
    allow_omitted_non_git: bool,
) -> Result<bool> {
    let issues = import_safety_issues(targets)?;
    let needs_dirty = !issues.dirty_git_roots.is_empty() && !allow_dirty;
    let needs_omitted = !issues.omitted_non_git_dirs.is_empty() && !allow_omitted_non_git;
    let needs_scratch = !issues.scratch_git_roots.is_empty() && !allow_omitted_non_git;
    if !needs_dirty && !needs_omitted && !needs_scratch {
        return Ok(true);
    }
    if needs_dirty {
        eprintln!("{DIRTY_IMPORT_WARNING}");
        for (root, summary) in &issues.dirty_git_roots {
            eprintln!("  {} — {summary}", root.display());
        }
    }
    if needs_omitted {
        eprintln!("These edited directories are outside Git and cannot be included:");
        for directory in &issues.omitted_non_git_dirs {
            eprintln!("  {}", directory.display());
        }
    }
    if needs_scratch {
        eprintln!(
            "The session wrote to scratch repositories under temporary directories; they will not be part of the session's workspace on resume:"
        );
        for root in &issues.scratch_git_roots {
            eprintln!("  {}", root.display());
        }
    }
    if !io::stdin().is_terminal() {
        let flags = match (needs_dirty, needs_omitted || needs_scratch) {
            (true, true) => "--allow-dirty and --allow-omitted-non-git",
            (true, false) => "--allow-dirty",
            (false, true) => "--allow-omitted-non-git",
            (false, false) => unreachable!(),
        };
        bail!("pass {flags} to acknowledge import safety warnings");
    }
    let answer = mj_controller::readline::LineReader::default()
        .read_line("Proceed? [y/N]: ")?
        .unwrap_or_default();
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

pub(crate) fn persist_imported_session(session: &SessionRecord) -> Result<()> {
    let session = session.clone();
    tokio::runtime::Handle::try_current().context(IMPORT_RUNTIME_CONTEXT)?;
    mj_core::runtime::block_on(async {
        crate::daemon::connect_or_start()
            .await?
            .persist_imported_session(session)
            .await
    })?
}

#[derive(Clone)]
pub(crate) struct PendingDashboardImport {
    pub(crate) profile_id: String,
    pub(crate) native_session_id: String,
    pub(crate) display_title: String,
}

#[derive(Clone, Copy)]
pub(crate) struct DashboardImportSafety {
    pub(crate) create_managed_worktree: Option<bool>,
    pub(crate) accepted: bool,
    pub(crate) include_untracked: bool,
}

pub(crate) struct ImportBundlePrompt {
    pub(crate) managed_worktree: mj_core::state::ManagedWorktreeOptions,
    pub(crate) dirty_git_roots: Vec<String>,
    pub(crate) omitted_non_git_dirs: Vec<String>,
    pub(crate) scratch_git_roots: Vec<String>,
    pub(crate) has_untracked_files: bool,
}

pub(crate) struct DashboardImportSuccess {
    pub(crate) harness: &'static str,
    pub(crate) session_id: String,
    pub(crate) controller: Controller,
}

pub(crate) enum DashboardImportTaskResult {
    NeedsBundle(ImportBundlePrompt),
    Imported(Box<DashboardImportSuccess>),
    Cancelled,
}

pub(crate) enum DashboardImportUpdate {
    Progress {
        task_id: u64,
        step: usize,
        total: Option<usize>,
        message: String,
    },
    Finished {
        task_id: u64,
        pending: PendingDashboardImport,
        result: Box<Result<DashboardImportTaskResult>>,
    },
}

pub(crate) struct DashboardImportRequest {
    pub(crate) workspace_id: String,
    pub(crate) pending: PendingDashboardImport,
    pub(crate) safety: DashboardImportSafety,
    pub(crate) task_id: u64,
    pub(crate) cancelled: Arc<AtomicBool>,
}

/// How often a running scan publishes the profile it is building. Publishing
/// after every file clones the whole growing list each time, which is
/// quadratic in the session count; the dialog cannot show more than a few
/// updates a second anyway.
const SCAN_PUBLISH_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) fn discover_import_profile(
    profile_id: String,
    harness_kind: mj_core::config::HarnessKind,
    home: PathBuf,
    cache: &NativeScanCache,
    mut publish: impl FnMut(&ImportProfileOption),
) -> ImportProfileOption {
    let mut profile = ImportProfileOption {
        profile_id,
        harness_kind,
        sessions: Vec::new(),
        scan_progress: None,
        error: None,
    };
    let mut last_publish: Option<Instant> = None;
    let mut git_availability = HashMap::new();
    let discovered = scan_native_sessions(harness_kind, &home, cache, |progress| {
        profile.scan_progress = Some((progress.scanned, progress.total));
        if let Some(session) = progress.session {
            let reason = git_availability
                .entry(session.cwd.clone())
                .or_insert_with(
                    || match mj_controller::import::git_root_for_path(&session.cwd) {
                        Ok(Some(_)) => None,
                        Ok(None) => Some("missing Git repo".to_owned()),
                        Err(error) => Some(format!("could not inspect Git repo: {error:#}")),
                    },
                );
            let mut option = import_session_option(session);
            if option.unavailable_reason.is_none() {
                option.unavailable_reason = reason.clone();
            }
            push_unique_session(&mut profile.sessions, option);
        }
        if last_publish.is_none_or(|last| last.elapsed() >= SCAN_PUBLISH_INTERVAL) {
            last_publish = Some(Instant::now());
            publish(&profile);
        }
    });
    if let Err(error) = discovered {
        profile.error = Some(format!("{error:#}"));
    }
    // The last state always reaches the dialog, whatever the throttle skipped.
    publish(&profile);
    profile
}

/// Adds one scanned session unless the profile already lists it. A harness
/// can keep one native session under more than one working-directory entry
/// (Grok Build did, I2-16); scans run newest first, so the first listing is
/// the one kept.
fn push_unique_session(sessions: &mut Vec<ImportSessionOption>, option: ImportSessionOption) {
    if sessions
        .iter()
        .any(|listed| listed.native_session_id == option.native_session_id)
    {
        return;
    }
    sessions.push(option);
}

fn import_session_option(
    session: mj_controller::import::NativeSessionListing,
) -> ImportSessionOption {
    let project_directory = display_home_relative(&session.cwd);
    let details = format!(
        "{} · {} · {}",
        session.git_branch,
        format_byte_size(session.size_bytes),
        project_directory
    );
    ImportSessionOption {
        native_session_id: session.native_session_id,
        title: session.title,
        project_directory,
        details,
        unavailable_reason: session.unavailable_reason.map(ToOwned::to_owned),
        last_activity_ms: system_time_epoch_ms(session.modified_at),
        natively_archived: session.natively_archived,
    }
}

/// Epoch milliseconds for a file timestamp. Times before the epoch clamp to
/// zero rather than sorting as though they were in the future.
fn system_time_epoch_ms(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}

fn format_byte_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / KIB)
    } else {
        format!("{:.1}MB", bytes as f64 / MIB)
    }
}

fn display_home_relative(path: &std::path::Path) -> String {
    dirs::home_dir()
        .and_then(|home| path.strip_prefix(home).ok().map(PathBuf::from))
        .map(|relative| format!("~/{}", relative.display()))
        .unwrap_or_else(|| path.display().to_string())
}

pub(crate) fn spawn_dashboard_import(
    controller: &Controller,
    request: DashboardImportRequest,
    updates: tokio::sync::mpsc::Sender<DashboardImportUpdate>,
    tracker: crate::dashboard::CriticalOperationTracker,
) {
    let DashboardImportRequest {
        workspace_id,
        pending,
        safety,
        task_id,
        cancelled,
    } = request;
    let guard = tracker.begin_cancellable("importing session", cancelled.clone());
    let worker_controller = Controller {
        config: controller.config.clone(),
        state: controller.state.clone(),
    };
    tokio::task::spawn_blocking(move || {
        let last_detail_update = Mutex::new(Instant::now() - Duration::from_secs(1));
        let report = |step: usize, total: Option<usize>, message: &str, force: bool| {
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            if force {
                if let Err(error) = updates.blocking_send(DashboardImportUpdate::Progress {
                    task_id,
                    step,
                    total,
                    message: message.into(),
                }) {
                    tracing::debug!(task_id, %error, "import progress consumer closed");
                }
                return;
            }
            let mut last_update = last_detail_update.lock().expect("import progress lock");
            let now = Instant::now();
            if now.duration_since(*last_update) < Duration::from_millis(250) {
                return;
            }
            match updates.try_send(DashboardImportUpdate::Progress {
                task_id,
                step,
                total,
                message: message.into(),
            }) {
                Ok(()) => *last_update = now,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    tracing::debug!(task_id, "import progress consumer closed");
                }
            }
        };
        let mut result = import_session_from_profile(
            worker_controller,
            &pending.profile_id,
            &pending.native_session_id,
            &pending.display_title,
            safety,
            &cancelled,
            report,
        );
        if let Ok(DashboardImportTaskResult::Imported(imported)) = &mut result
            && let Some(session) = imported
                .controller
                .state
                .sessions
                .get_mut(&imported.session_id)
        {
            session.workspace_id = workspace_id;
        }
        if cancelled.load(Ordering::Acquire) {
            if let Ok(DashboardImportTaskResult::Imported(imported)) = &result
                && let Some(path) = imported
                    .controller
                    .state
                    .sessions
                    .get(&imported.session_id)
                    .and_then(|session| session.checkpoint.as_ref())
                    .map(|checkpoint| checkpoint.archive_path.clone())
                && let Err(error) = std::fs::remove_file(&path)
            {
                tracing::warn!(path = %path.display(), %error, "could not remove cancelled import checkpoint");
            }
            result = Ok(DashboardImportTaskResult::Cancelled);
        }
        if let Err(error) = updates.blocking_send(DashboardImportUpdate::Finished {
            task_id,
            pending,
            result: Box::new(result),
        }) {
            tracing::debug!(task_id, %error, "import completion consumer closed");
        }
        drop(guard);
    });
}

enum BackgroundBundleResolution {
    Ready(String),
    NeedsConfirmation(ImportBundlePrompt),
}

fn report_import_archive_progress(
    progress: ImportArchiveProgress,
    report: &(impl Fn(usize, Option<usize>, &str, bool) + Sync),
) {
    match progress {
        ImportArchiveProgress::Repository { current, total, id } => report(
            current,
            Some(total),
            &format!("Snapshotting repository {current}/{total}: {id}"),
            true,
        ),
        ImportArchiveProgress::UntrackedFile {
            repository_id,
            current,
            total,
            path,
        } => report(
            current,
            Some(total),
            &format!(
                "Repository {repository_id}: archiving untracked file {current}/{total}: {}",
                path.display()
            ),
            current == 1 || current == total,
        ),
        ImportArchiveProgress::WritingArchive => report(
            1,
            None,
            "Writing, syncing, and verifying the archive…",
            true,
        ),
    }
}

fn resolve_background_import_bundle(
    config: &mut Config,
    transcript: &ClaudeTranscript,
    profile_home: &std::path::Path,
    safety_accepted: bool,
) -> Result<BackgroundBundleResolution> {
    let targets = session_edit_targets(transcript, profile_home)?;
    let bundle_id = match resolve_bundle(config, &transcript.cwd, &targets, None)? {
        BundleResolution::Existing(bundle_id) => bundle_id,
        BundleResolution::Synthesized { id, bundle } => {
            config.bundles.insert(id.clone(), bundle);
            id
        }
    };
    let managed_worktree = match mj_controller::import::raw_project_import(config, &targets) {
        Some((directory, target)) => Controller {
            config: config.clone(),
            state: State::default(),
        }
        .managed_worktree_options(
            &target,
            &directory,
            &mj_controller::targets::CancellableProcessExecutor::with_timeout(Duration::from_secs(
                30,
            )),
        )?,
        None => mj_core::state::ManagedWorktreeOptions::default(),
    };
    let issues = import_safety_issues(&targets)?;
    if !safety_accepted
        && (managed_worktree.available
            || !issues.dirty_git_roots.is_empty()
            || !issues.omitted_non_git_dirs.is_empty()
            || !issues.scratch_git_roots.is_empty())
    {
        return Ok(BackgroundBundleResolution::NeedsConfirmation(
            ImportBundlePrompt {
                managed_worktree,
                dirty_git_roots: issues
                    .dirty_git_roots
                    .into_iter()
                    .map(|(root, summary)| format!("{} — {summary}", root.display()))
                    .collect(),
                omitted_non_git_dirs: issues
                    .omitted_non_git_dirs
                    .into_iter()
                    .map(|path| path.display().to_string())
                    .collect(),
                scratch_git_roots: issues
                    .scratch_git_roots
                    .into_iter()
                    .map(|path| path.display().to_string())
                    .collect(),
                has_untracked_files: issues.has_untracked_files,
            },
        ));
    }
    Ok(BackgroundBundleResolution::Ready(bundle_id))
}

fn import_session_from_profile(
    mut controller: Controller,
    profile_id: &str,
    native_session_id: &str,
    display_title: &str,
    safety: DashboardImportSafety,
    cancelled: &AtomicBool,
    report: impl Fn(usize, Option<usize>, &str, bool) + Sync,
) -> Result<DashboardImportTaskResult> {
    report(1, None, "Locating native session…", true);
    let profile = controller
        .config
        .profiles
        .get(profile_id)
        .with_context(|| format!("unknown profile {profile_id:?}"))?
        .clone();
    ensure!(profile.enabled, "profile {profile_id:?} is disabled");
    let source = locate_native_session(
        profile.kind,
        &profile.home,
        &ClaudeSessionSelection::NativeSessionId(native_session_id.into()),
    )?;
    let transcript = read_native_transcript(profile.kind, &source.source_path)?;
    report(2, Some(4), "Native session parsed.", true);
    let bundle_id = match resolve_background_import_bundle(
        &mut controller.config,
        &transcript,
        &profile.home,
        safety.accepted,
    )? {
        BackgroundBundleResolution::Ready(bundle_id) => bundle_id,
        BackgroundBundleResolution::NeedsConfirmation(prompt) => {
            return Ok(DashboardImportTaskResult::NeedsBundle(prompt));
        }
    };
    let archive_progress = |progress| report_import_archive_progress(progress, &report);
    let control = ImportControl {
        cancelled,
        progress: &archive_progress,
        include_untracked: safety.include_untracked,
    };
    let imported = import_native_session(
        &controller.config,
        &mut controller.state,
        NativeImportRequest {
            harness: profile.kind,
            harness_home: &profile.home,
            native_session_id: &source.native_session_id,
            source_path: &source.source_path,
            transcript: &transcript,
            bundle_id: &bundle_id,
            profile_id: Some(profile_id),
            title: Some(display_title),
            archive_directory: &sessions_dir(),
        },
        Some(&control),
    )?;
    controller
        .state
        .sessions
        .get_mut(&imported.session_id)
        .context("import did not add its session to controller state")?
        .create_managed_worktree = safety.create_managed_worktree;
    report(4, Some(4), "Finalizing imported session…", true);
    Ok(DashboardImportTaskResult::Imported(Box::new(
        DashboardImportSuccess {
            harness: profile.kind.display_name(),
            session_id: imported.session_id,
            controller,
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn a_native_session_is_listed_once() {
        let option = |id: &str, title: &str| ImportSessionOption {
            native_session_id: id.into(),
            title: title.into(),
            project_directory: String::new(),
            details: String::new(),
            unavailable_reason: None,
            last_activity_ms: 0,
            natively_archived: false,
        };
        let mut sessions = Vec::new();
        push_unique_session(&mut sessions, option("c8180e55", "newest"));
        push_unique_session(&mut sessions, option("7459ec1d", "other"));
        push_unique_session(&mut sessions, option("c8180e55", "older copy"));
        let titles: Vec<_> = sessions.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["newest", "other"]);
    }

    #[test]
    fn discovery_checks_current_git_availability_even_with_cached_session_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("profile");
        let project = directory.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let sessions = home.join("projects/test");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("native-test.jsonl"),
            serde_json::json!({
                "type": "user", "cwd": project, "sessionId": "native-test",
                "message": {"role": "user", "content": "Investigate a hang"}
            })
            .to_string(),
        )
        .unwrap();
        let cache = NativeScanCache::new();
        let scan = || {
            discover_import_profile(
                "test".into(),
                HarnessKind::Claude,
                home.clone(),
                &cache,
                |_| {},
            )
        };
        let profile = scan();
        assert!(profile.error.is_none(), "{:?}", profile.error);
        assert_eq!(profile.sessions.len(), 1);
        assert_eq!(
            profile.sessions[0].unavailable_reason.as_deref(),
            Some("missing Git repo")
        );

        assert!(
            mj_core::subprocess::run_with_input(
                std::process::Command::new("git")
                    .arg("init")
                    .arg("--quiet")
                    .arg(&project),
                &[],
            )
            .unwrap()
            .status
            .success()
        );
        assert_eq!(scan().sessions[0].unavailable_reason, None);

        std::fs::write(project.join(".git/config"), "[broken").unwrap();
        let profile = scan();
        let reason = profile.sessions[0].unavailable_reason.as_deref().unwrap();
        assert!(reason.contains("could not inspect Git repo"), "{reason}");
        assert!(reason.contains("bad config"), "{reason}");
    }

    fn parse_import(arguments: &[&str]) -> (HarnessKind, NativeImportArgs) {
        let cli = crate::Cli::try_parse_from(arguments).expect("import subcommand parses");
        let Some(crate::Command::Import(args)) = cli.command else {
            panic!("{arguments:?} did not parse as an import command");
        };
        args.command.split()
    }

    /// All harnesses share one implementation, so each subcommand must
    /// still name its own harness and take the same selection arguments.
    #[test]
    fn an_import_reads_the_configured_profiles_home_not_the_stock_one() {
        let profile = |kind: HarnessKind, home: &str| mj_core::config::HarnessProfile {
            enabled: true,
            kind,
            home: PathBuf::from(home),
            environment: Default::default(),
            context_window_bytes: None,
            guardian_review_model: None,
        };
        let mut config = Config::default();
        config
            .profiles
            .insert("fake".into(), profile(HarnessKind::Codex, "/lab/profile"));
        config
            .profiles
            .insert("claude".into(), profile(HarnessKind::Claude, "/lab/claude"));

        assert_eq!(
            import_source(&config, HarnessKind::Codex, None).unwrap(),
            ImportSource {
                profile_id: Some("fake".into()),
                home: PathBuf::from("/lab/profile"),
            }
        );

        config
            .profiles
            .insert("work".into(), profile(HarnessKind::Codex, "/lab/work"));
        let error = import_source(&config, HarnessKind::Codex, None).unwrap_err();
        assert!(
            error.to_string().contains("--profile") && error.to_string().contains("work"),
            "two candidate homes are not guessed between: {error}"
        );
        assert_eq!(
            import_source(&config, HarnessKind::Codex, Some("work"))
                .unwrap()
                .home,
            PathBuf::from("/lab/work")
        );
        assert!(
            import_source(&config, HarnessKind::Codex, Some("claude")).is_err(),
            "a profile for another harness holds none of its sessions"
        );
    }

    #[test]
    fn every_import_subcommand_names_its_harness_and_takes_the_same_arguments() {
        for (subcommand, expected) in [
            ("claude", HarnessKind::Claude),
            ("codex", HarnessKind::Codex),
            ("kimi", HarnessKind::Kimi),
            ("grok", HarnessKind::Grok),
            ("muse", HarnessKind::Muse),
        ] {
            let (harness, args) = parse_import(&[
                "hel",
                "import",
                subcommand,
                "--session",
                "abc",
                "--allow-dirty",
                "--allow-omitted-non-git",
            ]);
            assert_eq!(harness, expected);
            assert_eq!(args.session.as_deref(), Some("abc"));
            assert!(args.allow_dirty_local);
            assert!(args.allow_omitted_non_git);
            assert!(!args.latest);

            // The long-standing alias has to keep working.
            let (_, aliased) = parse_import(&[
                "hel",
                "import",
                subcommand,
                "--latest",
                "--allow-dirty-local",
            ]);
            assert!(aliased.latest);
            assert!(aliased.allow_dirty_local);

            // Exactly one way to choose a session, and one is required.
            assert!(crate::Cli::try_parse_from(["hel", "import", subcommand]).is_err());
            assert!(
                crate::Cli::try_parse_from([
                    "hel",
                    "import",
                    subcommand,
                    "--latest",
                    "--session",
                    "abc",
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn import_help_uses_mjolnir_product_wording() {
        use clap::CommandFactory;

        let mut command = crate::Cli::command();
        let help = command
            .find_subcommand_mut("import")
            .expect("import command")
            .find_subcommand_mut("claude")
            .expect("claude import command")
            .render_long_help()
            .to_string();
        assert!(help.contains("Title displayed in Mjolnir's dashboard"));
        assert!(!help.contains("Title displayed in Hel's dashboard"));
    }

    #[test]
    fn import_status_messages_use_mjolnir_product_wording() {
        let imported = ImportedClaudeSession {
            session_id: "session".into(),
            native_session_id: "native".into(),
            source_jsonl: PathBuf::from("native.jsonl"),
            source_cwd: PathBuf::from("workspace"),
            bundle_id: "bundle".into(),
            archive_path: PathBuf::from("checkpoint.zip"),
        };
        let messages = [
            IMPORT_CANCELLED_MESSAGE.to_owned(),
            DIRTY_IMPORT_WARNING.to_owned(),
            IMPORT_RUNTIME_CONTEXT.to_owned(),
            import_success_message(&imported),
        ];

        for message in messages {
            assert!(message.contains("Mjolnir"), "{message}");
            assert!(!message.contains("Hel"), "{message}");
        }
    }
    #[test]
    fn clean_raw_imports_require_a_worktree_choice_and_keep_it_when_imported() {
        if crate::test_support::rerun_in_isolated_child(
            "MJ_TEST_IMPORT_WORKTREE_CHOICE",
            "import::tests::clean_raw_imports_require_a_worktree_choice_and_keep_it_when_imported",
        ) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("project");
        std::fs::create_dir(&root).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.name", "Import Test"],
            vec!["config", "user.email", "test@example.invalid"],
            vec!["commit", "--allow-empty", "-m", "base"],
        ] {
            let mut command = std::process::Command::new("git");
            command.arg("-C").arg(&root).args(args);
            let output = mj_core::subprocess::run_with_input(&mut command, &[]).unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let home = directory.path().join("codex");
        let native_id = "11111111-2222-4333-8444-555555555555";
        let rollout = home
            .join("sessions/2026/09/13")
            .join(format!("rollout-{native_id}.jsonl"));
        std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        let records = [
            serde_json::json!({"timestamp":"2026-09-13T12:00:00Z", "type":"session_meta", "payload":{"id":native_id,"cwd":root,"history_mode":"paginated"}}),
            serde_json::json!({"timestamp":"2026-09-13T12:00:01Z", "type":"event_msg", "payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"import this"}]}}}),
        ];
        std::fs::write(
            rollout,
            records
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let mut config = Config::default();
        config
            .targets
            .insert("local".into(), mj_core::config::TargetTemplate::LocalBare);
        config.profiles.insert(
            "codex".into(),
            mj_core::config::HarnessProfile {
                kind: HarnessKind::Codex,
                home,
                enabled: true,
                environment: Default::default(),
                context_window_bytes: None,
                guardian_review_model: None,
            },
        );
        let controller = Controller {
            config: config.clone(),
            state: State::default(),
        };
        let result = import_session_from_profile(
            controller,
            "codex",
            native_id,
            "Imported",
            DashboardImportSafety {
                accepted: false,
                include_untracked: true,
                create_managed_worktree: None,
            },
            &AtomicBool::new(false),
            |_, _, _, _| {},
        )
        .unwrap();
        let DashboardImportTaskResult::NeedsBundle(prompt) = result else {
            panic!("clean import must offer worktree choice")
        };
        assert!(prompt.dirty_git_roots.is_empty());
        assert!(prompt.managed_worktree.available);
        assert!(prompt.managed_worktree.default_create);
        for choice in [false, true] {
            let controller = Controller {
                config: config.clone(),
                state: State::default(),
            };
            let result = import_session_from_profile(
                controller,
                "codex",
                native_id,
                "Imported",
                DashboardImportSafety {
                    accepted: true,
                    include_untracked: true,
                    create_managed_worktree: Some(choice),
                },
                &AtomicBool::new(false),
                |_, _, _, _| {},
            )
            .unwrap();
            let DashboardImportTaskResult::Imported(imported) = result else {
                panic!("accepted import must complete")
            };
            let record = &imported.controller.state.sessions[&imported.session_id];
            assert_eq!(record.create_managed_worktree, Some(choice));
            assert_eq!(record.project_directory, Some(root.canonicalize().unwrap()));
            assert!(record.managed_worktree.is_none());
            let database = directory.path().join(format!("import-{choice}.sqlite3"));
            mj_controller::database::save_state_to(&database, &imported.controller.state).unwrap();
            assert_eq!(
                mj_controller::database::load_state_from(&database)
                    .unwrap()
                    .sessions[&record.id]
                    .create_managed_worktree,
                Some(choice)
            );
        }
    }
}
