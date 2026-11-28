// Wire types for the Mjolnir viewer API.
//
// Every shape below was read from the Rust serde definitions (see
// .mj/agents/e4596e0b…/wire-schemas.md and the source files named beside each
// group). Notation: a field is optional (`?`) where Rust uses
// `skip_serializing_if`; nullable (`| null`) where Rust serializes `None` as
// `null`. Types that are opaque to the browser (ACP content, mailbox events,
// relay cursors) are `unknown` or `JsonObject` on purpose: the browser echoes
// them and never reads inside.

export type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };
export type JsonObject = { [key: string]: JsonValue };

/* -------------------------------------------------------------------------- */
/* Shared small enums                                                         */
/* -------------------------------------------------------------------------- */

export type SessionAttention =
  | 'needs_input'
  | 'important'
  | 'unread'
  | 'failed'
  | 'unreachable'
  | 'working'
  | 'idle'
  | 'inactive'
  | 'unknown';

export type NotificationKind = 'needs_input' | 'important';
export type ReviewTrigger = 'off' | 'push';
export type PublicationState = 'published' | 'unpublished' | 'unknown';
export type ManagedCheckoutKind = 'worktree' | 'clone';
export type ViewerLifecycleCategory = 'live' | 'starting' | 'suspending' | 'suspended' | 'failed';
export type ViewerChatPhase = 'idle' | 'running' | 'closing' | 'closed';
export type SessionTransitionKind =
  | 'starting'
  | 'resuming'
  | 'moving'
  | 'suspending'
  | 'destroying'
  | 'stopping';
/** `mj-client/src/runtime_feed.rs` `SessionOffer`. */
export type SessionOffer =
  | 'hidden'
  | 'ready'
  | 'move_queue_held'
  | 'transitioning'
  | 'transition_failed'
  | 'operation_in_progress'
  | 'no_recovery_copy'
  | 'no_other_workspace'
  | 'active'
  | 'move_environment_retained';
export type ResumeQueueDisposition = 'start' | 'discard';
export type MountAccess = 'ro' | 'cow' | 'rw';
export type ResourceAllocationKind = 'fixed' | 'container' | 'aws-ec2';
export type StorageCondition = 'ok' | 'low' | 'full';
export type ResetCountdownStyle = 'long' | 'five_hour';
export type LaunchAvailability = 'ready' | 'stale' | 'unavailable' | 'unknown';
export type WikiIndexState = 'ready' | 'indexing' | 'version_mismatch';
export type WikiSessionStatus = 'mine' | 'archived' | 'native';
export type TransferRoute = 'same_host' | 'direct_push' | 'direct_pull' | 'daemon_stream';
/** `mj-core/src/state/session_move.rs` `RoutePreview`. */
export type RoutePreview = 'same_host' | 'direct_or_daemon_stream';
/** `mj-core/src/state/session_move.rs` `DestinationChecks`. */
export type DestinationChecks = 'checked' | 'after_provisioning';
export type PermissionMode = string;

/* -------------------------------------------------------------------------- */
/* Policies and choices                                                       */
/* -------------------------------------------------------------------------- */

export interface SubagentChoice {
  model: string;
  effort?: string;
}

/** Internally tagged on `mode`. */
export type SubagentPolicy =
  | { mode: 'native' }
  | { mode: 'mjolnir'; straightforward?: SubagentChoice; subtle?: SubagentChoice; orchestrators?: boolean }
  | { mode: 'none' };

/** Viewer `review` field on `new`: `deserialize_session_review`. */
export type SessionReview =
  | { mode: 'inherit'; reviewers?: SubagentChoice[] }
  | { mode: 'off' }
  | { mode: 'push'; reviewers?: SubagentChoice[] };

export interface AvailabilityNote {
  text: string;
  full: string;
  routine: boolean;
}

export type AvailabilityVerdict =
  | { status: 'available' }
  | { status: 'missing'; reason: string }
  | { status: 'unknown' };

/* -------------------------------------------------------------------------- */
/* Mounts and resources                                                       */
/* -------------------------------------------------------------------------- */

/**
 * In-browser mount. `access` is always explicit; the wire form is produced by
 * `serializeAdditionalMount` because Rust uses `deny_unknown_fields` and a
 * legacy `read_only` flag with an optional `access`.
 */
export interface AdditionalMount {
  source: string;
  destination: string;
  access: MountAccess;
}

/** `AdditionalMountRepr` as it appears on the wire. */
export interface AdditionalMountWire {
  source: string;
  destination: string;
  read_only?: boolean;
  access?: MountAccess;
}

export interface HostContainerSize {
  cpus: number;
  memory_bytes: number;
}

export interface HostContainerCapacity {
  total: HostContainerSize;
  reserved: HostContainerSize;
  committed: HostContainerSize;
  remaining: HostContainerSize;
  complete: boolean;
}

/** Internally tagged on `kind`. */
export type SessionResourceAllocation =
  | { kind: 'container'; cpus: number; memory_bytes: number }
  | { kind: 'aws-ec2'; instance_type: string; vcpus: number; memory_bytes: number };

/* -------------------------------------------------------------------------- */
/* Session pieces                                                             */
/* -------------------------------------------------------------------------- */

export interface ViewerSessionCapabilities {
  open: boolean;
  prompt: boolean;
  run_shell: boolean;
  interrupt_turn: boolean;
  cancel_operation: boolean;
  suspend: boolean;
  destroy: boolean;
  rename: boolean;
  resume: boolean;
  set_config: boolean;
  set_plan_mode: boolean;
  // `#[serde(default)]` on the Rust side; always serialized by the server.
  clear_context?: boolean;
  read_history?: boolean;
  dismiss_notification?: boolean;
  move_session?: boolean;
  change_workspace?: boolean;
  container_settings?: boolean;
  restart?: boolean;
  interrupt_all?: boolean;
}

export interface SessionVerdict {
  transition?: SessionTransitionKind | null;
  transition_failure?: SessionTransitionKind | null;
  live_dashboard: boolean;
  needs_person: boolean;
  move_queue_held: boolean;
  idle: boolean;
  working: boolean;
  review_working?: boolean;
  active_work: boolean;
  unfinished: boolean;
  interrupt: boolean;
  interrupt_shell?: boolean;
  interrupt_member: boolean;
  interrupt_all: boolean;
  interrupt_count: number;
  stop: SessionOffer;
  restart: SessionOffer;
  move_session: SessionOffer;
  change_workspace: SessionOffer;
  container_settings: SessionOffer;
  resume: SessionOffer;
  working_children: number;
  working_descendants?: number;
  unfinished_children: number;
}

export interface FailureIdentity {
  generation: number;
  state: string;
  last_error?: string | null;
}

export interface InferredQuestion {
  turn_id: string;
  completed_ordinal: number;
}

/** Externally tagged, snake_case. */
export type NotificationDismissal =
  | { failure: FailureIdentity }
  | { question: InferredQuestion }
  | { read_through: number }
  | { stack: { revision: number } };

/** Externally tagged, snake_case, no `tag` attribute. */
export type EntryMessage = 'locating' | { found: string } | 'none';

export interface NotificationStackEntry {
  completed_ordinal: number;
  completed_at_ms?: number;
  kind: NotificationKind;
  message: EntryMessage;
}

export type CardFormSource = 'unknown' | 'elicitation' | 'native' | 'review_findings';

export interface CardForm {
  source?: CardFormSource;
  raised_at_ms?: number;
  request_id: string;
  message: string;
}

export interface NotificationCard {
  since_ms?: number;
  badge: NotificationKind;
  /** Oldest first. */
  entries: NotificationStackEntry[];
  dismissal: NotificationDismissal | null;
  holder: string | null;
  form: CardForm | null;
}

