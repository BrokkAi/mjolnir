const { test, expect } = require('@playwright/test');
const path = require('node:path');

test.use({ viewport: { width: 390, height: 844 }, serviceWorkers: 'block' });

const SESSION_ID = 'attachment-session';
const ONE_PIXEL_PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=',
  'base64',
);

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit += 1)
      crc = (crc >>> 1) ^ (crc & 1 ? 0xedb88320 : 0);
  }
  const output = Buffer.alloc(4);
  output.writeUInt32BE((crc ^ 0xffffffff) >>> 0);
  return output;
}

function pngWithId(id) {
  const type = Buffer.from('tEXt');
  const data = Buffer.from(`id\0${id}`);
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  return Buffer.concat([
    ONE_PIXEL_PNG.subarray(0, 33),
    length,
    type,
    data,
    crc32(Buffer.concat([type, data])),
    ONE_PIXEL_PNG.subarray(33),
  ]);
}

function snapshot() {
  return {
    revision: 1,
    generated_at: '2026-09-07T00:00:00Z',
    workspaces: [{ id: 'workspace-1', name: 'Attachments' }],
    sessions: [{
      id: SESSION_ID,
      workspace_id: 'workspace-1',
      title: 'Attachment browser test',
      harness_kind: 'codex',
      profile_id: 'codex',
      bundle_id: 'bundle-1',
      target_id: 'local',
      state: 'running',
      created_at: '2026-09-07T00:00:00Z',
      updated_at: '2026-09-07T00:00:00Z',
      has_error: false,
      preview: [],
      queued_prompts: [],
      active_user_shells: [],
      pending_elicitations: [],
      conversation_available: true,
      prompt_images_supported: true,
      incompatible_resume_targets: [],
      compatible_resume_targets: ['local'],
      project_label: 'attachments',
      project_key: 'attachments',
      lifecycle: 'live',
      latest_event_ordinal: 1,
      activity: '',
      operation: null,
      chat_phase: 'idle',
      is_idle: true,
      config_options: [],
      plan_mode_active: false,
      turn_review: null,
      available_commands: [],
      capabilities: {
        open: true,
        prompt: true,
        run_shell: false,
        cancel_turn: false,
        cancel_operation: false,
        stop: false,
        rename: false,
        resume: false,
        set_config: false,
        set_plan_mode: false,
      },
    }],
    profiles: [{ id: 'codex', harness_kind: 'codex' }],
    targets: [{ id: 'local', kind: 'local', requires_project_directory: false }],
    bundles: [{ id: 'bundle-1', primary_repository: null, repositories: [] }],
    review_config: { enabled: false, tier: 'quick', profile: null },
  };
}

async function mockViewer(page, uploadHandler) {
  const root = path.resolve(__dirname, '../../../mj-controller/src/web');
  await page.route('**/*', route => {
    const pathname = new URL(route.request().url()).pathname;
    const file = pathname === '/' ? 'viewer.html' : pathname.slice(1);
    if (!['viewer.html', 'viewer.js', 'viewer.css', 'markdown.js', 'tool-output.js', 'manifest.webmanifest', 'icon.svg'].includes(file))
      return route.fulfill({ status: 404, body: '' });
    const asset = file === 'icon.svg' ? path.join(root, '../icons/icon.svg') : path.join(root, file);
    return route.fulfill({ path: asset });
  });
  const state = { snapshot: snapshot(), actions: [], draft: '' };
  await page.route('**/api/snapshot', route => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify(state.snapshot),
  }));
  await page.route('**/api/events', route => route.fulfill({
    status: 200,
    headers: { 'content-type': 'text/event-stream' },
    body: ': mocked event stream\n\n',
  }));
  await page.route(`**/api/conversations/${SESSION_ID}/read`, route => route.fulfill({ status: 204, body: '' }));
  await page.route(`**/api/conversations/${SESSION_ID}*`, route => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify({ latest_seq: 0, window_start_seq: 0, reset: false, entries: [] }),
  }));
  await page.route(`**/api/sessions/${SESSION_ID}/client-state`, route => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify({ draft: state.draft, through_event_ordinal: 0 }),
  }));
  await page.route(`**/api/sessions/${SESSION_ID}/draft`, async route => {
    const body = JSON.parse(route.request().postData() || '{}');
    state.draft = body.draft || '';
    await route.fulfill({ status: 204, body: '' });
  });
  await page.route(`**/api/sessions/${SESSION_ID}/attachments`, uploadHandler);
  await page.route('**/api/actions', async route => {
    const body = JSON.parse(route.request().postData() || '{}');
    state.actions.push(body);
    await route.fulfill({ status: 202, body: '' });
  });
  await page.goto('https://viewer.test/#workspace/workspace-1');
  await expect(page.locator('#sessions .session')).toHaveCount(1);
  await page.locator('#sessions .session h3').click();
  await expect(page).toHaveURL(new RegExp(`#conversation/${SESSION_ID}$`));
  return state;
}

function file(name, type = 'image/png', buffer = ONE_PIXEL_PNG) {
  return { name, mimeType: type, buffer };
}

