import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const viewerSource = readFileSync(
  new URL('../../../mj-controller/src/web/viewer.js', import.meta.url),
  'utf8',
);

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
    dataset: {},
    hidden: false,
    disabled: false,
    children: [],
    attributes: {},
    append(...children) {
      this.children.push(...children);
    },
    replaceChildren(...children) {
      this.children = children;
    },
    setAttribute(name, value) {
      this.attributes[name] = String(value);
    },
    removeAttribute(name) {
      delete this.attributes[name];
    },
  };
}

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

function cardHarness() {
  const source = sourceBetween('function sessionCard(session) {', '\nfunction action(');
  const context = vm.createContext({
    pendingActions: new Set(),
    openSessionMenuId: null,
    snapshot: {},
    epochMs(value) { return typeof value === 'number' ? value : null; },
    sessionLifecycleLabel(session) { return session.state || session.lifecycle || 'live'; },
    isTransitioningSession(session) {
      return session.transitioning === true
        || Boolean(session.operation && session.operation.kind !== 'checkpoint');
    },
    document: { createElement: makeNode },
  });
  vm.runInContext(
    `
function el(name, className, textContent) {
  const node = document.createElement(name);
  node.className = className || '';
  if (textContent !== undefined) node.textContent = textContent;
  return node;
}
function button(label, className, data) {
  const node = el('button', className, label);
  for (const [key, value] of Object.entries(data || {})) node.dataset[key] = value;
  return node;
}
function renderSessionTitle(node, session) {
  node.textContent = session.title;
}
function withHiddenGlyph(glyph) {
  return el('span', 'state-glyph', glyph);
}
function action(label, className, data) {
  return button(label, className, data);
}
${source}
globalThis.render = sessionCard;
`,
    context,
  );
  return context;
}

test('session cards are focusable, have no Open button, and retain nested actions', () => {
  const card = vm.runInContext(
    `render({
      id: 'session-1', title: 'Build', lifecycle: 'live', state: 'Ready',
      target_id: 'target', profile_id: 'profile', capabilities: {
        open: true, rename: true, cancel_operation: true, stop: true, resume: true,
      },
    })`,
    cardHarness(),
  );
  const buttons = descendants(card, 'button');

  assert.equal(card.attributes.role, 'link');
  assert.equal(card.attributes.tabindex, '0');
  assert.equal(card.attributes['aria-label'], 'Open session Build');
  assert.deepEqual(
    buttons.map(button => button.textContent),
    ['⋯', 'Rename', 'Cancel operation', 'Stop session', 'Resume'],
  );
  assert.ok(!buttons.some(button => button.textContent === 'Open'));

  const closed = vm.runInContext(
    `render({ id: 'closed', title: 'Closed', lifecycle: 'stopped', state: 'Stopped', target_id: 'target', profile_id: 'profile', capabilities: {} })`,
    cardHarness(),
  );
  assert.equal(closed.attributes.role, undefined);
  assert.equal(closed.attributes.tabindex, undefined);
});

test('session clocks keep the TUI units and structured activity truth', () => {
  const clockSource = sourceBetween('function formatClock(', '\nfunction localClock');
  const clockContext = vm.createContext({});
  vm.runInContext(`${clockSource}\nglobalThis.formatClockForTest = formatClock;`, clockContext);
  const formatClock = clockContext.formatClockForTest;
  assert.equal(formatClock(36_000), '36s');
  assert.equal(formatClock(60_000), '1m00s');
  assert.equal(formatClock(65_000), '1m05s');
  assert.equal(formatClock(3_600_000), '1h00m');
  assert.equal(formatClock(6_216_000), '1h43m');
  assert.equal(formatClock(172_800_000), '2d00h');

  const activitySource = sourceBetween('function sessionActivityLabel(', '\nfunction updateSessionActivity');
  const activityContext = vm.createContext({
    snapshot: {},
    epochMs(value) { return typeof value === 'number' ? value : null; },
    formatClock,
    serverClockMs: () => 100_000,
    idleSinceLabel: () => 'Idle since 12:00',
    operationLabel: () => 'Operation 1m00s',
    sessionLifecycleLabel: session => session.lifecycle || 'live',
  });
  vm.runInContext(`${activitySource}\nglobalThis.activityForTest = sessionActivityLabel;`, activityContext);
  const activity = activityContext.activityForTest;
  assert.equal(activity({ activity_details: { kind: 'turn', turn_started_at_ms: 40_000 } }, 100_000), 'Turn 1m00s · Step 1m00s');
  assert.equal(activity({ activity_details: { kind: 'step', step_started_at_ms: 40_000 } }, 100_000), 'Step 1m00s');
  assert.equal(activity({ activity_details: { kind: 'idle' } }, 100_000), 'Idle');
  assert.equal(activity({ activity_details: { kind: 'background' } }, 100_000), 'Background');
  assert.equal(activity({ activity_details: { kind: 'lifecycle', label: 'Stopping' } }, 100_000), 'Stopping');
  assert.equal(activity({ lifecycle: 'starting', activity_details: { kind: 'turn', turn_started_at_ms: 40_000 } }, 100_000), 'starting');
});