export interface ViewerQueuedPrompt {
  id: string;
  /** Omitted when the command is a plain prompt. */
  kind?: 'prompt' | { set_config: { key: string; value: string } };
  text: string;
  created_at: string;
}

export interface ViewerUserShell {
  id: string;
  command: string;
  started_at_ms: number | null;
}

export interface ViewerBackgroundTask {
  id: string;
  command: string;
  started_at_ms: number;
  can_stop: boolean;
}

export interface ViewerOperationStage {
  label: string;
  started_at_epoch_seconds: number;
}

export type ViewerOperationKind =
  | 'create'
  | 'resume'
  | 'move'
  | 'suspend'
  | 'destroy'
  | 'stop'
  | 'cleanup'
  | 'checkpoint';

export interface ViewerOperation {
  id: string;
  session_id: string;
  kind: ViewerOperationKind;
  started_at_epoch_seconds: number;
  stages?: ViewerOperationStage[];
  notice?: string;
  cancellable: boolean;
}

export interface ViewerConfigChoice {
  value: string;
  name: string;
  description?: string;
}

export interface ViewerConfigOption {
  key: string;
  label: string;
  current?: string;
  choices: ViewerConfigChoice[];
}

export interface ViewerReviewRole {
  label: string;
  state: string;
  child_session_id?: string;
}

export interface ViewerReviewVerdict {
  kind: string;
  text: string;
}

export interface ViewerTurnReview {
  child_session_id?: string;
  status: string;
  roles?: ViewerReviewRole[];
  verdict?: ViewerReviewVerdict;
}

export interface ViewerMjCommand {
  name: string;
  description: string;
  source: 'mj' | 'agent';
  argument?: string;
}

/** `mj-core/src/storage.rs` `ApiActivityDetails` (re-exported as `ViewerActivityDetails`). */
export interface ViewerActivityDetails {
  kind: 'turn' | 'step' | 'background' | 'idle' | 'lifecycle';
  turn_started_at_ms?: number;
  step_started_at_ms?: number;
  background_started_at_ms?: number;
  idle_since_ms?: number;
  last_activity_at_ms?: number;
  label?: string;
}

/** `mj-core/src/activity.rs` `ActivityState`; internally tagged on `state`. */
export type ActivityState =
  | { state: 'unknown'; last_known: ActivityState; since_ms?: number }
  | { state: 'closed' }
  | { state: 'closing' }
  | { state: 'turn'; started_at_ms?: number; last_activity_at_ms?: number }
  | { state: 'tool'; tool_call_id: string; started_at_ms: number; last_activity_at_ms?: number }
  | { state: 'background'; started_at_ms?: number }
  | { state: 'expecting'; since_ms: number }
  | { state: 'goal' }
  | { state: 'checking-continuation' }
  | { state: 'retry' }
  | { state: 'idle'; since_ms?: number };

/** `mj-core/src/relay/capacity.rs` `CapacityRetry`. `submitted` is internal but serialized. */
export interface CapacityRetry {
  attempt: number;
  retry_at_ms: number;
  command_id: string;
  submitted?: boolean;
}

/** `mj-core/src/continuation.rs` `QuotaRecovery`. */
export interface QuotaRecovery {
  user_command_id: string;
  completed_command_id: string;
  profile_id: string;
  reset_at_ms: number | null;
  retry_at_ms: number | null;
  notice: string;
  submitted: boolean;
}

export type SteeringStatus = 'pending' | 'unconfirmed' | 'failed' | 'applied' | 'resolved';

/** `mj-core/src/relay/snapshot.rs` `SteeringOperation`. */
export interface SteeringOperation {
  command_id: string;
  active_prompt_id: string;
  queued_prompt_id: string;
  status: SteeringStatus;
  message: string | null;
}

export type TurnControlStatus = 'ready' | 'pending' | 'unconfirmed' | 'unavailable';
export type TurnControlKind = 'steer' | 'apply_queued' | 'interrupt';

export type TurnInterruptTarget =
  | { kind: 'active_prompt'; command_id: string }
  | { kind: 'queued_restart'; command_id: string }
  | { kind: 'autonomous'; start_ordinal: number }
  | { kind: 'goal_resume'; command_id: string }
  | { kind: 'capacity_retry'; command_id: string }
  | { kind: 'quota_recovery'; completed_command_id: string };

/** Opaque ACP/relay values the browser forwards but does not inspect. */
export type RelayCursor = JsonObject;
export type ContentBlock = JsonObject;

/**
 * `mj-core/src/relay/snapshot.rs` `RelayCommand`: adjacently tagged
 * (`type` + `data`, snake_case). Unit variants have no `data`.
 */
export type RelayCommand =
  | { type: 'install_prompt_context'; data: { text: string } }
  | { type: 'seed_assessment_context'; data: { expected: RelayCursor; context: JsonObject } }
  | { type: 'dismiss_question'; data: { question: InferredQuestion } }
  | { type: 'clear_notifications'; data: { revision: number } }
  | { type: 'notifications_read'; data: { through: number } }
  | { type: 'clear_context' }
  | { type: 'prompt'; data: { prompt: ContentBlock[] } }
  | { type: 'set_subagent_wait'; data: { active_prompt_id: string; waiting: boolean } }
  | {
      type: 'handback_reminder';
      data: { completed_command_id: string; completed_ordinal: number; running?: string[] };
    }
  | {
      type: 'hold_follow_up';
      data: { completed_command_id: string; completed_ordinal: number; kind: string; running: string[] };
    }
  | { type: 'deliver_mailbox_event'; data: { event: JsonObject } }
  | { type: 'mailbox_wake'; data: { events: JsonObject[] } }
  | {
      type: 'continue_authorized_work';
      data: {
        expected: RelayCursor;
        user_command_id: string;
        completed_command_id: string;
        attempt: number;
      };
    }
  | { type: 'set_quota_recovery'; data: { expected: RelayCursor; recovery: QuotaRecovery | null } }
  | { type: 'resume_after_quota'; data: { expected: RelayCursor; completed_command_id: string } }
  | { type: 'run_user_shell'; data: { command: string } }
  | { type: 'cancel_user_shell'; data: { shell_command_id: string } }
  | { type: 'remove_queued_prompt'; data: { queued_command_id: string } }
  | { type: 'clear_queued_prompts' }
  | { type: 'set_config'; data: { key: string; value: string } }
  | { type: 'restore_execution_mode' }
  | { type: 'goal_control'; data: { action: JsonValue } }
  | { type: 'set_session_mode'; data: { mode_id: string } }
  | { type: 'cancel_turn' }
  | { type: 'steer'; data: { active_prompt_id: string; queued_prompt_id: string } }
  | { type: 'cancel_turn_for'; data: { active_prompt_id: string } }
  | { type: 'cancel_turn_target'; data: { target: TurnInterruptTarget } }
  | { type: 'apply_queued_for'; data: { active_prompt_id: string; queued_command_id: string } }
  | { type: 'resolve_steering'; data: { steering_id: string } }
  | { type: 'cancel' }
  | { type: 'close'; data: { barrier_command_id: string; expected: RelayCursor } }
  | { type: 'begin_checkpoint'; data: { reason: string | null } }
  | { type: 'complete_checkpoint'; data: { barrier_command_id: string } }
  | { type: 'release_checkpoint'; data: { barrier_command_id: string } }
  | { type: 'advance_recovery_floor'; data: { through: RelayCursor } }
  | { type: 'record_notice'; data: { text: string } };

export interface TurnControlAction {
  kind: TurnControlKind;
  command: RelayCommand;
}

/** `mj-core/src/relay/snapshot/turn_control.rs` `TurnControls`. */
export interface TurnControls {
  status: TurnControlStatus;
  primary: TurnControlAction | null;
  interrupt: RelayCommand | null;
  message: string | null;
}

