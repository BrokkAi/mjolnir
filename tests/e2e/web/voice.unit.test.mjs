import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const webRoot = new URL('../../../mj-controller/src/web/', import.meta.url);
const viewerSource = readFileSync(new URL('viewer.js', webRoot), 'utf8');
const workletSource = readFileSync(new URL('voice-worklet.js', webRoot), 'utf8');
const workerSource = readFileSync(new URL('voice-worker.js', webRoot), 'utf8');

function sourceBetween(source, from, to) {
  const start = source.indexOf(from);
  assert.notEqual(start, -1, `missing ${from}`);
  const end = source.indexOf(to, start);
  assert.notEqual(end, -1, `missing ${to}`);
  return source.slice(start, end);
}

function workerHarness() {
  const messages = [];
  const context = vm.createContext({
    ArrayBuffer,
    DataView,
    Int16Array,
    self: { postMessage(message) { messages.push(message); } },
  });
  vm.runInContext(workerSource, context);
  return { context, messages, send(message) { context.self.onmessage({ data: message }); } };
}

function workletHarness(sampleRate) {
  const messages = [];
  const processors = {};
  const context = vm.createContext({
    Float32Array,
    Int16Array,
    Math,
    sampleRate,
    AudioWorkletProcessor: class {
      constructor() {
        this.port = {
          onmessage: null,
          postMessage(message) { messages.push(message); },
        };
      }
    },
    registerProcessor(name, Processor) { processors[name] = Processor; },
  });
  vm.runInContext(workletSource, context);
  const processor = new processors['voice-capture-processor']();
  return {
    messages,
    processor,
    process(channels) {
      const frames = channels[0].length;
      return processor.process([channels], [[new Float32Array(frames)]]);
    },
    flush() { processor.port.onmessage({ data: { type: 'flush' } }); },
  };
}

test('the WAV worker assembles many chunks across the pipe buffer boundary', () => {
  const harness = workerHarness();
  harness.send({ type: 'start' });
  const first = new Int16Array(40_001);
  const second = new Int16Array(40_002);
  first.fill(-1234);
  second.fill(2345);
  harness.send({ type: 'pcm', samples: first });
  harness.send({ type: 'pcm', samples: second });
  harness.send({ type: 'finish' });

  assert.equal(harness.messages.length, 1);
  const wav = harness.messages[0].buffer;
  const view = new DataView(wav);
  assert.equal(wav.byteLength, 44 + (first.length + second.length) * 2);
  assert.equal(new TextDecoder().decode(new Uint8Array(wav, 0, 4)), 'RIFF');
  assert.equal(new TextDecoder().decode(new Uint8Array(wav, 8, 4)), 'WAVE');
  assert.equal(view.getUint16(20, true), 1);
  assert.equal(view.getUint32(24, true), 16_000);
  assert.equal(view.getUint16(34, true), 16);
  assert.equal(view.getUint32(40, true), (first.length + second.length) * 2);
  assert.equal(view.getInt16(44, true), -1234);
  assert.equal(view.getInt16(44 + first.length * 2, true), 2345);
});

test('the WAV worker clips a delayed timer at the ten minute sample cap', () => {
  const harness = workerHarness();
  harness.send({ type: 'start' });
  harness.send({ type: 'pcm', samples: new Int16Array(9_600_001) });
  assert.deepEqual(harness.messages.map(message => message.type), ['limit']);
  harness.send({ type: 'finish' });
  assert.equal(harness.messages[1].type, 'wav');
  assert.equal(harness.messages[1].buffer.byteLength, 44 + 9_600_000 * 2);
});

test('the AudioWorklet downmixes and resamples each rate continuously', () => {
  const at48k = workletHarness(48_000);
  // Opposite channels prove the output is a mono downmix, including when a
  // chunk boundary splits the signal.
  for (let chunk = 0; chunk < 12; chunk += 1) {
    at48k.process([
      Float32Array.from({ length: 128 }, () => 0.75),
      Float32Array.from({ length: 128 }, () => -0.75),
    ]);
  }
  at48k.flush();
  const downmixed = at48k.messages.filter(message => message.type === 'pcm')
    .flatMap(message => [...message.samples]);
  assert.equal(downmixed.length, Math.ceil((12 * 128) / 3));
  assert.ok(downmixed.every(sample => sample === 0));

  const at44k = workletHarness(44_100);
  for (let chunk = 0; chunk < 12; chunk += 1) {
    at44k.process([
      Float32Array.from({ length: 128 }, (_, index) => (chunk * 128 + index) / 2000),
    ]);
  }
  at44k.flush();
  const resampled = at44k.messages.filter(message => message.type === 'pcm')
    .flatMap(message => [...message.samples]);
  assert.equal(resampled.length, Math.ceil((12 * 128) / (44_100 / 16_000)));
  assert.ok(resampled[0] < resampled.at(-1));
  assert.equal(at44k.messages.at(-1).type, 'flushed');
  const messageCount = at44k.messages.length;
  assert.equal(at44k.process([Float32Array.from({ length: 128 }, () => 1)]), false);
  assert.equal(at44k.messages.length, messageCount, 'a late render quantum emitted audio after flush');
});

