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

function queueHarness() {
  const source = sourceBetween('function renderQueue(session) {', '\n// Every snapshot revision');
  const context = vm.createContext({
    pendingLifecycleActions: new Map(),
    queue: makeNode('div'),
    shells: makeNode('div'),
    queueHeading: makeNode('h3'),
    shellsHeading: makeNode('h3'),
    backgroundTasks: makeNode('div'),
    backgroundTasksHeading: makeNode('h3'),
    conversationSide: makeNode('details'),
    conversationSummary: makeNode('summary'),
    pendingActions: new Set(),
    pendingLifecycleActions: new Map(),
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

test('background task stop requests are deduplicated and retain pending state until refresh', async () => {
  const requests = [];
  let refreshes = 0;
  let resolveRequest;
  const session = {
    id: 'session-1',
    background_tasks: [{ id: 'task-1', can_stop: true }],
  };
  const context = vm.createContext({
    pendingLifecycleActions: new Map(),
    currentSession: session.id,
    snapshot: { sessions: [session] },
    pendingActions: new Set(),
    pendingLifecycleActions: new Map(),
    backgroundTaskErrors: new Map(),
    activeSession: () => session,
    sessionById: id => id === session.id ? session : undefined,
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