export type NativeAgentAvailability = 'available' | 'unknown' | 'unavailable';
export type NativeAgentState = 'running' | 'completed' | 'failed' | 'cancelled' | 'disconnected';

export interface NativeAgentCapabilities {
  cancel: boolean;
  close: boolean;
}

/** `mj-core/src/native_agent.rs` `NativeAgent`; `task` is stripped on the browser wire. */
export interface NativeAgent {
  availability: NativeAgentAvailability;
  availability_reason?: string;
  stable_id?: string;
  owner_session_id: string;
  session_id: string;
  parent_session_id: string | null;
  name: string;
  capabilities: NativeAgentCapabilities;
  state: NativeAgentState;
}

/** `mj-session-types/src/viewer_move_recovery.rs` `ViewerMoveRecovery`. */
export interface ViewerMoveRecovery {
  failure_notice?: string;
  oracle?: boolean;
  oracle_choice?: SubagentChoice;
  operation_id: string;
  source_profile_id: string;
  source_target_template_id: string;
  destination_profile_id: string;
  destination_target_template_id: string;
  phase: string;
  queue: string;
  clear_resource_allocation: boolean;
  source_additional_mounts: AdditionalMountWire[];
  source_resource_allocation: SessionResourceAllocation | null;
  destination_additional_mounts: AdditionalMountWire[];
  destination_resource_allocation: SessionResourceAllocation | null;
  checkpoint_retained: boolean;
  environment_retained: boolean;
  destination_ready: boolean;
  queue_admission_started: boolean;
  queue_admission_finished: boolean;
}

/* -------------------------------------------------------------------------- */
/* Elicitation                                                                */
/* -------------------------------------------------------------------------- */

export interface ElicitationOption {
  value: string;
  title: string;
  description?: string;
  preview?: string;
}

interface ElicitationFieldBase {
  id: string;
  title: string;
  description?: string;
  required: boolean;
  secret?: boolean;
  custom_answer_for?: string;
  /** Absent: Alternative. `ALONGSIDE_OPTION`: Alongside. Otherwise: Detail. */
  custom_answer_option?: string;
}

export type ElicitationField = ElicitationFieldBase &
  (
    | {
        kind: 'text';
        default?: string;
        min_length?: number;
        max_length?: number;
        pattern?: string;
        format?: string;
      }
    | { kind: 'single_select'; options: ElicitationOption[]; default?: string }
    | {
        kind: 'multi_select';
        options: ElicitationOption[];
        default?: string[];
        min_items?: number;
        max_items?: number;
      }
    | { kind: 'boolean'; default?: boolean }
    | { kind: 'integer'; default?: number; minimum?: number; maximum?: number }
    | { kind: 'number'; default?: number; minimum?: number; maximum?: number }
  );

/** U+001F + "alongside". */
export const ALONGSIDE_OPTION = '\u001falongside';

export interface ToolPermissionForm {
  tool_call_id?: string;
  skip: boolean;
}

export interface ElicitationRequest {
  id: string;
  message: string;
  title?: string;
  description?: string;
  fields: ElicitationField[];
  tool_permission?: ToolPermissionForm;
}

/** Untagged in Rust: string, integer, float, boolean, string[]. */
export type ElicitationValue = string | number | boolean | string[];

export type ElicitationResponse =
  | { action: 'accept'; content?: Record<string, ElicitationValue> }
  | { action: 'decline' }
  | { action: 'cancel' };

/* -------------------------------------------------------------------------- */
/* ViewerSession                                                              */
/* -------------------------------------------------------------------------- */

/** The five fields the server interns by content hash. */
export const INTERNED_FIELDS = [
  'available_commands',
  'capabilities',
  'config_options',
  'compatible_resume_targets',
  'incompatible_resume_targets',
] as const;
export type InternedField = (typeof INTERNED_FIELDS)[number];

/** Full detail row (`ViewerSession`, `deny_unknown_fields`). */
export interface ViewerSession {
  attention: SessionAttention;
  unread_frontier?: number;
  notification_dismissal: NotificationDismissal | null;
  notification_card: NotificationCard | null;
  form_holder?: string;
  notifications_ignored?: boolean;
  oracle: boolean;
  oracle_choice?: SubagentChoice;
  review_trigger: ReviewTrigger;
  review_count: number;
  review_unavailable?: string;
  container_upgrade_supported?: boolean;
  subagents: SubagentPolicy;
  at?: string;
  branch?: string;
  base?: string;
  no_later_history?: boolean;
  targeted_turn_control_supported: boolean;
  turn_controls?: TurnControls;
  native_subagents?: NativeAgent[];
  native_subagent_attention?: Record<string, SessionAttention>;
  steering?: SteeringOperation;
  active_prompt_id?: string;
  cancelling_prompt_id?: string;
  capacity_retry?: CapacityRetry;
  retry_assessment_pending: boolean;
  quota_recovery?: QuotaRecovery;
  id: string;
  publication_state?: PublicationState;
  managed_checkout_kind?: ManagedCheckoutKind;
  workspace_id?: string;
  title: string;
  given_title?: string;
  subagent_parent_id?: string;
  subagent_task_name?: string;
  subagent_orchestrator?: boolean;
  subagent_session_ids?: string[];
  reviewer_session_ids?: string[];
  harness_kind: string;
  profile_id: string;
  bundle_id: string;
  target_id: string;
  /** A string: `SessionState` kebab-case values or `"launching"`. */
  state: string;
  active: boolean;
  created_at: string;
  updated_at: string;
  has_error: boolean;
  has_checkpoint: boolean;
  failed_launch_retry?: boolean;
  configuration_issue?: string;
  launch_error?: string;
  storage_problem?: string;
  preview?: string[];
  queued_prompts?: ViewerQueuedPrompt[];
  active_user_shells?: ViewerUserShell[];
  background_tasks?: ViewerBackgroundTask[];
  pending_elicitations?: ElicitationRequest[];
  conversation_available: boolean;
  prompt_images_supported: boolean;
  incompatible_resume_targets?: string[];
  compatible_resume_targets?: string[];
  project_label?: string;
  project_key?: string;
  display_location: string;
  additional_mounts?: AdditionalMountWire[];
  resource_allocation?: SessionResourceAllocation;
  lifecycle: ViewerLifecycleCategory;
  transitioning: boolean;
  verdict: SessionVerdict;
  latest_event_ordinal: number;
  last_activity_at_ms?: number;
  last_message_at_ms?: number;
  activity_details?: ViewerActivityDetails;
  operation?: ViewerOperation;
  move_recovery?: ViewerMoveRecovery;
  chat_phase: ViewerChatPhase;
  is_idle: boolean;
  activity_state?: ActivityState;
  question_message?: string;
  activity?: string;
  config_options?: ViewerConfigOption[];
  policy_config_keys?: string[];
  plan_mode_active?: boolean;
  turn_review?: ViewerTurnReview;
  available_commands?: ViewerMjCommand[];
  capabilities: ViewerSessionCapabilities;
  /** Absent on a detail row. */
  detail?: undefined;
}

/** The server's `SUMMARY_FIELDS` allow-list (viewer_wire.rs). */
export type SummaryField =
  | 'attention'
  | 'notification_dismissal'
  | 'notification_card'
  | 'form_holder'
  | 'unread_frontier'
  | 'id'
  | 'workspace_id'
  | 'bundle_id'
  | 'title'
  | 'given_title'
  | 'subagent_parent_id'
  | 'subagent_session_ids'
  | 'reviewer_session_ids'
  | 'publication_state'
  | 'profile_id'
  | 'target_id'
  | 'state'
  | 'container_upgrade_supported'
  | 'resource_allocation'
  | 'created_at'
  | 'updated_at'
  | 'has_error'
  | 'configuration_issue'
  | 'launch_error'
  | 'storage_problem'
  | 'project_label'
  | 'display_location'
  | 'lifecycle'
  | 'transitioning'
  | 'verdict'
  | 'last_activity_at_ms'
  | 'last_message_at_ms'
  | 'capacity_retry'
  | 'retry_assessment_pending'
  | 'question_message'
  | 'quota_recovery'
  | 'operation'
  | 'move_recovery'
  | 'chat_phase'
  | 'is_idle'
  | 'active'
  | 'activity_details'
  | 'activity'
  | 'capabilities';