test('a transcription request timeout becomes a visible timeout instead of a stuck state', async () => {
  const source = sourceBetween(viewerSource, 'async function transcribeVoice(', '\nfunction appendVoiceText(');
  let timeoutCallback;
  const context = vm.createContext({
    AbortController,
    VOICE_TRANSCRIPTION_TIMEOUT_MS: 120_000,
    clearTimeout() {},
    setTimeout(callback) {
      timeoutCallback = callback;
      return 1;
    },
    encodeURIComponent,
    fetch(_url, options) {
      return new Promise((_resolve, reject) => {
        options.signal.addEventListener('abort', () => {
          const error = new Error('aborted');
          error.name = 'AbortError';
          reject(error);
        }, { once: true });
      });
    },
  });
  vm.runInContext(`${source}\nglobalThis.run = transcribeVoice;`, context);
  const operation = { sessionId: 'session', controller: new AbortController() };
  context.operation = operation;
  const pending = vm.runInContext('run(operation, new ArrayBuffer(44))', context, { filename: 'voice-timeout.js' });
  timeoutCallback();
  await assert.rejects(pending, /timed out after 120 seconds/);
});

test('stopping and completing a recording retire its ten minute timer', async () => {
  const stopSource = sourceBetween(viewerSource, 'function finishVoiceRecording(', '\nasync function startVoiceInput(');
  const completeSource = sourceBetween(viewerSource, 'async function completeVoiceTranscription(', '\nfunction handleVoiceWorkerMessage(');
  const cleared = [];
  const context = vm.createContext({
    clearTimeout(timer) { cleared.push(timer); },
    setTimeout() { return 'new-timer'; },
    currentSession: 'session',
    voiceGeneration: 1,
    voiceOperation: null,
    voiceState: 'recording',
    voiceOperationIsCurrent: operation => context.voiceOperation === operation,
    renderVoiceState() {},
    stopVoiceTracks() {},
    finishVoiceRecording(operation) { operation.finishRequested = true; },
    closeVoiceAudio() {},
    transcribeVoice: async () => 'voice result',
    appendVoiceText() {},
    promptText: {},
  });
  vm.runInContext(`${stopSource}\n${completeSource}`, context);
  const first = { sessionId: 'session', limitTimer: 'first-timer', finishRequested: false };
  context.voiceOperation = first;
  context.first = first;
  vm.runInContext('stopVoiceRecording()', context);
  assert.deepEqual(cleared, ['first-timer']);
  assert.equal(first.limitTimer, null);

  // A successful transcription also clears defensively, so a timer from a
  // prior recording cannot stop a subsequent recording.
  const second = { sessionId: 'session', limitTimer: 'second-timer', worker: null };
  context.voiceOperation = second;
  context.second = second;
  context.voiceState = 'transcribing';
  await vm.runInContext('completeVoiceTranscription(second, new ArrayBuffer(44))', context);
  assert.deepEqual(cleared, ['first-timer', 'second-timer']);
});

test('streaming conversion preserves sample positions across more than 64 KiB at each rate', () => {
  const input = Float32Array.from({ length: 80_003 }, (_, index) => Math.sin(index / 17) * 0.8);
  for (const rate of [8000, 16000, 44100, 48000, 96000]) {
    const harness = workletHarness(rate);
    for (let offset = 0; offset < input.length; offset += 128)
      harness.process([input.subarray(offset, offset + 128)]);
    harness.flush();
    const samples = harness.messages.filter(message => message.type === 'pcm').flatMap(message => [...message.samples]);
    assert.equal(samples.length, Math.ceil(input.length * 16000 / rate), `sample count at ${rate}`);
    for (let index = 0; index < samples.length; index += 1) {
      const position = index * rate / 16000;
      const lower = Math.floor(position);
      const value = input[lower] + ((input[lower + 1] ?? input[lower]) - input[lower]) * (position - lower);
      const expected = Math.round(value * (value < 0 ? 32768 : 32767));
      assert.ok(Math.abs(samples[index] - expected) <= 1, `sample ${index} at ${rate}`);
    }
  }
});
