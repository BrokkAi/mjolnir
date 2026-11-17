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
    this.listeners = {};
  }
  addEventListener(type, listener) { (this.listeners[type] ||= []).push(listener); }
  fire(type, event = {}) { for (const listener of this.listeners[type] || []) listener(event); }
  append(...nodes) { this.children.push(...nodes); }
  insertBefore(node, reference) { this.children.splice(this.children.indexOf(reference), 0, node); }
  setAttribute(name, value) { this.attributes[name] = String(value); }
}

/// The notifications screen running on its real source with hand-built fakes
/// for the daemon (`request`), the router and the DOM.
function screen(rows, { launchFailures = [], texts = {} } = {}) {
  const requests = [];
  const drafts = [];
  const answers = new Map(Object.entries(texts));
  const nav = [];
  const container = new Node('div');
  const context = vm.createContext({
    document: {
      createElement: tag => new Node(tag),
      querySelector: selector => (selector === '#inbox' ? container : (context.errorNode ??= new Node('p'))),
    },
    viewerState: { cursor: { sequence: 1 }, rows: new Map(rows.map(row => [row.id, row])) },
    dismissedNotifications: new Map(),
    pendingActions: new Set(),
    route: { name: 'notifications' },
    requests,
    answers,
    nav,
    snapshot: { launch_failures: launchFailures },
    URLSearchParams,
    dismissedLaunchFailures: new Set(),
    pendingSubmissions: new Map(),
    renderPendingSubmissions: () => {},
    renderRoute: () => context.rendered.push('route'),
    rendered: [],
    drafts,
    draftWriters: new Map(),
    composer: '',
    composerGeneration: 0,
    conversationGeneration: 1,
    draftComposerBaseline: null,
    acknowledged: 0,
    composerText: () => context.composer,
    setComposerText: text => { context.composer = text; context.composerGeneration += 1; },
    updateCommandPalette: () => {},
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
      const answer = answers.get(body ? `${body.action}:${body.session_id}` : url.split('?')[0]);
      if (answer instanceof Error) throw answer;
      if (typeof answer === 'function') return answer();
      return answer ?? null;
    },
  });
  vm.runInContext(sourceBetween('function el(', 'const bootStatus'), context);
  vm.runInContext(sourceBetween('/// A slash command typed in a notification', 'let historyOpen'), context);
  vm.runInContext(sourceBetween('function newCommandId(', '/// Guard against sending twice.'), context);
  vm.runInContext(sourceBetween('function submissionBody(', 'async function submitPrompt('), context);
  vm.runInContext(sourceBetween('function hasNotification(', '/// Notifications, unread output'), context);
  vm.runInContext(sourceBetween('// The notifications screen\n', 'function renderSessions() {'), context);
  const read = expression => vm.runInContext(expression, context);
  read('renderInbox()'); // the screen is drawn before anyone can press a button
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
  assert.equal(view.read('inboxSkipped.some(key => key.startsWith("a|"))'), false, 'a gone session leaves the skip list');
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

test('quick reply builds the composer\'s own action: ! runs a shell command and a slash command is never sent as a prompt', async () => {
  const view = screen([row('a', 'needs_input', 3)]);
  view.render();
  view.nodes.input.value = '!ls -l';
  view.nodes.input.oninput();
  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  assert.deepEqual(plain(view.requests.find(r => r.body?.action === 'run-shell').body).command, 'ls -l');
  assert.equal(view.requests.some(r => r.body?.action === 'prompt'), false);

  // Slash commands are the composer's: see the hand-off tests below.
  assert.equal(view.requests.some(r => r.body?.text === '/model fast'), false);
});

test('Go to Session opens a pending form\'s holder and returns here on Back', () => {
  const view = screen([row('parent', 'needs_input', 3, { form_holder: 'child' }),
    row('child', 'working', 1, { subagent_parent_id: 'parent' })]);
  view.render();
  view.nodes.open.onclick();
  assert.deepEqual(plain(view.nav), [{ name: 'conversation', sessionId: 'child', from: 'notifications' }]);
  // Both the history entry and the viewer's own back control follow the route.
  vm.runInContext(sourceBetween('const ID = ', '/// Go to a route.'), view.context);
  assert.equal(view.read('routeHash(nav[0])'), '#notifications/child');
  assert.deepEqual(plain(view.read('parseRoute("#notifications/child")')), { name: 'conversation', sessionId: 'child', from: 'notifications' });
});

