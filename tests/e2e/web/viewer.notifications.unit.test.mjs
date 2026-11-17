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
    draftTimer: null,
    composer: '',
    composerGeneration: 0,
    conversationGeneration: 1,
    draftComposerBaseline: null,
    currentSession: null,
    promptImages: [],
    acknowledged: 0,
    composerText: () => context.composer,
    setComposerText: text => { context.composer = text; context.composerGeneration += 1; },
    updateCommandPalette: () => {},
    retireConversationRequest: () => {},
    clearConversationContents: () => {},
    clearPromptImages: () => {},
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
    canViewConversation: row => row.capabilities?.open === true,
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
  vm.runInContext(sourceBetween('function saveDraft()', 'function queueDraftSave('), context);
  vm.runInContext(sourceBetween('/// Text from a notification', 'let historyOpen'), context);
  vm.runInContext(sourceBetween('function leaveConversation()', "document.querySelector('#login-form')"), context);
  vm.runInContext(sourceBetween('const ID = ', '/// Go to a route.'), context);
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
  id, title: id.toUpperCase(), attention, last_message_at_ms: at, project_label: 'proj', capabilities: { open: true },
  notification_dismissal: attention === 'needs_input' ? { question: { turn_id: id } } : { read_through: at },
  ...extra,
});

const settle = () => new Promise(resolve => setImmediate(resolve));

