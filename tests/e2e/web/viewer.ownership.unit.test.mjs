import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../../../mj-controller/src/web/viewer.js', import.meta.url), 'utf8');
function extract(from, to) {
  const start = source.indexOf(from);
  assert.notEqual(start, -1);
  const end = source.indexOf(to, start);
  assert.notEqual(end, -1);
  return source.slice(start, end);
}
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function conversationHarness() {
  const requests = [], rendered = [];
  const context = vm.createContext({
    currentSession: 'A', conversationGeneration: 1, conversationRequest: null,
    cursor: 0, presentationKey: null, acknowledged: 0,
    AbortController, URLSearchParams,
    sessionById: id => ({ id, capabilities: { open: true } }),
    isTransitioningSession: () => false, isLoadingConversationSession: () => false,
    renderEntries: entries => rendered.push(entries),
    document: { querySelector: () => ({}) },
    request: (url, options) => {
      const response = deferred();
      requests.push({ url, options, ...response });
      return response.promise;
    },
  });
  vm.runInContext(extract('async function loadConversation(', '\nasync function openConversation('), context);
  return { context, requests, rendered, load: () => vm.runInContext('loadConversation(false)', context) };
}

test('navigation starts B while A is pending and A cannot consume B reload', async () => {
  const h = conversationHarness();
  const a = h.load();
  h.context.currentSession = 'B'; h.context.conversationGeneration++;
  const b = h.load();
  assert.equal(h.requests.length, 2);
  assert.equal(h.requests[0].options.signal.aborted, true);
  await h.load(); // A second B publication coalesces under B's operation.
  h.requests[0].resolve({ entries: ['old'], latest_seq: 0 });
  await a;
  assert.equal(h.context.conversationRequest.pending, true);
  h.requests[1].resolve({ entries: ['new'], latest_seq: 0 });
  await b;
  assert.equal(h.requests.length, 3);
  assert.equal(h.requests[2].url, '/api/conversations/B');
  assert.deepEqual(h.rendered, [['new']]);
  h.requests[2].resolve({ entries: ['latest'], latest_seq: 0 });
});

test('draft writers drain newest edits and clears independently per session', async () => {
  const requests = [];
  const context = vm.createContext({
    currentSession: 'A', draftTimer: null, draftComposerBaseline: null, composerGeneration: 0, draft: 'old', clearTimeout,
    composerText: () => context.draft,
    document: { querySelector: () => ({}) },
    request: (url, options) => {
      const response = deferred();
      requests.push({ url, draft: JSON.parse(options.body).draft, ...response });
      return response.promise;
    },
  });
  vm.runInContext(extract('// A session owns its desired draft', '\n/// Put back'), context);
  const first = vm.runInContext('saveDraft()', context);
  context.composerGeneration++; context.draft = 'new'; vm.runInContext('saveDraft()', context);
  context.composerGeneration++; context.draft = ''; vm.runInContext('saveDraft()', context); // submitted prompt
  context.composerGeneration++; context.currentSession = 'B'; context.draft = 'other';
  const second = vm.runInContext('saveDraft()', context);
  assert.deepEqual(requests.map(x => x.draft), ['old', 'other']);
  requests[0].resolve(); await new Promise(setImmediate);
  assert.deepEqual(requests.map(x => x.draft), ['old', 'other', '']);
  assert.equal(requests[2].url, '/api/sessions/A/draft');
  requests[1].resolve(); requests[2].resolve();
  await Promise.all([first, second]);
});

test('history results and failures belong to the current search operation', async () => {
  const requests = [], error = { textContent: '' };
  const context = vm.createContext({
    currentSession: 'A', conversationGeneration: 1, historyOpen: true, historyRequest: null,
    AbortController, paletteMatches: [], paletteSelected: 0,
    commandPalette: { replaceChildren() {}, classList: { remove() {} } },
    el: () => ({}), document: { querySelector: () => error },
    request: (_url, options) => {
      const response = deferred(); requests.push({ ...response, options }); return response.promise;
    },
  });
  vm.runInContext(extract('async function searchHistory(', '\n// ---------------------------------------------------------------------------'), context);
  const a = vm.runInContext("searchHistory('a')", context);
  const b = vm.runInContext("searchHistory('ab')", context);
  assert.equal(requests[0].options.signal.aborted, true);
  requests[1].resolve({ entries: [] }); await b;
  requests[0].resolve({ entries: ['obsolete'] }); await a;
  assert.equal(context.paletteMatches.length, 0);
  const old = vm.runInContext("searchHistory('abc')", context);
  context.conversationGeneration++;
  requests[2].reject(new Error('old failure')); await old;
  assert.equal(error.textContent, '');
});

test('navigation captures outgoing text before clearing the composer', async () => {
  const requests = [];
  const context = vm.createContext({
    currentSession: 'A', draftTimer: null, draftComposerBaseline: 0, composerGeneration: 1,
    draft: 'leaving draft', conversationGeneration: 1, conversationMode: 'conversation',
    cursor: 0, presentationKey: null, acknowledged: 0, clearTimeout,
    composerText: () => context.draft,
    setComposerText: text => { context.draft = text; context.composerGeneration++; },
    cancelVoiceInput() {}, retireConversationRequest() {}, clearConversationContents() {}, clearPromptImages() {},
    request: async (url, options) => requests.push({ url, draft: JSON.parse(options.body).draft }),
  });
  vm.runInContext(extract('// A session owns its desired draft', '\n/// Put back'), context);
  vm.runInContext(extract('function leaveConversation()', "\ndocument.querySelector('#login-form')"), context);
  vm.runInContext('leaveConversation()', context);
  assert.deepEqual(requests, [{ url: '/api/sessions/A/draft', draft: 'leaving draft' }]);
  assert.equal(context.currentSession, null);
  assert.equal(context.draft, '');
});

test('unrestored draft is not cleared on navigation and late restore respects an explicit clear', async () => {
  const response = deferred(); let saves = 0;
  const context = vm.createContext({
    currentSession: 'A', draftTimer: null, draftComposerBaseline: 0, composerGeneration: 0,
    draft: '', conversationGeneration: 1, acknowledged: 0, clearTimeout,
    composerText: () => context.draft,
    setComposerText: text => { context.draft = text; context.composerGeneration++; },
    updateCommandPalette() {}, request: (_url, options) => {
      if (options?.method === 'PUT') { saves++; return Promise.resolve(); }
      return response.promise;
    },
  });
  vm.runInContext(extract('// A session owns its desired draft', '\nlet historyOpen'), context);
  const restore = vm.runInContext("restoreDraft('A', 1)", context);
  await vm.runInContext('saveDraft()', context);
  assert.equal(saves, 0, 'navigation before restore does not overwrite an unknown draft');
  vm.runInContext("setComposerText('typed'); setComposerText('')", context);
  response.resolve({ draft: 'old saved draft', through_event_ordinal: 0 });
  await restore;
  assert.equal(context.draft, '', 'late restore cannot reverse the user clear');
});
