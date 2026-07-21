import path from 'node:path';
import { fileURLToPath } from 'node:url';

import sharp from 'sharp';

const scriptsDir = path.dirname(fileURLToPath(import.meta.url));
const docsDir = path.resolve(scriptsDir, '..');
const source = path.join(docsDir, 'src/assets/mjolnir-social-card.svg');
const output = path.join(docsDir, 'public/og.png');

await sharp(source, { density: 144 })
  .resize(1200, 630)
  .png({ compressionLevel: 9 })
  .toFile(output);

console.log(`Rendered ${path.relative(docsDir, output)} from ${path.relative(docsDir, source)}`);
