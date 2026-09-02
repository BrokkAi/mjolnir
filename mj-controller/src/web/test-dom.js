// A small DOM for running the viewer's rendering code under Node.
//
// This file is never served. It exists so the renderers in `markdown.js` can
// be exercised by `cargo test` without a browser, and it implements only what
// those renderers actually touch. Keeping it small is the point: a shim that
// grows to imitate a browser stops proving anything, because a bug can then
// hide in the shim instead of in the code under test.

const ELEMENT_NODE = 1;
const TEXT_NODE = 3;
const FRAGMENT_NODE = 11;

class FakeNode {
  constructor(nodeType, nodeName) {
    this.nodeType = nodeType;
    this.nodeName = nodeName;
    this.childNodes = [];
    this.attributes = new Map();
    this.className = '';
    this.value = '';
    this.dataset = {};
    this.listeners = new Map();
    // `<details>` carries its own openness, and the fold under test builds its
    // content on the first toggle, so the shim has to model both.
    this.open = false;
  }

  addEventListener(type, listener) {
    const existing = this.listeners.get(type) || [];
    existing.push(listener);
    this.listeners.set(type, existing);
  }

  /// Fire an event the way a browser would, so a check can open a fold and see
  /// what the fold then built.
  dispatch(type) {
    for (const listener of this.listeners.get(type) || []) listener();
  }

  get tagName() {
    return this.nodeType === ELEMENT_NODE ? this.nodeName : undefined;
  }

  get textContent() {
    if (this.nodeType === TEXT_NODE) return this.value;
    return this.childNodes.map(child => child.textContent).join('');
  }

  set textContent(value) {
    if (this.nodeType === TEXT_NODE) {
      this.value = String(value);
      return;
    }
    this.childNodes = [];
    this.append(new FakeNode(TEXT_NODE, '#text'));
    this.childNodes[0].value = String(value);
  }

  append(...nodes) {
    for (const node of nodes) this.appendChild(node);
  }

  appendChild(node) {
    // A fragment contributes its children and not itself, which is what makes
    // `parent.append(renderMarkdown(...))` behave the way a browser does.
    if (node.nodeType === FRAGMENT_NODE) {
      this.childNodes.push(...node.childNodes);
      node.childNodes = [];
      return node;
    }
    this.childNodes.push(node);
    return node;
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  getAttribute(name) {
    return this.attributes.has(name) ? this.attributes.get(name) : null;
  }
}

export function installDocument() {
  globalThis.document = {
    createElement(name) {
      return new FakeNode(ELEMENT_NODE, String(name).toUpperCase());
    },
    createTextNode(value) {
      const node = new FakeNode(TEXT_NODE, '#text');
      node.value = String(value);
      return node;
    },
    createDocumentFragment() {
      return new FakeNode(FRAGMENT_NODE, '#document-fragment');
    },
  };
}

/// Every element under `root` with this tag name, in document order.
export function elements(root, tagName) {
  const wanted = tagName.toUpperCase();
  const found = [];
  const walk = node => {
    if (node.nodeType === ELEMENT_NODE && node.nodeName === wanted) found.push(node);
    for (const child of node.childNodes) walk(child);
  };
  walk(root);
  return found;
}

/// The one element under `root` with this tag name. Fails loudly when there is
/// not exactly one, because a check that silently reads the first of several is
/// a check that stops noticing.
export function only(root, tagName) {
  const found = elements(root, tagName);
  if (found.length !== 1) {
    throw new Error(`expected exactly one <${tagName}>, found ${found.length}`);
  }
  return found[0];
}

/// Open a `<details>` the way a tap would: set `open`, then fire `toggle`.
export function openFold(details) {
  details.open = true;
  details.dispatch('toggle');
  return details;
}

/// Assert, with a message that says what was expected and what happened.
export function check(condition, message) {
  if (!condition) throw new Error(message);
}

export function checkEqual(actual, expected, message) {
  if (actual !== expected) {
    throw new Error(
      `${message}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`,
    );
  }
}
