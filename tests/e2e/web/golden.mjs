import assert from 'node:assert/strict';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const goldenDirectory = join(dirname(fileURLToPath(import.meta.url)), 'golden');

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
  assert.strictEqual(
    normalizeOneTrailingNewline(actual),
    expected,
    `Golden output differs at ${path}; set MJ_UPDATE_GOLDEN=1 to update it`,
  );
}
