import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useSessionStore } from '../../../stores/useSessionStore';
import {
  clearAllElicitationDrafts,
  elicitationDraftKey,
  loadElicitationDraft,
  saveElicitationDraft,
} from '../elicitationDrafts';

beforeEach(() => {
  sessionStorage.clear();
  clearAllElicitationDrafts();
});

afterEach(() => {
  vi.restoreAllMocks();
  sessionStorage.clear();
  clearAllElicitationDrafts();
});

const NO_SECRETS: ReadonlySet<string> = new Set();

describe('elicitation drafts', () => {
  it('sign-out clears stored and in-memory drafts', () => {
    saveElicitationDraft('s1', 'e1', { values: { token: 'hunter2', name: 'a' }, step: 0 }, new Set(['token']));
    saveElicitationDraft('s1', 'e2', { values: { name: 'b' }, step: 1 }, NO_SECRETS);
    expect(loadElicitationDraft('s1', 'e1')).not.toBeNull();
    expect(loadElicitationDraft('s1', 'e2')).not.toBeNull();

    useSessionStore.getState().handleSignedOut();

    expect(loadElicitationDraft('s1', 'e1')).toBeNull();
    expect(loadElicitationDraft('s1', 'e2')).toBeNull();
    expect(sessionStorage.length).toBe(0);
  });

  it('keeps secret answers in memory only: never in sessionStorage, gone after sign-out', () => {
    const secrets = new Set(['token']);
    saveElicitationDraft('s1', 'e1', { values: { token: 'hunter2', name: 'ada' }, step: 1 }, secrets);

    // In this tab a peek or reopen finds the whole draft.
    expect(loadElicitationDraft('s1', 'e1')).toEqual({ values: { token: 'hunter2', name: 'ada' }, step: 1 });

    // Browser storage holds the non-secret fields and nothing of the secret.
    const stored = sessionStorage.getItem(elicitationDraftKey('s1', 'e1')) ?? '';
    expect(JSON.parse(stored)).toEqual({ values: { name: 'ada' }, step: 1 });
    for (let index = 0; index < sessionStorage.length; index += 1) {
      const raw = sessionStorage.getItem(sessionStorage.key(index) as string) ?? '';
      expect(raw).not.toContain('hunter2');
    }

    // A fresh tab (no memory) restores the non-secret part only.
    clearMemoryOnly();
    expect(loadElicitationDraft('s1', 'e1')).toEqual({ values: { name: 'ada' }, step: 1 });

    saveElicitationDraft('s1', 'e1', { values: { token: 'hunter2', name: 'ada' }, step: 1 }, secrets);
    useSessionStore.getState().handleSignedOut();
    expect(loadElicitationDraft('s1', 'e1')).toBeNull();
  });

  it('serves the in-memory draft when storage refuses the write', () => {
    saveElicitationDraft('s1', 'e1', { values: { name: 'old' }, step: 0 }, NO_SECRETS);
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new DOMException('full', 'QuotaExceededError');
    });
    saveElicitationDraft('s1', 'e1', { values: { name: 'new' }, step: 2 }, NO_SECRETS);

    expect(loadElicitationDraft('s1', 'e1')).toEqual({ values: { name: 'new' }, step: 2 });
  });
});

/** Simulates a reload: sessionStorage survives, the module's memory does not. */
function clearMemoryOnly(): void {
  const saved = Object.fromEntries(
    Array.from({ length: sessionStorage.length }, (_, index) => {
      const key = sessionStorage.key(index) as string;
      return [key, sessionStorage.getItem(key) as string];
    }),
  );
  clearAllElicitationDrafts();
  for (const [key, value] of Object.entries(saved)) sessionStorage.setItem(key, value);
}
