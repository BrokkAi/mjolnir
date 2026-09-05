//! Adopting a native coding-agent session as a stopped Mjolnir session.
//!
//! One implementation serves every harness: the `mj import <harness>`
//! subcommands differ only in which agent home they read and what they call the
//! session file, and the dashboard's background import runs the same steps with
//! progress reporting and cancellation.

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Args, Subcommand};
use hel::hel_archive::verify_archive_streaming;
use hel::hel_config::{HarnessKind, HelConfig, sessions_dir};
use hel::hel_projection::materialized_session_from_canonical;
use hel::hel_state::{HelState, SessionRecord};
use hel_tui::{ImportProfileOption, ImportSessionOption};
use mj_controller::hel_controller::Controller;
use mj_controller::hel_import::{
    BundleResolution, ClaudeImportRequest, ClaudeSessionSelection, ClaudeTranscript,
    CodexImportRequest, GrokImportRequest, ImportArchiveProgress, ImportControl,
    ImportedClaudeSession, KimiImportRequest, NativeImportRequest, SessionEditTargets,
    claude_config_home, codex_config_home, grok_config_home, import_claude_session,
    import_codex_session, import_grok_session, import_kimi_session,
    import_native_session_with_control, import_safety_issues, kimi_config_home,
    locate_claude_session, locate_codex_session, locate_grok_session, locate_kimi_session,
    locate_native_session, read_native_transcript, resolve_bundle, scan_native_sessions,
    session_edit_targets,
};

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
}

/// The arguments every `mj import <harness>` subcommand takes. The harness is
/// the subcommand name, so nothing else about the four differs.
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
}

impl ImportCommand {
    /// The harness the subcommand names, and the arguments it took.
    fn split(self) -> (HarnessKind, NativeImportArgs) {
        match self {
            ImportCommand::Claude(args) => (HarnessKind::Claude, args),
            ImportCommand::Codex(args) => (HarnessKind::Codex, args),
            ImportCommand::Kimi(args) => (HarnessKind::Kimi, args),
            ImportCommand::Grok(args) => (HarnessKind::Grok, args),
        }
    }
}

pub(crate) fn import(args: ImportArgs, workspace_id: &str) -> Result<()> {
    let (harness, args) = args.command.split();
    import_native(harness, args, workspace_id)
}

/// How the CLI names a harness while it reports what it selected.
const fn import_label(harness: HarnessKind) -> &'static str {
    match harness {
        HarnessKind::Claude => "Claude",
        HarnessKind::Codex => "Codex",
        HarnessKind::Kimi => "Kimi",
        HarnessKind::Grok => "Grok Build",
        HarnessKind::Deepseek => "DeepSeek Harness",
    }
}

/// Where a harness keeps the sessions Mjolnir may read. Never modified.
fn harness_config_home(harness: HarnessKind) -> Result<PathBuf> {
    match harness {
        HarnessKind::Claude => claude_config_home(),
        HarnessKind::Codex => codex_config_home(),
        HarnessKind::Kimi => kimi_config_home(),
        HarnessKind::Grok => grok_config_home(),
        HarnessKind::Deepseek => bail!(
            "DeepSeek Harness sessions resume directly through ACP; native import is unavailable"
        ),
    }
}

/// The harness's own importer, already bound to the session that was located.
/// Each harness has its own request shape, so binding it here is what lets the
/// steps around it be written once.
type HarnessImport = Box<
    dyn FnOnce(
        &HelConfig,
        &mut HelState,
        &ClaudeTranscript,
        &str,
        Option<&str>,
    ) -> Result<ImportedClaudeSession>,
>;

/// One located native session plus the importer that can adopt it.
struct LocatedImport {
    native_session_id: String,
    source_path: PathBuf,
    import: HarnessImport,
}