/** Row for a non-live/suspended session: only the allow-list plus counts. */
export type SessionSummary = Pick<ViewerSession, SummaryField> & {
  detail: false;
  queued_prompt_count: number;
  pending_elicitation_count: number;
  pending_elicitation_ids: string[];
};

export type SessionRow = ViewerSession | SessionSummary;

export function isSummaryRow(row: SessionRow): row is SessionSummary {
  return row.detail === false;
}

/** A row as sent: interned fields may be `*_ref` keys into the `interned` table. */
export type WireSessionRow = (ViewerSession | SessionSummary) & {
  available_commands_ref?: string;
  capabilities_ref?: string;
  config_options_ref?: string;
  compatible_resume_targets_ref?: string;
  incompatible_resume_targets_ref?: string;
};

export type InternedTable = Record<string, JsonValue>;

/* -------------------------------------------------------------------------- */
/* Snapshot and SSE frames                                                    */
/* -------------------------------------------------------------------------- */

export interface ViewerWorkspace {
  id: string;
  name: string;
  last_opened_at: string;
}

export interface ViewerQuotaWindow {
  label: string;
  pool?: string;
  resets_at_epoch_seconds?: number;
  reset_countdown_style: ResetCountdownStyle;
  banked_resets?: number;
  percent_used?: number;
  fable_remaining_percent?: number;
  resets_at?: string;
  projects_exhaustion_before_reset: boolean;
}

export interface ViewerQuota {
  summary: string;
  windows?: ViewerQuotaWindow[];
  resets_at?: string;
  stale: boolean;
  refreshed_at_epoch_seconds: number;
  rate_limited_until_epoch_seconds?: number;
  next_check_at_epoch_seconds?: number;
  has_error: boolean;
}

export interface ViewerProfile {
  id: string;
  availability: AvailabilityVerdict;
  capabilities_key: string;
  harness_kind: string;
  quota_pools?: string[];
  quota?: ViewerQuota;
}

export interface ViewerTarget {
  id: string;
  kind: string;
  permission_mode?: PermissionMode;
  yolo: boolean;
  subagents_configured: boolean;
  oracle_configured: boolean;
  resource_allocation_kind: ResourceAllocationKind;
  requires_project_directory: boolean;
  remembered_container_size?: HostContainerSize;
  container_host_limits?: HostContainerSize;
  container_capacity?: HostContainerCapacity;
  container_memory_hard_limit_reason?: string;
  default_resource_allocation?: SessionResourceAllocation;
  runtime_missing?: boolean;
  default_candidate?: boolean;
  availability: AvailabilityVerdict;
  recent_project_directories: string[];
}

export interface ViewerTargetStorage {
  machine?: string;
  mount: string;
  available_bytes: number;
  reserved_bytes: number;
  condition: StorageCondition;
  detail?: string;
}

export interface ViewerTargetCapacity {
  id: string;
  label: string;
  target_ids: string[];
  cpu_percent?: number;
  memory_used_bytes?: number;
  memory_total_bytes?: number;
  logical_cores?: number;
  container_capacity?: HostContainerCapacity;
  disk_total_bytes?: number;
  virtual_machines?: number;
  sampled_at_epoch_seconds?: number;
  refreshing: boolean;
  stale: boolean;
  has_error: boolean;
  storage?: ViewerTargetStorage[];
}

export interface ViewerRepository {
  id: string;
  /** Always serialized; `null` when absent. */
  github: string | null;
  destination: string;
}

export interface ViewerBundle {
  id: string;
  primary_repository: string;
  repositories: ViewerRepository[];
}

export interface ViewerReviewConfig {
  trigger: ReviewTrigger;
  reviewers?: SubagentChoice[];
}

export interface ViewerLaunchFailure {
  id: string;
  workspace_id: string;
  session_id?: string;
  error?: string;
}

/** `mj-client/src/daemon.rs` `RuntimeNotice`. */
export interface RuntimeNotice {
  id: number;
  session_id: string;
  text: string;
}

export interface SessionConfigChoice {
  value: string;
  name: string;
  /** Always serialized; `null` when absent. */
  description: string | null;
}

/** `mj-core/src/worker_launch.rs` `ProfileConfig`. */
export interface ProfileConfig {
  model: string | null;
  models: SessionConfigChoice[];
  efforts: SessionConfigChoice[];
  observed_at: number;
}

/** Adjacently tagged: `status` + `value`. */
export type CapabilityState<T> =
  | { status: 'pending' }
  | { status: 'ready'; value: T }
  | { status: 'failed'; value: string };

export interface ProfileCapabilities {
  choices: CapabilityState<ProfileConfig>;
  efforts: Record<string, CapabilityState<SessionConfigChoice[]>>;
}

export interface ProfileCapabilitiesSnapshot {
  profiles: Record<string, ProfileCapabilities>;
}

export interface RuntimeCursor {
  alert_sequence?: number;
  incarnation: string;
  sequence: number;
  subagent_parents?: string[];
}

/** Everything in `GET /api/v1/snapshot` except `sessions`, `cursor`, `interned`. */
export interface SnapshotMetadata {
  operator_away: boolean;
  auto_afk_minutes: number;
  profile_capabilities: ProfileCapabilitiesSnapshot;
  last_subagent_policies: Record<string, SubagentPolicy>;
  last_oracle_choices: Record<string, boolean>;
  oracle_configured: boolean;
  orchestrators_configured: boolean;
  subagents_configured: boolean;
  revision: number;
  generated_at: string;
  server_time_ms: number;
  server_version?: string;
  workspaces?: ViewerWorkspace[];
  profiles: ViewerProfile[];
  targets: ViewerTarget[];
  target_availability_notes?: AvailabilityNote[];
  bundles: ViewerBundle[];
  review_config: ViewerReviewConfig;
  capacity?: ViewerTargetCapacity[];
  launch_failures?: ViewerLaunchFailure[];
  notices?: RuntimeNotice[];
}

export interface ViewerSnapshotWire extends SnapshotMetadata {
  sessions: WireSessionRow[];
  cursor: RuntimeCursor;
  interned: InternedTable;
}

/** What changed in a transcript: new rows move `latest_seq`, regrouping moves `presentation_key`. */
export interface ConversationVersion {
  latest_seq: number;
  presentation_key: string;
}

/**
 * `mj-server/src/server/viewer_feed.rs` `ViewerFrame`. `conversation_versions`
 * is additive: older servers omit it.
 */
export type ViewerFrame =
  | {
      kind: 'snapshot';
      cursor: RuntimeCursor;
      snapshot: ViewerSnapshotWire;
      conversation_versions?: Record<string, ConversationVersion>;
    }
  | {
      kind: 'delta';
      from: RuntimeCursor;
      cursor: RuntimeCursor;
      metadata: Partial<SnapshotMetadata> & { revision: number; server_time_ms: number; generated_at: string };
      sessions: [string, WireSessionRow | null][];
      detail_changed: string[];
      interned: InternedTable;
      conversation_versions?: Record<string, ConversationVersion>;
    }
  | { kind: 'reset_required' };

/** `GET /api/v1/sessions/{id}/row`. Interned fields are already inlined. */
export interface SessionRowResponse {
  revision: number;
  row: ViewerSession;
}

