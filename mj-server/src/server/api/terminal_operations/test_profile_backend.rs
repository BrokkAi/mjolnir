use super::super::dashboard::*;
use super::*;
use mj_client::daemon::login_terminal::TerminalInput;
use mj_client::daemon::*;
use mj_client::discovery::SetupDetection;
use mj_client::http::machines::*;
use mj_client::http::repositories::*;
use mj_client::http::setup::*;
use mj_client::http::*;
use mj_client::runtime_feed::*;
use mj_client::web::{WebListenerProcess, WebViewerAccess};
use mj_core::attention::NotificationDismissal;
use mj_core::config::Config;
use mj_core::elicitation::ElicitationResponse;
use mj_core::local_git::LocalRemoteRepair;
use mj_core::move_workspace::RetainedMoveSource;
use mj_core::state::*;
use mj_core::subagent::SubagentOptions;
use mj_core::suspend_consent::{SuspendConsent, SuspendPreflight};
use mj_core::targets::HostCapacityRow;
use mj_core::workspace::{ConversationLayout, PaneSize, PaneSizes};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
#[derive(Default)]
pub(in crate::server::api) struct ProfileBackend;

#[allow(unused_variables)]
impl TerminalOperations for ProfileBackend {
    fn terminal_transcription_available(
        self: Arc<Self>,
        preferred_profile: String,
    ) -> BoxFuture<'static, AnyResult<bool>> {
        unimplemented!()
    }
    fn terminal_transcribe_audio(
        self: Arc<Self>,
        preferred_profile: String,
        bytes: Vec<u8>,
    ) -> BoxFuture<'static, AnyResult<String>> {
        unimplemented!()
    }
    fn terminal_refresh_imported_session(self: Arc<Self>) -> BoxFuture<'static, ()> {
        unimplemented!()
    }
    fn terminal_interrupt_all(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, AnyResult<InterruptAllResult>> {
        unimplemented!()
    }
    fn terminal_submit_session_command(
        self: Arc<Self>,
        session_id: String,
        request: SessionCommandRequest,
    ) -> BoxFuture<'static, AnyResult<u64>> {
        unimplemented!()
    }
    fn terminal_queue_startup_prompt(
        self: Arc<Self>,
        session_id: String,
        request: StartupPromptRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn terminal_withdraw_startup_prompt(
        self: Arc<Self>,
        session_id: String,
        command_id: String,
    ) -> BoxFuture<'static, AnyResult<bool>> {
        unimplemented!()
    }
    fn terminal_startup_prompt_outcomes(
        self: Arc<Self>,
        session_id: String,
        request: StartupPromptOutcomesRequest,
    ) -> BoxFuture<'static, AnyResult<BTreeMap<String, StartupPromptOutcome>>> {
        unimplemented!()
    }
    fn terminal_sync_session(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn terminal_respond_elicitation(
        self: Arc<Self>,
        session_id: String,
        elicitation_id: String,
        request: ElicitationResponse,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn terminal_record_abandoned_prompt(
        self: Arc<Self>,
        session_id: String,
        request: AbandonedPromptRequest,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn terminal_start_session_lifecycle(
        self: Arc<Self>,
        request: SessionLifecycleRequest,
    ) -> BoxFuture<'static, AnyResult<LifecycleReceipt>> {
        unimplemented!()
    }
    fn terminal_wait_session_lifecycle(
        self: Arc<Self>,
        request: LifecycleReceipt,
    ) -> BoxFuture<'static, AnyResult<LifecycleCompletion>> {
        unimplemented!()
    }
    fn terminal_suspend_preflight(
        self: Arc<Self>,
        session_id: String,
        request: SuspendPreflightRequest,
    ) -> BoxFuture<'static, AnyResult<SuspendPreflight>> {
        unimplemented!()
    }
    fn terminal_suspend_session_with_consent(
        self: Arc<Self>,
        session_id: String,
        request: SuspendConsent,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn terminal_restart_session_with_ack(
        self: Arc<Self>,
        session_id: String,
        request: RestartSessionRequest,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn terminal_start_create_session(
        self: Arc<Self>,
        request: CreateSessionRequest,
    ) -> BoxFuture<'static, AnyResult<RegisteredSession>> {
        unimplemented!()
    }
    fn terminal_wait_create_session(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn terminal_resume_session(
        self: Arc<Self>,
        request: ResumeSessionRequest,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn terminal_move_session(
        self: Arc<Self>,
        request: MoveSessionRequest,
    ) -> BoxFuture<'static, AnyResult<MoveOutcome>> {
        unimplemented!()
    }
    fn terminal_move_sources(
        self: Arc<Self>,
        session_id: String,
        request: MoveSourcesRequest,
    ) -> BoxFuture<'static, AnyResult<Vec<RetainedMoveSource>>> {
        unimplemented!()
    }
    fn terminal_destroy_stopped_session(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn terminal_force_destroy_session(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn terminal_cancel_lifecycle(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
}

#[allow(unused_variables)]
impl MachinesOperations for ProfileBackend {
    fn machine_preview_build_cache(
        self: Arc<Self>,
        request: PreviewBuildCacheRequest,
    ) -> BoxFuture<'static, AnyResult<BuildCacheAvailability>> {
        unimplemented!()
    }
    fn machine_install_mbx(
        self: Arc<Self>,
        request: InstallMbxRequest,
    ) -> BoxFuture<'static, AnyResult<String>> {
        unimplemented!()
    }
    fn machine_validate_project_directory(
        self: Arc<Self>,
        request: ValidateProjectDirectoryRequest,
    ) -> BoxFuture<'static, AnyResult<PreflightNew>> {
        unimplemented!()
    }
    fn machine_checkpoint_archive_sizes(
        self: Arc<Self>,
        request: CheckpointArchiveSizesRequest,
    ) -> BoxFuture<'static, AnyResult<ArchiveSizes>> {
        unimplemented!()
    }
    fn machine_test_target(
        self: Arc<Self>,
        request: TestTargetRequest,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn machine_profile_login_statuses(
        self: Arc<Self>,
    ) -> BoxFuture<'static, AnyResult<ProfileLoginStatuses>> {
        unimplemented!()
    }
    fn machine_discover_setup(
        self: Arc<Self>,
        request: DiscoverSetupRequest,
    ) -> BoxFuture<'static, AnyResult<SetupDetection>> {
        unimplemented!()
    }
    fn machine_check_project_directories(
        self: Arc<Self>,
        request: CheckProjectDirectoriesRequest,
    ) -> BoxFuture<'static, AnyResult<Vec<(PathBuf, bool)>>> {
        unimplemented!()
    }
    fn machine_resolve_container_path(
        self: Arc<Self>,
        request: ResolveContainerPathRequest,
    ) -> BoxFuture<'static, AnyResult<PathBuf>> {
        unimplemented!()
    }
    fn machine_resolve_setup_path(
        self: Arc<Self>,
        request: ResolveSetupPathRequest,
    ) -> BoxFuture<'static, AnyResult<PathBuf>> {
        unimplemented!()
    }
    fn machine_validate_mount_source(
        self: Arc<Self>,
        request: ValidateMountSourceRequest,
    ) -> BoxFuture<'static, AnyResult<MountValidation>> {
        unimplemented!()
    }
    fn machine_validate_session_mounts(
        self: Arc<Self>,
        request: ValidateSessionMountsRequest,
    ) -> BoxFuture<'static, AnyResult<SessionMountValidation>> {
        unimplemented!()
    }
}

