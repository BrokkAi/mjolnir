import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const webRoot = new URL('../../../mj-controller/src/web/', import.meta.url);
const viewerPath = new URL('viewer.js', webRoot);
const htmlPath = new URL('viewer.html', webRoot);
const manifestPath = new URL('manifest.webmanifest', webRoot);
const serviceWorkerPath = new URL('service-worker.js', webRoot);
const viewerCssPath = new URL('viewer.css', webRoot);
const toolOutputPath = new URL('tool-output.js', webRoot);
const viewerSource = readFileSync(viewerPath, 'utf8');
const serviceWorkerSource = readFileSync(serviceWorkerPath, 'utf8');

function sourceBetween(from, to) {
  const start = viewerSource.indexOf(from);
  assert.notEqual(start, -1, `viewer.js is missing ${JSON.stringify(from)}`);
  const end = viewerSource.indexOf(to, start);
  assert.notEqual(end, -1, `viewer.js is missing ${JSON.stringify(to)} after ${from}`);
  return viewerSource.slice(start, end);
}

function makeNode(tag = 'div') {
  return {
    tagName: tag.toUpperCase(),
    className: '',
    textContent: '',
    type: '',
    disabled: false,
    children: [],
    listeners: new Map(),
    append(...children) {
      this.children.push(...children);
    },
    replaceChildren(...children) {
      this.children = children;
    },
    setAttribute() {},
    addEventListener(type, listener) {
      this.listeners.set(type, listener);
    },
    dispatch(type) {
      const listener = this.listeners.get(type);
      assert.ok(listener, `${this.tagName} has no ${type} listener`);
      return listener({ currentTarget: this, target: this });
    },
  };
}

test('conversation deltas carry the last presentation key and accept older responses', async () => {
  const requests = [];
  const renders = [];
  const responses = [
    { entries: [{ id: 1 }], latest_seq: 5, presentation_key: 'key one', reset: false },
    {},
    { entries: [{ id: 2 }], latest_seq: 6, reset: false },
    {},
  ];
  const context = vm.createContext({
    currentSession: 'session/1',
    snapshot: { sessions: [{ id: 'session/1', capabilities: { open: true } }] },
    conversationInFlight: false,
    conversationPending: false,
    conversationGeneration: 0,
    cursor: 0,
    presentationKey: null,
    acknowledged: 0,
    URLSearchParams,
    encodeURIComponent,
    JSON,
    request: async (url, options) => {
      requests.push({ url, options });
      return responses.shift();
    },
    renderEntries: (entries, replace) => renders.push({ entries, replace }),
    isTransitioningSession: () => false,
    isLoadingConversationSession: () => false,
    renderConversationTransition: () => assert.fail('conversation unexpectedly transitioned'),
    showLogin: () => assert.fail('conversation unexpectedly requested login'),
    document: { querySelector: () => ({ textContent: '' }) },
  });
  vm.runInContext(
    sourceBetween('async function loadConversation(', '\nasync function openConversation('),
    context,
  );

  await vm.runInContext('loadConversation(false)', context);
  assert.equal(requests[0].url, '/api/conversations/session%2F1');
  assert.equal(context.presentationKey, 'key one');
  assert.deepEqual(renders[0], { entries: [{ id: 1 }], replace: true });

  await vm.runInContext('loadConversation(true)', context);
  assert.equal(
    requests[2].url,
    '/api/conversations/session%2F1?after_seq=5&presentation_key=key+one',
  );
  assert.equal(context.presentationKey, null, 'missing keys remain compatible with an older server');
  assert.deepEqual(renders[1], { entries: [{ id: 2 }], replace: false });
});

test('session titles stay blue while truly idle and clear blue when activity resumes', () => {
  const classes = new Set();
  const node = {
    textContent: '',
    classList: { toggle: (name, enabled) => enabled ? classes.add(name) : classes.delete(name) },
  };
  const context = vm.createContext({ node, session: { title: 'Test session', is_idle: true } });
  vm.runInContext(sourceBetween('function renderSessionTitle(', '\nfunction renderConversationHeader('), context);
  vm.runInContext('renderSessionTitle(node, session)', context);
  assert.equal(node.textContent, 'Test session');
  assert.ok(classes.has('idle-title'));
  // A read-cursor update does not change the operational idle classification.
  context.session.latest_event_ordinal = 20;
  vm.runInContext('renderSessionTitle(node, session)', context);
  assert.ok(classes.has('idle-title'));
  context.session.is_idle = false;
  vm.runInContext('renderSessionTitle(node, session)', context);
  assert.ok(!classes.has('idle-title'));
  delete context.session.is_idle;
  vm.runInContext('renderSessionTitle(node, session)', context);
  assert.ok(!classes.has('idle-title'), 'unknown activity is not confirmed idle');
});

