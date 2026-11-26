// Auth, the runtime snapshot, and its live stream.
//
// Boot is deliberately thin: one `GET /api/v1/snapshot` answers both "am I
// signed in" (401 means no) and "what is the state". It does not call
// `/auth/session` or `/api/v1/options`. When the initial route is a
// conversation, that conversation's fetch starts in parallel with the
// snapshot, since neither needs the other.
//
// Rows live in `sessionsById`. A summary row (`detail: false`) is replaced by
// a detail row when `ensureSessionDetail` fetched it and nothing changed the
// row meanwhile. A later stream change (a new summary row or a `detail_changed`
// notice) never takes a detail row away: the row stays in place, marked in
// `staleDetailIds`, and the next detail fetch replaces it. A mounted
// conversation therefore keeps its scroll, focus, attachments and open
// elicitations while its detail refreshes.

import { create } from 'zustand';
import {
  ApiError,
  configureApi,
  dispatchAction as postAction,
  getSessionRow,
  getSnapshot,
  login,
  logout,
} from '../api/client';
import { ViewerEventStream, type StreamStatus } from '../api/events';
import {
  resolveSessionInterned,
  type ActionResult,
  type ControllerAction,
  type ConversationVersion,
  type InternedTable,
  type RuntimeCursor,
  type SessionRow,
  type SessionSummary,
  type SnapshotMetadata,
  type ViewerFrame,
  type ViewerSnapshotWire,
} from '../api/types';
import { parseRoute, useUiStore } from './useUiStore';
import { useTranscriptStore } from './useTranscriptStore';
import { clearAllDraftState } from '../features/conversation/draftQueue';
import { clearAllElicitationDrafts } from '../features/elicitation/elicitationDrafts';

export type AuthState = 'checking' | 'signed_out' | 'signed_in';
/** `upgrading` overrides the stream status while the daemon hands off. */
export type ConnectionState = StreamStatus | 'upgrading';

const sameCursor = (a: RuntimeCursor | null, b: RuntimeCursor | null): boolean =>
  Boolean(a && b && a.incarnation === b.incarnation && a.sequence === b.sequence);

interface SessionState {
  auth: AuthState;
  streamStatus: StreamStatus;
  upgrading: boolean;
  bootError: string | null;
  meta: SnapshotMetadata | null;
  sessionsById: Record<string, SessionRow>;
  /** Sessions whose detail row is older than the stream; a fetch replaces it in place. */
  staleDetailIds: Record<string, true>;
  /** Bumped by every row change and `detail_changed`, so an in-flight detail fetch can tell it is stale. */
  detailEpochs: Record<string, number>;
  detailLoadingIds: Record<string, true>;
  detailErrors: Record<string, string>;
  interned: InternedTable;
  cursor: RuntimeCursor | null;
  /** Latest server transcript version per session, from stream frames and conversation fetches. */
  conversationVersions: Record<string, ConversationVersion>;

  boot: () => Promise<void>;
  signIn: (code: string) => Promise<void>;
  signOut: () => Promise<void>;
  /** Install a snapshot and start the stream. Public so tests and re-sync can use it. */
  applySnapshot: (snapshot: ViewerSnapshotWire, versions?: Record<string, ConversationVersion>) => void;
  /** Apply a stream frame. Returns whether state changed. Throws when the frame cannot apply. */
  applyFrame: (frame: ViewerFrame) => boolean;
  /** Fetch detail for a summary row or a stale detail row; resolves with the fresh row. */
  ensureSessionDetail: (id: string) => Promise<SessionRow | undefined>;
  dispatchAction: (action: ControllerAction) => Promise<ActionResult>;
  /** Record a conversation version seen outside the stream; never lowers a known `latest_seq`. */
  noteConversationVersion: (id: string, version: ConversationVersion) => void;
  handleSignedOut: () => void;
}

let stream: ViewerEventStream | undefined;
const detailRequests = new Map<string, Promise<void>>();
let bootGeneration = 0;

function stopStream(): void {
  stream?.stop();
  stream = undefined;
}

