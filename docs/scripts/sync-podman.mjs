import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const docsRoot = fileURLToPath(new URL('..', import.meta.url));
for (const guide of [
  {
    source: 'PODMAN.md',
    target: 'podman.md',
    title: 'Podman for Mjolnir',
    description: 'Rootless Podman installation, verification postconditions, and remediation for Mjolnir container targets.',
  },
  {
    source: 'DOCKER.md',
    target: 'docker.md',
    title: 'Docker for Mjolnir',
    description: 'Docker setup, OverlayFS attachments, lifecycle, and verification for Mjolnir container targets.',
  },
]) {
  const sourcePath = join(docsRoot, guide.source);
  const targetPath = join(docsRoot, 'src', 'content', 'docs', guide.target);
  const heading = `# ${guide.title}`;
  const source = readFileSync(sourcePath, 'utf8');
  if (!source.startsWith(`${heading}\n`)) {
    console.error(`${sourcePath} does not start with the expected heading ${JSON.stringify(heading)}.`);
    process.exit(1);
  }
  const body = source.slice(heading.length).replace(/^\s*\n/, '');
  const frontmatter = [
    '---',
    `title: ${JSON.stringify(guide.title)}`,
    `description: ${JSON.stringify(guide.description)}`,
    '---',
    '',
    '',
  ].join('\n');
  writeFileSync(targetPath, frontmatter + body);
  console.log(`Synced ${sourcePath} -> ${targetPath}`);
}
