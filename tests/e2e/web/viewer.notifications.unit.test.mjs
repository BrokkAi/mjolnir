import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const viewerSource = readFileSync(
  new URL('../../../mj-controller/src/web/viewer.js', import.meta.url),
  'utf8',
);

const plain = value => JSON.parse(JSON.stringify(value));

function sourceBetween(from, to) {
  const start = viewerSource.indexOf(from);
  assert.notEqual(start, -1, `viewer.js is missing ${JSON.stringify(from)}`);
  const end = viewerSource.indexOf(to, start);
  assert.notEqual(end, -1, `viewer.js is missing ${JSON.stringify(to)} after ${from}`);
  return viewerSource.slice(start, end);
}

class Node {
  constructor(tag) {
    this.tagName = tag;
    this.className = '';
    this.textContent = '';
    this.value = '';
    this.hidden = false;
    this.disabled = false;
    this.dataset = {};
    this.children = [];
    this.attributes = {};
  }
  append(...nodes) { this.children.push(...nodes); }
  insertBefore(node, reference) { this.children.splice(this.children.indexOf(reference), 0, node); }
  setAttribute(name, value) { this.attributes[name] = String(value); }
}

/// The notifications screen running on its real source with hand-built fakes
/// for the daemon (`request`), the router and the DOM.
function screen(rows, { launchFailures = [] } = {}) {
  const requests = [];
  const drafts = [];
  const answers = new Map();
  const nav = [];
  const container = new Node('div');
  const context = vm.createContext({
    document: {
      createElement: tag => new Node(tag),
      querySelector: selector => (selector === '#inbox' ? container : (context.errorNode ??= new Node('p'))),
    },
    viewerState: { rows: new Map(rows.map(row => [row.id, row])) },
    dismissedNotifications: new Map(),
    pendingActions: new Set(),
    route: { name: 'notifications' },
    requests,
    answers,
    nav,
    snapshot: { launch_failures: launchFailures },
    dismissedLaunchFailures: new Set(),
    pendingSubmissions: new Map(),
    renderPendingSubmissions: () => {},
    renderRoute: () => context.rendered.push('route'),
    rendered: [],
    drafts,
    queueDraftSave: async (id, text) => { drafts.push([id, text]); },
    MAX_PROMPT_REQUEST_BYTES: 32 * 1024 * 1024,
    TextEncoder,
    crypto: { getRandomValues: array => array.fill(7) },
    snapshotReceivedAtMs: 0,
    serverClockMs: () => 100_000,
    formatClock: ms => `${Math.floor(ms / 1000)}s`,
    epochMs: value => value,
    isSubagentSession: row => Boolean(row.subagent_parent_id),
    childFormHolder: row => (row.form_holder && row.form_holder !== row.id ? row.form_holder : null),
    sessionById: id => context.viewerState.rows.get(id),
    sessionMessageMs: row => row.last_message_at_ms ?? 0,
    navigate: next => nav.push(next),
    refresh: async () => {},
    request: async (url, options = {}) => {
      const body = options.body ? JSON.parse(options.body) : null;
      requests.push({ url, body });
      const answer = answers.get(body ? `${body.action}:${body.session_id}` : url);
      if (answer instanceof Error) throw answer;
      if (typeof answer === 'function') return answer();
      return answer ?? null;
    },
  });
  vm.runInContext(sourceBetween('function el(', 'const bootStatus'), context);
  vm.runInContext(sourceBetween('function newCommandId(', '/// Guard against sending twice.'), context);
  vm.runInContext(sourceBetween('function submissionBody(', 'async function submitPrompt('), context);
  vm.runInContext(sourceBetween('function hasNotification(', '/// Notifications, unread output'), context);
  vm.runInContext(sourceBetween('// The notifications screen\n', 'function renderSessions() {'), context);
  const read = expression => vm.runInContext(expression, context);
  return {
    context, requests, answers, nav, container, drafts,
    read,
    nodes: read('inboxNodes'),
    render: () => read('renderInbox()'),
    ids: () => plain(read('inboxEntries()').map(row => row.id)),
  };
}

const row = (id, attention, at, extra = {}) => ({
  id, title: id.toUpperCase(), attention, last_message_at_ms: at, project_label: 'proj',
  notification_dismissal: attention === 'needs_input' ? { question: { turn_id: id } } : { read_through: at },
  ...extra,
});

const settle = () => new Promise(resolve => setImmediate(resolve));

test('cards run ? before !, newest first, one per session, without sub-agents or failures', () => {
  const view = screen([
    row('a', 'important', 5), row('b', 'needs_input', 1), row('c', 'needs_input', 9),
    row('d', 'important', 8), row('e', 'failed', 20), row('f', 'needs_input', 30, { subagent_parent_id: 'a' }),
    row('g', 'needs_input', 40, { notifications_ignored: true }),
  ]);
  assert.deepEqual(view.ids(), ['c', 'b', 'd', 'a']);
});

