import assert from 'node:assert/strict';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const goldenDirectory = join(here, 'golden');
// CI uploads test-results/, so a mismatch there keeps the whole actual output.
const actualDirectory = join(here, 'test-results', 'golden-actual');

function normalizeOneTrailingNewline(text) {
  return text.endsWith('\n') ? text.slice(0, -1) : text;
}

export function assertGolden(name, actual) {
  const path = join(goldenDirectory, `${name}.txt`);
  if (process.env.MJ_UPDATE_GOLDEN === '1') {
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, `${normalizeOneTrailingNewline(actual)}\n`);
    return;
  }

  if (!existsSync(path)) {
    throw new Error(
      `Missing golden ${path}; set MJ_UPDATE_GOLDEN=1 to create or update it`,
    );
  }
  const expected = normalizeOneTrailingNewline(readFileSync(path, 'utf8'));
  const normalized = normalizeOneTrailingNewline(actual);
  if (normalized === expected) return;
  const actualPath = join(actualDirectory, `${name}.txt`);
  mkdirSync(actualDirectory, { recursive: true });
  writeFileSync(actualPath, `${normalized}\n`);
  assert.strictEqual(
    normalized,
    expected,
    `Golden output differs at ${path} (actual output: ${actualPath}); set MJ_UPDATE_GOLDEN=1 to update it`,
  );
}