#[allow(unused_variables)]
impl SetupOperations for ProfileBackend {
    fn config(self: Arc<Self>) -> BoxFuture<'static, AnyResult<Config>> {
        unimplemented!()
    }
    fn first_run_setup_due(self: Arc<Self>) -> BoxFuture<'static, AnyResult<bool>> {
        unimplemented!()
    }
    fn run_first_run_setup(
        self: Arc<Self>,
        request: RunFirstRunSetupRequest,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'static, AnyResult<Option<FirstRunSetupReport>>> {
        unimplemented!()
    }
    fn run_doctor(self: Arc<Self>) -> BoxFuture<'static, AnyResult<Vec<String>>> {
        unimplemented!()
    }
    fn save_setup(
        self: Arc<Self>,
        request: SaveSetupRequest,
    ) -> BoxFuture<'static, AnyResult<Config>> {
        unimplemented!()
    }
    fn api_credential(self: Arc<Self>) -> BoxFuture<'static, AnyResult<String>> {
        unimplemented!()
    }
    fn web_access(self: Arc<Self>) -> BoxFuture<'static, AnyResult<WebViewerAccess>> {
        unimplemented!()
    }
    fn recover_web_viewer(
        self: Arc<Self>,
        request: RecoverWebViewerRequest,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn inspect_web_listener(
        self: Arc<Self>,
    ) -> BoxFuture<'static, AnyResult<Vec<WebListenerProcess>>> {
        unimplemented!()
    }
    fn save_spinner_style(
        self: Arc<Self>,
        request: SaveSpinnerStyleRequest,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn check_availability(self: Arc<Self>) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn host_capacity(self: Arc<Self>) -> BoxFuture<'static, AnyResult<Vec<HostCapacityRow>>> {
        unimplemented!()
    }
    fn refresh_capacity(self: Arc<Self>) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn refresh_quota(self: Arc<Self>) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn probe_quota_profiles(
        self: Arc<Self>,
        request: ProbeQuotaProfilesRequest,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn subagent_options(
        self: Arc<Self>,
        request: SubagentOptionsRequest,
    ) -> BoxFuture<'static, AnyResult<SubagentOptions>> {
        unimplemented!()
    }
    fn warm_profile_capabilities(
        self: Arc<Self>,
        request: WarmProfileCapabilitiesRequest,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn rename_profile(
        self: Arc<Self>,
        request: RenameProfileRequest,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn rename_target(
        self: Arc<Self>,
        request: RenameTargetRequest,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
}

#[allow(unused_variables)]
impl RepositoriesOperations for ProfileBackend {
    fn new_session_remote_repairs(
        self: Arc<Self>,
        request: NewSessionRemoteRepairsRequest,
    ) -> BoxFuture<'static, AnyResult<Vec<LocalRemoteRepair>>> {
        unimplemented!()
    }
    fn repair_repository_remotes(
        self: Arc<Self>,
        request: RepairRepositoryRemotesRequest,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn session_project_source(
        self: Arc<Self>,
        request: SessionProjectSourceRequest,
    ) -> BoxFuture<'static, AnyResult<ProjectSourceIdentity>> {
        unimplemented!()
    }
    fn scan_recovery(
        self: Arc<Self>,
        request: ScanRecoveryRequest,
    ) -> BoxFuture<'static, AnyResult<RecoveryScan>> {
        unimplemented!()
    }
    fn destroy_recovery(
        self: Arc<Self>,
        request: DestroyRecoveryRequest,
    ) -> BoxFuture<'static, AnyResult<()>> {
        unimplemented!()
    }
    fn resume_repository_preflight(
        self: Arc<Self>,
        request: ResumeRepositoryPreflightRequest,
    ) -> BoxFuture<'static, AnyResult<ResumeRepositorySourcePreflight>> {
        unimplemented!()
    }
    fn replace_resume_repository_origin(
        self: Arc<Self>,
        request: ReplaceResumeRepositoryOriginRequest,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'static, AnyResult<ResumeRepositorySourcePreflight>> {
        unimplemented!()
    }
    fn archive_space_preview(
        self: Arc<Self>,
        request: ArchiveSpacePreviewRequest,
    ) -> BoxFuture<'static, AnyResult<ArchiveSpacePreview>> {
        unimplemented!()
    }
}

