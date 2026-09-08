const { test, expect } = require('@playwright/test');
const path = require('node:path');
const http = require('node:http');
const { readFileSync } = require('node:fs');

test.use({ viewport: { width: 390, height: 844 }, serviceWorkers: 'block' });

const SESSION_ID = 'voice-session';
const ONE_PIXEL_PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=',
  'base64',
);

function snapshot() {
  return {
    revision: 1,
    generated_at: '2026-09-07T00:00:00Z',
    workspaces: [{ id: 'workspace-1', name: 'Voice' }],
    sessions: [{
      id: SESSION_ID,
      workspace_id: 'workspace-1',
      title: 'Voice browser test',
      harness_kind: 'codex',
      profile_id: 'codex',
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
      project_label: 'voice',
      project_key: 'voice',
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
    bundles: [],
    review_config: { enabled: false, tier: 'quick', profile: null },
  };
}

async function mount(page, { fakeCapture = true, baseUrl = 'https://viewer.test' } = {}) {
  const root = path.resolve(__dirname, '../../../mj-controller/src/web');
  const state = {
    snapshot: snapshot(),
    availabilityRequests: 0,
    transcriptionRequests: 0,
    transcriptionBodies: [],
    releaseAvailability: null,
    availabilityGate: null,
    releaseTranscription: null,
    transcriptionGate: null,
  };
  if (fakeCapture) await page.addInitScript(() => {
    window.__voiceStreams = [];
    window.__resolveVoiceMic = null;
    window.makeVoiceStream = () => {
      const track = new EventTarget();
      track.stopped = false;
      track.stop = () => { track.stopped = true; };
      const stream = { getTracks: () => [track] };
      window.__voiceStreams.push(stream);
      return stream;
    };
    navigator.mediaDevices.getUserMedia = () => new Promise(resolve => {
      window.__resolveVoiceMic = resolve;
    });
    class FakeAudioContext {
      constructor() {
        this.state = 'running';
        this.destination = {};
        this.audioWorklet = { addModule: async () => {} };
      }
      createMediaStreamSource() { return { connect() {}, disconnect() {} }; }
      createGain() { return { gain: { value: 1 }, connect() {}, disconnect() {} }; }
      resume() { return Promise.resolve(); }
      close() { this.state = 'closed'; return Promise.resolve(); }
      addEventListener() {}
    }
    class FakeWorkletNode extends EventTarget {
      constructor() {
        super();
        this.port = {
          onmessage: null,
          start() {},
          close() {},
          postMessage: message => {
            if (message.type !== 'flush') return;
            // Deliver a large enough PCM chunk to exercise transferable worker
            // assembly, then acknowledge it in port order.
            const samples = new Int16Array(40_001);
            samples.fill(1234);
            setTimeout(() => this.port.onmessage?.({ data: { type: 'pcm', samples } }), 0);
            setTimeout(() => this.port.onmessage?.({ data: { type: 'flushed' } }), 0);
          },
        };
      }
      connect() {}
      disconnect() {}
    }
    window.AudioContext = FakeAudioContext;
    window.AudioWorkletNode = FakeWorkletNode;
  });
  await page.route('**/*', async route => {
    const pathname = new URL(route.request().url()).pathname;
    const file = pathname === '/' ? 'viewer.html' : pathname.slice(1);
    const assets = [
      'viewer.html', 'viewer.js', 'viewer.css', 'markdown.js', 'tool-output.js',
      'voice-worklet.js', 'voice-worker.js', 'manifest.webmanifest', 'icon.svg',
    ];
    if (assets.includes(file)) {
      return route.fulfill({ path: path.join(root, file === 'icon.svg' ? '../icons/icon.svg' : file) });
    }
    if (pathname === '/api/snapshot') {
      return route.fulfill({ contentType: 'application/json', body: JSON.stringify(state.snapshot) });
    }
    if (pathname === '/api/events') {
      return route.fulfill({ contentType: 'text/event-stream', body: ': fixture\n\n' });
    }
    if (pathname === `/api/sessions/${SESSION_ID}/client-state`) {
      return route.fulfill({ contentType: 'application/json', body: JSON.stringify({ draft: '', through_event_ordinal: 0 }) });
    }
    if (pathname === `/api/sessions/${SESSION_ID}/draft`) {
      return route.fulfill({ status: 204, body: '' });
    }
    if (pathname === `/api/sessions/${SESSION_ID}/dictation` && route.request().method() === 'GET') {
      state.availabilityRequests += 1;
      if (state.availabilityGate) await state.availabilityGate;
      return route.fulfill({ contentType: 'application/json', body: JSON.stringify({ available: true }) });
    }
    if (pathname === `/api/sessions/${SESSION_ID}/dictation` && route.request().method() === 'POST') {
      state.transcriptionRequests += 1;
      state.transcriptionBodies.push(route.request().postDataBuffer());
      if (state.transcriptionGate) await state.transcriptionGate;
      return route.fulfill({ contentType: 'application/json', body: JSON.stringify({ text: 'voice result' }) });
    }
    if (pathname === `/api/sessions/${SESSION_ID}/attachments`) {
      return route.fulfill({ contentType: 'application/json', body: JSON.stringify({
        attachment: 'image-1', mime_type: 'image/png', width: 1, height: 1,
      }) });
    }
    if (pathname === `/api/conversations/${SESSION_ID}` || pathname === `/api/conversations/${SESSION_ID}/read`) {
      return route.fulfill({ status: pathname.endsWith('/read') ? 204 : 200, contentType: 'application/json', body: JSON.stringify({ latest_seq: 0, entries: [] }) });
    }
    return route.fulfill({ status: 404, body: '' });
  });
  await page.goto(`${baseUrl}/#workspace/workspace-1`);
  await expect(page.locator('#sessions .session')).toHaveCount(1);
  await page.locator('#sessions .session h3').click();
  await expect(page.locator('#voice-input')).toBeVisible();
  return state;
}

async function resolveMicrophone(page) {
  await expect.poll(() => page.evaluate(() => Boolean(window.__resolveVoiceMic))).toBe(true);
  await page.evaluate(() => {
    const resolve = window.__resolveVoiceMic;
    window.__resolveVoiceMic = null;
    resolve(window.makeVoiceStream());
  });
  await expect(page.locator('#voice-status')).toHaveText('Recording. Tap stop when finished.');
}

test('voice states, WAV upload, draft append, and image preservation are accessible', async ({ page }) => {
  const state = await mount(page);
  await page.locator('#prompt-text').fill('typed before');
  await page.locator('#image-picker').setInputFiles({ name: 'one.png', mimeType: 'image/png', buffer: ONE_PIXEL_PNG });
  await expect(page.locator('#attachments .attachment-ready')).toHaveCount(1);

  state.availabilityGate = new Promise(resolve => { state.releaseAvailability = resolve; });
  await page.locator('#voice-input').click();
  await expect(page.locator('#voice-status')).toHaveText('Requesting microphone permission…');
  state.releaseAvailability();
  await resolveMicrophone(page);
  await expect(page.locator('#send-button')).toBeDisabled();

  await page.locator('#prompt-text').fill('edited while recording');
  state.transcriptionGate = new Promise(resolve => { state.releaseTranscription = resolve; });
  await page.getByRole('button', { name: 'Stop recording and transcribe' }).click();
  await expect(page.locator('#voice-status')).toHaveText('Transcribing…');
  await page.locator('#prompt-text').fill('edited during transcription');
  await expect(page.locator('#send-button')).toBeDisabled();
  state.releaseTranscription();
  await expect(page.locator('#prompt-text')).toHaveText('edited during transcription voice result');
  await expect(page.locator('#voice-controls')).toBeHidden();
  await expect(page.locator('#attachments .attachment-ready')).toHaveCount(1);
  expect(state.transcriptionRequests).toBe(1);
  const wav = state.transcriptionBodies[0];
  expect(wav.subarray(0, 4).toString()).toBe('RIFF');
  expect(wav.readUInt32LE(24)).toBe(16_000);
  expect(wav.readUInt16LE(34)).toBe(16);
});

test('cancel retires late transcription and stale permission streams', async ({ page }) => {
  const state = await mount(page);
  await page.locator('#prompt-text').fill('keep this draft');
  await page.locator('#voice-input').click();
  await expect(page.locator('#voice-status')).toHaveText('Requesting microphone permission…');
  await expect.poll(() => page.evaluate(() => Boolean(window.__resolveVoiceMic))).toBe(true);
  await page.locator('#voice-cancel').click();
  await page.evaluate(() => {
    const resolve = window.__resolveVoiceMic;
    window.__resolveVoiceMic = null;
    resolve(window.makeVoiceStream());
  });
  await expect.poll(() => page.evaluate(() => window.__voiceStreams.at(-1).getTracks()[0].stopped)).toBe(true);
  await expect(page.locator('#voice-controls')).toBeHidden();

  await page.locator('#voice-input').click();
  await resolveMicrophone(page);
  state.transcriptionGate = new Promise(resolve => { state.releaseTranscription = resolve; });
  await page.getByRole('button', { name: 'Stop recording and transcribe' }).click();
  await expect(page.locator('#voice-status')).toHaveText('Transcribing…');
  await page.locator('#voice-cancel').click();
  await expect(page.locator('#voice-controls')).toBeHidden();
  state.releaseTranscription();
  await page.waitForTimeout(100);
  await expect(page.locator('#prompt-text')).toHaveText('keep this draft');
  expect(state.transcriptionRequests).toBe(1);
});

test('leaving the conversation stops recording and discards late transcription', async ({ page }) => {
  const state = await mount(page);
  await page.locator('#prompt-text').fill('saved words');
  await page.locator('#voice-input').click();
  await resolveMicrophone(page);
  await page.evaluate(() => { location.hash = '#workspace/workspace-1'; });
  await expect.poll(() => page.evaluate(() => window.__voiceStreams.at(-1).getTracks()[0].stopped)).toBe(true);
  expect(state.transcriptionRequests).toBe(0);
  await page.locator('#sessions .session h3').click();
  await page.locator('#voice-input').click();
  await resolveMicrophone(page);
  state.transcriptionGate = new Promise(resolve => { state.releaseTranscription = resolve; });
  await page.getByRole('button', { name: 'Stop recording and transcribe' }).click();
  await expect.poll(() => state.transcriptionRequests).toBe(1);
  await page.evaluate(() => { location.hash = '#workspace/workspace-1'; });
  state.releaseTranscription();
  await page.locator('#sessions .session h3').click();
  await expect(page.locator('#voice-controls')).toBeHidden();
  await expect(page.locator('#prompt-text')).not.toContainText('voice result');
});

test('permission and provider failures preserve the draft and attached images', async ({ page }) => {
  await mount(page);
  await page.locator('#prompt-text').fill('keep text');
  await page.locator('#image-picker').setInputFiles({ name: 'one.png', mimeType: 'image/png', buffer: ONE_PIXEL_PNG });
  await expect(page.locator('#attachments .attachment-ready')).toHaveCount(1);
  await page.evaluate(() => {
    window.__originalGetUserMedia = navigator.mediaDevices.getUserMedia;
    navigator.mediaDevices.getUserMedia = async () => { throw new DOMException('denied', 'NotAllowedError'); };
  });
  await page.locator('#voice-input').click();
  await expect(page.locator('#conversation-error')).toContainText('permission was denied');
  await expect(page.locator('#voice-controls')).toBeHidden();
  await expect(page.locator('#send-button')).toBeEnabled();
  await page.evaluate(() => { navigator.mediaDevices.getUserMedia = window.__originalGetUserMedia; });
  await page.route(`**/api/sessions/${SESSION_ID}/dictation`, route => route.fulfill({
    status: route.request().method() === 'POST' ? 502 : 200,
    contentType: 'application/json',
    body: JSON.stringify(route.request().method() === 'POST' ? { error: 'Transcription provider unavailable' } : { available: true }),
  }));
  await page.locator('#voice-input').click();
  await resolveMicrophone(page);
  await page.getByRole('button', { name: 'Stop recording and transcribe' }).click();
  await expect(page.locator('#conversation-error')).toContainText('provider unavailable');
  await expect(page.locator('#voice-controls')).toBeHidden();
  await expect(page.locator('#prompt-text')).toHaveText('keep text');
  await expect(page.locator('#attachments .attachment-ready')).toHaveCount(1);
  await expect(page.locator('#send-button')).toBeEnabled();
});

test('missing credentials fail before requesting microphone permission', async ({ page }) => {
  await mount(page);
  await page.route(`**/api/sessions/${SESSION_ID}/dictation`, route => route.fulfill({
    contentType: 'application/json',
    body: JSON.stringify({ available: false, reason: 'No Codex subscription is signed in.' }),
  }));
  await page.locator('#voice-input').click();
  await expect(page.locator('#conversation-error')).toContainText('No Codex subscription');
  await expect(page.locator('#voice-controls')).toBeHidden();
  expect(await page.evaluate(() => window.__resolveVoiceMic)).toBeNull();
});


test.use({ launchOptions: { args: process.env.MJ_BROWSER_ENGINE === 'firefox' ? [] : [
    '--ignore-certificate-errors',
    '--use-fake-device-for-media-stream',
    '--use-fake-ui-for-media-stream',
  ] } });

test.describe('browser microphone pipeline', () => {
  test('real AudioWorklet and Worker produce WAV from a browser microphone stream', async ({ page, browserName }) => {
    test.skip(browserName !== 'chromium', 'This capture test uses Chromium synthetic microphone flags.');
    // Chromium's AudioWorklet loader bypasses Playwright page routing. Serve
    // the actual scripts over loopback so this test exercises that loader too.
    const root = path.resolve(__dirname, '../../../mj-controller/src/web');
    const scripts = new Map(['voice-worklet.js', 'voice-worker.js'].map(file =>
      [`/${file}`, readFileSync(path.join(root, file))]));
    const server = http.createServer((request, response) => {
      const script = scripts.get(request.url);
      response.writeHead(script ? 200 : 404, { 'Content-Type': 'text/javascript' });
      response.end(script || '');
    });
    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
    try {
      const state = await mount(page, { fakeCapture: false, baseUrl: `http://127.0.0.1:${server.address().port}` });
      await page.locator('#voice-input').click();
      await expect(page.locator('#voice-status')).toContainText('Recording.');
      // Let the actual browser audio render thread capture several quanta.
      await page.waitForTimeout(350);
      await page.getByRole('button', { name: 'Stop recording and transcribe' }).click();
      await expect(page.locator('#prompt-text')).toHaveText('voice result');
      expect(state.transcriptionRequests).toBe(1);
      const wav = state.transcriptionBodies[0];
      expect(wav.readUInt16LE(22)).toBe(1);
      expect(wav.readUInt32LE(24)).toBe(16000);
      expect(wav.readUInt16LE(34)).toBe(16);
      expect(wav.readUInt32LE(40)).toBe(wav.length - 44);
      expect(wav.length).toBeGreaterThan(3200);
    } finally {
      server.closeAllConnections();
      await new Promise(resolve => server.close(resolve));
    }
  });
});