test('the full message loads from the daemon: loading, text, and not available', async () => {
  const view = screen([row('a', 'needs_input', 3, { question_message: 'agent:1' })],
    { texts: { '/api/sessions/a/notification-message': { session_id: 'a', text: 'Line one\n\nLine two' } } });
  assert.equal(view.nodes.body.textContent, 'loading…');
  await settle();
  assert.equal(view.nodes.body.textContent, 'Line one\n\nLine two');

  const missing = screen([row('m', 'important', 3)],
    { texts: { '/api/sessions/m/notification-message': { session_id: 'm', text: null } } });
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
  const view = screen([row('a', 'needs_input', 3), row('b', 'needs_input', 2)],
    { texts: { '/api/sessions/a/notification-message': new Error('offline') } });
  await settle();
  const reads = () => view.requests.filter(r => r.url.includes('/a/notification-message')).length;
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
  const other = screen([row('a', 'needs_input', 3)],
    { texts: { '/api/sessions/a/notification-message': new Error('offline') } });
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
  const reads = () => view.requests.filter(r => r.url.includes('/p/notification-message')).length;
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

test('text, Quick reply and Go to Session all name the form holder', async () => {
  const view = screen([row('parent', 'needs_input', 3, { form_holder: 'child' }),
    row('child', 'working', 1, { subagent_parent_id: 'parent', pending_elicitation_ids: ['r1'] })]);
  view.render();
  view.nodes.input.value = 'yes';
  view.nodes.input.oninput();
  assert.equal(view.read('inboxDrafts.has("child")'), true, 'the draft belongs to the destination');
  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  assert.equal(view.requests.find(r => r.body?.action === 'prompt').body.session_id, 'child');
  view.nodes.open.onclick();
  assert.equal(plain(view.nav).at(-1).sessionId, 'child');
  assert.equal(view.read('inboxDestination(inboxEntries()[0])'), 'child');
});

// Hard-won: review of 045f5fe81: persistent buttons acted on the new first card after a snapshot reordered the stack.
test('a gesture acts on the card it started on, even if a snapshot changes the first card meanwhile', async () => {
  const view = screen([row('a', 'needs_input', 3), row('b', 'needs_input', 2)]);
  view.render();
  view.nodes.clear.fire('pointerdown');
  // A newer question arrives and sorts first before the click lands.
  view.context.viewerState.rows.set('n', row('n', 'needs_input', 9));
  view.render();
  assert.equal(view.ids()[0], 'n');
  view.nodes.clear.onclick();
  await settle();
  const dismissal = view.requests.find(r => r.body?.action === 'dismiss-notification').body;
  assert.equal(dismissal.session_id, 'a', 'not the new first card');
  assert.deepEqual(plain(dismissal.target), { question: { turn_id: 'a' } });

  view.nodes.open.fire('pointerdown');
  view.context.viewerState.rows.set('z', row('z', 'needs_input', 10));
  view.render();
  view.nodes.open.onclick();
  assert.equal(plain(view.nav).at(-1).sessionId, 'n');
});

test('the message text node is replaced only when the text changes', async () => {
  const view = screen([row('a', 'needs_input', 3, { question_message: 'q' })],
    { texts: { '/api/sessions/a/notification-message': { text: 'long message' } } });
  let writes = 0;
  let current = view.nodes.body.textContent;
  Object.defineProperty(view.nodes.body, 'textContent', {
    get: () => current,
    set: value => { writes += 1; current = value; },
  });
  await settle();
  const settled = writes;
  assert.equal(current, 'long message');
  view.render(); view.render();
  assert.equal(writes, settled, 'a snapshot that changes nothing leaves the selection alone');
});

test('a new notification from a skipped session is not deferred', () => {
  const view = screen([row('a', 'needs_input', 3), row('b', 'needs_input', 2)]);
  view.nodes.skip.onclick();
  assert.deepEqual(view.ids(), ['b', 'a']);
  view.context.viewerState.rows.get('a').notification_dismissal = { question: { turn_id: 'a2' } };
  assert.deepEqual(view.ids(), ['a', 'b'], 'the new question is a new card and comes first');
  assert.equal(view.read('inboxSkipped.length'), 0, 'the old deferral is dropped');
});

test('a slash command is handed to an empty composer in page only, and Quick reply clears once it is there', async () => {
  const view = screen([row('a', 'needs_input', 3)], { texts: { '/api/sessions/a/client-state': { draft: '', through_event_ordinal: 0 } } });
  view.nodes.input.value = '/model fast';
  view.nodes.input.oninput();
  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  assert.deepEqual(plain(view.nav), [{ name: 'conversation', sessionId: 'a', from: 'notifications' }]);
  assert.equal(view.nodes.input.value, '/model fast', 'kept until the composer holds it');
  await view.read("restoreDraft('a', 1)");
  assert.equal(view.context.composer, '/model fast');
  assert.equal(view.nodes.input.value, '', 'cleared once placed');
  assert.equal(view.read('inboxDrafts.has("a")'), false);
  assert.deepEqual(view.drafts, [], 'Quick reply never writes the saved draft');
  assert.equal(view.requests.some(r => r.body?.action === 'prompt'), false);
});

test('a slash command never overwrites the conversation draft and keeps the quick reply', async () => {
  const view = screen([row('a', 'needs_input', 3)], { texts: { '/api/sessions/a/client-state': { draft: 'my unsent thoughts' } } });
  view.nodes.input.value = '/model fast';
  view.nodes.input.oninput();
  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  assert.deepEqual(view.nav, []);
  assert.equal(view.read('composerHandoff'), null);
  assert.equal(view.nodes.input.value, '/model fast');
  assert.equal(view.nodes.cardError.textContent, 'Slash commands run in the conversation; your draft there is kept. Go to Session to run it.');
  assert.deepEqual(view.drafts, []);
});

test('when the draft cannot be checked, or the composer is no longer empty, nothing is lost', async () => {
  const failing = screen([row('a', 'needs_input', 3)], { texts: { '/api/sessions/a/client-state': new Error('offline') } });
  failing.nodes.input.value = '/model fast';
  failing.nodes.input.oninput();
  failing.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  assert.deepEqual(failing.nav, []);
  assert.equal(failing.nodes.input.value, '/model fast');
  assert.match(failing.nodes.cardError.textContent, /^Not sent: could not check/);

  const typed = screen([row('a', 'needs_input', 3)], { texts: { '/api/sessions/a/client-state': { draft: '' } } });
  typed.nodes.input.value = '/model fast';
  typed.nodes.input.oninput();
  typed.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  // The person typed in the opened composer before the hand-off landed.
  typed.context.composer = 'hello';
  await typed.read("restoreDraft('a', 1)");
  assert.equal(typed.context.composer, 'hello');
  assert.equal(typed.nodes.input.value, '/model fast', 'the command stays in Quick reply');
  assert.equal(typed.read('composerHandoff'), null);
});

test('the message request carries the identity the card captured, and a conflict is discarded until the next snapshot', async () => {
  const conflict = Object.assign(new Error('the notification changed'), { code: 'notification_changed', status: 409 });
  const view = screen([row('p', 'needs_input', 3, { form_holder: 'c1', question_message: 'agent:p' }),
    row('c1', 'working', 1, { subagent_parent_id: 'p', pending_elicitation_ids: ['r1', 'r2'] })],
  { texts: { '/api/sessions/p/notification-message': conflict } });
  await settle();
  const reads = () => view.requests.filter(r => r.url.includes('/p/notification-message'));
  const params = new URL(reads()[0].url, 'http://x').searchParams;
  assert.deepEqual(Object.fromEntries(params), { holder: 'c1', requests: 'r1,r2', question: '' });
  assert.equal(view.nodes.body.textContent, 'loading…', 'a conflicting answer shows no text');
  assert.equal(view.read('[...inboxTexts.values()].some(entry => entry.state === "ready")'), false, 'nothing is cached under the card');
  view.render(); view.render();
  await settle();
  assert.equal(reads().length, 1, 'the same snapshot does not ask again');
  // The next snapshot brings the new holder; the card asks for that identity.
  view.answers.set('/api/sessions/p/notification-message', { session_id: 'c2', text: 'B asks' });
  view.context.viewerState.rows.set('c2', row('c2', 'working', 1, { subagent_parent_id: 'p', pending_elicitation_ids: ['r9'] }));
  view.context.viewerState.rows.get('p').form_holder = 'c2';
  view.context.viewerState.cursor = { sequence: 2 };
  view.render();
  await settle();
  const latest = new URL(reads().at(-1).url, 'http://x').searchParams;
  assert.deepEqual(Object.fromEntries(latest), { holder: 'c2', requests: 'r9', question: '' });
  assert.equal(view.nodes.body.textContent, 'B asks');
  assert.equal(view.read('inboxDestination(inboxEntries()[0])'), 'c2', 'text and destination name the same holder');
});

test('a card without a Quick reply hides its form even though the form is a flex box', () => {
  const css = readFileSync(new URL('../../../mj-controller/src/web/viewer.css', import.meta.url), 'utf8');
  assert.match(css, /\.inbox-reply\s*\{[^}]*display:\s*flex/);
  assert.match(css, /\.inbox-reply\[hidden\]\s*\{\s*display:\s*none;/);
  const view = screen([row('a', 'important', 1)], { launchFailures: [{ id: 'f', workspace_id: 'w' }] });
  view.nodes.skip.onclick();
  assert.equal(view.nodes.reply.hidden, true);
});