fn locate_for_import(
    harness: HarnessKind,
    home: PathBuf,
    selection: &ClaudeSessionSelection,
) -> Result<LocatedImport> {
    let archives = sessions_dir();
    Ok(match harness {
        HarnessKind::Claude => {
            let source = locate_claude_session(&home, selection)?;
            LocatedImport {
                native_session_id: source.native_session_id.clone(),
                source_path: source.jsonl_path.clone(),
                import: Box::new(move |config, state, transcript, bundle_id, title| {
                    import_claude_session(
                        config,
                        state,
                        ClaudeImportRequest {
                            claude_home: &home,
                            source: &source,
                            transcript,
                            bundle_id,
                            profile_id: None,
                            title,
                            archive_directory: &archives,
                        },
                    )
                }),
            }
        }
        HarnessKind::Codex => {
            let source = locate_codex_session(&home, selection)?;
            LocatedImport {
                native_session_id: source.native_session_id.clone(),
                source_path: source.jsonl_path.clone(),
                import: Box::new(move |config, state, transcript, bundle_id, title| {
                    import_codex_session(
                        config,
                        state,
                        CodexImportRequest {
                            codex_home: &home,
                            source: &source,
                            transcript,
                            bundle_id,
                            profile_id: None,
                            title,
                            archive_directory: &archives,
                        },
                    )
                }),
            }
        }
        HarnessKind::Kimi => {
            let source = locate_kimi_session(&home, selection)?;
            LocatedImport {
                native_session_id: source.native_session_id.clone(),
                source_path: source.session_path.clone(),
                import: Box::new(move |config, state, transcript, bundle_id, title| {
                    import_kimi_session(
                        config,
                        state,
                        KimiImportRequest {
                            kimi_home: &home,
                            source: &source,
                            transcript,
                            bundle_id,
                            profile_id: None,
                            title,
                            archive_directory: &archives,
                        },
                    )
                }),
            }
        }
        HarnessKind::Grok => {
            let source = locate_grok_session(&home, selection)?;
            LocatedImport {
                native_session_id: source.native_session_id.clone(),
                source_path: source.session_path.clone(),
                import: Box::new(move |config, state, transcript, bundle_id, title| {
                    import_grok_session(
                        config,
                        state,
                        GrokImportRequest {
                            grok_home: &home,
                            source: &source,
                            transcript,
                            bundle_id,
                            profile_id: None,
                            title,
                            archive_directory: &archives,
                        },
                    )
                }),
            }
        }
        HarnessKind::Deepseek => bail!(
            "DeepSeek Harness sessions resume directly through ACP; native import is unavailable"
        ),
    })
}

