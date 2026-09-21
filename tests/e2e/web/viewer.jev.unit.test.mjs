import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';
const source = readFileSync(new URL('../../../mj-controller/src/web/viewer.js', import.meta.url), 'utf8');
const inspector = source.slice(source.indexOf('async function openJevDecisions('), source.indexOf('/// One session row.'));
function setup(request) {
  function node(tag, cls = '', text = '') {
    return { tag, textContent: text, children: [], listeners: {}, open: false,
      append(...children) { this.children.push(...children); },
      replaceChildren(...children) { this.children = children; },
      addEventListener(name, callback) { this.listeners[name] = callback; },
      showModal() { this.open = true; },
      close() { this.open = false; this.listeners.close?.(); },
      remove() { this.removed = true; },
    };
  }
  const body = node('body');
  const context = vm.createContext({ el: node, button: text => node('button', '', text), document: { body }, request, AbortController, Date, JSON, encodeURIComponent });
  vm.runInContext(`${inspector}\nglobalThis.openInspector = openJevDecisions;`, context);
  return { open: context.openInspector, body };
}
const record = { id: 'check/1', kind: 'activity', status: 'applied', started_at_ms: 1, checked: 'What is next?', answer: 'Finished', action: 'Marked ready', scope: 'Current request and runtime facts', technical: { request: { text: '<script>private input</script>' } } };
function texts(node) { return [node.textContent, ...node.children.flatMap(texts)].join('\n'); }
const tick = () => new Promise(resolve => setImmediate(resolve));
test('Jev list fetches exact inputs only on expansion and cancels reads on close', async () => {
  const calls = [];
  const { open, body } = setup(async (url, options) => {
    calls.push({ url, options });
    return { warnings: [], decisions: [url.endsWith('/jev-decisions') ? { ...record, technical: undefined } : record] };
  });
  await open('session/1');
  assert.equal(calls.length, 1);
  assert.match(calls[0].url, /session%2F1\/jev-decisions$/);
  const modal = body.children[0];
  const content = modal.children.at(-1);
  const row = content.children[0];
  row.open = true; row.listeners.toggle(); await tick();
  assert.equal(calls.length, 2);
  assert.match(calls[1].url, /check%2F1$/);
  const detail = row.children.at(-1);
  assert.match(texts(detail), /Action: Marked ready/);
  const technical = detail.children.at(-1);
  assert.equal(technical.tag, 'details');
  assert.equal(technical.open, false);
  assert.equal(technical.children[1].tag, 'pre');
  assert.match(technical.children[1].textContent, /<script>private input/);
  modal.close();
  assert.equal(calls[1].options.signal.aborted, true);
});
test('rotated decision and unavailable worker are explained separately', async () => {
  const { open, body } = setup(async () => ({ warnings: ['Worker details unavailable'], decisions: [] }));
  await open('s', 'old');
  assert.match(texts(body), /Details no longer available/);
  assert.match(texts(body), /Worker details unavailable/);
});
test('inspector can close immediately while its first request is pending', async () => {
  let finish; let signal;
  const { open, body } = setup((_url, options) => { signal = options.signal; return new Promise(resolve => { finish = resolve; }); });
  const opened = open('s');
  const modal = body.children[0];
  assert.match(texts(modal), /Loading/);
  modal.close();
  assert.equal(signal.aborted, true);
  finish({ warnings: [], decisions: [record] }); await opened;
  assert.equal(modal.removed, true);
  assert.doesNotMatch(texts(modal), /Marked ready/);
});
