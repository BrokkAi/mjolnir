#!/usr/bin/env node
// Published packages must not rely on files outside their package directory.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const mode = process.argv[2] ?? 'check';
if (!['check', 'sync'].includes(mode)) throw new Error('usage: node scripts/sync-package-assets.mjs [check|sync]');
const copies = ['mj-core', 'mj-worker', 'mj-client', 'mj-controller', 'mj-chat', 'mj-tui', 'mj-cli', 'mj-desktop', 'voice-worker'].map(dir => ['LICENSE', `${dir}/LICENSE`]);
for (const file of ['README.md', 'licenses/THIRD_PARTY_LICENSES.html', 'licenses/SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt']) copies.push([file, `mj-core/${file}`]);
for (const file of ['docs/DOCKER.md', 'docs/PODMAN.md']) copies.push([file, `mj-controller/${file}`]);
for (const [source, destination] of copies) {
  const expected = fs.readFileSync(path.join(root, source));
  const target = path.join(root, destination);
  const differs = !fs.existsSync(target) || !expected.equals(fs.readFileSync(target));
  if (differs && mode === 'sync') {
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.writeFileSync(target, expected);
  } else if (differs) {
    throw new Error(`${destination} differs from ${source}; run node scripts/sync-package-assets.mjs sync`);
  }
}
console.log(`Package assets ${mode === 'sync' ? 'synchronized' : 'verified'}.`);