/// Adopt one native session from the command line. Every harness takes these
/// same steps; only the locator and the importer bound in [`LocatedImport`]
/// differ.
fn import_native(harness: HarnessKind, args: NativeImportArgs, workspace_id: &str) -> Result<()> {
    let home = harness_config_home(harness)?;
    let selection = match args.session {
        Some(session) => ClaudeSessionSelection::NativeSessionId(session),
        None => ClaudeSessionSelection::Latest,
    };
    let located = locate_for_import(harness, home.clone(), &selection)?;
    println!(
        "Selected {} session {} at {}",
        import_label(harness),
        located.native_session_id,
        located.source_path.display()
    );
    let transcript = read_native_transcript(harness, &located.source_path)?;
    println!("Original cwd: {}", transcript.cwd.display());

    let config = HelConfig::load()?;
    let mut state = HelState::load()?;
    state.validate_against_config(&config)?;
    let targets = session_edit_targets(&transcript, &home)?;
    if !confirm_import_safety(&targets, args.allow_dirty_local, args.allow_omitted_non_git)? {
        println!("{IMPORT_CANCELLED_MESSAGE}");
        return Ok(());
    }
    // Resolve and persist a synthesized bundle while holding the config lock;
    // the archive scan then runs outside that lock so other settings do not
    // wait behind a large import.
    let (config, bundle_id) = HelConfig::update(|config| {
        resolve_import_bundle(config, &transcript, &targets, args.bundle.as_deref())
    })?;
    let imported = (located.import)(
        &config,
        &mut state,
        &transcript,
        &bundle_id,
        args.title.as_deref(),
    )?;
    let session = state
        .sessions
        .get_mut(&imported.session_id)
        .context("import did not add its session to controller state")?;
    session.workspace_id = workspace_id.to_owned();
    persist_imported_session(session)?;
    println!("{}", import_success_message(&imported));
    Ok(())
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
    config: &mut HelConfig,
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
    let answer = mj_controller::hel_readline::LineReader::default()
        .read_line("Proceed? [y/N]: ")?
        .unwrap_or_default();
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

pub(crate) fn persist_imported_session(session: &SessionRecord) -> Result<()> {
    let session = session.clone();
    tokio::runtime::Handle::try_current()
        .context(IMPORT_RUNTIME_CONTEXT)?
        .block_on(async {
            crate::daemon::connect_or_start()
                .await?
                .persist_imported_session(session)
                .await
        })
}

pub(crate) fn persist_imported_session_locally(session: &SessionRecord) -> Result<()> {
    hel::hel_database::save_session(session)?;
    let checkpoint = session
        .checkpoint
        .as_ref()
        .context("imported session has no checkpoint")?;
    let canonical = verify_archive_streaming(&checkpoint.archive_path)?.canonical_session;
    let materialized = materialized_session_from_canonical(session.id.clone(), &canonical)?;
    hel::hel_database::save_materialized_session(&materialized)
}

#[derive(Clone)]
pub(crate) struct PendingDashboardImport {
    pub(crate) profile_id: String,
    pub(crate) native_session_id: String,
    pub(crate) display_title: String,
}

#[derive(Clone, Copy)]
pub(crate) struct DashboardImportSafety {
    pub(crate) accepted: bool,
    pub(crate) include_untracked: bool,
}

pub(crate) struct ImportBundlePrompt {
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

pub(crate) fn discover_import_profile(
    profile_id: String,
    harness_kind: hel::hel_config::HarnessKind,
    home: PathBuf,
    mut publish: impl FnMut(&ImportProfileOption),
) -> ImportProfileOption {
    let mut profile = ImportProfileOption {
        profile_id,
        harness_kind,
        sessions: Vec::new(),
        scan_progress: None,
        error: None,
    };
    let discovered = scan_native_sessions(harness_kind, &home, |progress| {
        profile.scan_progress = Some((progress.scanned, progress.total));
        if let Some(session) = progress.session {
            profile.sessions.push(import_session_option(session));
        }
        publish(&profile);
    });
    if let Err(error) = discovered {
        profile.error = Some(format!("{error:#}"));
        publish(&profile);
    }
    profile
}

fn import_session_option(
    session: mj_controller::hel_import::NativeSessionListing,
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
    config: &mut HelConfig,
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
    let issues = import_safety_issues(&targets)?;
    if !safety_accepted
        && (!issues.dirty_git_roots.is_empty()
            || !issues.omitted_non_git_dirs.is_empty()
            || !issues.scratch_git_roots.is_empty())
    {
        return Ok(BackgroundBundleResolution::NeedsConfirmation(
            ImportBundlePrompt {
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
    let imported = import_native_session_with_control(
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
        &control,
    )?;
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

    fn parse_import(arguments: &[&str]) -> (HarnessKind, NativeImportArgs) {
        let cli = crate::Cli::try_parse_from(arguments).expect("import subcommand parses");
        let Some(crate::Command::Import(args)) = cli.command else {
            panic!("{arguments:?} did not parse as an import command");
        };
        args.command.split()
    }

    /// All four harnesses share one implementation, so each subcommand must
    /// still name its own harness and take the same selection arguments.
    #[test]
    fn every_import_subcommand_names_its_harness_and_takes_the_same_arguments() {
        for (subcommand, expected) in [
            ("claude", HarnessKind::Claude),
            ("codex", HarnessKind::Codex),
            ("kimi", HarnessKind::Kimi),
            ("grok", HarnessKind::Grok),
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
}
