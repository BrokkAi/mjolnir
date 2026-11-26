// One owner for each session's draft writes.
//
// The composer reports every edit here; nothing else calls `putDraft`. Per
// session at most one PUT is in flight, and the next write is always the
// latest text, so the server's final draft is the reader's final edit however
// the requests overlap. `lastSaved` moves only after a PUT succeeds: a failed
// or in-flight write is never recorded as saved. A failure keeps the text
// pending and reports it (`getDraftSaveError`); it is not retried in a loop.
// The next edit, flush or submit retries it, and so does the browser coming
// back online. A newer edit that arrived while a write failed is still written.
//
// The queue outlives the composer. Leaving a session flushes its pending text,
// and returning before that write lands still sees the newest local text
// through `getPendingDraft`, never an older server draft.

import { putDraft } from '../../api/client';

export const DRAFT_SAVE_DELAY_MS = 600;

interface SessionDraft {
  timer: ReturnType<typeof setTimeout> | null;
  /** Newest local text not yet confirmed saved; `null` when nothing is outstanding. */
  pendingText: string | null;
  inFlight: boolean;
  /** The last text the server confirmed (or reported); `null` while unknown. */
  lastSaved: string | null;
  /** Why the newest write failed; cleared by the next successful write. */
  error: string | null;
}

const drafts = new Map<string, SessionDraft>();
const errorListeners = new Map<string, Set<() => void>>();
/** Bumped by `clearAllDraftState`; a write that started under an older epoch is discarded when it lands. */
let epoch = 0;

function draftOf(sessionId: string): SessionDraft {
  let draft = drafts.get(sessionId);
  if (!draft) {
    draft = { timer: null, pendingText: null, inFlight: false, lastSaved: null, error: null };
    drafts.set(sessionId, draft);
  }
  return draft;
}

function cancelTimer(draft: SessionDraft): void {
  if (draft.timer !== null) {
    clearTimeout(draft.timer);
    draft.timer = null;
  }
}

function notifyError(sessionId: string): void {
  for (const listener of errorListeners.get(sessionId) ?? []) listener();
}

function setError(sessionId: string, draft: SessionDraft, error: string | null): void {
  if (draft.error === error) return;
  draft.error = error;
  notifyError(sessionId);
}

/** Why the newest draft write failed, or `null` when none is outstanding. */
export function getDraftSaveError(sessionId: string): string | null {
  return drafts.get(sessionId)?.error ?? null;
}

/** Call `listener` when the session's save error appears or clears. Returns the unsubscribe. */
export function subscribeDraftSaveError(sessionId: string, listener: () => void): () => void {
  let listeners = errorListeners.get(sessionId);
  if (!listeners) {
    listeners = new Set();
    errorListeners.set(sessionId, listeners);
  }
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0 && errorListeners.get(sessionId) === listeners) errorListeners.delete(sessionId);
  };
}

/** Record the draft the server holds, unless a newer local write is outstanding. */
export function noteServerDraft(sessionId: string, serverDraft: string): void {
  const draft = draftOf(sessionId);
  if (draft.pendingText === null && !draft.inFlight) draft.lastSaved = serverDraft;
}

/** The newest local text the server may not have yet, or `null`. */
export function getPendingDraft(sessionId: string): string | null {
  return drafts.get(sessionId)?.pendingText ?? null;
}

async function drain(sessionId: string): Promise<void> {
  const draft = draftOf(sessionId);
  if (draft.inFlight) return; // the running loop picks up the newest text
  draft.inFlight = true;
  const writeEpoch = epoch;
  try {
    while (draft.pendingText !== null && draft.pendingText !== draft.lastSaved) {
      const text = draft.pendingText;
      try {
        await putDraft(sessionId, text);
      } catch (error) {
        // The viewer signed out while this write was in flight: nothing is recorded.
        if (epoch !== writeEpoch) return;
        // A newer edit arrived meanwhile: write it next. Otherwise keep this text
        // pending and report it; `online`, a flush or the next edit retries.
        if (draft.pendingText !== text) continue;
        setError(sessionId, draft, error instanceof Error ? error.message : String(error));
        return;
      }
      // The viewer signed out while this write was in flight: it is not recorded and no follow-up goes out.
      if (epoch !== writeEpoch) return;
      setError(sessionId, draft, null);
      draft.lastSaved = text;
      if (draft.pendingText === text) draft.pendingText = null;
    }
    if (draft.pendingText === draft.lastSaved) draft.pendingText = null;
  } finally {
    draft.inFlight = false;
  }
}

/** Save `text` after a quiet period; a newer call replaces it. */
export function scheduleDraftSave(sessionId: string, text: string, delayMs = DRAFT_SAVE_DELAY_MS): void {
  const draft = draftOf(sessionId);
  cancelTimer(draft);
  draft.pendingText = text === draft.lastSaved && !draft.inFlight ? null : text;
  if (draft.pendingText === null) return;
  draft.timer = setTimeout(() => {
    draft.timer = null;
    void drain(sessionId);
  }, delayMs);
}

/** Write any unsaved text now (leaving the composer). */
export function flushDraftSave(sessionId: string): void {
  const draft = drafts.get(sessionId);
  if (!draft) return;
  cancelTimer(draft);
  if (draft.pendingText !== null) void drain(sessionId);
}

/** Write `text` now, after any request already in flight (prompt submit clears the draft). */
export function writeDraftNow(sessionId: string, text: string): void {
  const draft = draftOf(sessionId);
  cancelTimer(draft);
  draft.pendingText = text;
  void drain(sessionId);
}

/** Drop every session's draft state: nothing typed by one viewer may reach the next (sign-out). */
export function clearAllDraftState(): void {
  epoch += 1;
  const sessionIds = [...drafts.keys()];
  for (const draft of drafts.values()) cancelTimer(draft);
  drafts.clear();
  for (const sessionId of sessionIds) notifyError(sessionId);
}

// Drafts whose write failed go out again when the browser regains its connection.
// `clearAllDraftState` empties `drafts`, so a signed-out viewer's text is never retried.
if (typeof window !== 'undefined') {
  window.addEventListener('online', () => {
    for (const [sessionId, draft] of drafts) {
      if (draft.error !== null && draft.pendingText !== null) void drain(sessionId);
    }
  });
}
