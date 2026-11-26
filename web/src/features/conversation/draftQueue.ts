// One owner for each session's draft writes.
//
// The composer reports every edit here; nothing else calls `putDraft`. Per
// session at most one PUT is in flight, and the next write is always the
// latest text, so the server's final draft is the reader's final edit however
// the requests overlap. `lastSaved` moves only after a PUT succeeds: a failed
// or in-flight write is never recorded as saved, and a failure is not retried
// in a loop (the next edit, flush or submit retries it).
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
}

const drafts = new Map<string, SessionDraft>();

function draftOf(sessionId: string): SessionDraft {
  let draft = drafts.get(sessionId);
  if (!draft) {
    draft = { timer: null, pendingText: null, inFlight: false, lastSaved: null };
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
  try {
    while (draft.pendingText !== null && draft.pendingText !== draft.lastSaved) {
      const text = draft.pendingText;
      try {
        await putDraft(sessionId, text);
      } catch {
        // Left pending and unsaved; a lost draft must not block the composer.
        return;
      }
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

export function resetDraftQueueForTests(): void {
  for (const draft of drafts.values()) cancelTimer(draft);
  drafts.clear();
}
