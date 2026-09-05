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
  const source = sourceBetween('function sessionCard(session) {', '\n/// A glyph that repeats');
  const context = vm.createContext({
    LIFECYCLE_ICON: { live: '●' },
    pendingActions: new Set(),
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
    ['Rename', 'Cancel', 'Stop', 'Resume'],
  );
  assert.ok(!buttons.some(button => button.textContent === 'Open'));

  const closed = vm.runInContext(
    `render({ id: 'closed', title: 'Closed', lifecycle: 'stopped', state: 'Stopped', target_id: 'target', profile_id: 'profile', capabilities: {} })`,
    cardHarness(),
  );
  assert.equal(closed.attributes.role, undefined);
  assert.equal(closed.attributes.tabindex, undefined);
});

function cardEventHarness() {
  const source = sourceBetween(
    '/// Find a session card for an event',
    '\n// ---------------------------------------------------------------------------\n// The other pages',
  );
  const navigations = [];
  const context = vm.createContext({
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
    conversationSide: makeNode('details'),
    conversationSummary: makeNode('summary'),
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
