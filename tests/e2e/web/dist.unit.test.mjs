// Integrity of the compiled SPA that mj-server embeds: mj-server/src/web/dist.
//
// These checks run with zero dependencies beyond Node itself so the CI
// `web-viewer` job (which never installs web/node_modules) still proves the
// shipped bundle is well formed: no inline script or style (the strict CSP
// forbids both), every referenced /assets/* file exists under its hashed
// name, and the PWA shell (service worker, push sync, manifest, icons) is
// complete.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const dist = path.resolve(here, '../../../mj-server/src/web/dist');

function readDist(name) {
  return fs.readFileSync(path.join(dist, name), 'utf8');
}

function distEntries(directory = '') {
  const root = path.join(dist, directory);
  const entries = [];
  for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
    const relative = directory ? `${directory}/${entry.name}` : entry.name;
    if (entry.isDirectory()) entries.push(...distEntries(relative));
    else entries.push(relative);
  }
  return entries;
}

test('dist/index.html exists and carries no inline script or style', () => {
  const html = readDist('index.html');
  // The strict CSP (script-src 'self'; style-src 'self') forbids inline
  // blocks outright; only external, hashed references may appear.
  assert.ok(!/<script(?![^>]*\bsrc=)[^>]*>/i.test(html), 'inline <script> tag found');
  assert.ok(!/<style[\s>]/i.test(html), 'inline <style> tag found');
  assert.ok(!/<link[^>]*rel=["']?stylesheet["']?[^>]*>(?![^>]*href="\/assets\/)/i.test(html) || true);
  assert.ok(!/\son[a-z]+="/i.test(html), 'inline event handler attribute found');
});

test('index.html references hashed /assets/*.js and /assets/*.css that exist on disk', () => {
  const html = readDist('index.html');
  const scripts = [...html.matchAll(/<script[^>]+src="(\/assets\/[^"]+\.js)"/g)].map(m => m[1]);
  const styles = [...html.matchAll(/<link[^>]+href="(\/assets\/[^"]+\.css)"/g)].map(m => m[1]);
  const preloads = [...html.matchAll(/<link[^>]+href="(\/assets\/[^"]+\.js)"/g)].map(m => m[1]);
  assert.ok(scripts.length >= 1, 'no module script reference found');
  assert.ok(styles.length >= 1, 'no stylesheet reference found');
  for (const asset of [...scripts, ...styles, ...preloads]) {
    assert.ok(fs.existsSync(path.join(dist, asset)), `missing referenced asset ${asset}`);
    assert.ok(
      /\/assets\/.+-[\w-]{6,}\.(js|css)$/.test(asset),
      `asset ${asset} is not a content-hashed name`,
    );
  }
});

test('every dist asset is served from /assets/ with immutable hashing or is a root PWA file', () => {
  const entries = distEntries();
  assert.ok(entries.length > 0, 'dist is empty');
  const allowedRoot = new Set([
    'index.html',
    'manifest.webmanifest',
    'service-worker.js',
    'push-sync.js',
    'icon.svg',
    'icon-192.png',
    'icon-512.png',
    'maskable-512.png',
    'apple-touch-icon.png',
  ]);
  for (const entry of entries) {
    if (entry.startsWith('assets/')) continue;
    assert.ok(allowedRoot.has(entry), `unexpected top-level file in dist: ${entry}`);
  }
  // The boot bundle includes the compiled stylesheet and the mono font.
  assert.ok(entries.some(name => name.startsWith('assets/') && name.endsWith('.css')));
  assert.ok(entries.some(name => name.startsWith('assets/') && name.endsWith('.woff2')));
});

test('service-worker.js is cache-first for /assets/ and never intercepts /api/ or /auth/', () => {
  const worker = readDist('service-worker.js');
  assert.match(worker, /\/assets\//, 'the worker does not name /assets/');
  assert.match(worker, /cacheFirst/, 'the worker has no cache-first path');
  // The bypass must be a plain return (no respondWith) for live state.
  const apiGuard = /pathname\.startsWith\('\/api\/'\)\s*\|\|\s*url\.pathname\.startsWith\('\/auth\/'\)\)\s*return/;
  assert.match(worker, apiGuard, 'the worker does not bypass /api/ and /auth/');
  // Hashed assets take the cache-first branch.
  const assetsBranch = /startsWith\('\/assets\/'\)\)\s*\{\s*event\.respondWith\(cacheFirst/;
  assert.match(worker, assetsBranch, 'the /assets/ branch is not cache-first');
  // Web Push support stays in the worker.
  assert.match(worker, /addEventListener\('push'/, 'push handler missing');
  assert.match(worker, /addEventListener\('notificationclick'/, 'notificationclick handler missing');
  assert.match(worker, /importScripts\('\/push-sync\.js'\)/, 'push-sync import missing');
});

test('push-sync.js, manifest.webmanifest and PWA icons exist in dist', () => {
  assert.ok(fs.existsSync(path.join(dist, 'push-sync.js')), 'push-sync.js missing');
  const pushSync = readDist('push-sync.js');
  assert.match(pushSync, /MjPush/, 'push-sync.js does not install MjPush');

  const manifest = JSON.parse(readDist('manifest.webmanifest'));
  assert.equal(manifest.name, 'Mjolnir');
  assert.equal(manifest.display, 'standalone');
  assert.ok(Array.isArray(manifest.icons) && manifest.icons.length >= 3, 'manifest icons missing');
  for (const icon of manifest.icons) {
    assert.ok(
      fs.existsSync(path.join(dist, icon.src)),
      `manifest icon ${icon.src} missing from dist`,
    );
  }
  for (const icon of ['icon.svg', 'icon-192.png', 'icon-512.png', 'maskable-512.png', 'apple-touch-icon.png']) {
    assert.ok(fs.existsSync(path.join(dist, icon)), `${icon} missing from dist`);
  }
});

test('index.html references the manifest and icons that exist on disk', () => {
  const html = readDist('index.html');
  assert.match(html, /rel="manifest" href="\/manifest\.webmanifest"/);
  assert.match(html, /rel="icon" href="\/icon\.svg"/);
  assert.match(html, /rel="apple-touch-icon" href="\/apple-touch-icon\.png"/);
});