test('remote tracking is repaired only after confirmation and preflight then continues', async () => {
  const repair = {
    path: '/project', branch: 'main', missing_remote: 'upstream', replacement_remote: 'origin',
    fetch_url: 'https://example.com/repo.git', push_urls: ['ssh://git@example.com/repo.git'],
  };
  for (const approve of [false, true]) {
    const requests = [];
    const context = vm.createContext({
      newDraft: { profileId: 'codex', bundleId: 'project', targetId: 'docker' },
      pendingNewPreflight: null, pendingNewPreflightController: null, AbortController,
      targetIsBare: () => false, selectedWorkspaceId: () => 'workspace', renderNewForm() {},
      request: async (_url, options) => {
        requests.push(JSON.parse(options.body));
        return requests.length === 1 ? { remote_repairs: [repair] } : {
          remote_repositories: [{ id: 'project' }], local_changes_excluded: true,
        };
      },
      confirm: text => {
        assert.match(text, /upstream/);
        assert.match(text, /origin/);
        assert.match(text, /ssh:\/\/git@example.com\/repo.git/);
        return approve;
      },
    });
    vm.runInContext(sourceBetween('async function preflightNew()', '\nasync function advanceNew()'), context);
    assert.equal(await vm.runInContext('preflightNew()', context), approve);
    assert.deepEqual(requests[0].remote_repairs, []);
    assert.equal(requests.length, approve ? 2 : 1);
    if (approve) {
      assert.deepEqual(requests[1].remote_repairs, [repair]);
      assert.equal(context.newDraft.preflighted, true);
    } else {
      assert.notEqual(context.newDraft.preflighted, true);
    }
  }
});

test('project preflight prevents duplicate checks and ignores a cancelled wizard response', async () => {
  let complete;
  let requests = 0;
  const draft = { profileId: 'test', targetId: 'raw', projectDirectory: '/project' };
  const context = vm.createContext({
    newDraft: draft,
    pendingNewPreflight: null,
    pendingNewPreflightController: null,
    AbortController,
    targetIsBare: () => true,
    selectedWorkspaceId: () => 'test',
    renderNewForm: () => {},
    request: () => {
      requests++;
      return new Promise(resolve => { complete = resolve; });
    },
  });
  vm.runInContext(sourceBetween('async function preflightNew()', '\nasync function advanceNew()'), context);
  const pending = vm.runInContext('preflightNew()', context);
  assert.equal(context.pendingNewPreflight, draft);
  assert.equal(await vm.runInContext('preflightNew()', context), false);
  assert.equal(requests, 1);
  context.newDraft = { profileId: 'another wizard' };
  complete({ dirty_repositories: ['old-project'] });
  assert.equal(await pending, false);
  assert.equal(context.newDraft.dirty, undefined);
  assert.equal(context.pendingNewPreflight, null);
  context.request = async () => { throw new Error('invalid project'); };
  assert.equal(await vm.runInContext('preflightNew()', context), false);
  assert.match(context.newDraft.preflightError, /invalid project/);
  assert.equal(context.pendingNewPreflight, null, 'failure releases the checking state');
  assert.equal(context.newDraft.preflighted, false);
});

test('an aborted preflight cannot clear a replacement check for the same draft', async () => {
  const completions = [];
  const draft = { targetId: 'podman' };
  const context = vm.createContext({
    newDraft: draft, pendingNewPreflight: null, pendingNewPreflightController: null,
    AbortController, targetIsBare: () => false, selectedWorkspaceId: () => 'test',
    renderNewForm: () => {},
    request: () => new Promise(resolve => completions.push(resolve)),
  });
  vm.runInContext(sourceBetween('function abortPendingNewPreflight()', '\nfunction freshDraft()'), context);
  vm.runInContext(sourceBetween('async function preflightNew()', '\nasync function advanceNew()'), context);
  const old = vm.runInContext('preflightNew()', context);
  vm.runInContext('abortPendingNewPreflight()', context);
  const current = vm.runInContext('preflightNew()', context);
  completions[0]({ remote_repositories: [{ id: 'old' }] });
  assert.equal(await old, false);
  assert.equal(context.pendingNewPreflight, draft);
  assert.equal(draft.preflighted, false);
  completions[1]({ remote_repositories: [{ id: 'current' }] });
  assert.equal(await current, true);
  assert.equal(draft.remoteRepositories[0].id, 'current');
  assert.equal(context.pendingNewPreflight, null);
});

test('commit refuses unready, pending, or failed preflight even when called directly', async () => {
  for (const state of ['unready', 'pending', 'failed']) {
    const draft = { preflighted: state !== 'unready', preflightError: state === 'failed' ? 'failed' : '' };
    const context = vm.createContext({
      newDraft: draft,
      pendingNewPreflight: state === 'pending' ? draft : null,
      request: () => assert.fail('unready draft cannot launch'),
    });
    vm.runInContext(sourceBetween('async function commitNew()', '\n/// Resume is a workspace-scoped list'), context);
    await vm.runInContext('commitNew()', context);
  }
});