test('skip moves the card to the back and is held in the page only', () => {
  const view = screen([row('a', 'needs_input', 3), row('b', 'needs_input', 2), row('c', 'important', 1)]);
  view.nodes.skip.onclick();
  assert.deepEqual(view.ids(), ['b', 'c', 'a']);
  view.nodes.skip.onclick();
  assert.deepEqual(view.ids(), ['c', 'a', 'b']);
  view.context.viewerState.rows.delete('a');
  assert.deepEqual(view.ids(), ['c', 'b']);
  assert.equal(view.read('inboxSkipped.includes("a")'), false, 'a gone session leaves the skip list');
});

test('Clear & Next dismisses the identity captured when it starts and shows a stale refusal', async () => {
  const view = screen([row('a', 'needs_input', 3), row('b', 'important', 2)]);
  view.render();
  view.nodes.clear.onclick();
  // The daemon moves on while the request is in flight; the request already named its target.
  view.context.viewerState.rows.get('a').notification_dismissal = { question: { turn_id: 'newer' } };
  await settle();
  assert.deepEqual(plain(view.requests.findLast(r => r.body?.action === 'dismiss-notification').body), {
    action: 'dismiss-notification', session_id: 'a', target: { question: { turn_id: 'a' } },
  });
  // The newer target is a different notification, so the card is still there.
  assert.deepEqual(view.ids(), ['a', 'b']);

  view.answers.set('dismiss-notification:a', new Error('notification target is stale'));
  view.nodes.clear.onclick();
  await settle();
  assert.equal(view.nodes.cardError.textContent, 'notification target is stale');
  assert.deepEqual(view.ids(), ['a', 'b'], 'a refused card stays');

  view.answers.delete('dismiss-notification:a');
  view.nodes.clear.onclick();
  await settle();
  assert.deepEqual(view.ids(), ['b']);
});

test('Clear all ! dismisses only ! cards and leaves ? cards', async () => {
  const view = screen([row('q', 'needs_input', 9), row('i1', 'important', 5), row('i2', 'important', 4)]);
  view.render();
  assert.equal(view.nodes.clearAll.textContent, 'Clear all ! (2)');
  view.nodes.clearAll.onclick();
  await settle();
  const dismissed = view.requests.filter(r => r.body?.action === 'dismiss-notification').map(r => r.body.session_id).sort();
  assert.deepEqual(dismissed, ['i1', 'i2']);
  assert.deepEqual(view.ids(), ['q']);
  view.render();
  assert.equal(view.nodes.clearAll.hidden, true);
});

test('quick reply sends to the captured session; drafts follow their session; empty Enter goes to the session', async () => {
  const view = screen([row('a', 'needs_input', 3), row('b', 'important', 2)]);
  view.render();
  view.nodes.input.value = 'draft for a';
  view.nodes.input.oninput();
  view.nodes.skip.onclick();
  assert.equal(view.nodes.input.value, '', 'the next card does not inherit the draft');
  view.nodes.input.value = 'for b';
  view.nodes.input.oninput();
  view.nodes.skip.onclick();
  assert.equal(view.nodes.input.value, 'draft for a', 'the draft returns with its card');

  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  assert.deepEqual(plain(view.requests.at(-1).body), {
    action: 'prompt', session_id: 'a', text: 'draft for a', images: [], command_id: 'web-prompt-07070707070707070707070707070707',
  });
  assert.equal(view.nodes.input.value, '');
  assert.equal(view.read('inboxDrafts.has("a")'), false);
  assert.equal(view.read('inboxDrafts.get("b")'), 'for b', 'another session\'s draft is untouched');

  view.nodes.reply.onsubmit({ preventDefault() {} });
  assert.deepEqual(plain(view.nav), [{ name: 'conversation', sessionId: 'a', from: 'notifications' }]);
});

test('a send the daemon refuses keeps the draft and says so', async () => {
  const view = screen([row('a', 'needs_input', 3)]);
  view.render();
  view.nodes.input.value = 'hello';
  view.nodes.input.oninput();
  view.answers.set('prompt:a', Object.assign(new Error('session is closed'), { status: 409 }));
  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  assert.equal(view.read('inboxDrafts.get("a")'), 'hello');
  assert.equal(view.nodes.input.value, 'hello');
  assert.equal(view.nodes.cardError.textContent, 'Not sent: session is closed');
  assert.equal(view.read('pendingSubmissions.size'), 0, 'a refused line leaves no pending row behind');
});