/* -------------------------------------------------------------------------- */
/* Conversations and history                                                  */
/* -------------------------------------------------------------------------- */

export interface TranscriptDisclosure {
  summary: string[];
  body: string[];
}

export interface BrowserDiffStat {
  path: string;
  insertions: number;
  deletions: number;
}

/** `role_tag` in mj-client/src/transcript.rs. */
export type BrowserRole =
  | 'user'
  | 'message'
  | 'agent'
  | 'thought'
  | 'tool'
  | 'plan'
  | 'plan-proposal'
  | 'system';

/** `mj-client/src/web.rs` `BrowserTranscriptEntry`. */
export interface BrowserTranscriptEntry {
  command_id?: string;
  stable_id?: string;
  tool_detail_trimmed?: boolean;
  id: number;
  updated_seq: number;
  role: BrowserRole | (string & {});
  label: string;
  recorded_at_ms: number | null;
  tool_ended_at_ms?: number;
  generated?: boolean;
  lines: string[];
  disclosures?: TranscriptDisclosure[];
  glyph: string;
  /** `entry_tone`: a semantic colour name, never a colour. */
  tone: string;
  /** Only on tool rows: `running` | `done` | `failed`. */
  tool_status?: 'running' | 'done' | 'failed' | (string & {});
  diffstats?: BrowserDiffStat[];
}

/** `GET /api/v1/conversations/{id}`. */
export interface BrowserTranscript {
  latest_seq: number;
  presentation_key: string;
  window_start_seq: number;
  reset: boolean;
  entries: BrowserTranscriptEntry[];
}

export interface TranscriptCursor {
  position: number;
  stable_id: string;
}

/** `mj-client/src/transcript.rs` `HistoryEntry`. */
export interface HistoryEntry {
  stable_id: string;
  position: number;
  role: string;
  tool_detail_trimmed?: boolean;
  text: string;
  label?: string;
  recorded_at_ms?: number;
}

/** `GET /api/v1/sessions/{id}/history`. */
export interface HistoryPage {
  items: HistoryEntry[];
  before: TranscriptCursor | null;
  frontier: number;
}

/** `GET …/native-agents/{child}/history` without `full`. */
export interface NativeHistoryItem {
  stable_id: string;
  tool_detail_trimmed: boolean;
  position: number;
  role: string;
  text: string;
}

export interface NativeHistoryPage {
  generation: number;
  items: NativeHistoryItem[];
  has_more: boolean;
}

/** `mj-core/src/transcript.rs` `TranscriptBody`; internally tagged on `kind`. Payloads are ACP JSON. */
export type TranscriptBody =
  | { kind: 'message'; event: JsonObject }
  | { kind: 'user'; content: JsonObject[] }
  | { kind: 'agent'; chunks: JsonObject[]; streaming: boolean }
  | { kind: 'thought'; chunks: JsonObject[]; streaming: boolean }
  | {
      kind: 'tool';
      call: JsonObject;
      terminal_outputs?: TerminalOutputRecord[];
      terminal_refs?: string[];
      presentation?: JsonObject;
    }
  | {
      kind: 'terminal_output';
      record: TerminalOutputRecord;
      output_line_count?: number;
      first_error_line?: string;
      detail_trimmed?: boolean;
    }
  | { kind: 'plan'; plan: JsonObject }
  | { kind: 'plan_proposal'; proposal_id: string; plan: string }
  | { kind: 'system'; text: string };

/** `PackedText` serializes as a plain string. */
export interface TerminalOutputRecord {
  terminal_id: string;
  output: string;
  truncated?: boolean;
  exit_code?: number;
  signal?: string;
}

/** `mj-core/src/diff.rs` `DiffPatch` (camelCase on the wire). */
export interface DiffPatch {
  text: string;
  insertions: number;
  deletions: number;
  created?: boolean;
  truncated?: boolean;
}

/** `mj-core/src/transcript.rs` `ChatEntry` as serialized in a tool-detail response. */
export interface ChatEntry {
  start_seq: number;
  seq: number;
  /** `ChatRole` PascalCase: User, Agent, Message, Thought, Tool, Plan, PlanProposal, System. */
  role: string;
  text: string;
  recorded_at_ms: number | null;
  tool_ended_at_ms?: number;
  revision: number;
  message_id: string | null;
  incoming_message?: JsonObject;
  stable_id?: string;
  tool_detail_trimmed?: boolean;
  tool_call_id: string | null;
  tool_status: 'Pending' | 'Running' | 'Completed' | 'Failed' | null;
  tool_summary?: string;
  tool_presentation?: JsonObject;
  tool_input?: JsonValue;
  tool_error?: string;
  tool_content: string[];
  tool_diffstats: string[];
  tool_locations: string[];
  plan: { text: string; status: 'Pending' | 'Running' | 'Completed' | string }[];
  leading_omitted?: boolean;
  raw_only?: boolean;
  ended_after_interrupt?: boolean;
  awaiting_approval?: boolean;
}

/** `GET …/items/{stable_id}/detail`; internally tagged on `status`. */
export type ToolCallDetailResponse =
  | {
      status: 'full';
      stable_id: string;
      entry: ChatEntry;
      body: TranscriptBody;
      call: JsonValue;
      diffs: DiffPatch[];
      terminal_outputs: TerminalOutputRecord[];
    }
  | {
      status: 'unavailable';
      stable_id: string;
      summary: TranscriptBody | null;
      unavailable_reason: string;
    };

export interface DiffStatFile {
  path: string;
  insertions?: number;
  deletions?: number;
  binary: boolean;
}

/** `GET /sessions/{id}/diff?json=true`. */
export interface SessionDiff {
  diff: string;
  base: string;
  head: string;
  head_descends_from_base: boolean | null;
  tracked_files?: DiffStatFile[];
  untracked_files?: string[];
  untracked_total?: number;
}

export interface ViewerClientState {
  draft: string;
  through_event_ordinal: number;
}

export interface PromptHistory {
  entries: string[];
  truncated: boolean;
}

export type PromptHistoryScope = 'session' | 'project' | 'all';

export interface NotificationMessage {
  session_id: string;
  text: string;
}

/** Exactly one selector. */
export type NotificationMessageSelector =
  | { message_id: string }
  | { through: number }
  | { holder: string; request_id: string; raised_at_ms: number };

export interface ViewerPromptImage {
  data_base64?: string;
  mime_type: string;
  width: number;
  height: number;
  attachment?: JsonObject;
}

export interface FileAttachmentRef {
  sha256: string;
  filename: string;
  mime_type: string;
  size: number;
}

/** Untagged: an image or `{ file }`. */
export type UploadedAttachment = ViewerPromptImage | { file: FileAttachmentRef };

/* -------------------------------------------------------------------------- */
/* Consent and suspend                                                        */
/* -------------------------------------------------------------------------- */

export interface CheckoutKey {
  repository_id: string;
  checkout_path: string;
}

/** `FileIdentity` is `#[serde(transparent)]`. */
export type FileIdentity = string;

/** Default externally tagged: PascalCase variant names. */
export type UntrackedItem =
  | { Path: { checkout: CheckoutKey; path: string; identity: FileIdentity } }
  | { NestedRepository: { checkout: CheckoutKey; path: string; identity: FileIdentity } }
  | { Stash: { checkout: CheckoutKey; commit: string; message: string } }
  | { KeepDirFile: { checkout: CheckoutKey; path: string; identity: FileIdentity } };

export type GitOperationKind = 'Merge' | 'Rebase' | 'CherryPick' | 'Revert' | 'Am' | 'Bisect';

export interface GitOperation {
  checkout: CheckoutKey;
  kind: GitOperationKind;
}