test('entering Review does not wait for project preflight', async () => {
  let complete;
  const draft = { step: 0, bundleId: 'project' };
  const context = vm.createContext({
    newDraft: draft, pendingNewPreflight: null,
    visibleSteps: () => [{ key: 'project' }, { key: 'review' }],
    snapshot: { bundles: [{ id: 'project' }] }, targetIsBare: () => false,
    newError: makeNode(), renderNewForm() {},
    preflightNew: () => new Promise(resolve => { complete = resolve; }),
  });
  vm.runInContext(sourceBetween('async function advanceNew()', '\nasync function commitNew()'), context);
  const pending = vm.runInContext('advanceNew()', context);
  assert.equal(draft.step, 1, 'Review is entered before the response arrives');
  assert.equal(draft.preflighted, false);
  complete(false);
  await pending;
  assert.equal(draft.step, 1, 'failure stays on Review for retry');
});

test('the create payload carries a sub-agent choice only for Claude and Codex', async () => {
  const posted = [];
  const makeContext = (profileId, harnessKind, mjolnirSubagents) => vm.createContext({
    snapshot: { profiles: [{ id: profileId, harness_kind: harnessKind }] },
    newDraft: {
      workspaceId: 'test',
      preflighted: true,
      profileId,
      bundleId: 'bundle',
      targetId: 'container',
      projectDirectory: '',
      title: '',
      worktreeOptions: { available: false, default_create: false },
      createManagedWorktree: false,
      mjolnirSubagents,
    },
    pendingNewPreflight: null,
    targetIsBare: () => false,
    renderNewForm: () => {},
    refresh: async () => {},
    navigate: () => {},
    newError: makeNode(),
    request: async (_path, options) => { posted.push(JSON.parse(options.body)); },
  });

  for (const [kind, choice, expected] of [
    ['claude', true, true],
    ['claude', false, false],
    ['codex', true, true],
    ['grok', true, null],
    ['kimi', false, null],
  ]) {
    const context = makeContext('profile', kind, choice);
    context.newDraft.committing = false;
    vm.runInContext(sourceBetween('/// Only Claude and Codex receive', '\nfunction targetIsBare('), context);
    vm.runInContext(sourceBetween('async function commitNew()', '\n/// Resume is a workspace-scoped list'), context);
    await vm.runInContext('commitNew()', context);
    assert.equal(posted.at(-1).mjolnir_subagents, expected, `${kind} with ${choice}`);
  }
});

test('rolled-back launch errors remain visible only in their workspace and can be dismissed', () => {
  const notices = makeNode();
  const context = vm.createContext({
    snapshot: { sessions: [], launch_failures: [
      { id: 'first', workspace_id: 'test' },
      { id: 'second', workspace_id: 'primary' },
    ] },
    route: { name: 'dashboard' },
    selectedWorkspaceId: () => 'test',
    document: { querySelector: () => notices },
    el: (tag, className, textContent) => Object.assign(makeNode(tag), { className, textContent }),
  });
  vm.runInContext(sourceBetween('const dismissedLaunchFailures =', '\nfunction renderWorkspaces()'), context);
  vm.runInContext('renderLaunchFailures()', context);
  assert.equal(notices.children.length, 1);
  assert.match(notices.children[0].children[0].textContent, /could not be started/);
  notices.children[0].children[1].onclick();
  assert.equal(notices.children.length, 0);
  context.selectedWorkspaceId = () => 'primary';
  vm.runInContext('renderLaunchFailures()', context);
  assert.equal(notices.children.length, 1);
  context.route.name = 'conversation';
  vm.runInContext('renderLaunchFailures()', context);
  assert.equal(notices.children.length, 0);
});

function descendants(root, tag) {
  const wanted = tag.toUpperCase();
  const found = [];
  const visit = node => {
    if (node.tagName === wanted) found.push(node);
    for (const child of node.children || []) visit(child);
  };
  visit(root);
  return found;
}

function turnReviewHarness() {
  const renderSource = sourceBetween(
    'function renderTurnReview(session) {',
    '\nfunction renderElicitations(session) {',
  );
  const context = vm.createContext({ assert, makeNode });
  vm.runInContext(
    `
const reviewHost = makeNode('div');
const document = { createElement: makeNode };
let reviewSignature = null;
const pendingReviewSessions = new Set();
let currentSession = null;
let active = null;
let actionResult = false;
const sent = [];
function el(name, className, textContent) {
  const node = makeNode(name);
  node.className = className || '';
  if (textContent !== undefined) node.textContent = textContent;
  return node;
}
function activeSession() {
  return active;
}
async function sendAction(body) {
  sent.push(body);
  return actionResult;
}
${renderSource}
globalThis.harness = {
  reviewHost,
  sent,
  renderTurnReview,
  setActive(session) {
    active = session;
    currentSession = session?.id || null;
  },
  setActionResult(result) {
    actionResult = result;
  },
};
`,
    context,
  );
  return context.harness;
}

function review() {
  return {
    tier: 'extended',
    status: 'Choose what to do with the findings.',
    roles: [],
    verdict: {
      text: '[P1] a concrete finding',
      allowed: ['forward', 'dismiss', 'cancel'],
    },
  };
}