test('uploads in selection order with two requests in flight and sends references', async ({ page }) => {
  let active = 0;
  let maximum = 0;
  let started = 0;
  const releaseById = new Map();
  const state = await mockViewer(page, async route => {
    const requestOrder = ++started;
    const body = route.request().postDataBuffer();
    const id = [1, 2, 3, 4].find(candidate => body.includes(Buffer.from(`id\0${candidate}`))) || requestOrder;
    active += 1;
    maximum = Math.max(maximum, active);
    await new Promise(resolve => releaseById.set(id, resolve));
    active -= 1;
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        data_base64: '',
        mime_type: 'image/png',
        width: 1,
        height: 1,
        attachment: { sha256: `digest-${id}`, mime_type: 'image/png', size: 68, width: 1, height: 1 },
      }),
    });
  });

  await page.locator('#image-picker').setInputFiles([
    file('first.png', 'image/png', pngWithId(1)),
    file('second.png', 'image/png', pngWithId(2)),
    file('third.png', 'image/png', pngWithId(3)),
    file('fourth.png', 'image/png', pngWithId(4)),
  ]);
  await expect(page.locator('#attachments .attachment')).toHaveCount(4);
  await expect(page.locator('#send-button')).toBeDisabled();
  await expect.poll(() => maximum).toBe(2);
  expect(await page.locator('#attachments .attachment-processing')).toHaveCount(4);

  const release = id => {
    const resolve = releaseById.get(id);
    expect(resolve, `upload ${id} was not in flight`).toBeTruthy();
    releaseById.delete(id);
    resolve();
  };
  await expect.poll(() => releaseById.has(1) && releaseById.has(2)).toBe(true);
  release(2);
  await expect.poll(() => releaseById.has(3)).toBe(true);
  release(1);
  await expect.poll(() => releaseById.has(4)).toBe(true);
  release(3);
  release(4);
  await expect(page.locator('#attachments .attachment-ready')).toHaveCount(4);
  await expect(page.locator('#send-button')).toBeEnabled();

  await page.locator('#prompt-text').fill('inspect these photos');
  await page.locator('#send-button').click();
  await expect.poll(() => state.actions.length).toBe(1);
  expect(state.actions[0].images.map(image => image.attachment.sha256)).toEqual([
    'digest-1', 'digest-2', 'digest-3', 'digest-4',
  ]);
  expect(state.actions[0].images.every(image => image.data_base64 === '')).toBe(true);
});

test('removing an in-flight upload ignores its late response and keeps the text draft', async ({ page }) => {
  let started = false;
  let release;
  const state = await mockViewer(page, async route => {
    started = true;
    await new Promise(resolve => { release = resolve; });
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        data_base64: '',
        mime_type: 'image/png',
        width: 1,
        height: 1,
        attachment: { sha256: 'late-digest', mime_type: 'image/png', size: 68, width: 1, height: 1 },
      }),
    });
  });

  await page.locator('#image-picker').setInputFiles(file('late.png', 'image/png', pngWithId(9)));
  await expect.poll(() => started).toBe(true);
  await expect(page.locator('#attachments .attachment-processing')).toHaveCount(1);
  await page.locator('#prompt-text').fill('keep this draft');
  await expect.poll(() => state.draft).toBe('keep this draft');
  await page.getByRole('button', { name: 'Remove late.png' }).click();
  await expect(page.locator('#attachments .attachment')).toHaveCount(0);

  release();
  await expect.poll(() => state.actions.length).toBe(0);
  await page.waitForTimeout(100);
  await expect(page.locator('#attachments .attachment')).toHaveCount(0);

  await page.reload();
  await expect(page.locator('#prompt-text')).toHaveText('keep this draft');
  await expect(page.locator('#attachments .attachment')).toHaveCount(0);
});

test('the eleventh selected image is refused while the first ten remain sendable', async ({ page }) => {
  let uploads = 0;
  const state = await mockViewer(page, async route => {
    uploads += 1;
    const body = route.request().postDataBuffer();
    const id = Number(body.toString('latin1').match(/id\0(\d+)/)?.[1]);
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        data_base64: '',
        mime_type: 'image/png',
        width: 1,
        height: 1,
        attachment: { sha256: `digest-${id}`, mime_type: 'image/png', size: 68, width: 1, height: 1 },
      }),
    });
  });

  await page.locator('#image-picker').setInputFiles(
    Array.from({ length: 11 }, (_, index) => file(`photo-${index + 1}.png`, 'image/png', pngWithId(index + 1))),
  );
  await expect(page.locator('#attachments .attachment')).toHaveCount(10);
  await expect(page.locator('#conversation-error')).toHaveText('A prompt may contain at most 10 images.');
  await expect.poll(() => uploads).toBe(10);
  await expect(page.locator('#attachments .attachment-ready')).toHaveCount(10);
  await expect(page.locator('#send-button')).toBeEnabled();

  await page.locator('#prompt-text').fill('inspect ten photos');
  await page.locator('#send-button').click();
  await expect.poll(() => state.actions.length).toBe(1);
  expect(state.actions[0].images.map(image => image.attachment.sha256)).toEqual(
    Array.from({ length: 10 }, (_, index) => `digest-${index + 1}`),
  );
  expect(state.actions[0].images.every(image => image.data_base64 === '')).toBe(true);
});

test('unsupported images remain visibly failed until removed', async ({ page }) => {
  const state = await mockViewer(page, async route => route.fulfill({ status: 500, body: '' }));
  await page.locator('#image-picker').setInputFiles(file('photo.gif', 'image/gif'));
  await expect(page.locator('#attachments .attachment-failed')).toContainText(
    'use JPEG, PNG, or WebP',
  );
  await expect(page.locator('#send-button')).toBeDisabled();
  await page.getByRole('button', { name: 'Remove photo.gif' }).click();
  await expect(page.locator('#attachments .attachment')).toHaveCount(0);
  await expect(page.locator('#send-button')).toBeEnabled();
  await page.locator('#prompt-text').fill('text remains sendable');
  await page.locator('#send-button').click();
  await expect.poll(() => state.actions.length).toBe(1);
  expect(state.actions[0]).toMatchObject({ action: 'prompt', text: 'text remains sendable', images: [] });
});