test('cards run ? before !, newest first, one per session, without sub-agents or ignored sessions', () => {
  const view = screen([
    row('a', 'important', 5), row('b', 'needs_input', 1), row('c', 'needs_input', 9),
    row('d', 'important', 8), row('f', 'needs_input', 30, { subagent_parent_id: 'a' }),
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

test('the header count includes ? and ! and × sessions', () => {
  const view = screen([row('a', 'needs_input', 3), row('b', 'important', 2),
    row('c', 'failed', 1), row('d', 'unread', 1), row('e', 'unreachable', 1)]);
  assert.equal(view.read('inboxEntries().length'), 4);
  const withFailure = screen([row('a', 'needs_input', 3)], { launchFailures: [{ id: 'f', workspace_id: 'w' }] });
  assert.equal(withFailure.read('inboxEntries().length'), 2, 'launch failures count too');
});

// Hard-won: 759664d52: filtering for ? and ! hid failed and unreachable sessions.
test('session failures follow launch failures and keep their daemon-owned actions', async () => {
  const target = { failure: { generation: 3, state: 'error', last_error: 'startup failed' } };
  const view = screen([
    row('a', 'needs_input', 1),
    row('b', 'failed', 9, {
      launch_error: 'startup failed', workspace_id: 'main', capabilities: { open: false }, notification_dismissal: target,
    }),
    row('c', 'unreachable', 8, { storage_problem: 'disk full', notification_dismissal: null }),
  ], { launchFailures: [{ id: 'f' }] });
  assert.deepEqual(view.ids(), ['a', 'launch-failure:f', 'b', 'c']);
  view.nodes.skip.onclick();
  view.nodes.skip.onclick();
  assert.equal(view.nodes.glyph.textContent, '×');
  assert.equal(view.nodes.glyph.attributes['aria-label'], 'Failed');
  assert.equal(view.nodes.body.textContent, 'startup failed');
  assert.equal(view.nodes.reply.hidden, true);
  view.nodes.open.onclick();
  const detail = { name: 'resume', workspaceId: 'main', sessionId: 'b', from: 'notifications' };
  assert.deepEqual(plain(view.nav.at(-1)), detail);
  assert.deepEqual(plain(view.read('parseRoute(routeHash(nav.at(-1)))')), detail);
  view.context.backButton = {};
  vm.runInContext(sourceBetween('backButton.onclick =', 'resumeDetailBack.onclick ='), view.context);
  view.read('route = parseRoute(routeHash(nav.at(-1))); backButton.onclick()');
  assert.deepEqual(plain(view.nav.at(-1)), { name: 'notifications' });
  view.nodes.clear.onclick();
  await settle();
  assert.deepEqual(plain(view.requests.find(r => r.body?.action === 'dismiss-notification')?.body),
    { action: 'dismiss-notification', session_id: 'b', target });
  assert.equal(view.nodes.glyph.attributes['aria-label'], 'Unreachable');
  assert.equal(view.nodes.body.textContent, 'disk full');
  assert.equal(view.nodes.clear.disabled, true);
  assert.equal(view.nodes.clearAll.hidden, true);
  view.nodes.open.onclick();
  assert.deepEqual(plain(view.nav.at(-1)), { name: 'conversation', sessionId: 'c', from: 'notifications' });
  assert.equal(view.requests.some(r => /\/[bc]\/notification-message/.test(r.url)), false);
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

// Hard-won: 759664d52: Back discarded a hand-off that was marked as already saved.
test('Back saves an untouched slash command handed to an empty composer', async () => {
  const view = screen([row('a', 'needs_input', 3)], { texts: { '/api/sessions/a/client-state': { draft: '', through_event_ordinal: 0 } } });
  view.nodes.input.value = '/model fast';
  view.nodes.input.oninput();
  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  assert.deepEqual(plain(view.nav), [{ name: 'conversation', sessionId: 'a', from: 'notifications' }]);
  assert.equal(view.nodes.input.value, '/model fast', 'kept until the composer holds it');
  view.context.currentSession = 'a';
  await view.read("restoreDraft('a', 1)");
  assert.equal(view.context.composer, '/model fast');
  assert.equal(view.nodes.input.value, '', 'cleared once placed');
  assert.equal(view.read('inboxDrafts.has("a")'), false);
  assert.deepEqual(view.drafts, [], 'placement alone does not write the saved draft');
  view.read('leaveConversation()');
  assert.deepEqual(view.drafts, [['a', '/model fast']]);
  assert.equal(view.context.composer, '');
  assert.equal(view.requests.some(r => r.body?.action === 'prompt'), false);
});

test('a slash command never overwrites the conversation draft and keeps the quick reply', async () => {
  const view = screen([row('a', 'needs_input', 3)], { texts: { '/api/sessions/a/client-state': { draft: 'my unsent thoughts' } } });
  view.nodes.input.value = '/model fast';
  view.nodes.input.oninput();
  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  assert.deepEqual(view.nav, []);
  assert.equal(view.read('composerHandoffs.size'), 0);
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
  assert.equal(typed.read('composerHandoffs.size'), 0);
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

// Hard-won: review of a1ab175cc: forms have no dismissal target, so every request of a session shared one skip key.
test('a new form request or holder after a skip is a new card, not deferred', () => {
  const parent = row('p', 'needs_input', 3, { form_holder: 'c1', notification_dismissal: null });
  const child = (id, requests) => row(id, 'working', 1, { subagent_parent_id: 'p', pending_elicitation_ids: requests });
  const view = screen([parent, child('c1', ['r1']), child('c2', []), row('b', 'needs_input', 2)]);
  view.nodes.skip.onclick();
  assert.deepEqual(view.ids(), ['b', 'p']);
  view.context.viewerState.rows.get('c1').pending_elicitation_ids = ['r2'];
  assert.deepEqual(view.ids(), ['p', 'b'], 'a new request id from the same holder');
  view.render();
  view.nodes.skip.onclick();
  assert.deepEqual(view.ids(), ['b', 'p']);
  view.context.viewerState.rows.get('c2').pending_elicitation_ids = ['r2'];
  view.context.viewerState.rows.get('p').form_holder = 'c2';
  assert.deepEqual(view.ids(), ['p', 'b'], 'a new holder');
  assert.equal(view.read('inboxSkipped.length'), 0);
});

test('an ! card asks for the message at its ReadThrough ordinal, and the cache keys on the same identity as skip', async () => {
  const view = screen([row('a', 'important', 3, { notification_dismissal: { read_through: 41 } })],
    { texts: { '/api/sessions/a/notification-message': { text: 'the reply at 41' } } });
  await settle();
  const params = new URL(view.requests.find(r => r.url.includes('/a/notification-message')).url, 'http://x').searchParams;
  assert.equal(params.get('through'), '41');
  const entry = view.read('inboxEntries()[0]');
  assert.equal(view.read('[...inboxTexts.keys()][0]'), view.read('inboxKey(inboxEntries()[0])'));
  assert.equal(view.read('inboxCard(inboxEntries()[0]).key'), view.read('inboxKey(inboxEntries()[0])'));
  assert.ok(entry);
});

/// A Quick reply whose refusal arrives after `act` has run; resolves with the view.
async function refusedReply(act, { draft = '' } = {}) {
  const view = screen([row('a', 'needs_input', 3), row('b', 'needs_input', 2)],
    { texts: { '/api/sessions/a/client-state': { draft, through_event_ordinal: 0 } } });
  view.nodes.input.value = 'my reply, in full';
  view.nodes.input.oninput();
  let refuse;
  view.answers.set('prompt:a', () => new Promise((_, reject) => {
    refuse = () => reject(Object.assign(new Error('session is closed'), { status: 409 }));
  }));
  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  act(view);
  refuse();
  await settle();
  return view;
}
const notices = view => plain(view.read('[...inboxFailureNotices.values()]'));
const noticeCard = view => view.read('inboxNoticeCards()')[0];
const noticeButton = (view, label) => noticeCard(view)?.children.find(node => node.textContent === label);

test('a refusal after Clear is reported with the full text and handed to an empty composer', async () => {
  const view = await refusedReply(v => { v.context.dismissedNotifications.set('a', JSON.stringify({ question: { turn_id: 'a' } })); });
  assert.deepEqual(view.ids(), ['b'], 'the card is gone');
  const [notice] = notices(view);
  assert.equal(notice.destination, 'a');
  assert.equal(notice.text, 'my reply, in full');
  assert.match(notice.status, /^Not sent: session is closed/);
  assert.match(notice.note, /placed in the session's composer/);
  assert.equal(view.read('composerHandoffs.get("a").text'), 'my reply, in full');
  const cards = view.read('inboxNoticeCards()');
  assert.match(cards[0].children[0].textContent, /^Quick reply to A · Not sent: session is closed/);
  assert.equal(cards[0].children[1].textContent, 'my reply, in full', 'the full unsent text is on the notice');
  // Opening the session shows it, unsent, and the notice goes.
  view.context.currentSession = 'a';
  await view.read("restoreDraft('a', 1)");
  assert.equal(view.context.composer, 'my reply, in full');
  assert.deepEqual(notices(view), []);
  assert.equal(view.read('inboxDrafts.has("a")'), false);
  assert.deepEqual(view.drafts, []);
  view.read('leaveConversation()');
  assert.deepEqual(view.drafts, [['a', 'my reply, in full']]);
});

test('a refusal after the notification changed is reported, and an occupied composer is never overwritten', async () => {
  const view = await refusedReply(v => { v.context.viewerState.rows.get('a').notification_dismissal = { question: { turn_id: 'a2' } }; },
    { draft: 'my unsent thoughts' });
  const [notice] = notices(view);
  assert.equal(notice.text, 'my reply, in full');
  assert.match(notice.note, /composer has a draft, which is kept/);
  assert.equal(view.read('composerHandoffs.size'), 0);
  assert.equal(view.read('inboxDrafts.get("a")'), 'my reply, in full', 'kept where it can be restored');
  assert.deepEqual(view.drafts, []);
  // Restore puts it back in Quick reply, but never over text already there.
  noticeButton(view, 'Restore to Quick reply').onclick();
  assert.deepEqual(notices(view), [], 'restored');
});

// Hard-won: 759664d52: a stale Restore control moved the only visible copy to a missing card.
test('when a card disappears, the refusal stays copyable and can be handed to an empty conversation', async () => {
  const view = await refusedReply(() => {}, { draft: 'already there' });
  const restore = noticeButton(view, 'Restore to Quick reply');
  view.context.dismissedNotifications.set('a', JSON.stringify({ question: { turn_id: 'a' } }));
  restore.onclick();
  assert.equal(noticeButton(view, 'Restore to Quick reply'), undefined);
  assert.equal(noticeCard(view).children[1].textContent, 'my reply, in full');
  assert.ok(noticeButton(view, 'Dismiss notice'));
  noticeButton(view, 'Hand to conversation').onclick();
  assert.deepEqual(plain(view.nav.at(-1)), { name: 'conversation', sessionId: 'a', from: 'notifications' });
  view.context.currentSession = 'a';
  await view.read("restoreDraft('a', 1)");
  assert.equal(view.context.composer, 'already there');
  assert.equal(noticeCard(view).children[1].textContent, 'my reply, in full');

  noticeButton(view, 'Hand to conversation').onclick();
  assert.equal(view.context.composer, 'already there', 'an already open, occupied composer keeps its text');
  view.read("setComposerText(''); saveDraft()");
  view.answers.set('/api/sessions/a/client-state', { draft: '' });
  noticeButton(view, 'Hand to conversation').onclick();
  await settle();
  assert.equal(view.context.composer, 'my reply, in full');
  assert.deepEqual(notices(view), []);
});

test('a refusal while the card is still shown keeps the text in Quick reply and still reports it', async () => {
  const view = await refusedReply(() => {});
  const [notice] = notices(view);
  assert.ok(noticeCard(view).children.some(node => node.textContent === 'Your text is kept in Quick reply.'));
  assert.equal(view.read('composerHandoffs.size'), 0);
  assert.equal(view.nodes.input.value, 'my reply, in full');
  assert.equal(view.nodes.cardError.textContent, 'Not sent: session is closed');
});

test('an unconfirmed delivery is reported but never handed back for resending', async () => {
  const view = screen([row('a', 'needs_input', 3)]);
  view.nodes.input.value = 'hello';
  view.nodes.input.oninput();
  view.answers.set('prompt:a', new Error('network lost'));
  view.nodes.reply.onsubmit({ preventDefault() {} });
  await settle();
  const [notice] = notices(view);
  assert.match(notice.status, /^Delivery unconfirmed/);
  assert.equal(notice.text, 'hello');
  assert.equal(view.read('composerHandoffs.size'), 0);
  assert.match(noticeCard(view).children[0].textContent, /Delivery unconfirmed: it may already be running/);
  assert.equal(noticeCard(view).children[1].textContent, 'hello');
  assert.deepEqual(plain(noticeCard(view).children.filter(node => node.tagName === 'button').map(node => node.textContent)),
    ['Dismiss notice']);
});