test('identical reviews in different sessions rebuild actions for the current session', async () => {
  const harness = turnReviewHarness();
  const firstSession = { id: 'session-a', turn_review: review() };
  const secondSession = { id: 'session-b', turn_review: review() };

  harness.setActive(firstSession);
  harness.renderTurnReview(firstSession);
  const firstCard = harness.reviewHost.children[0];

  harness.setActive(secondSession);
  harness.renderTurnReview(secondSession);
  const secondCard = harness.reviewHost.children[0];
  assert.notStrictEqual(secondCard, firstCard, 'the second session reused the first session card');

  const cancel = descendants(secondCard, 'button').find(button => button.textContent === 'Cancel');
  await cancel.dispatch('click');
  assert.deepEqual(JSON.parse(JSON.stringify(harness.sent.at(-1))), {
    action: 'resolve-review',
    session_id: 'session-b',
    resolution: 'cancel',
  });
});

test('a failed review resolution restores every action allowed by the snapshot', async () => {
  const harness = turnReviewHarness();
  const session = { id: 'session-a', turn_review: review() };
  harness.setActive(session);
  harness.renderTurnReview(session);

  const oldCard = harness.reviewHost.children[0];
  const oldButtons = descendants(oldCard, 'button');
  const forward = oldButtons.find(button => button.textContent === 'Forward findings');
  const request = forward.dispatch('click');
  assert.ok(oldButtons.every(button => button.disabled), 'the card accepted a second resolution');
  await request;

  const restoredCard = harness.reviewHost.children[0];
  assert.notStrictEqual(restoredCard, oldCard, 'the failed action left the disabled card mounted');
  assert.ok(
    descendants(restoredCard, 'button').every(button => !button.disabled),
    'an allowed resolution stayed disabled after failure',
  );
});

test('phone review status exactly mirrors the shared status sentences', () => {
  const statusSource = sourceBetween(
    'function reviewStatusLine(review, open) {',
    '\n/// Run a local command, or report that nothing here can.',
  );
  const context = vm.createContext({});
  vm.runInContext(`${statusSource}\nglobalThis.reviewStatusLineForTest = reviewStatusLine;`, context);
  const status = context.reviewStatusLineForTest;

  assert.equal(
    status({ enabled: true, tier: 'extended', profile: 'reviewer' }, false),
    'Reviewing every completed turn with [review] profile "reviewer" (extended tier)',
  );
  assert.equal(
    status({ enabled: true, tier: 'quick' }, false),
    '[review] enabled = true but no profile is named, so nothing can review',
  );
  assert.equal(
    status({ enabled: false, tier: 'quick', profile: 'reviewer' }, false),
    'Automatic review is off; /review reviews one turn with "reviewer" (quick tier)',
  );
  assert.equal(
    status({ enabled: false, tier: 'quick', profile: null }, false),
    'Turn review needs a reviewer: set [review] profile in config.toml',
  );
  assert.equal(
    status({ enabled: false, tier: 'quick', profile: 'reviewer' }, true),
    'Automatic review is off; /review reviews one turn with "reviewer" (quick tier). A review is open now.',
  );
});

test('help labels projected commands by their actual source', () => {
  const helpSource = sourceBetween(
    'function showHelp() {',
    '\n/// The shared `/review status` sentence',
  );
  const context = vm.createContext({ makeNode });
  vm.runInContext(
    `
const feed = makeNode('div');
function el(name, className, textContent) {
  const node = makeNode(name);
  node.className = className || '';
  if (textContent !== undefined) node.textContent = textContent;
  return node;
}
function availableCommands() {
  return [
    { name: 'help', description: 'show help', source: 'mj' },
    { name: 'agent-check', description: 'ask the agent', source: 'agent' },
    { name: 'legacy', description: 'from an older snapshot' },
  ];
}
function scrollToTail() {}
${helpSource}
showHelp();
globalThis.helpText = feed.children[0].children.find(node => node.tagName === 'PRE').textContent;
`,
    context,
  );
  assert.match(context.helpText, /\/help — show help \[mj\]/);
  assert.match(context.helpText, /\/agent-check — ask the agent \[agent\]/);
  assert.match(context.helpText, /\/legacy — from an older snapshot \[mj\]/);
});

test('review action failures return false without leaking errors across sessions', async () => {
  const actionSource = sourceBetween(
    'async function sendAction(body) {',
    '\n/// Guard against sending twice.',
  );
  const context = vm.createContext({});
  vm.runInContext(
    `
let currentSession = 'session-a';
let composerGeneration = 0;
const error = { textContent: '' };
const document = { querySelector() { return error; } };
async function request() { throw new Error('resolution refused'); }
function composerText() { return ''; }
function setComposerText() { throw new Error('a failed action cleared the composer'); }
async function refresh() {}
${actionSource}
globalThis.actionHarness = {
  error,
  sendAction,
  setSession(id) { currentSession = id; },
};
`,
    context,
  );

  assert.equal(
    await context.actionHarness.sendAction({ action: 'resolve-review', session_id: 'session-a' }),
    false,
  );
  assert.equal(context.actionHarness.error.textContent, 'resolution refused');

  context.actionHarness.error.textContent = 'new conversation error';
  context.actionHarness.setSession('session-b');
  assert.equal(
    await context.actionHarness.sendAction({ action: 'resolve-review', session_id: 'session-a' }),
    false,
  );
  assert.equal(context.actionHarness.error.textContent, 'new conversation error');
});