// A lost acknowledgement does not authorize replay (AGENTS.md).
test('a lost acknowledgement reports delivery unconfirmed and never mints a second command', async () => {
  const view = screen([row('a', 'needs_input', 3)]);
  view.render();
  view.nodes.input.value = 'hello';
  view.nodes.input.oninput();
  view.answers.set('prompt:a', new Error('network lost'));
  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  const prompts = () => view.requests.filter(r => r.body?.action === 'prompt');
  assert.equal(prompts().length, 1);
  const id = prompts()[0].body.command_id;
  const [pending] = view.read('[...pendingSubmissions.values()]');
  assert.equal(pending.id, id, 'the pending row keeps the original command id for reconciliation');
  assert.match(pending.status, /^Delivery unconfirmed/);
  assert.match(view.nodes.cardError.textContent, /^Delivery unconfirmed: network lost/);
  assert.equal(view.nodes.input.value, '', 'an unconfirmed line is not offered for resending');
  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  assert.equal(prompts().length, 1, 'Send again does not create a second command');
});

test('an edit made while a send is in flight survives its acknowledgement', async () => {
  const view = screen([row('a', 'needs_input', 3)]);
  view.render();
  view.nodes.input.value = 'first';
  view.nodes.input.oninput();
  let acknowledge;
  view.answers.set('prompt:a', () => new Promise(resolve => { acknowledge = resolve; }));
  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  assert.equal(view.nodes.send.disabled, true, 'a second send of the same session waits');
  view.nodes.input.value = 'first, and more';
  view.nodes.input.oninput();
  acknowledge(null);
  await settle();
  assert.equal(view.nodes.input.value, 'first, and more');
  assert.equal(view.read('inboxDrafts.get("a")'), 'first, and more');
  assert.equal(view.nodes.send.disabled, false);
});

test('quick reply builds the composer\'s own action: ! runs a shell command and a slash command goes to the composer unsent', async () => {
  const view = screen([row('a', 'needs_input', 3)]);
  view.render();
  view.nodes.input.value = '!ls -l';
  view.nodes.input.oninput();
  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  assert.deepEqual(plain(view.requests.find(r => r.body?.action === 'run-shell').body).command, 'ls -l');
  assert.equal(view.requests.some(r => r.body?.action === 'prompt'), false);

  view.nodes.input.value = '/model fast';
  view.nodes.input.oninput();
  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  assert.deepEqual(plain(view.drafts), [['a', '/model fast']]);
  assert.deepEqual(plain(view.nav), [{ name: 'conversation', sessionId: 'a', from: 'notifications' }]);
  assert.equal(view.requests.some(r => r.body?.text === '/model fast'), false, 'a local command is never sent as a prompt');
});

test('Go to Session opens a pending form\'s holder and returns here on Back', () => {
  const view = screen([row('parent', 'needs_input', 3, { form_holder: 'child' })]);
  view.render();
  view.nodes.open.onclick();
  assert.deepEqual(plain(view.nav), [{ name: 'conversation', sessionId: 'child', from: 'notifications' }]);
  // Both the history entry and the viewer's own back control follow the route.
  vm.runInContext(sourceBetween('const ID = ', '/// Go to a route.'), view.context);
  assert.equal(view.read('routeHash(nav[0])'), '#notifications/child');
  assert.deepEqual(plain(view.read('parseRoute("#notifications/child")')), { name: 'conversation', sessionId: 'child', from: 'notifications' });
});

test('the full message loads from the daemon: loading, text, and not available', async () => {
  const view = screen([row('a', 'needs_input', 3, { question_message: 'agent:1' })]);
  view.answers.set('/api/sessions/a/notification-message', { session_id: 'a', text: 'Line one\n\nLine two' });
  view.render();
  assert.equal(view.nodes.body.textContent, 'loading…');
  await settle();
  assert.equal(view.nodes.body.textContent, 'Line one\n\nLine two');

  const missing = screen([row('m', 'important', 3)]);
  missing.answers.set('/api/sessions/m/notification-message', { session_id: 'm', text: null });
  missing.render();
  await settle();
  assert.equal(missing.nodes.body.textContent, 'message not available');
});

test('the header count is the ? and ! sessions', () => {
  const view = screen([row('a', 'needs_input', 3), row('b', 'important', 2), row('c', 'failed', 1), row('d', 'unread', 1)]);
  assert.equal(view.read('inboxEntries().length'), 2);
  const withFailure = screen([row('a', 'needs_input', 3)], { launchFailures: [{ id: 'f', workspace_id: 'w' }] });
  assert.equal(withFailure.read('inboxEntries().length'), 2, 'launch failures count too');
});

