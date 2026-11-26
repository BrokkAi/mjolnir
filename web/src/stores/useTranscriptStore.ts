// Per-session transcript cache.
//
// Opening a conversation shows whatever is cached at once. A request goes out
// only when the server's latest sequence for that session (from the runtime
// stream) is unknown or ahead of the cache, and then only for what changed:
// `GET /conversations/{id}?after_seq=N&presentation_key=K`. Returning to an
// unchanged cached session sends nothing, and a stream delta about another
// session sends nothing.
//
// One owner decides whether a session needs a fetch: `needsSync`. Every
// trigger (open, a stream delta, a transition ending) asks it, and at most one
// request per session is in flight; triggers that arrive meanwhile coalesce
// into one follow-up.

import { create } from 'zustand';
import { ApiError, getConversation, getHistory } from '../api/client';
import type {
  BrowserTranscript,
  BrowserTranscriptEntry,
  HistoryEntry,
  SessionRow,
  TranscriptCursor,
} from '../api/types';
import { useSessionStore } from './useSessionStore';

/** Sessions kept in memory. A phone holds a handful of open conversations; this bounds memory, not requests. */
export const TRANSCRIPT_CACHE_CAPACITY = 16;

export interface PendingLocalPrompt {
  commandId: string;
  text: string;
  createdAt: number;
}

export type TranscriptBlock = 'transitioning' | 'unavailable';

export interface CachedTranscript {
  id: string;
  entriesById: Record<string, BrowserTranscriptEntry>;
  /** Keys of `entriesById` in display order. */
  orderedIds: string[];
  latestSeq: number;
  presentationKey: string | null;
  windowStartSeq: number;
  /** Stored messages older than the live window, ascending by position. */
  olderItems: HistoryEntry[];
  /** Where the next older page starts; `null` until the first page is read. */
  historyCursor: TranscriptCursor | null;
  hasMoreHistory: boolean;
  hasLoaded: boolean;
  loading: boolean;
  loadingOlder: boolean;
  error: string | null;
  /** The server refused for a reason that ends on its own; a later stream change retries. */
  blocked: TranscriptBlock | null;
  pendingLocalPrompts: PendingLocalPrompt[];
}

/** A row's stable identity: its durable id, else its start position. */
export function entryKey(entry: Pick<BrowserTranscriptEntry, 'stable_id' | 'id'>): string {
  return entry.stable_id ?? `ordinal:${entry.id}`;
}

export function emptyTranscript(id: string): CachedTranscript {
  return {
    id,
    entriesById: {},
    orderedIds: [],
    latestSeq: 0,
    presentationKey: null,
    windowStartSeq: 0,
    olderItems: [],
    historyCursor: null,
    hasMoreHistory: true,
    hasLoaded: false,
    loading: false,
    loadingOlder: false,
    error: null,
    blocked: null,
    pendingLocalPrompts: [],
  };
}

/** Fold one `GET /conversations/{id}` answer into the cache. Pure. */
export function applyTranscriptResponse(
  cached: CachedTranscript,
  response: BrowserTranscript,
): CachedTranscript {
  const replace = response.reset || !cached.hasLoaded;
  const entriesById: Record<string, BrowserTranscriptEntry> = replace ? {} : { ...cached.entriesById };
  for (const entry of response.entries) {
    const key = entryKey(entry);
    const existing = entriesById[key];
    if (!existing || entry.updated_seq >= existing.updated_seq) entriesById[key] = entry;
  }
  const orderedIds = Object.keys(entriesById).sort(
    (a, b) => entriesById[a].id - entriesById[b].id || a.localeCompare(b),
  );
  const answered = new Set(response.entries.map(entry => entry.command_id).filter(Boolean));
  return {
    ...cached,
    entriesById,
    orderedIds,
    latestSeq: response.latest_seq,
    presentationKey: response.presentation_key,
    windowStartSeq: response.window_start_seq,
    hasLoaded: true,
    loading: false,
    error: null,
    blocked: null,
    pendingLocalPrompts: cached.pendingLocalPrompts.filter(prompt => !answered.has(prompt.commandId)),
  };
}

/** Position of the oldest live row; stored messages at or after it are already in the feed. */
export function oldestLivePosition(cached: CachedTranscript): number | null {
  const first = cached.orderedIds[0];
  return first === undefined ? null : cached.entriesById[first].id;
}

/** Older stored messages that are not also live rows. */
export function visibleOlderItems(cached: CachedTranscript): HistoryEntry[] {
  const oldest = oldestLivePosition(cached);
  return oldest === null ? cached.olderItems : cached.olderItems.filter(item => item.position < oldest);
}

function isReadable(row: SessionRow | undefined): boolean {
  return Boolean(row && row.capabilities.open && !row.verdict.transition);
}