test('viewer chrome and install metadata use Mjolnir branding', () => {
  const html = readFileSync(htmlPath, 'utf8');
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));

  assert.match(html, /<title>Mjolnir<\/title>/);
  assert.match(html, /id="shell-title">MJ<\/h1>/);
  assert.match(html, /aria-label="Mjolnir"/);
  assert.match(html, /<code>mj daemon status<\/code>/);
  assert.doesNotMatch(html, /\bHel\b|\bhel daemon\b/);
  assert.equal(manifest.name, 'Mjolnir');
  assert.equal(manifest.short_name, 'MJ');
  assert.match(viewerSource, /\}\[name\] \|\| 'MJ';/);
});

test('offline shell uses a Mjolnir cache without caching live requests', async () => {
  const listeners = new Map();
  const operations = [];
  let installedCache;
  let fetchImplementation = async request => ({
    ok: true,
    request,
    clone() {
      return { clonedFrom: request.url };
    },
  });
  let cachedFallback = null;
  const caches = {
    async open(name) {
      operations.push(['open', name]);
      return {
        async addAll(paths) {
          operations.push(['addAll', name, [...paths]]);
        },
        async put(request, response) {
          operations.push(['put', name, request.url, response]);
        },
      };
    },
    async keys() {
      return [installedCache, 'hel-v2', 'mjolnir-shell-v0'];
    },
    async delete(name) {
      operations.push(['delete', name]);
      return true;
    },
    async match(request) {
      operations.push(['match', request.url]);
      return cachedFallback;
    },
  };
  const self = {
    location: { origin: 'https://viewer.example' },
    clients: {
      async claim() {
        operations.push(['claim']);
      },
    },
    async skipWaiting() {
      operations.push(['skipWaiting']);
    },
    addEventListener(name, listener) {
      listeners.set(name, listener);
    },
  };
  const context = vm.createContext({
    URL,
    caches,
    self,
    fetch(request) {
      operations.push(['fetch', request.url]);
      return fetchImplementation(request);
    },
  });
  vm.runInContext(serviceWorkerSource, context);

  let lifetime;
  listeners.get('install')({ waitUntil(promise) { lifetime = promise; } });
  await lifetime;
  installedCache = operations[0][1];
  assert.match(installedCache, /^mjolnir-shell-v[1-9]\d*$/);
  assert.deepEqual(operations.slice(0, 3), [
    ['open', installedCache],
    [
      'addAll',
      installedCache,
      [
        '/',
        '/viewer.css',
        '/viewer.js',
        '/voice-worklet.js',
        '/voice-worker.js',
        '/manifest.webmanifest',
        '/icon.svg',
      ],
    ],
    ['skipWaiting'],
  ]);

  operations.length = 0;
  listeners.get('activate')({ waitUntil(promise) { lifetime = promise; } });
  await lifetime;
  assert.deepEqual(operations, [
    ['delete', 'hel-v2'],
    ['delete', 'mjolnir-shell-v0'],
    ['claim'],
  ]);

  function dispatchFetch(pathname) {
    let response;
    listeners.get('fetch')({
      request: { method: 'GET', url: `https://viewer.example${pathname}` },
      respondWith(promise) {
        response = promise;
      },
    });
    return response;
  }

  operations.length = 0;
  assert.equal(dispatchFetch('/api/snapshot'), undefined);
  assert.equal(dispatchFetch('/auth/login'), undefined);
  assert.deepEqual(operations, []);

  const networkResponse = { ok: true, clone: () => ({ cached: true }) };
  fetchImplementation = async () => networkResponse;
  const navigation = dispatchFetch('/session/one');
  assert.ok(navigation, 'navigation was not intercepted');
  assert.strictEqual(await navigation, networkResponse);
  assert.deepEqual(operations.map(operation => operation[0]), ['fetch', 'open', 'put']);
  assert.equal(operations[1][1], installedCache);

  operations.length = 0;
  cachedFallback = { offline: true };
  fetchImplementation = async () => {
    throw new Error('offline');
  };
  assert.strictEqual(await dispatchFetch('/session/two'), cachedFallback);
  assert.deepEqual(operations.map(operation => operation[0]), ['fetch', 'match']);
});

function wikiContext(overrides = {}) {
  const timers = new Map();
  let nextTimerId = 1;
  const context = vm.createContext({
    encodeURIComponent,
    JSON,
    route: { name: 'resume' },
    renders: 0,
    renderResumable() { context.renders += 1; },
    setTimeout: (fn, ms) => {
      const id = nextTimerId;
      nextTimerId += 1;
      timers.set(id, { fn, ms });
      return id;
    },
    clearTimeout: id => timers.delete(id),
    request: async () => ({ rows: [], status: { state: 'ready', topping_up: false } }),
    resumeSearch: { disabled: false, placeholder: '' },
    ...overrides,
  });
  vm.runInContext(
    sourceBetween('const WIKI_SEARCH_DEBOUNCE_MS', '\nfunction wikiDraft('),
    context,
  );
  return {
    context,
    timers,
    fire() {
      assert.equal(timers.size, 1, 'exactly one search was scheduled');
      const [id, timer] = [...timers.entries()][0];
      timers.delete(id);
      return timer.fn();
    },
  };
}