export type UntrackedConsent = 'None' | 'Any' | { Shown: UntrackedItem[] };
export type OperationConsent = 'None' | 'Any' | { Shown: GitOperation[] };

/** `mj-core/src/suspend_consent.rs` `SuspendConsent`. */
export interface SuspendConsent {
  discard_untracked?: UntrackedConsent;
  abort_operations?: OperationConsent;
  without_checkpoint?: boolean;
  confirm_no_checkpoint?: boolean;
  history_only?: boolean;
}

export interface UntrackedPath {
  checkout: CheckoutKey;
  path: string;
  bytes: number;
  identity: FileIdentity;
}

export interface SuspendUntrackedStash {
  checkout: CheckoutKey;
  commit: string;
  message: string;
}

export interface KeepDirEntry {
  checkout: CheckoutKey;
  path: string;
  bytes: number;
  identity: FileIdentity;
}

export interface SuspendBlocker {
  kind: string;
  paths: string[];
  remedy: string;
}

export interface ExistingCheckpointSummary {
  verified: boolean;
  created_at?: string;
  age_seconds?: number;
}

export interface DryRunEstimate {
  git_delta_bytes?: number;
  git_delta_unavailable_reason?: string;
  session_state_bytes?: number;
  keep_dir_bytes: number;
  transfer_route?: string;
}

export interface SuspendPreflight {
  session_id: string;
  untracked: UntrackedPath[];
  nested_repositories: UntrackedPath[];
  untracked_stashes: SuspendUntrackedStash[];
  keep_dir_entries: KeepDirEntry[];
  keep_dir_bytes: number;
  operations: GitOperation[];
  raw: boolean;
  interrupting: boolean;
  children_not_handed_back: string[];
  owning_children_to_suspend?: number;
  blockers: SuspendBlocker[];
  existing_checkpoint?: ExistingCheckpointSummary;
  dry_run?: DryRunEstimate;
}

export interface SuspendPreflightQuery {
  dry_run?: boolean;
  without_checkpoint?: boolean;
  history_only?: boolean;
}

/* -------------------------------------------------------------------------- */
/* Containers, moves, resume                                                  */
/* -------------------------------------------------------------------------- */

export interface ContainerSettingsChange {
  resource_allocation: SessionResourceAllocation | null;
  mounts: AdditionalMountWire[];
  mount_history?: string[] | null;
}

export interface ContainerUpgradePreview {
  session_id: string;
  source_container_id: string;
  current_image: string;
  current_image_id: string;
  target_image: string;
  target_image_id: string | null;
  available: boolean;
  retained_mounts: string[];
  warning: string;
  settings?: ContainerSettingsChange;
}

export interface ContainerUpgradeRequest {
  selection: ContainerUpgradePreview;
  acknowledge_writable_layer_loss: boolean;
}

export interface MaterializedQueuedPrompt {
  command_id: string;
  kind?: 'prompt' | { set_config: { key: string; value: string } };
  content: JsonObject[];
  queued_at_ms: number;
  accepted_ordinal?: number;
}

export interface RawConversionPreview {
  checkout: string;
  destination: string;
  branch: string | null;
  fetch_url: string;
  push_urls: string[];
  default_branch: string;
  unpushed_commits: number;
  staged_files: number;
  unstaged_files: number;
  untracked_files: number;
  untracked_bytes: number;
  host_checkout_retained: boolean;
}

export interface MoveSelection {
  consent?: SuspendConsent;
  skip_keep_paths?: string[];
  clear_resource_allocation?: boolean;
  session_id: string;
  profile_id?: string | null;
  target_template_id?: string | null;
  additional_mounts?: AdditionalMountWire[] | null;
  resource_allocation?: SessionResourceAllocation | null;
  subagents?: SubagentPolicy;
  oracle?: boolean | null;
  oracle_choice?: SubagentChoice | null;
}

export interface MovePreparation {
  container_upgrade?: ContainerUpgradePreview;
  destination_checks?: DestinationChecks;
  route_preview?: RoutePreview;
  suspend_preflight?: SuspendPreflight;
  /** `[path, bytes]` pairs. */
  skip_keep_sizes?: [string, number][];
  source_unavailable?: boolean;
  in_place?: boolean;
  stops_subagents?: boolean;
  conversion?: RawConversionPreview;
  selection: MoveSelection;
  sized_by_destination_default?: boolean;
  source_profile_id: string;
  source_target_template_id: string;
  cross_harness: boolean;
  active: boolean;
  queued_commands: MaterializedQueuedPrompt[];
  locked_queue?: ResumeQueueDisposition;
  fingerprint: string;
  operation_id: string;
}

export interface MoveSessionRequest {
  preparation: MovePreparation;
  queue?: ResumeQueueDisposition | null;
  acknowledge_interruption: boolean;
}

/** `POST /api/v1/preflight/resume` request. */
export interface PreflightResumeRequest {
  session_id: string;
  target_id: string;
}

/** Internally tagged on `kind`, kebab-case. */
export type PreflightResume =
  | { kind: 'ready' }
  | { kind: 'converting-raw-checkout'; preview: RawConversionPreview }
  | { kind: 'unavailable'; detail: string };

/* -------------------------------------------------------------------------- */
/* Launch, projects, preflight                                                */
/* -------------------------------------------------------------------------- */

export interface LaunchProfile {
  id: string;
  harness: string;
}

export interface LaunchRepository {
  id: string;
  github?: string;
  destination: string;
}

export interface LaunchBundle {
  id: string;
  primary_repository: string;
  repositories: LaunchRepository[];
}

export interface LaunchHost {
  id: string;
  label: string;
  targets: string[];
  stale: boolean;
  refreshing: boolean;
  has_error: boolean;
}

export interface LaunchTarget {
  id: string;
  kind: string;
  requires_project_directory: boolean;
  availability: LaunchAvailability;
  unavailable_reason?: string;
  runtime_missing?: boolean;
  default_candidate?: boolean;
  host?: string;
}

/** `mj-server/src/server/api/types.rs` `LaunchDefault`. */
export interface LaunchDefault {
  profile_id: string;
  target_id: string;
}

export interface LaunchOptions {
  revision: number;
  primary_configured?: boolean;
  profiles: LaunchProfile[];
  targets: LaunchTarget[];
  bundles: LaunchBundle[];
  hosts?: LaunchHost[];
  default?: LaunchDefault;
}

/** `RepositoryIdentity`: externally tagged, snake_case. */
export type RepositoryIdentity =
  | { github: [string, string] }
  | { network: string }
  | { local: string }
  | { remote_directory: { host: string; root: string } };

export interface ProjectRepository {
  id: string;
  github?: string;
  mbx_namespace?: string;
  local?: string;
  destination: string;
  git_ref?: string;
}

export interface ProjectBundle {
  primary_repo: string;
  repositories: ProjectRepository[];
}

export interface ProjectBundleSnapshot {
  bundle: ProjectBundle;
  identities: Record<string, RepositoryIdentity>;
  /** Network clone sources; opaque to the browser. */
  network_sources?: Record<string, JsonValue>;
  plain_directories?: string[];
}

export interface SavedProject {
  bundle_id: string;
  name: string;
  project: ProjectBundleSnapshot;
}

export interface ProjectLocation {
  host: string;
  directory: string;
  checkout_root: string;
  repository_root: string;
  identity: RepositoryIdentity;
  seen_at: string;
}

/** Internally tagged on `state`. */
export type ProjectCatalogStatus =
  | { state: 'ready' }
  | { state: 'refreshing' }
  | { state: 'failed'; errors: string[] };

export interface ProjectCatalogView {
  projects: SavedProject[];
  locations: ProjectLocation[];
  status: ProjectCatalogStatus;
}