#[allow(unused_variables)]
impl TerminalStreams for ProfileBackend {
    fn terminal_runtime_changes(
        self: Arc<Self>,
        request: RuntimeChangesRequest,
    ) -> BoxFuture<'static, AnyResult<RuntimeFrame>> {
        unimplemented!()
    }
    fn terminal_adopt_recovery(
        self: Arc<Self>,
        request: RecoveryAdoptionRequest,
        shutdown: CancellationToken,
        upgrade_gate: Arc<mj_core::upgrade::Gate>,
    ) -> TerminalStream {
        unimplemented!()
    }
    fn terminal_open_login(
        self: Arc<Self>,
        request: LoginTerminalRequest,
        shutdown: CancellationToken,
    ) -> Result<(String, TerminalStream), ApiFailure> {
        unimplemented!()
    }
    fn terminal_send_login_input(
        self: Arc<Self>,
        id: String,
        input: TerminalInput,
    ) -> BoxFuture<'static, Result<(), ApiFailure>> {
        unimplemented!()
    }
}

#[allow(unused_variables)]
impl DashboardBackend for ProfileBackend {
    fn profile_providers(
        &self,
    ) -> BoxFuture<'_, AnyResult<std::collections::BTreeMap<String, mj_core::config::QuotaProvider>>>
    {
        Box::pin(async {
            Ok([(
                String::from("codex-1"),
                mj_core::config::QuotaProvider::OpenAI,
            )]
            .into_iter()
            .collect())
        })
    }
    fn select_provider_profile(
        &self,
        request: mj_client::http::ProviderProfileSelectionRequest,
    ) -> BoxFuture<'_, AnyResult<String>> {
        Box::pin(async { Ok("codex-1".to_owned()) })
    }
    fn dashboard_startup(
        &self,
        client_id: String,
        workspace_id: String,
    ) -> BoxFuture<'_, AnyResult<DashboardStartup>> {
        unimplemented!()
    }
    fn workspace_layouts(
        &self,
        workspace_ids: Vec<String>,
    ) -> BoxFuture<'_, AnyResult<WorkspaceLayouts>> {
        unimplemented!()
    }
    fn stored_session_metadata(&self) -> BoxFuture<'_, AnyResult<StoredSessionMetadata>> {
        unimplemented!()
    }
    fn stored_projection_tail(
        &self,
        session_id: String,
        items: usize,
    ) -> BoxFuture<'_, AnyResult<Option<MaterializedSession>>> {
        unimplemented!()
    }
    fn stored_transcript_item(
        &self,
        session_id: String,
        stable_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<TranscriptItem>>> {
        unimplemented!()
    }
    fn stored_agent_message_through(
        &self,
        session_id: String,
        through: u64,
    ) -> BoxFuture<'_, AnyResult<Option<TranscriptItem>>> {
        unimplemented!()
    }
    fn stored_session_summary(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<MaterializedSessionSummary>>> {
        unimplemented!()
    }
    fn session_tail(
        &self,
        session_id: String,
        cursor: RuntimeCursor,
    ) -> BoxFuture<'_, AnyResult<SessionTailReply>> {
        unimplemented!()
    }
    fn resume_candidates(&self) -> BoxFuture<'_, AnyResult<ResumeCandidates>> {
        unimplemented!()
    }
    fn session_record_with_verdict(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<SessionRecordWithVerdict>>> {
        unimplemented!()
    }
    fn go_startup_session(
        &self,
        workspace_id: String,
        last_session_id: Option<String>,
    ) -> BoxFuture<'_, AnyResult<Option<SessionRecord>>> {
        unimplemented!()
    }
    fn session_text_search(&self, q: String) -> BoxFuture<'_, AnyResult<SessionTextSearchPage>> {
        unimplemented!()
    }
    fn dismiss_notification(
        &self,
        session_id: String,
        target: NotificationDismissal,
    ) -> BoxFuture<'_, AnyResult<u64>> {
        unimplemented!()
    }
    fn record_operator_activity(&self) -> BoxFuture<'_, AnyResult<()>> {
        unimplemented!()
    }
    fn set_operator_away(&self, away: bool) -> BoxFuture<'_, AnyResult<()>> {
        unimplemented!()
    }
    fn attach(&self, client_id: String) -> BoxFuture<'_, AnyResult<()>> {
        unimplemented!()
    }
    fn detach(&self, client_id: String) -> BoxFuture<'_, AnyResult<()>> {
        unimplemented!()
    }
    fn save_sidebar_size(&self, size: PaneSize) -> BoxFuture<'_, AnyResult<()>> {
        unimplemented!()
    }
    fn save_workspace_pane_sizes(
        &self,
        workspace_id: String,
        sizes: PaneSizes,
    ) -> BoxFuture<'_, AnyResult<()>> {
        unimplemented!()
    }
    fn save_workspace_layout(
        &self,
        workspace_id: String,
        layout: ConversationLayout,
    ) -> BoxFuture<'_, AnyResult<()>> {
        unimplemented!()
    }
    fn set_session_title(
        &self,
        session_id: String,
        title: String,
    ) -> BoxFuture<'_, AnyResult<String>> {
        unimplemented!()
    }
    fn set_session_notifications_ignored(
        &self,
        session_id: String,
        ignored: bool,
    ) -> BoxFuture<'_, AnyResult<()>> {
        unimplemented!()
    }
    fn set_session_workspace(
        &self,
        session_id: String,
        workspace_id: String,
    ) -> BoxFuture<'_, AnyResult<()>> {
        unimplemented!()
    }
    fn set_session_container_settings(
        self: Arc<Self>,
        session_id: String,
        settings: ContainerSettingsRequest,
    ) -> BoxFuture<'static, AnyResult<ContainerSettingsResponse>> {
        unimplemented!()
    }
}