test('wiki search waits out a burst of typing and drops an overtaken answer', async () => {
  const pending = [];
  const urls = [];
  const { context, timers, fire } = wikiContext({
    request: url => {
      urls.push(url);
      return new Promise(resolve => pending.push(resolve));
    },
  });

  vm.runInContext("scheduleWikiSearch('a'); scheduleWikiSearch('ab'); scheduleWikiSearch('abc');", context);
  assert.equal(timers.size, 1, 'each keystroke replaces the pending request');
  assert.equal([...timers.values()][0].ms, 250);
  assert.equal(urls.length, 0, 'nothing is requested while the typing continues');

  const first = fire();
  assert.deepEqual(urls, ['/api/v1/wiki/search?q=abc&limit=50']);

  // A later keystroke starts a second request before the first has answered.
  vm.runInContext("scheduleWikiSearch('zebra')", context);
  const second = fire();
  assert.equal(urls.length, 2);

  pending[1]({ rows: [{ id: 'new' }], status: { state: 'ready', topping_up: false } });
  await second;
  pending[0]({ rows: [{ id: 'stale' }], status: { state: 'ready', topping_up: false } });
  await first;
  assert.deepEqual(
    vm.runInContext('wikiState.rows.map(row => row.id)', context),
    ['new'],
    'the overtaken answer was dropped',
  );
  assert.equal(context.renders, 1, 'only the current request redraws the page');
});

test('a daemon without the wiki routes stops being asked', async () => {
  const { context, timers, fire } = wikiContext({
    request: async () => {
      const failure = new Error('not found');
      failure.status = 404;
      throw failure;
    },
  });
  vm.runInContext("scheduleWikiSearch('anything')", context);
  await fire();
  assert.equal(vm.runInContext('wikiState.unavailable', context), true);
  assert.equal(vm.runInContext('wikiState.rows.length', context), 0);
  assert.equal(vm.runInContext('wikiState.notice', context), '', 'an old daemon is not news');
  assert.equal(
    vm.runInContext('wikiSearchPlaceholder()', context),
    'Search is unavailable',
    'there is nothing to search, so the box says so',
  );
  vm.runInContext("scheduleWikiSearch('more typing')", context);
  assert.equal(timers.size, 0, 'an index that is not there is not asked again');
});

test('a building index closes the search box and is asked again every five seconds', async () => {
  let status = { state: 'indexing', topping_up: true };
  const { context, timers, fire } = wikiContext({
    request: async () => ({ rows: [], status }),
  });

  vm.runInContext("scheduleWikiSearch('')", context);
  await fire();
  assert.equal(vm.runInContext('wikiSearchPlaceholder()', context), 'Indexing…');
  vm.runInContext('renderWikiSearchBox()', context);
  assert.equal(context.resumeSearch.disabled, true);
  assert.equal(context.resumeSearch.placeholder, 'Indexing…');
  assert.equal(timers.size, 1, 'the page asks again by itself');
  assert.equal([...timers.values()][0].ms, 5000);

  status = { state: 'ready', topping_up: false };
  await fire();
  assert.equal(vm.runInContext('wikiSearchPlaceholder()', context), null);
  vm.runInContext('renderWikiSearchBox()', context);
  assert.equal(context.resumeSearch.disabled, false, 'the box opens without a reload');
  assert.equal(timers.size, 0, 'a ready, idle index is not polled');
});

test('an index at another version is reported and never polled', async () => {
  const { context, timers, fire } = wikiContext({
    request: async () => ({ rows: [], status: { state: 'version_mismatch', topping_up: false } }),
  });
  vm.runInContext("scheduleWikiSearch('')", context);
  await fire();
  assert.equal(
    vm.runInContext('wikiSearchPlaceholder()', context),
    'SessionWiki index is at a different version',
  );
  assert.equal(timers.size, 0);
});

test('a running top-up repeats the query at most ten times', async () => {
  const { context, timers, fire } = wikiContext({
    request: async () => ({ rows: [], status: { state: 'ready', topping_up: true } }),
  });
  vm.runInContext("scheduleWikiSearch('pomegranate')", context);
  for (let attempt = 0; attempt < 10; attempt += 1) {
    await fire();
    assert.equal(timers.size, 1, `repeat ${attempt} was not scheduled`);
    assert.equal([...timers.values()][0].ms, 2000);
  }
  await fire();
  assert.equal(timers.size, 0, 'the repeats are bounded');
  assert.equal(vm.runInContext('wikiState.topUps', context), 10);

  // A new query gets its own budget.
  vm.runInContext("scheduleWikiSearch('something else')", context);
  assert.equal(vm.runInContext('wikiState.topUps', context), 0);
  await fire();
  assert.equal(timers.size, 1);
});