/** `POST /api/v1/projects/discover` body; internally tagged on `kind`. */
export type ProjectDiscoveryRequest =
  | { kind: 'github'; query: string }
  | { kind: 'directory'; path: string; filter?: string };

export interface ProjectEntry {
  name: string;
  source: string;
  description: string;
  kind: 'repository' | 'directory';
}

export interface ProjectDiscovery {
  entries: ProjectEntry[];
  directory: string | null;
  parent: string | null;
  truncated: boolean;
}

export type CompletionKind = 'directories' | 'any';

export interface CompletePathRequest {
  target_id?: string;
  prefix: string;
  kind?: CompletionKind;
}

export interface PathCompletion {
  candidates: string[];
  insert?: string;
  truncated: boolean;
}

export type CreateBundleRequest = (
  | { source: string; sources?: undefined }
  | { sources: string[]; source?: undefined }
) & { include_project?: boolean };

export interface CreateBundleResponse {
  bundle_id: string;
  project?: SavedProject;
}

/** `mj-core/src/state.rs` `ManagedWorktreeOptions`. */
export interface ManagedWorktreeOptions {
  available: boolean;
  default_create: boolean;
}

/** `mj-core/src/local_git.rs` `LocalRemoteRepair`. */
export interface LocalRemoteRepair {
  path: string;
  branch: string;
  missing_remote: string;
  replacement_remote: string;
  fetch_url: string;
  push_urls: string[];
}

/** `mj-client/src/daemon.rs` `PreflightRepository`. */
export interface PreflightRepository {
  id: string;
  fetch_url: string;
  default_branch: string;
  push_urls: string[];
}

export interface NewSessionPreflightRequest {
  additional_mounts?: AdditionalMountWire[];
  remote_repairs?: LocalRemoteRepair[];
  workspace_id?: string;
  profile_id: string;
  bundle_id: string;
  target_id: string;
  project_directory?: string | null;
}

export interface PreflightNew {
  project_directory?: string;
  managed_worktree: ManagedWorktreeOptions;
  remote_repairs: LocalRemoteRepair[];
  dirty_repositories: string[];
  remote_repositories: PreflightRepository[];
  local_changes_excluded: boolean;
  review_notice?: string;
}

export interface SubagentOptions {
  models: SessionConfigChoice[];
  efforts: SessionConfigChoice[];
  unavailable: string[];
}

/* -------------------------------------------------------------------------- */
/* Wiki                                                                       */
/* -------------------------------------------------------------------------- */

export interface WikiRow {
  id: string;
  tool: string;
  project: string;
  title: string;
  started: string | null;
  msgs: number;
  preview: string | null;
  archived: boolean;
  native_id: string | null;
  snippet: string | null;
  hel_session_id: string | null;
  target?: string | null;
  profile?: string | null;
  harness?: string | null;
}

export interface WikiStatus {
  state: WikiIndexState;
}

export interface WikiSearchPage {
  rows: WikiRow[];
  status: WikiStatus;
}

export type WikiSyncOutcome = 'succeeded' | { failed: string };

export interface WikiSyncProgress {
  phase: string;
  tool: string;
  adapter: number;
  adapters: number;
  done: number;
  total: number | null;
}

export interface WikiSyncStatus {
  epoch: number;
  version: number;
  requested: number;
  started: number;
  finished: number;
  successful: number;
  outcome: WikiSyncOutcome | null;
  running: WikiSyncProgress | null;
  target: number | null;
}

export interface WikiSessionInfo {
  wiki_id: string;
  tool: string;
  path: string;
  status: WikiSessionStatus;
  mjolnir_session_id?: string | null;
  profile_id?: string | null;
  target_template_id?: string | null;
  harness?: string | null;
  title: string;
  project: string;
  nothing_to_restore?: boolean;
}

export interface WikiBrief {
  markdown: string;
}

export interface WikiHitBlock {
  role: 'user' | 'assistant' | 'tool' | (string & {});
  text: string;
  hits: [number, number][];
  omitted_before: number;
  truncated: boolean;
}

export interface WikiHitTranscript {
  blocks: WikiHitBlock[];
  omitted_after: number;
}

export interface WikiRestoreBody {
  workspace_id?: string;
  profile_id: string;
  target_id: string;
  project_directory?: string;
  model?: string;
  effort?: string;
  additional_mounts?: AdditionalMountWire[];
  resource_allocation?: SessionResourceAllocation;
  include_registration?: boolean;
}

export interface WikiRestoreResponse {
  session_id: string;
  registered?: JsonObject;
}

/* -------------------------------------------------------------------------- */
/* Settings, git identity, push                                               */
/* -------------------------------------------------------------------------- */

export type SettingsControl =
  | { control: 'page'; summary: string }
  | { control: 'checkbox'; checked: boolean }
  | { control: 'choice'; value: JsonValue; choices: { value: JsonValue; label: string }[] }
  | { control: 'text'; value: string; placeholder: string; number: boolean }
  | { control: 'hidden' }
  | { control: 'read_only'; text: string };

export type SettingsRow = { path: string[]; label: string; help?: string } & SettingsControl;

export interface SettingsGroup {
  heading?: string;
  rows: SettingsRow[];
}

export interface SettingsPage {
  path: string[];
  breadcrumb: string[];
  help: string;
  groups: SettingsGroup[];
  actions?: JsonValue[];
  add?: JsonValue;
  removable: boolean;
  notices?: string[];
}

export interface SettingsDocument {
  original: JsonValue;
  draft: JsonValue;
  page: SettingsPage;
}

export interface SettingsPageResponse {
  page: SettingsPage;
  subagent_options: JsonValue;
  reviewer_options: JsonValue;
}

export type SettingsEdit =
  | { kind: 'set'; path: string[]; text?: string; choice?: JsonValue }
  | { kind: 'toggle'; path: string[] }
  | { kind: 'add'; path: string[]; name?: string }
  | { kind: 'remove'; path: string[] }
  | { kind: 'action'; id: string };

export interface SettingsEditResponse {
  draft: JsonValue;
  page: SettingsPage;
  notice?: string;
  error?: string;
}

export interface GitIdentity {
  name: string | null;
  email: string | null;
  per_directory: boolean;
}

export interface PushSubscriptionJson {
  endpoint: string;
  keys: { p256dh: string; auth: string };
  expirationTime?: number | null;
}

export interface PushKey {
  public_key: string;
}

/** `mj-store` `push_device_status`. */
export interface PushDeviceStatus {
  subscribed: boolean;
  error: string | null;
}

export interface AuthSessionStatus {
  signed_in: boolean;
}

/* -------------------------------------------------------------------------- */
/* ControllerAction (POST /api/v1/actions)                                    */
/* -------------------------------------------------------------------------- */

export interface NewSessionAction {
  action: 'new';
  remember_container_size?: boolean;
  create_managed_worktree?: boolean | null;
  at?: string;
  branch?: string;
  base?: string;
  no_later_history?: boolean;
  subagents?: SubagentPolicy | null;
  oracle?: boolean;
  oracle_choice?: SubagentChoice;
  review?: SessionReview | null;
  no_project_memory?: boolean;
  no_github_events?: boolean;
  no_github_credentials?: boolean;
  notifications_ignored?: boolean;
  workspace_id?: string;
  profile_id?: string;
  bundle_id: string;
  target_id: string;
  additional_mounts?: AdditionalMountWire[];
  resource_allocation?: SessionResourceAllocation;
  title?: string | null;
  project_directory?: string | null;
  dirty_ack?: string[];
}

