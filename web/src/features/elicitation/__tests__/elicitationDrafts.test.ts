import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useSessionStore } from '../../../stores/useSessionStore';
import {
  clearAllElicitationDrafts,
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

describe('elicitation drafts', () => {
  it('sign-out clears stored and in-memory drafts', () => {
    saveElicitationDraft('s1', 'e1', { values: { token: 'hunter2' }, step: 0 });
    vi.spyOn(Storage.prototype, 'setItem').mockImplementationOnce(() => {
      throw new DOMException('full', 'QuotaExceededError');
    });
    saveElicitationDraft('s1', 'e2', { values: { token: 'swordfish' }, step: 1 });
    expect(loadElicitationDraft('s1', 'e1')).not.toBeNull();
    expect(loadElicitationDraft('s1', 'e2')).not.toBeNull();

    useSessionStore.getState().handleSignedOut();

    expect(loadElicitationDraft('s1', 'e1')).toBeNull();
    expect(loadElicitationDraft('s1', 'e2')).toBeNull();
    expect(sessionStorage.length).toBe(0);
  });

  it('prefers the in-memory draft when storage refused the newer write', () => {
    saveElicitationDraft('s1', 'e1', { values: { name: 'old' }, step: 0 });
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new DOMException('full', 'QuotaExceededError');
    });
    saveElicitationDraft('s1', 'e1', { values: { name: 'new' }, step: 2 });

    expect(loadElicitationDraft('s1', 'e1')).toEqual({ values: { name: 'new' }, step: 2 });

    // Once storage works again, the stored copy is the newer one.
    vi.restoreAllMocks();
    saveElicitationDraft('s1', 'e1', { values: { name: 'newest' }, step: 3 });
    expect(loadElicitationDraft('s1', 'e1')).toEqual({ values: { name: 'newest' }, step: 3 });
  });
});