export const useSessionStore = create<SessionState>((set, get) => ({
  auth: 'checking',
  streamStatus: 'idle',
  upgrading: false,
  bootError: null,
  meta: null,
  sessionsById: {},
  staleDetailIds: {},
  detailEpochs: {},
  detailLoadingIds: {},
  detailErrors: {},
  interned: {},
  cursor: null,
  conversationVersions: {},

  boot: async () => {
    const generation = ++bootGeneration;
    configureApi({
      onUnauthorized: () => get().handleSignedOut(),
      onUpgrade: pending => set({ upgrading: pending }),
    });
    set({ auth: 'checking', bootError: null });
    // A conversation route starts its fetch now; the snapshot is not needed to ask for it.
    const route = parseRoute(location.hash);
    if (route.name === 'conversation') {
      void useTranscriptStore.getState().openConversation(route.sessionId);
    }
    try {
      const snapshot = await getSnapshot();
      if (generation !== bootGeneration) return;
      get().applySnapshot(snapshot);
      set({ auth: 'signed_in' });
      startStream();
    } catch (error) {
      if (generation !== bootGeneration) return;
      if (error instanceof ApiError && error.status === 401) return; // `handleSignedOut` already ran
      set({ auth: 'checking', bootError: error instanceof Error ? error.message : String(error) });
      useUiStore.getState().pushToast('error', `Could not load Mjolnir: ${describe(error)}`);
    }
  },

  signIn: async code => {
    await login(code);
    await get().boot();
  },

  signOut: async () => {
    try {
      await logout();
    } catch (error) {
      useUiStore.getState().pushToast('error', `Could not sign out: ${describe(error)}`);
      return;
    }
    get().handleSignedOut();
  },

  handleSignedOut: () => {
    bootGeneration += 1;
    stopStream();
    detailRequests.clear();
    // Nothing from the previous viewer survives a sign-out in this tab.
    useTranscriptStore.getState().clearAll();
    clearAllDraftState();
    clearAllElicitationDrafts();
    set({
      auth: 'signed_out',
      streamStatus: 'idle',
      upgrading: false,
      bootError: null,
      meta: null,
      sessionsById: {},
      staleDetailIds: {},
      detailEpochs: {},
      detailLoadingIds: {},
      detailErrors: {},
      interned: {},
      cursor: null,
      conversationVersions: {},
    });
  },

  applySnapshot: (snapshot, versions) => {
    const interned = snapshot.interned ?? {};
    const rows: Record<string, SessionRow> = {};
    for (const wire of snapshot.sessions) {
      const row = resolveSessionInterned(wire, interned);
      rows[row.id] = row;
    }
    const { sessions: _sessions, cursor, interned: _interned, ...meta } = snapshot;
    set(state => ({
      meta: meta as SnapshotMetadata,
      sessionsById: rows,
      staleDetailIds: {},
      detailEpochs: Object.fromEntries(Object.keys(rows).map(id => [id, (state.detailEpochs[id] ?? 0) + 1])),
      interned,
      cursor,
      // A snapshot frame lists every session; the REST snapshot lists none, so keep what is known.
      conversationVersions: versions ? { ...versions } : state.conversationVersions,
    }));
  },

  applyFrame: frame => {
    if (frame.kind === 'reset_required') {
      // The snapshot that follows on the same stream replaces everything.
      set({ cursor: null });
      return false;
    }
    if (frame.kind === 'snapshot') {
      get().applySnapshot(frame.snapshot, frame.conversation_versions ?? {});
      set({ cursor: frame.cursor });
      return true;
    }
    if (frame.kind !== 'delta') throw new Error('Unknown runtime publication');
    const state = get();
    if (sameCursor(state.cursor, frame.cursor)) {
      // A transcript-only frame advances conversation versions without moving the runtime
      // cursor (from == cursor). Any other repeat of the current cursor is a duplicate.
      const versions = Object.entries(frame.conversation_versions ?? {});
      if (versions.length === 0 || !sameCursor(state.cursor, frame.from)) return false;
      set(current => {
        const conversationVersions = { ...current.conversationVersions };
        for (const [id, version] of versions) {
          if (current.sessionsById[id]) conversationVersions[id] = version;
        }
        return { conversationVersions };
      });
      return true;
    }
    if (!sameCursor(state.cursor, frame.from)) throw new Error('Runtime cursor gap');
    if (
      !frame.cursor ||
      frame.cursor.incarnation !== frame.from.incarnation ||
      frame.cursor.sequence <= frame.from.sequence
    ) {
      throw new Error('Invalid runtime cursor');
    }
    const interned: InternedTable = { ...state.interned, ...(frame.interned ?? {}) };
    // Resolve every row first: an unknown reference must leave state untouched.
    const changes = frame.sessions.map(([id, row]) => {
      if (row && row.id !== id) throw new Error('Invalid runtime session changes');
      return [id, row ? resolveSessionInterned(row, interned) : null] as const;
    });
    set(current => {
      const sessionsById = { ...current.sessionsById };
      const staleDetailIds = { ...current.staleDetailIds };
      const detailEpochs = { ...current.detailEpochs };
      const conversationVersions = { ...current.conversationVersions };
      for (const [id, row] of changes) {
        detailEpochs[id] = (detailEpochs[id] ?? 0) + 1;
        const held = sessionsById[id];
        if (!row) {
          delete sessionsById[id];
          delete staleDetailIds[id];
          delete conversationVersions[id];
        } else if (held && held.detail !== false && row.detail === false) {
          // Keep the mounted detail row; the summary's fields are newer, the rest refreshes.
          sessionsById[id] = overlaySummary(held, row);
          staleDetailIds[id] = true;
        } else {
          sessionsById[id] = row;
          delete staleDetailIds[id];
        }
      }
      for (const id of frame.detail_changed ?? []) {
        detailEpochs[id] = (detailEpochs[id] ?? 0) + 1;
        const held = sessionsById[id];
        if (held && held.detail !== false) staleDetailIds[id] = true;
      }
      for (const [id, version] of Object.entries(frame.conversation_versions ?? {})) {
        if (sessionsById[id]) conversationVersions[id] = version;
      }
      const { sessions: _s, cursor: _c, interned: _i, ...metadata } = frame.metadata as Record<string, unknown>;
      return {
        sessionsById,
        staleDetailIds,
        detailEpochs,
        conversationVersions,
        interned,
        meta: { ...(current.meta as SnapshotMetadata), ...(metadata as Partial<SnapshotMetadata>) },
        cursor: frame.cursor,
      };
    });
    return true;
  },

  ensureSessionDetail: async id => {
    for (;;) {
      const current = get().sessionsById[id];
      if (!current || (current.detail !== false && !get().staleDetailIds[id])) return current;
      const active = detailRequests.get(id);
      if (active) {
        await active;
        continue;
      }
      const epoch = get().detailEpochs[id] ?? 0;
      const request = (async () => {
        set(state => ({ detailLoadingIds: { ...state.detailLoadingIds, [id]: true } }));
        try {
          const result = await getSessionRow(id);
          if (!result?.row || result.row.id !== id) {
            throw new Error('The server returned an invalid session detail row');
          }
          set(state => {
            // The row changed or was invalidated while fetching: this answer is stale.
            if (!state.sessionsById[id] || (state.detailEpochs[id] ?? 0) !== epoch) return state;
            const { [id]: _error, ...detailErrors } = state.detailErrors;
            const { [id]: _stale, ...staleDetailIds } = state.staleDetailIds;
            return {
              sessionsById: { ...state.sessionsById, [id]: result.row },
              staleDetailIds,
              detailErrors,
            };
          });
        } catch (error) {
          if ((get().detailEpochs[id] ?? 0) !== epoch) return;
          set(state => ({ detailErrors: { ...state.detailErrors, [id]: describe(error) } }));
          throw error;
        } finally {
          set(state => {
            const { [id]: _done, ...detailLoadingIds } = state.detailLoadingIds;
            return { detailLoadingIds };
          });
        }
      })();
      detailRequests.set(id, request);
      try {
        await request;
      } finally {
        if (detailRequests.get(id) === request) detailRequests.delete(id);
      }
    }
  },

  dispatchAction: action => postAction(action),

  noteConversationVersion: (id, version) =>
    set(state => {
      const known = state.conversationVersions[id];
      // The event stream owns this fact; a fetch only seeds it when absent or
      // strictly ahead, so an in-flight response never replaces a newer key
      // announced at the same sequence.
      if (known && known.latest_seq >= version.latest_seq) return state;
      return { conversationVersions: { ...state.conversationVersions, [id]: version } };
    }),
}));

/** A summary row's fields laid over the detail row it must not unmount. */
function overlaySummary(held: SessionRow, summary: SessionSummary): SessionRow {
  const {
    detail: _detail,
    queued_prompt_count: _queued,
    pending_elicitation_count: _pending,
    pending_elicitation_ids: _ids,
    ...fields
  } = summary;
  return { ...held, ...fields } as SessionRow;
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function startStream(): void {
  stopStream();
  stream = new ViewerEventStream({
    getCursor: () => useSessionStore.getState().cursor,
    onStatus: status => useSessionStore.setState({ streamStatus: status }),
    onFrame: frame => {
      useSessionStore.getState().applyFrame(frame);
    },
  });
  stream.start();
}

/** The connection the shell shows: an upgrade handoff outranks the stream's own status. */
export function selectConnection(state: Pick<SessionState, 'upgrading' | 'streamStatus'>): ConnectionState {
  return state.upgrading ? 'upgrading' : state.streamStatus;
}