test('wiki rows split into archived rows and snippets for live sessions', () => {
  const { context } = wikiContext();
  context.rows = [
    { id: 'gone', archived: true, hel_session_id: null, snippet: 'old work' },
    { id: 'kept', archived: false, hel_session_id: 'live-1', snippet: 'a match here' },
    { id: 'indexed-live', archived: true, hel_session_id: 'live-2', snippet: 'still here' },
    { id: 'recent', archived: false, hel_session_id: null, snippet: null },
  ];
  vm.runInContext('wikiState.rows = rows', context);
  assert.deepEqual(
    vm.runInContext('wikiArchivedRows().map(row => row.id)', context),
    ['gone'],
    'only rows the tool has lost and this daemon cannot see are archived rows',
  );
  assert.equal(vm.runInContext("wikiSnippetFor('live-1')", context), 'a match here');
  assert.equal(vm.runInContext("wikiSnippetFor('missing')", context), '');
  assert.equal(
    vm.runInContext('JSON.stringify([...wikiSessionRanks()])', context),
    JSON.stringify([['live-1', 1], ['live-2', 2]]),
    'a live row keeps the place the index gave it, so a query can be listed in that order',
  );
});

test('a search snippet marks the matched words as elements, not markup', () => {
  const built = [];
  const { context } = wikiContext({
    el: (name, className, textContent) => {
      built.push([name, textContent]);
      return { name, className, textContent };
    },
  });
  const marked = `read README now`;
  context.marked = marked;
  vm.runInContext('wikiSnippetNodes(marked)', context);
  assert.deepEqual(built, [['span', 'read '], ['mark', 'README'], ['span', ' now']]);
});

test('shipped viewer source comments use Mjolnir terminology', () => {
  for (const source of [
    serviceWorkerSource,
    readFileSync(viewerCssPath, 'utf8'),
    readFileSync(toolOutputPath, 'utf8'),
  ]) {
    assert.doesNotMatch(source, /\bHel\b|`hel`|\bhel publishes\b/);
  }
});

// A turn the harness ended without answering offers its prompt back rather
// than resending it (#970).
test('an unanswered turn offers the prompt that was running', () => {
  const context = vm.createContext({});
  vm.runInContext(
    sourceBetween("const PROMPT_UNANSWERED_MARKER", "\nfunction paintEntry("),
    context,
  );
  vm.runInContext(
    sourceBetween('/// The prompt an unanswered-turn row offers', '\nfunction renderEntries('),
    context,
  );
  const warning = {
    role: 'system',
    lines: ['warning: ACP prompt returned no session updates: Claude Code ended the turn'],
  };
  assert.equal(
    vm.runInContext('unansweredPromptFor', context)(warning, 'rename the module'),
    'rename the module',
  );
  assert.equal(
    vm.runInContext('unansweredPromptFor', context)({ role: 'agent', lines: ['done'] }, 'x'),
    null,
  );
  assert.equal(vm.runInContext('unansweredPromptFor', context)(warning, null), null);
});

