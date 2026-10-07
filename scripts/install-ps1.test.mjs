import assert from 'node:assert/strict';
import { copyFileSync, appendFileSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync, existsSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import test from 'node:test';

const root = resolve(import.meta.dirname, '..');
const target = 'x86_64-pc-windows-msvc';

// Build a release zip the way release.yml lays it out. node.exe stands in for
// mj.exe because the installer runs `mj.exe --version` before installing;
// bytes appended to a PE image are ignored, so each release's controller
// stays runnable and distinguishable.
function releaseZip(dir, tag) {
  const name = `brokk-mjolnir-${tag}-${target}`;
  const bundle = join(dir, tag, name);
  mkdirSync(bundle, { recursive: true });
  copyFileSync(process.execPath, join(bundle, 'mj.exe'));
  appendFileSync(join(bundle, 'mj.exe'), `release ${tag}`);
  for (const file of ['mj-desktop.exe', 'mj-worker-x86_64-unknown-linux-musl', 'mj-worker-aarch64-unknown-linux-musl', 'README.md']) {
    writeFileSync(join(bundle, file), `${file} ${tag}`);
  }
  const zip = join(dir, `${name}.zip`);
  const tar = join(process.env.SystemRoot, 'System32', 'tar.exe');
  const result = spawnSync(tar, ['-a', '-c', '-f', zip, '-C', join(dir, tag), name], { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr);
  const bytes = readFileSync(zip);
  return { name: `${name}.zip`, bytes, sum: `${createHash('sha256').update(bytes).digest('hex')}  ${name}.zip\n` };
}

async function serveReleases(releases) {
  const server = createServer((request, response) => {
    const tag = request.url.endsWith('/releases/latest') ? releases.latest : request.url.split('/releases/tags/')[1];
    if (tag && releases[tag]) {
      const { zip, sum } = releases[tag];
      const base = `http://127.0.0.1:${server.address().port}/download/${tag}`;
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify({
        tag_name: tag,
        assets: [
          { name: zip.name, browser_download_url: `${base}/${zip.name}` },
          { name: `${zip.name}.sha256`, browser_download_url: `${base}/${zip.name}.sha256` },
        ],
      }));
      return;
    }
    const [, , downloadTag, file] = request.url.split('/');
    const release = releases[downloadTag];
    if (release && file === release.zip.name) return response.end(release.zip.bytes);
    if (release && file === `${release.zip.name}.sha256`) return response.end(release.sum);
    response.statusCode = 404;
    response.end();
  });
  await new Promise(done => server.listen(0, '127.0.0.1', done));
  return server;
}

// Asynchronous, so this process keeps serving the release while the installer runs.
function install(server, installDir, version) {
  const child = spawn('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', join(root, 'install.ps1')], {
    env: {
      ...process.env,
      MJOLNIR_GITHUB_API: `http://127.0.0.1:${server.address().port}`,
      MJOLNIR_INSTALL_DIR: installDir,
      MJOLNIR_VERSION: version ?? '',
      // Never touch the account's persistent Path from a test.
      MJOLNIR_NO_MODIFY_PATH: '1',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let stdout = '';
  let stderr = '';
  child.stdout.on('data', chunk => { stdout += chunk; });
  child.stderr.on('data', chunk => { stderr += chunk; });
  return new Promise((done, fail) => {
    child.once('error', fail);
    child.once('close', status => done({ status, stdout, stderr }));
  });
}

test('the PowerShell installer installs, upgrades a running mj.exe, and refuses a bad checksum', {
  skip: process.platform !== 'win32' && 'the installer targets native Windows',
  timeout: 300_000,
}, async t => {
  const dir = mkdtempSync(join(tmpdir(), 'mj-install-ps1-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const first = releaseZip(dir, 'v9.9.9');
  const second = releaseZip(dir, 'v9.9.10');
  const releases = {
    latest: 'v9.9.9',
    'v9.9.9': { zip: first, sum: first.sum },
    'v9.9.10': { zip: second, sum: second.sum },
    // The sidecar names a different archive's hash.
    'v9.9.11': { zip: second, sum: first.sum.replace(first.name, second.name) },
  };
  const server = await serveReleases(releases);
  t.after(() => server.close());
  const installDir = join(dir, 'Programs', 'Mjolnir', 'bin');
  const installed = file => readFileSync(join(installDir, file));
  const endsWith = (bytes, text) => bytes.subarray(bytes.length - text.length).toString() === text;

  const fresh = await install(server, installDir);
  assert.equal(fresh.status, 0, fresh.stdout + fresh.stderr);
  assert.deepEqual(readdirSync(installDir).sort(), [
    'mj-desktop.exe', 'mj-worker-aarch64-unknown-linux-musl', 'mj-worker-x86_64-unknown-linux-musl', 'mj.exe',
  ]);
  assert.ok(endsWith(installed('mj.exe'), 'release v9.9.9'));

  // Windows refuses to overwrite a running image; the installer must still
  // replace it, as it must whenever the daemon or a terminal is running.
  const running = spawn(join(installDir, 'mj.exe'), ['-e', 'setTimeout(() => {}, 120000)'], { stdio: 'ignore' });
  t.after(() => running.kill());
  await new Promise(done => running.once('spawn', done));
  const upgrade = await install(server, installDir, 'v9.9.10');
  assert.equal(upgrade.status, 0, upgrade.stdout + upgrade.stderr);
  assert.ok(endsWith(installed('mj.exe'), 'release v9.9.10'));
  assert.equal(installed('mj-desktop.exe').toString(), 'mj-desktop.exe v9.9.10');
  const leftovers = readdirSync(installDir).filter(name => name.startsWith('.mj-'));
  assert.deepEqual(leftovers.map(name => name.replace(/-[0-9a-f]{32}$/, '')), ['.mj-replaced-mj.exe']);

  const corrupt = await install(server, installDir, 'v9.9.11');
  assert.notEqual(corrupt.status, 0, corrupt.stdout);
  assert.match(corrupt.stderr + corrupt.stdout, /checksum mismatch/);
  assert.ok(endsWith(installed('mj.exe'), 'release v9.9.10'));

  // Once nothing runs the replaced image, the next install removes it.
  running.kill();
  await new Promise(done => running.once('exit', done));
  const repair = await install(server, installDir, 'v9.9.10');
  assert.equal(repair.status, 0, repair.stdout + repair.stderr);
  assert.ok(!readdirSync(installDir).some(name => name.startsWith('.mj-')));
  assert.ok(existsSync(join(installDir, 'mj.exe')));
});