function cardEventHarness() {
  const source = sourceBetween(
    '/// Find a session card for an event',
    '\n// ---------------------------------------------------------------------------\n// The other pages',
  );
  const navigations = [];
  const context = vm.createContext({
    suppressedSessionClickId: null,
    closeSessionMenu() {},
    openSessionMenu() {},
    navigate(route) {
      navigations.push(route);
    },
  });
  vm.runInContext(`${source}\nglobalThis.cardEvent = { sessionCardFromEvent, openSessionCard, handleSessionCardKeydown };`, context);
  return { ...context.cardEvent, navigations };
}

function cardTarget(id = 'session-1') {
  return {
    dataset: { sessionId: id, openable: 'true' },
    closest(selector) {
      return selector === '.session[data-session-id]' ? this : null;
    },
  };
}

test('card navigation ignores nested controls and supports click, Enter, and Space', () => {
  const harness = cardEventHarness();
  const card = cardTarget();
  const control = {
    closest(selector) {
      return selector === 'button' ? this : card;
    },
  };

  assert.equal(harness.openSessionCard({ target: control }), false);
  assert.deepEqual(harness.navigations, []);

  assert.equal(harness.openSessionCard({ target: card }), true);
  assert.deepEqual(JSON.parse(JSON.stringify(harness.navigations)), [
    { name: 'conversation', sessionId: 'session-1' },
  ]);

  for (const key of ['Enter', ' ']) {
    const event = {
      key,
      target: card,
      defaultPrevented: false,
      preventDefault() {
        this.defaultPrevented = true;
      },
    };
    harness.handleSessionCardKeydown(event);
    assert.equal(event.defaultPrevented, true);
  }
  const blocked = {
    key: 'Enter',
    target: control,
    preventDefault() {
      throw new Error('nested controls must keep their own keyboard action');
    },
  };
  harness.handleSessionCardKeydown(blocked);
  assert.equal(harness.navigations.length, 3);
  card.dataset.openable = 'false';
  assert.equal(harness.openSessionCard({ target: card }), false);
});

function queueHarness() {
  const source = sourceBetween('function renderQueue(session) {', '\n// Every snapshot revision');
  const context = vm.createContext({
    queue: makeNode('div'),
    shells: makeNode('div'),
    queueHeading: makeNode('h3'),
    shellsHeading: makeNode('h3'),
    backgroundTasks: makeNode('div'),
    backgroundTasksHeading: makeNode('h3'),
    conversationSide: makeNode('details'),
    conversationSummary: makeNode('summary'),
    pendingActions: new Set(),
    backgroundTaskErrors: new Map(),
    epochMs: value => value,
    serverClockMs: () => 100_000,
    formatClock(milliseconds) {
      const seconds = Math.floor(milliseconds / 1000);
      return `${Math.floor(seconds / 60)}m${String(seconds % 60).padStart(2, '0')}s`;
    },
    document: { createElement: makeNode },
  });
  vm.runInContext(
    `
function el(name, className, textContent) {
  const node = document.createElement(name);
  node.className = className || '';
  if (textContent !== undefined) node.textContent = textContent;
  return node;
}
function button(label, className, data) {
  const node = el('button', className, label);
  for (const [key, value] of Object.entries(data || {})) node.dataset[key] = value;
  return node;
}
${source}
globalThis.render = renderQueue;
`,
    context,
  );
  return context;
}

test('queue details disappear when empty but preserve active shell cancellation', () => {
  const context = queueHarness();
  vm.runInContext('render({ queued_prompts: [], active_user_shells: [] })', context);
  assert.equal(context.conversationSide.hidden, true);
  assert.equal(context.queueHeading.hidden, true);
  assert.equal(context.shellsHeading.hidden, true);
  assert.equal(context.conversationSummary.textContent, 'Shell commands');

  vm.runInContext(
    `render({ queued_prompts: [], active_user_shells: [{ id: 'shell-1', command: 'cargo test' }] })`,
    context,
  );
  assert.equal(context.conversationSide.hidden, false);
  assert.equal(context.queue.hidden, true);
  assert.equal(context.shells.hidden, false);
  assert.equal(context.shellsHeading.hidden, false);
  assert.equal(context.conversationSummary.textContent, 'Shell commands');
  assert.equal(context.shells.children.length, 1);
  assert.equal(context.shells.children[0].children[1].textContent, 'Cancel');
  assert.equal(context.shells.children[0].children[1].dataset.shellId, 'shell-1');
});

