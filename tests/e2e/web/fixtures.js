// Fixture builders for the deterministic specs: wire-shaped snapshots, live
// session rows, transcripts and elicitation requests. Everything passes
// through lab-env's `viewerWireSnapshot`, the same interning/windowing
// converter the legacy lab specs used, so the mock server speaks exactly the
// dialect the real daemon speaks.

const { viewerWireSnapshot } = require('./lab-env');

const INCARNATION = 'e2e';

function cursor(sequence = 1) {
  return { incarnation: INCARNATION, sequence };
}

/** The five capabilities blocks every detail row needs, all grant-able. */
function capabilities(overrides = {}) {
  return {
    open: true,
    prompt: true,
    run_shell: true,
    interrupt_turn: true,
    cancel_operation: true,
    suspend: true,
    destroy: true,
    rename: true,
    resume: true,
    set_config: true,
    set_plan_mode: true,
    clear_context: true,
    read_history: true,
    dismiss_notification: true,
    move_session: true,
    change_workspace: true,
    container_settings: true,
    restart: true,
    interrupt_all: true,
    ...overrides,
  };
}

function verdict(overrides = {}) {
  return {
    transition: null,
    live_dashboard: true,
    needs_person: false,
    move_queue_held: false,
    idle: true,
    working: false,
    active_work: false,
    unfinished: false,
    interrupt: false,
    interrupt_member: false,
    interrupt_all: false,
    interrupt_count: 0,
    stop: 'ready',
    restart: 'ready',
    move_session: 'ready',
    change_workspace: 'ready',
    container_settings: 'ready',
    resume: 'active',
    working_children: 0,
    unfinished_children: 0,
    ...overrides,
  };
}

/** A full-detail live session row (ViewerSession). */
function liveSession(id, overrides = {}) {
  return {
    attention: 'idle',
    notification_dismissal: null,
    notification_card: null,
    oracle: false,
    review_trigger: 'off',
    review_count: 0,
    subagents: { mode: 'native' },
    targeted_turn_control_supported: false,
    retry_assessment_pending: false,
    id,
    title: overrides.title || `Session ${id}`,
    workspace_id: 'w1',
    bundle_id: 'b1',
    harness_kind: 'fake',
    profile_id: 'p1',
    target_id: 'localhost',
    state: 'ready',
    active: true,
    created_at: '2026-11-25T10:00:00Z',
    updated_at: '2026-11-25T10:05:00Z',
    has_error: false,
    has_checkpoint: true,
    display_location: 'localhost',
    lifecycle: 'live',
    transitioning: false,
    verdict: verdict(),
    latest_event_ordinal: 1,
    chat_phase: 'idle',
    is_idle: true,
    capabilities: capabilities(),
    conversation_available: true,
    prompt_images_supported: true,
    pending_elicitations: [],
    queued_prompts: [],
    available_commands: [],
    config_options: [],
    plan_mode_active: false,
    ...overrides,
    // Nested objects merge field-by-field so one flag does not drop the rest.
    ...(overrides.verdict ? { verdict: verdict(overrides.verdict) } : {}),
    ...(overrides.capabilities ? { capabilities: capabilities(overrides.capabilities) } : {}),
  };
}

/** A summary row (`detail: false`), the shape suspended sessions arrive in. */
function summarySession(id, overrides = {}) {
  const full = liveSession(id, overrides);
  return {
    detail: false,
    queued_prompt_count: overrides.queued_prompt_count ?? 0,
    pending_elicitation_count: overrides.pending_elicitation_count ?? 0,
    pending_elicitation_ids: overrides.pending_elicitation_ids ?? [],
    ...Object.fromEntries(
      Object.entries(full).filter(([key]) =>
        [
          'attention', 'notification_dismissal', 'notification_card', 'unread_frontier',
          'id', 'workspace_id', 'bundle_id', 'title', 'subagent_parent_id',
          'subagent_session_ids', 'reviewer_session_ids', 'publication_state', 'profile_id',
          'target_id', 'state', 'container_upgrade_supported', 'created_at', 'updated_at',
          'has_error', 'configuration_issue', 'launch_error', 'storage_problem',
          'project_label', 'display_location', 'lifecycle', 'transitioning', 'verdict',
          'last_activity_at_ms', 'last_message_at_ms', 'capacity_retry',
          'retry_assessment_pending', 'quota_recovery', 'operation', 'move_recovery',
          'chat_phase', 'is_idle', 'active', 'activity_details', 'activity', 'capabilities',
        ].includes(key),
      ),
    ),
  };
}

function profile(id = 'p1', overrides = {}) {
  return {
    id,
    availability: { status: 'available' },
    capabilities_key: `caps-${id}`,
    harness_kind: 'fake',
    quota_pools: [],
    ...overrides,
  };
}

function target(id = 'localhost', overrides = {}) {
  return {
    id,
    kind: 'local',
    yolo: false,
    subagents_configured: false,
    oracle_configured: false,
    resource_allocation_kind: 'fixed',
    requires_project_directory: false,
    availability: { status: 'available' },
    recent_project_directories: [],
    ...overrides,
  };
}

/** Snapshot metadata with one workspace, one profile and one target. */
function metadata(overrides = {}) {
  return {
    operator_away: false,
    auto_afk_minutes: 0,
    profile_capabilities: { profiles: {} },
    last_subagent_policies: {},
    last_oracle_choices: {},
    oracle_configured: false,
    orchestrators_configured: false,
    subagents_configured: false,
    revision: 1,
    generated_at: '2026-11-25T12:00:00Z',
    server_time_ms: Date.parse('2026-11-25T12:00:00Z'),
    server_version: '0.0.0-e2e',
    workspaces: [{ id: 'w1', name: 'Main workspace', last_opened_at: '2026-11-25T10:00:00Z' }],
    profiles: [profile()],
    targets: [target()],
    bundles: [],
    review_config: { trigger: 'off' },
    capacity: [],
    launch_failures: [],
    notices: [],
    ...overrides,
  };
}

/**
 * A wire snapshot exactly as `GET /api/v1/snapshot` serves it: sessions
 * interned and windowed by lab-env's converter.
 */
function snapshot({ sessions = [], meta = {}, sequence = 1 } = {}) {
  return viewerWireSnapshot({
    ...metadata(meta),
    revision: sequence,
    cursor: cursor(sequence),
    sessions,
  });
}

function transcriptEntry(id, overrides = {}) {
  return {
    id,
    updated_seq: id,
    role: 'agent',
    label: 'Assistant',
    recorded_at_ms: Date.parse('2026-11-25T11:00:00Z') + id * 1000,
    lines: [`Line ${id}`],
    glyph: '•',
    tone: 'normal',
    stable_id: `stable-${id}`,
    ...overrides,
  };
}

function transcript({ entries = [], latestSeq, presentationKey = 'pk-1', reset = true } = {}) {
  const latest = latestSeq ?? entries.reduce((max, entry) => Math.max(max, entry.updated_seq), 0);
  return {
    latest_seq: latest,
    presentation_key: presentationKey,
    window_start_seq: entries.length ? entries[0].id : 1,
    reset,
    entries,
  };
}

/** A tall text block, for transcripts that overflow a phone viewport. */
function tallLines(count, prefix = 'row') {
  return Array.from({ length: count }, (_, index) => `${prefix} ${index + 1}: some streamed text that wraps`);
}

module.exports = {
  INCARNATION,
  cursor,
  capabilities,
  verdict,
  liveSession,
  summarySession,
  profile,
  target,
  metadata,
  snapshot,
  transcriptEntry,
  transcript,
  tallLines,
};