// Hard-won: skipping past the last card appended a duplicate id, which stuck the screen and doubled counts.
test('skip rotates through unique cards however many times it is pressed', () => {
  const view = screen([row('a', 'needs_input', 3), row('b', 'needs_input', 2), row('c', 'important', 1)]);
  const seen = [];
  for (let press = 0; press < 7; press += 1) { seen.push(view.ids()[0]); view.nodes.skip.onclick(); }
  assert.deepEqual(seen, ['a', 'b', 'c', 'a', 'b', 'c', 'a']);
  assert.deepEqual(view.ids(), ['b', 'c', 'a']);
  assert.equal(new Set(plain(view.read('inboxSkipped'))).size, view.read('inboxSkipped.length'));
  view.render();
  assert.match(view.nodes.headerText.textContent, /3 left/);
  assert.equal(view.nodes.clearAll.textContent, 'Clear all ! (1)');
});

test('a failed message read is not cached: it retries when the card is shown again, not in a loop', async () => {
  const view = screen([row('a', 'needs_input', 3), row('b', 'needs_input', 2)]);
  view.answers.set('/api/sessions/a/notification-message', new Error('offline'));
  view.render();
  await settle();
  const reads = () => view.requests.filter(r => r.url.endsWith('/a/notification-message')).length;
  assert.equal(reads(), 1);
  assert.match(view.nodes.body.textContent, /Could not read the message: offline/);
  assert.equal(view.nodes.retry.hidden, false);
  view.render(); view.render();
  await settle();
  assert.equal(reads(), 1, 'rendering the same card again does not refetch');
  view.answers.set('/api/sessions/a/notification-message', { text: 'back again' });
  view.nodes.retry.onclick();
  await settle();
  assert.equal(view.nodes.body.textContent, 'back again');

  // A failure is also retried on the next visit to the card.
  const other = screen([row('a', 'needs_input', 3)]);
  other.answers.set('/api/sessions/a/notification-message', new Error('offline'));
  other.render();
  await settle();
  other.answers.set('/api/sessions/a/notification-message', { text: 'recovered' });
  other.read('inboxShownKey = null');
  other.render();
  await settle();
  assert.equal(other.nodes.body.textContent, 'recovered');
});

test('a new pending form, from the same or another holder, reads the message again', async () => {
  const parent = row('p', 'needs_input', 3, { form_holder: 'c1' });
  const child = (id, requests) => row(id, 'working', 1, { pending_elicitation_ids: requests, subagent_parent_id: 'p' });
  const view = screen([parent, child('c1', ['r1']), child('c2', [])]);
  view.answers.set('/api/sessions/p/notification-message', { text: 'form r1' });
  view.render();
  await settle();
  const reads = () => view.requests.filter(r => r.url.endsWith('/p/notification-message')).length;
  assert.equal(reads(), 1);
  view.context.viewerState.rows.get('c1').pending_elicitation_ids = ['r2'];
  view.answers.set('/api/sessions/p/notification-message', { text: 'form r2' });
  view.render();
  await settle();
  assert.equal(reads(), 2, 'a new request id from the same holder');
  assert.equal(view.nodes.body.textContent, 'form r2');
  view.context.viewerState.rows.get('c2').pending_elicitation_ids = ['r2'];
  view.context.viewerState.rows.get('p').form_holder = 'c2';
  view.render();
  await settle();
  assert.equal(reads(), 3, 'the same request id under another holder');
});

test('launch failures are cards after the ! cards, counted, openable and dismissable', () => {
  const view = screen([row('a', 'important', 3), row('b', 'needs_input', 1, { notifications_ignored: true })], {
    launchFailures: [
      { id: 'f1', workspace_id: 'w', session_id: 'a', error: 'target is down' },
      { id: 'f2', workspace_id: 'w' },
    ],
  });
  assert.deepEqual(view.ids(), ['a', 'launch-failure:f1', 'launch-failure:f2']);
  view.nodes.skip.onclick();
  view.render();
  assert.equal(view.nodes.body.textContent, 'target is down');
  assert.equal(view.nodes.reply.hidden, true);
  assert.equal(view.nodes.open.hidden, false);
  view.nodes.open.onclick();
  assert.deepEqual(plain(view.nav), [{ name: 'conversation', sessionId: 'a', from: 'notifications' }]);
  view.nodes.clear.onclick();
  assert.deepEqual(plain([...view.read('dismissedLaunchFailures')]), ['f1']);
  assert.deepEqual(view.ids(), ['launch-failure:f2', 'a']);
  view.render();
  assert.equal(view.nodes.open.hidden, true, 'a failure with no session has nothing to open');
  assert.equal(view.nodes.clearAll.textContent, 'Clear all ! (1)', 'only the ! card counts for Clear all !');
});
