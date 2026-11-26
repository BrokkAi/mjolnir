// Draft answers survive a collapsed sheet, a dropped connection and a remount.
//
// One key per (session, request): `mj-elicit:${sessionId}:${reqId}`. Values are
// plain JSON, never the request itself, so a changed form simply gets its
// still-valid answers merged back by `mergeDraftValues`.
//
// `memory` always holds the whole draft, secret fields included, for a peek or
// reopen in this tab. `sessionStorage` gets the draft without its secret
// fields, so a typed secret never reaches browser storage.

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
  // `memory` is written on every save, so it is the newer and fuller copy.
  const held = memory.get(key);
  if (held) return held;
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
  secretFieldIds: ReadonlySet<string>,
): void {
  const key = elicitationDraftKey(sessionId, requestId);
  memory.set(key, draft);
  const store = storage();
  if (!store) return;
  const values: DraftValues = {};
  for (const [id, value] of Object.entries(draft.values)) {
    if (!secretFieldIds.has(id)) values[id] = value;
  }
  try {
    store.setItem(key, JSON.stringify({ values, step: draft.step }));
  } catch {
    // Private mode or a full quota: the in-memory copy still serves this tab.
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