/**
 * A stopped sub-agent or reviewer: the daemon serves its stored history but no
 * live conversation (`read_history` without `open`).
 */
export function isHistoryOnly(row: SessionRow | undefined): boolean {
  return Boolean(row && row.capabilities.read_history && !row.capabilities.open);
}

interface TranscriptState {
  entries: Record<string, CachedTranscript>;
  /** Least recently used first. */
  order: string[];
  activeId: string | null;

  openConversation: (id: string) => Promise<void>;
  closeConversation: () => void;
  syncConversation: (id: string, options?: { force?: boolean }) => Promise<void>;
  loadEarlierHistory: (id: string) => Promise<void>;
  addOptimisticPrompt: (id: string, commandId: string, text: string) => void;
  removePendingPrompt: (id: string, commandId: string) => void;
  invalidateSession: (id: string) => void;
  clearAll: () => void;
}

const inflight = new Map<string, Promise<void>>();
const queued = new Map<string, { force: boolean }>();
/** Bumped when a session's cache is dropped, so an answer for the old cache is discarded. */
const epochs = new Map<string, number>();
const epochOf = (id: string) => epochs.get(id) ?? 0;

function needsSync(cached: CachedTranscript | undefined, serverSeq: number | undefined): boolean {
  if (!cached) return false;
  return !cached.hasLoaded || serverSeq === undefined || serverSeq > cached.latestSeq;
}