// Live path suggestions must never fight the person typing: a superseded
// request is aborted, a stale reply is dropped, and accepting a row both
// sets the value and re-announces the edit.
test('path suggestions abort superseded requests and drop stale replies', async () => {
  const requests = [];
  const input = {
    value: '',
    className: '',
    listeners: new Map(),
    after(node) {
      this.next = node;
    },
    setAttribute() {},
    addEventListener(type, listener) {
      const existing = this.listeners.get(type) || [];
      this.listeners.set(type, [...existing, listener]);
    },
    dispatchEvent(event) {
      for (const listener of this.listeners.get(event.type) || []) listener(event);
    },
    fire(type, event = {}) {
      this.dispatchEvent({ type, preventDefault() {}, ...event });
    },
  };
  const makeElement = (name, className, textContent) => ({
    tagName: name.toUpperCase(),
    className: className || '',
    textContent: textContent === undefined ? '' : textContent,
    dataset: {},
    children: [],
    classList: {
      add(name) {
        this.owner.className = `${this.owner.className.replace(` ${name}`, '')} ${name}`.trim();
      },
      remove(name) {
        this.owner.className = this.owner.className.replace(name, '').trim();
      },
    },
    append(...children) {
      this.children.push(...children);
    },
    replaceChildren(...children) {
      this.children = children;
    },
    setAttribute(key, value) {
      this.attributes[key] = value;
    },
    attributes: {},
    addEventListener() {},
  });
  const el = (name, className, textContent) => {
    const node = makeElement(name, className, textContent);
    node.classList.owner = node;
    return node;
  };
  let pending = null;
  const context = vm.createContext({
    el,
    PATH_SUGGESTION_DELAY_MS: Number(
      /const PATH_SUGGESTION_DELAY_MS = (\d+);/.exec(viewerSource)[1],
    ),
    AbortController,
    Event: class {
      constructor(type) {
        this.type = type;
      }
    },
    JSON,
    // The debounce is not what these checks are about, so it fires at once.
    setTimeout: run => run(),
    clearTimeout: () => {},
    document: { activeElement: input },
    request: (url, options) =>
      new Promise((resolve, reject) => {
        requests.push({ url, body: JSON.parse(options.body), signal: options.signal });
        pending = { resolve, reject };
      }),
  });
  vm.runInContext(
    sourceBetween('function attachPathSuggestions(', '\nfunction pathField('),
    context,
  );
  context.input = input;
  context.complete = { host: () => 'raw', kind: 'directories', applies: text => text.startsWith('/') };
  vm.runInContext('attachPathSuggestions(input, complete)', context);
  const list = input.next;
  const flush = () => new Promise(resolve => setImmediate(resolve));

  // Text that is not a path never reaches the controller.
  input.value = 'owner/repo';
  context.complete.applies = text => text.startsWith('/');
  input.fire('input');
  assert.equal(requests.length, 0, 'a non-path asked for suggestions');

  // A second edit abandons the request the first one started.
  input.value = '/wo';
  input.fire('input');
  assert.equal(requests.length, 1);
  const first = requests[0];
  assert.deepEqual(first.body, { target_id: 'raw', prefix: '/wo', kind: 'directories' });
  const firstPending = pending;
  input.value = '/work/re';
  input.fire('input');
  assert.ok(first.signal.aborted, 'the superseded request was not aborted');
  assert.equal(requests.length, 2);

  // The abandoned request's reply is not rendered, even if it arrives.
  firstPending.resolve({ candidates: ['/wo1/', '/wo2/'], insert: null, truncated: false });
  await flush();
  assert.equal(list.children.length, 0, 'a stale reply was rendered');

  // A reply for text the person has since changed is dropped too.
  const second = pending;
  input.value = '/work/rep';
  second.resolve({ candidates: ['/work/recent/', '/work/repos/'], insert: null, truncated: false });
  await flush();
  assert.equal(list.children.length, 0, 'a reply for changed text was rendered');

  // A reply that still answers the field is shown, and Enter accepts it.
  input.value = '/work/re';
  input.fire('input');
  assert.equal(requests.length, 3);
  pending.resolve({ candidates: ['/work/recent/', '/work/repos/'], insert: '/work/re', truncated: true });
  await flush();
  assert.equal(list.children.length, 3, 'the truncation notice is missing');
  assert.equal(list.children[2].textContent, 'More matches — keep typing');
  assert.equal(input.value, '/work/re', 'the typed text was rewritten');

  input.fire('keydown', { key: 'ArrowDown' });
  input.fire('keydown', { key: 'Enter' });
  assert.equal(input.value, '/work/repos/');
  // Accepting re-announces the edit, which is what updates the draft and
  // asks for the accepted directory's children.
  assert.equal(requests.length, 4);
  assert.equal(requests[3].body.prefix, '/work/repos/');
});

test('a signed-out load shows the login form without requesting the snapshot', async () => {
  // Finding G-3: learning "signed out" from a 401 on /api/snapshot put a
  // console error on every load of the login page.
  const run = async statusResponse => {
    const calls = [];
    const context = vm.createContext({
      upgradeAwareFetch: async url => {
        calls.push(url);
        if (statusResponse instanceof Error) throw statusResponse;
        return statusResponse;
      },
      refresh: async () => { calls.push('refresh'); return true; },
      applyRoute: () => calls.push('applyRoute'),
      showLogin: () => calls.push('showLogin'),
    });
    vm.runInContext(
      sourceBetween('async function knownSignedOut()', 'function renderQueue('),
      context,
    );
    await vm.runInContext('restoreRoute()', context);
    return calls;
  };
  const json = body => ({ ok: true, json: async () => body });
  assert.deepEqual(await run(json({ signed_in: false })), ['/auth/session', 'showLogin']);
  assert.deepEqual(
    await run(json({ signed_in: true })),
    ['/auth/session', 'refresh', 'applyRoute'],
  );
  // A server without the route, or no answer at all, falls back to the
  // snapshot request, which still reaches the login form on a 401.
  assert.deepEqual(
    await run({ ok: false, json: async () => ({}) }),
    ['/auth/session', 'refresh', 'applyRoute'],
  );
  assert.deepEqual(
    await run(new Error('offline')),
    ['/auth/session', 'refresh', 'applyRoute'],
  );
});

test('a request answered without content still reads the empty body to its end', async () => {
  // Finding G-4: Chromium reports a fetch whose 202 or 204 body is never
  // read as net::ERR_ABORTED although the server completed it. Reading the
  // empty body lets the request finish in the browser too.
  const run = async status => {
    let read = false;
    const context = vm.createContext({
      upgradeAwareFetch: async () => ({
        status,
        ok: status < 400,
        arrayBuffer: async () => { read = true; return new ArrayBuffer(0); },
        json: async () => { read = true; return {}; },
      }),
      showLogin: () => {},
      JSON,
    });
    vm.runInContext(sourceBetween('async function request(', '/// Upload one image'), context);
    const result = await vm.runInContext(
      "request('/api/actions', { method: 'POST', body: '{}' })",
      context,
    ).catch(error => error);
    return { read, result };
  };
  for (const status of [202, 204]) {
    const { read, result } = await run(status);
    assert.equal(result, null);
    assert.ok(read, `a ${status} body was left unread`);
  }
  const { read } = await run(401);
  assert.ok(read, 'a 401 body was left unread');
});
