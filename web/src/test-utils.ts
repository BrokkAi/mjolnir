import { vi } from 'vitest';
import type {
  BrowserTranscript,
  BrowserTranscriptEntry,
  RuntimeCursor,
  SnapshotMetadata,
  ViewerSession,
  ViewerSnapshotWire,
  WireSessionRow,
} from './api/types';

/** A live detail row with the fields the stores read. */
export function liveRow(id: string, overrides: Partial<ViewerSession> = {}): ViewerSession {
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
    workspace_id: 'w1',
    title: `Session ${id}`,
    harness_kind: 'claude',
    profile_id: 'p1',
    bundle_id: 'b1',
    target_id: 't1',
    state: 'running',
    active: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
    has_error: false,
    has_checkpoint: false,
    conversation_available: true,
    prompt_images_supported: false,
    display_location: 'local',
    lifecycle: 'live',
    transitioning: false,
    verdict: {
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
      change_workspace: 'hidden',
      container_settings: 'hidden',
      resume: 'hidden',
      working_children: 0,
      unfinished_children: 0,
    },
    latest_event_ordinal: 0,
    chat_phase: 'idle',
    is_idle: true,
    capabilities: {
      open: true,
      prompt: true,
      run_shell: true,
      interrupt_turn: false,
      cancel_operation: false,
      suspend: true,
      destroy: true,
      rename: true,
      resume: false,
      set_config: true,
      set_plan_mode: true,
    },
    ...overrides,
  };
}

export function summaryRow(id: string): WireSessionRow {
  const { id: _id, ...rest } = liveRow(id, { lifecycle: 'suspended' });
  return {
    id,
    title: rest.title,
    workspace_id: 'w1',
    lifecycle: 'suspended',
    verdict: rest.verdict,
    capabilities: { ...rest.capabilities, open: false },
    state: 'parked',
    detail: false,
    queued_prompt_count: 0,
    pending_elicitation_count: 0,
    pending_elicitation_ids: [],
  } as unknown as WireSessionRow;
}

export const cursor = (sequence: number, incarnation = 'inc'): RuntimeCursor => ({ incarnation, sequence });

export function metadata(overrides: Partial<SnapshotMetadata> = {}): SnapshotMetadata {
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
    generated_at: '2026-01-01T00:00:00Z',
    server_time_ms: 1,
    profiles: [],
    targets: [],
    bundles: [],
    review_config: { trigger: 'off' },
    ...overrides,
  };
}

export function snapshot(
  sessions: WireSessionRow[],
  options: { sequence?: number; interned?: ViewerSnapshotWire['interned']; meta?: Partial<SnapshotMetadata> } = {},
): ViewerSnapshotWire {
  return {
    ...metadata(options.meta),
    sessions,
    cursor: cursor(options.sequence ?? 1),
    interned: options.interned ?? {},
  };
}

export function entry(id: number, overrides: Partial<BrowserTranscriptEntry> = {}): BrowserTranscriptEntry {
  return {
    id,
    updated_seq: id,
    role: 'agent',
    label: 'Agent',
    recorded_at_ms: null,
    lines: [`line ${id}`],
    glyph: '•',
    tone: 'agent',
    ...overrides,
  };
}

export function transcript(overrides: Partial<BrowserTranscript> = {}): BrowserTranscript {
  return {
    latest_seq: 3,
    presentation_key: 'k',
    window_start_seq: 1,
    reset: false,
    entries: [entry(1), entry(2), entry(3)],
    ...overrides,
  };
}

export function jsonResponse(body: unknown, init: ResponseInit = {}): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { 'content-type': 'application/json' },
    ...init,
  });
}

/** Resolve pending promises and timers scheduled with zero delay. */
export async function flush(times = 5): Promise<void> {
  for (let i = 0; i < times; i += 1) await new Promise(resolve => setTimeout(resolve, 0));
}

type Handler = (url: string, init?: RequestInit) => Response | Promise<Response>;

/** Replace `fetch` with a router on the path (without query). Unknown paths fail the test. */
export function mockFetch(routes: Record<string, Handler>) {
  const calls: { url: string; init?: RequestInit }[] = [];
  const impl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    calls.push({ url, init });
    const path = url.split('?')[0];
    const handler = routes[path];
    if (!handler) throw new Error(`Unexpected fetch: ${url}`);
    return handler(url, init);
  });
  vi.stubGlobal('fetch', impl);
  return { calls, impl, urls: () => calls.map(call => call.url) };
}

type Listener = (event: { data?: string }) => void;

/** Minimal EventSource that tests drive by hand. */
export class FakeEventSource {
  static instances: FakeEventSource[] = [];
  readonly url: string;
  closed = false;
  private listeners = new Map<string, Listener[]>();

  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }

  addEventListener(type: string, listener: Listener) {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
  }

  close() {
    this.closed = true;
  }

  emit(type: string, data?: unknown) {
    for (const listener of this.listeners.get(type) ?? []) {
      listener({ data: data === undefined ? undefined : JSON.stringify(data) });
    }
  }

  static reset() {
    FakeEventSource.instances = [];
  }

  static get last(): FakeEventSource {
    return FakeEventSource.instances[FakeEventSource.instances.length - 1];
  }
}