test('background task rows expose elapsed time, stop capability, and pending failures', () => {
  const context = queueHarness();
  vm.runInContext(
    `render({ id: 'session-1', queued_prompts: [], active_user_shells: [], background_tasks: [
      { id: 'task-1', command: 'cargo test', started_at_ms: 40_000, can_stop: true },
      { id: 'task-2', command: 'watch logs', started_at_ms: 90_000, can_stop: false },
    ] })`,
    context,
  );
  assert.equal(context.conversationSide.hidden, false);
  assert.equal(context.backgroundTasksHeading.hidden, false);
  assert.equal(context.backgroundTasks.children.length, 2);
  assert.equal(context.backgroundTasks.children[0].children[0].children[1].textContent, '1m00s');
  assert.equal(context.backgroundTasks.children[0].children[1].textContent, 'Stop');
  assert.equal(context.backgroundTasks.children[0].children[1].dataset.backgroundTaskId, 'task-1');
  assert.equal(context.backgroundTasks.children[1].children[1].textContent, 'Stop unavailable');

  context.pendingActions.add('stop-background:session-1:task-1');
  vm.runInContext(
    `render({ id: 'session-1', queued_prompts: [], active_user_shells: [], background_tasks: [
      { id: 'task-1', command: 'cargo test', started_at_ms: 40_000, can_stop: true },
    ] })`,
    context,
  );
  assert.equal(context.backgroundTasks.children[0].children[1].textContent, 'Stopping…');
  assert.equal(context.backgroundTasks.children[0].children[1].disabled, true);

  context.pendingActions.delete('stop-background:session-1:task-1');
  context.backgroundTaskErrors.set('stop-background:session-1:task-1', 'provider unavailable');
  vm.runInContext(
    `render({ id: 'session-1', queued_prompts: [], active_user_shells: [], background_tasks: [
      { id: 'task-1', command: 'cargo test', started_at_ms: 40_000, can_stop: true },
    ] })`,
    context,
  );
  const error = context.backgroundTasks.children[0].children[2];
  assert.equal(error.textContent, 'provider unavailable');
  assert.equal(error.attributes.role, 'alert');
});

test('background task stop requests are deduplicated and retain pending state until refresh', async () => {
  const requests = [];
  let refreshes = 0;
  let resolveRequest;
  const session = {
    id: 'session-1',
    background_tasks: [{ id: 'task-1', can_stop: true }],
  };
  const context = vm.createContext({
    currentSession: session.id,
    snapshot: { sessions: [session] },
    pendingActions: new Set(),
    backgroundTaskErrors: new Map(),
    activeSession: () => session,
    renderQueue: () => {},
    encodeURIComponent,
    request: (url, options) => {
      requests.push({ url, options });
      return new Promise((resolve, reject) => { resolveRequest = { resolve, reject }; });
    },
    refresh: async () => { refreshes++; },
  });
  vm.runInContext(
    `${sourceBetween('function backgroundTaskKey', '\n// Every snapshot revision')}
globalThis.stop = stopBackgroundTask;`,
    context,
  );
  const first = vm.runInContext('stop("task-1")', context);
  assert.equal(context.pendingActions.has('stop-background:session-1:task-1'), true);
  assert.equal(await vm.runInContext('stop("task-1")', context), false);
  assert.equal(requests.length, 1);
  resolveRequest.resolve();
  assert.equal(await first, true);
  assert.equal(refreshes, 1);
  assert.equal(context.pendingActions.has('stop-background:session-1:task-1'), true);

  context.request = () => Promise.reject(new Error('provider unavailable'));
  context.pendingActions.delete('stop-background:session-1:task-1');
  // The task still appears in the latest snapshot, so a failure has an inline
  // destination and the next click is available again.
  assert.equal(await vm.runInContext('stop("task-1")', context), false);
  assert.equal(context.pendingActions.has('stop-background:session-1:task-1'), false);
  assert.equal(
    context.backgroundTaskErrors.get('stop-background:session-1:task-1'),
    'provider unavailable',
  );
});