/** `destroy` is deliberately absent: it is `#[serde(skip)]`; use `destroySession`. */
export type ControllerAction =
  | { action: 'push-subscribe'; subscription: PushSubscriptionJson }
  | { action: 'push-renew'; old_endpoint: string; subscription: PushSubscriptionJson }
  | { action: 'push-unsubscribe'; endpoint: string }
  | { action: 'push-test'; endpoint: string }
  | { action: 'dismiss-notification'; session_id: string; target: NotificationDismissal }
  | { action: 'set-session-notifications-ignored'; session_id: string; ignored: boolean }
  | { action: 'record-operator-activity' }
  | { action: 'set-operator-away'; away: boolean }
  | { action: 'turn-control'; session_id: string; command: RelayCommand }
  | NewSessionAction
  | { action: 'rename'; session_id: string; title: string }
  | { action: 'interrupt-turn'; session_id: string }
  | { action: 'set-config'; session_id: string; key: string; value: string }
  | { action: 'set-plan-mode'; session_id: string; active: boolean }
  | { action: 'change-workspace'; session_id: string; workspace_id: string }
  | {
      action: 'set-container-settings';
      session_id: string;
      resource_allocation?: SessionResourceAllocation | null;
      mounts?: AdditionalMountWire[];
    }
  | { action: 'restart'; session_id: string; acknowledge_container_recreation?: boolean }
  | { action: 'upgrade-container'; request: ContainerUpgradeRequest }
  | { action: 'interrupt-all'; session_id: string }
  | { action: 'refresh-quota'; profile_id: string }
  | { action: 'check-availability' }
  | { action: 'refresh-capacity'; target_id: string }
  | {
      action: 'resume';
      session_id: string;
      workspace_id: string;
      profile_id: string;
      target_id: string;
      queue: ResumeQueueDisposition;
      additional_mounts?: AdditionalMountWire[] | null;
      resource_allocation?: SessionResourceAllocation | null;
    }
  | { action: 'move'; request: MoveSessionRequest }
  | { action: 'open'; session_id: string }
  | {
      action: 'prompt';
      command_id?: string;
      session_id: string;
      text: string;
      images?: ViewerPromptImage[];
      files?: FileAttachmentRef[];
    }
  | { action: 'run-shell'; command_id?: string; session_id: string; command: string }
  | { action: 'cancel-shell'; session_id: string; shell_command_id: string }
  | {
      action: 'suspend';
      session_id: string;
      discard_untracked?: UntrackedConsent;
      abort_operations?: OperationConsent;
      without_checkpoint?: boolean;
      confirm_no_checkpoint?: boolean;
      history_only?: boolean;
    }
  | { action: 'cancel'; session_id: string }
  | { action: 'cancel-container-upgrade'; session_id: string }
  | { action: 'start-review'; session_id: string }
  | { action: 'remove-queued-prompt'; session_id: string; queue_id: string }
  | {
      action: 'respond-elicitation';
      session_id: string;
      elicitation_id: string;
      response: ElicitationResponse;
    };

/** Result of `POST /api/v1/actions`: empty 202, or a container preview. */
export type ActionResult = { accepted: true; preview?: undefined } | { accepted: true; preview: ContainerUpgradePreview };

/* -------------------------------------------------------------------------- */
/* Mount normalization                                                        */
/* -------------------------------------------------------------------------- */

/**
 * Read a mount from the wire: `access ?? (read_only ? 'ro' : 'cow')`
 * (`mj-core/src/targets.rs` `AdditionalMountRepr`).
 */
export function normalizeMountAccess(mount: AdditionalMountWire | AdditionalMount): AdditionalMount {
  const access: MountAccess =
    mount.access ?? ((mount as AdditionalMountWire).read_only ? 'ro' : 'cow');
  return { source: mount.source, destination: mount.destination, access };
}

/**
 * Write a mount in the exact wire form. Rust's `AdditionalMountRepr` denies
 * unknown fields and reads `read_only` as the legacy flag, so `ro` and `cow`
 * stay in the legacy shape older daemons understand and only `rw` adds
 * `access`.
 */
export function serializeAdditionalMount(mount: AdditionalMount | AdditionalMountWire): AdditionalMountWire {
  const { source, destination, access } = normalizeMountAccess(mount);
  switch (access) {
    case 'ro':
      return { source, destination, read_only: true };
    case 'cow':
      return { source, destination, read_only: false };
    case 'rw':
      return { source, destination, read_only: false, access: 'rw' };
  }
}

/* -------------------------------------------------------------------------- */
/* Interning                                                                  */
/* -------------------------------------------------------------------------- */

export class UnknownInternedReferenceError extends Error {
  readonly field: string;
  constructor(field: string) {
    super(`Unknown runtime interned reference for ${field}`);
    this.name = 'UnknownInternedReferenceError';
    this.field = field;
  }
}

/**
 * Expand the five `*_ref` fields of a wire row from the interned table.
 * A missing key means the stream is out of step and a fresh snapshot is
 * required, which callers detect by the error class.
 */
export function resolveSessionInterned(
  row: WireSessionRow,
  interned: InternedTable | ReadonlyMap<string, JsonValue>,
): SessionRow {
  const lookup = (key: string): JsonValue | undefined =>
    interned instanceof Map ? interned.get(key) : (interned as InternedTable)[key];
  const resolved: Record<string, unknown> = { ...row };
  for (const field of INTERNED_FIELDS) {
    const refField = `${field}_ref`;
    if (!Object.hasOwn(resolved, refField)) continue;
    const key = resolved[refField];
    const value = typeof key === 'string' ? lookup(key) : undefined;
    if (value === undefined) throw new UnknownInternedReferenceError(field);
    if (Object.hasOwn(resolved, field)) {
      throw new Error(`Runtime row contains both ${field} and ${refField}`);
    }
    resolved[field] = value;
    delete resolved[refField];
  }
  return resolved as unknown as SessionRow;
}

export type CostUsdTicks = { Ok: number } | { Err: null };
export interface UsageReportQuery { profile?: string; hour_start_ms?: number; hour_starts_ms?: number[]; include_suspended?: boolean }
export interface UsageCoverage { recorded_turns: number; full_turn_reports: number; last_request_reports: number; unspecified_reports: number; missing_reports: number; unfinished_turns?: number; provider_cost_turns?: number; estimated_cost_turns?: number; missing_cost_turns?: number }
export interface ModelUsageEstimate { model: string | null; totals: Record<string, { tokens: number; reported_turns: number }>; unreported_turns?: number; cost_usd_ticks: CostUsdTicks }
export interface ProfileRoleUsageEstimate { profile: string | null; role: string; models: ModelUsageEstimate[]; cost_usd_ticks: CostUsdTicks }
export interface SessionUsageReport {
  since_ms: number; until_ms: number;
  hours: { start_ms: number; known_cost_usd_ticks: number; has_unknown_cost: boolean }[];
  profiles: string[];
  profile_providers?: Record<string, string>;
  sessions: { session_id: string; title: string; profile: string; suspended: boolean; subagent_count: number; models: ModelUsageEstimate[]; coverage: UsageCoverage; cost_usd_ticks: CostUsdTicks }[];
  agent_usage?: { role: string; models: ModelUsageEstimate[]; cost_usd_ticks: CostUsdTicks }[];
  profile_usage?: { profile: string | null; coverage: UsageCoverage; cost_usd_ticks: CostUsdTicks }[];
  profile_role_usage?: ProfileRoleUsageEstimate[];
  role_changes?: { id: number; changed_at_ms: number; summary: string }[];
  quota_usage?: { profile: string; consumed_basis_points: number | null; covered_seconds?: number; partial: boolean; monthly_price_usd: number | null }[];
  quota_projections?: { profile_id: string; window_label: string; is_short_window?: boolean; is_weekly_window?: boolean; remaining_percent: number; resets_at_epoch_seconds: number | null; observed_pct_per_hour: number | null; sustainable_pct_per_hour: number | null; pace_ratio: number | null; projected_exhaustion_epoch_seconds: number | null }[];
  cost_usd_ticks: CostUsdTicks;
}
