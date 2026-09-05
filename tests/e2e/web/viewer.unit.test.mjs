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

test('project preflight prevents duplicate checks and ignores a cancelled wizard response', async () => {
  let complete;
  let requests = 0;
  const draft = { profileId: 'test', targetId: 'raw', projectDirectory: '/project' };
  const context = vm.createContext({
    newDraft: draft,
    pendingNewPreflight: null,
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
  await assert.rejects(vm.runInContext('preflightNew()', context), /invalid project/);
  assert.equal(context.pendingNewPreflight, null, 'failure releases the checking state');
  assert.equal(context.newDraft.preflighted, undefined);
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
const error = { textContent: '' };
const document = { querySelector() { return error; } };
async function request() { throw new Error('resolution refused'); }
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
      ['/', '/viewer.css', '/viewer.js', '/manifest.webmanifest', '/icon.svg'],
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

test('shipped viewer source comments use Mjolnir terminology', () => {
  for (const source of [
    serviceWorkerSource,
    readFileSync(viewerCssPath, 'utf8'),
    readFileSync(toolOutputPath, 'utf8'),
  ]) {
    assert.doesNotMatch(source, /\bHel\b|`hel`|\bhel publishes\b/);
  }
});