export const useTranscriptStore = create<TranscriptState>((set, get) => {
  const patch = (id: string, change: Partial<CachedTranscript> | ((c: CachedTranscript) => CachedTranscript)) =>
    set(state => {
      const current = state.entries[id];
      if (!current) return state;
      const next = typeof change === 'function' ? change(current) : { ...current, ...change };
      return { entries: { ...state.entries, [id]: next } };
    });

  const touch = (id: string) =>
    set(state => {
      const order = [...state.order.filter(other => other !== id), id];
      const entries = { ...state.entries };
      while (order.length > TRANSCRIPT_CACHE_CAPACITY) {
        const evict = order.findIndex(other => other !== state.activeId && other !== id);
        if (evict < 0) break;
        const [gone] = order.splice(evict, 1);
        delete entries[gone];
        epochs.set(gone, epochOf(gone) + 1);
      }
      return { order, entries };
    });

  /** Show a stopped session's stored history as its whole transcript. */
  async function loadStoredHistory(id: string, epoch: number): Promise<boolean> {
    try {
      const page = await getHistory(id);
      if (epochOf(id) !== epoch) return true;
      patch(id, current => ({
        ...current,
        olderItems: page.items,
        historyCursor: page.before,
        hasMoreHistory: page.before !== null,
        hasLoaded: true,
        loading: false,
        error: null,
        blocked: null,
      }));
      return true;
    } catch (error) {
      if (epochOf(id) !== epoch) return true;
      patch(id, { loading: false, error: error instanceof Error ? error.message : String(error) });
      return !(error instanceof ApiError && error.status === 401);
    }
  }

  async function fetchOnce(id: string): Promise<boolean> {
    const cached = get().entries[id];
    if (!cached) return true;
    const epoch = epochOf(id);
    patch(id, { loading: true, error: null });
    if (!cached.hasLoaded && isHistoryOnly(useSessionStore.getState().sessionsById[id])) {
      return loadStoredHistory(id, epoch);
    }
    try {
      const response = await getConversation(
        id,
        cached.hasLoaded
          ? { afterSeq: cached.latestSeq, presentationKey: cached.presentationKey ?? undefined }
          : {},
      );
      if (epochOf(id) !== epoch) return true;
      patch(id, current => applyTranscriptResponse(current, response));
      // After the cache holds this sequence, so recording it triggers no second fetch.
      useSessionStore.getState().noteConversationSeq(id, response.latest_seq);
      return true;
    } catch (error) {
      if (epochOf(id) !== epoch) return true;
      if (error instanceof ApiError && error.status === 401) {
        patch(id, { loading: false });
        return false;
      }
      if (error instanceof ApiError && error.status === 409) {
        patch(id, { loading: false, blocked: 'transitioning', error: null });
        return true;
      }
      if (error instanceof ApiError && error.status === 404) {
        // No live conversation, but the daemon may still hold stored history for it.
        if (!cached.hasLoaded && useSessionStore.getState().sessionsById[id]?.capabilities.read_history) {
          return loadStoredHistory(id, epoch);
        }
        patch(id, { loading: false, blocked: 'unavailable', error: null });
        return true;
      }
      patch(id, { loading: false, error: error instanceof Error ? error.message : String(error) });
      return false;
    }
  }

  return {
    entries: {},
    order: [],
    activeId: null,

    openConversation: id => {
      set({ activeId: id });
      if (!get().entries[id]) set(state => ({ entries: { ...state.entries, [id]: emptyTranscript(id) } }));
      touch(id);
      return get().syncConversation(id);
    },

    closeConversation: () => set({ activeId: null }),

    syncConversation: (id, options = {}) => {
      const force = options.force ?? false;
      const running = inflight.get(id);
      if (running) {
        const pending = queued.get(id);
        queued.set(id, { force: force || (pending?.force ?? false) });
        return running;
      }
      const cached = get().entries[id];
      if (!cached) return Promise.resolve();
      const row = useSessionStore.getState().sessionsById[id];
      // Stored history of a stopped session never changes: read it once.
      if (!force && isHistoryOnly(row) && cached.hasLoaded) return Promise.resolve();
      if (!force && !needsSync(cached, useSessionStore.getState().conversationSeqs[id])) {
        return Promise.resolve();
      }
      const run = (async () => {
        let ok: boolean;
        try {
          ok = await fetchOnce(id);
        } finally {
          inflight.delete(id);
        }
        const again = queued.get(id);
        queued.delete(id);
        // A failed fetch is not retried by itself: the next stream change or an explicit retry is.
        if (!ok || !again) return;
        const latest = get().entries[id];
        if (again.force || needsSync(latest, useSessionStore.getState().conversationSeqs[id])) {
          await get().syncConversation(id, { force: again.force });
        }
      })();
      inflight.set(id, run);
      return run;
    },

    loadEarlierHistory: async id => {
      const cached = get().entries[id];
      if (!cached || cached.loadingOlder || !cached.hasMoreHistory) return;
      const epoch = epochOf(id);
      const oldest = oldestLivePosition(cached);
      const before = cached.historyCursor ?? (oldest === null ? null : { position: oldest, stable_id: '' });
      patch(id, { loadingOlder: true, error: null });
      try {
        const page = await getHistory(
          id,
          before ? { beforePosition: before.position, beforeId: before.stable_id } : undefined,
        );
        if (epochOf(id) !== epoch) return;
        patch(id, current => {
          const known = new Set(current.olderItems.map(item => item.stable_id));
          const olderItems = [...current.olderItems, ...page.items.filter(item => !known.has(item.stable_id))].sort(
            (a, b) => a.position - b.position || a.stable_id.localeCompare(b.stable_id),
          );
          return {
            ...current,
            olderItems,
            historyCursor: page.before,
            hasMoreHistory: page.before !== null,
            loadingOlder: false,
          };
        });
      } catch (error) {
        if (epochOf(id) !== epoch) return;
        patch(id, { loadingOlder: false, error: error instanceof Error ? error.message : String(error) });
      }
    },

    addOptimisticPrompt: (id, commandId, text) =>
      patch(id, current =>
        current.pendingLocalPrompts.some(prompt => prompt.commandId === commandId)
          ? current
          : {
              ...current,
              pendingLocalPrompts: [...current.pendingLocalPrompts, { commandId, text, createdAt: Date.now() }],
            },
      ),

    removePendingPrompt: (id, commandId) =>
      patch(id, current => ({
        ...current,
        pendingLocalPrompts: current.pendingLocalPrompts.filter(prompt => prompt.commandId !== commandId),
      })),

    invalidateSession: id => {
      epochs.set(id, epochOf(id) + 1);
      inflight.delete(id);
      queued.delete(id);
      set(state => {
        const { [id]: _gone, ...entries } = state.entries;
        return { entries, order: state.order.filter(other => other !== id) };
      });
      if (get().activeId === id) void get().openConversation(id);
    },

    clearAll: () => {
      for (const id of get().order) epochs.set(id, epochOf(id) + 1);
      inflight.clear();
      queued.clear();
      set({ entries: {}, order: [], activeId: null });
    },
  };
});

/**
 * Follow the runtime stream for the open conversation. A delta triggers a
 * fetch only when the server's sequence for that conversation moved past the
 * cache, or when the conversation becomes readable again (a transition ended).
 * Returns the unsubscribe function.
 */
export function startTranscriptSync(): () => void {
  return useSessionStore.subscribe((state, previous) => {
    const transcripts = useTranscriptStore.getState();
    const id = transcripts.activeId;
    if (!id) return;
    const cached = transcripts.entries[id];
    if (!cached) return;
    // A transition ended or the conversation appeared. The first snapshot after boot is not
    // such a change: boot already asked for this conversation.
    const row = state.sessionsById[id];
    const before = previous.sessionsById[id];
    if (isReadable(row) && !isReadable(before) && (before !== undefined || cached.blocked !== null)) {
      void transcripts.syncConversation(id, { force: true });
      return;
    }
    const seq = state.conversationSeqs[id];
    if (seq !== previous.conversationSeqs[id] && seq !== undefined && seq > cached.latestSeq) {
      void transcripts.syncConversation(id);
    }
  });
}
