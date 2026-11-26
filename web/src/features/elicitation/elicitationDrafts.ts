// Draft answers survive a collapsed sheet, a dropped connection and a remount.
//
// One key per (session, request): `mj-elicit:${sessionId}:${reqId}`. Values are
// plain JSON, never the request itself, so a changed form simply gets its
// still-valid answers merged back by `mergeDraftValues`.

import type { DraftValues } from './elicitationLogic';

export interface StoredElicitationDraft {
  values: DraftValues;
  step: number;
}

export const DRAFT_KEY_PREFIX = 'mj-elicit:';

const memory = new Map<string, StoredElicitationDraft>();

export function elicitationDraftKey(sessionId: string, requestId: string): string {
  return `${DRAFT_KEY_PREFIX}${sessionId}:${requestId}`;
}

function storage(): Storage | null {
  try {
    return typeof sessionStorage === 'undefined' ? null : sessionStorage;
  } catch {
    return null;
  }
}

function sanitize(value: unknown): StoredElicitationDraft | null {
  if (!value || typeof value !== 'object') return null;
  const record = value as { values?: unknown; step?: unknown };
  if (!record.values || typeof record.values !== 'object') return null;
  const values: DraftValues = {};
  for (const [id, item] of Object.entries(record.values as Record<string, unknown>)) {
    if (typeof item === 'string' || typeof item === 'boolean') {
      values[id] = item;
    } else if (Array.isArray(item)) {
      const list = item.filter((entry): entry is string => typeof entry === 'string');
      if (list.length) values[id] = list;
    }
  }
  const step = typeof record.step === 'number' && Number.isInteger(record.step) && record.step >= 0
    ? record.step
    : 0;
  return { values, step };
}

export function loadElicitationDraft(
  sessionId: string,
  requestId: string,
): StoredElicitationDraft | null {
  const key = elicitationDraftKey(sessionId, requestId);
  // `memory` holds only drafts that storage could not take, so it is always the newer copy.
  const fallback = memory.get(key);
  if (fallback) return fallback;
  const store = storage();
  if (!store) return null;
  try {
    const raw = store.getItem(key);
    if (!raw) return null;
    return sanitize(JSON.parse(raw));
  } catch {
    return null;
  }
}

export function saveElicitationDraft(
  sessionId: string,
  requestId: string,
  draft: StoredElicitationDraft,
): void {
  const key = elicitationDraftKey(sessionId, requestId);
  const store = storage();
  if (!store) {
    memory.set(key, draft);
    return;
  }
  try {
    store.setItem(key, JSON.stringify(draft));
    memory.delete(key);
  } catch {
    // Private mode or a full quota: the answer still works for this render.
    memory.set(key, draft);
  }
}

export function clearElicitationDraft(sessionId: string, requestId: string): void {
  const key = elicitationDraftKey(sessionId, requestId);
  memory.delete(key);
  const store = storage();
  try {
    store?.removeItem(key);
  } catch {
    // Nothing to clear.
  }
}

/** Drop every draft, secret fields included; nothing outlives a sign-out. */
export function clearAllElicitationDrafts(): void {
  memory.clear();
  const store = storage();
  if (!store) return;
  try {
    const keys: string[] = [];
    for (let index = 0; index < store.length; index += 1) {
      const key = store.key(index);
      if (key?.startsWith(DRAFT_KEY_PREFIX)) keys.push(key);
    }
    for (const key of keys) store.removeItem(key);
  } catch {
    // Storage is unavailable, so there is nothing stored to clear.
  }
}

/** Drop drafts for requests this session no longer has pending. */
export function pruneElicitationDrafts(sessionId: string, keepRequestIds: readonly string[]): void {
  const keep = new Set(keepRequestIds);
  const prefix = `${DRAFT_KEY_PREFIX}${sessionId}:`;
  const store = storage();
  if (store) {
    try {
      const stale: string[] = [];
      for (let index = 0; index < store.length; index += 1) {
        const key = store.key(index);
        if (!key?.startsWith(prefix)) continue;
        const requestId = key.slice(prefix.length);
        if (!keep.has(requestId)) stale.push(key);
      }
      for (const key of stale) store.removeItem(key);
    } catch {
      // A storage failure must not break answering.
    }
  }
  for (const key of [...memory.keys()]) {
    if (!key.startsWith(prefix)) continue;
    if (!keep.has(key.slice(prefix.length))) memory.delete(key);
  }
}
