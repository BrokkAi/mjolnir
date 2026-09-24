import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../../../mj-controller/src/web/viewer.js', import.meta.url), 'utf8');
const projectFunctions = source.slice(source.indexOf('function freshProjectPicker()'), source.indexOf('\nfunction textField('));

function fixture() {
  const requests = [];
  const advances = [];
  const context = vm.createContext({
    AbortController, clearTimeout, setTimeout,
    newDraft: null, newError: { textContent: '' }, renderNewForm() {},
    advanceNew: async () => advances.push(context.newDraft.bundleId),
    request: (url, options) => new Promise((resolve, reject) => {
      requests.push({ url, body: JSON.parse(options.body), signal: options.signal, resolve, reject });
    }),
  });
  vm.runInContext(projectFunctions, context);
  const fresh = () => ({
    targetId: 'remote', bundleSource: 'example/app', creatingBundle: false,
    projectPicker: vm.runInContext('freshProjectPicker()', context),
  });
  context.newDraft = fresh();
  const run = expression => vm.runInContext(expression, context);
  return { context, requests, advances, fresh, run };
}

const answer = name => ({ entries: [{ name, source: `https://github.com/example/${name}`, kind: 'repository' }], directory: null, parent: null, truncated: false });

test('discovery aborts superseded searches and ignores stale success and failure even if transport ignores abort', async () => {
  const { context, requests, run } = fixture();
  const picker = context.newDraft.projectPicker;
  picker.mode = 'github';
  const old = run('discoverProjects()');
  picker.query = 'new';
  const current = run('discoverProjects()');
  assert.equal(requests[0].signal.aborted, true);
  assert.deepEqual(requests[1].body, { kind: 'github', query: 'new' });
  requests[0].resolve(answer('old'));
  await old;
  assert.equal(picker.entries.length, 0);
  assert.equal(picker.loading, true);
  requests[1].resolve(answer('new'));
  await current;
  assert.equal(picker.entries[0].name, 'new');
  const failing = run('discoverProjects()');
  const replacement = run('discoverProjects()');
  requests[2].reject(new Error('old failure'));
  await failing;
  assert.equal(picker.error, '');
  assert.equal(picker.loading, true);
  requests[3].resolve(answer('replacement'));
  await replacement;
  assert.equal(picker.entries[0].name, 'replacement');
});

test('replacing the wizard cancels discovery and cannot insert results into its new picker', async () => {
  const { context, requests, fresh, run } = fixture();
  context.newDraft.projectPicker.mode = 'github';
  const old = run('discoverProjects()');
  run('cancelProjectRequests()');
  context.newDraft = fresh();
  requests[0].resolve(answer('old'));
  await old;
  assert.equal(requests[0].signal.aborted, true);
  assert.equal(context.newDraft.projectPicker.entries.length, 0);
  assert.equal(context.newDraft.projectPicker.loaded, false);
});

test('folder filtering uses controller discovery while target browsing uses host-aware completion', async () => {
  const { context, requests, run } = fixture();
  const picker = context.newDraft.projectPicker;
  picker.mode = 'directory';
  picker.path = '/controller';
  picker.filter = 'omitted';
  const local = run('discoverProjects()');
  assert.equal(requests[0].url, '/api/projects/discover');
  assert.deepEqual(requests[0].body, { kind: 'directory', path: '/controller', filter: 'omitted' });
  requests[0].resolve({ entries: [], directory: '/controller', parent: '/', truncated: false });
  await local;
  picker.mode = 'target-directory';
  picker.path = 'C:\\work\\';
  picker.filter = 'api';
  const remote = run('discoverProjects()');
  assert.equal(requests[1].url, '/api/paths/complete');
  assert.deepEqual(requests[1].body, { target_id: 'remote', prefix: 'C:/work/api', kind: 'directories' });
  requests[1].resolve({ candidates: ['C:/work/api/'], truncated: true });
  await remote;
  assert.equal(picker.directory, 'C:/work/');
  assert.equal(picker.parent, 'C:/');
  assert.equal(picker.entries[0].source, 'C:/work/api/');
  assert.equal(picker.truncated, true);
});

test('one source creates exact repository membership once and proceeds with the returned id without a snapshot', async () => {
  const { context, requests, advances, run } = fixture();
  const pending = run('createNewProject()');
  await run('createNewProject()');
  assert.equal(requests.length, 1);
  assert.equal(requests[0].url, '/api/bundles');
  assert.deepEqual(requests[0].body, { sources: ['example/app'] });
  requests[0].resolve({ bundle_id: 'exact-project' });
  await pending;
  assert.equal(context.newDraft.bundleId, 'exact-project');
  assert.equal(context.newDraft.createdBundleId, 'exact-project');
  assert.deepEqual(advances, ['exact-project']);
  assert.equal(context.newDraft.creatingBundle, false);
});

test('a cancelled source create cannot overwrite a newer create on the same wizard', async () => {
  const { context, requests, advances, run } = fixture();
  const old = run('createNewProject("example/old")');
  run('cancelProjectRequests()');
  const current = run('createNewProject("example/current")');
  assert.equal(requests[0].signal.aborted, true);
  requests[0].resolve({ bundle_id: 'old' });
  await old;
  assert.equal(context.newDraft.bundleId, undefined);
  assert.equal(context.newDraft.creatingBundle, true);
  requests[1].resolve({ bundle_id: 'current' });
  await current;
  assert.deepEqual(advances, ['current']);
  assert.equal(context.newDraft.bundleId, 'current');
});

test('source failure preserves entered text for retry and a replaced wizard ignores late errors', async () => {
  const { context, requests, advances, fresh, run } = fixture();
  const first = run('createNewProject()');
  requests[0].reject(new Error('Repository unavailable'));
  await first;
  assert.equal(context.newError.textContent, 'Repository unavailable');
  assert.equal(context.newDraft.bundleSource, 'example/app');
  assert.equal(context.newDraft.creatingBundle, false);
  const retry = run('createNewProject()');
  context.newDraft = fresh();
  requests[1].reject(new Error('Old request failed'));
  await retry;
  assert.equal(context.newError.textContent, '');
  assert.equal(context.newDraft.creatingBundle, false);
  assert.deepEqual(advances, []);
});
