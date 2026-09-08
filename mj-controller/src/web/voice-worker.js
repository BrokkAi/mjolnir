// Assemble the transferable PCM chunks from the AudioWorklet into one WAV.
// Keeping this allocation in a Worker prevents a ten minute recording from
// freezing the viewer while its header and sample data are copied.
const VOICE_WAV_SAMPLE_RATE = 16000;
const VOICE_WAV_CHANNELS = 1;
const VOICE_WAV_BITS = 16;
const VOICE_MAX_WAV_BYTES = 20 * 1024 * 1024;
const VOICE_MAX_SAMPLES = VOICE_WAV_SAMPLE_RATE * 600;

let chunks = [];
let byteLength = 0;
let recording = false;
let limitReached = false;

function reset() {
  chunks = [];
  byteLength = 0;
  recording = false;
  limitReached = false;
}

function fail(message) {
  reset();
  self.postMessage({ type: 'error', message });
}

function pcmChunk(value) {
  if (value instanceof Int16Array) return value;
  if (value instanceof ArrayBuffer) return new Int16Array(value);
  if (value?.buffer instanceof ArrayBuffer) {
    return new Int16Array(value.buffer, value.byteOffset || 0, value.byteLength / 2);
  }
  throw new Error('the audio worker received an invalid PCM chunk');
}

function writeAscii(view, offset, value) {
  for (let index = 0; index < value.length; index += 1)
    view.setUint8(offset + index, value.charCodeAt(index));
}

function buildWav() {
  const buffer = new ArrayBuffer(44 + byteLength);
  const view = new DataView(buffer);
  writeAscii(view, 0, 'RIFF');
  view.setUint32(4, 36 + byteLength, true);
  writeAscii(view, 8, 'WAVE');
  writeAscii(view, 12, 'fmt ');
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, VOICE_WAV_CHANNELS, true);
  view.setUint32(24, VOICE_WAV_SAMPLE_RATE, true);
  view.setUint32(28, VOICE_WAV_SAMPLE_RATE * VOICE_WAV_CHANNELS * VOICE_WAV_BITS / 8, true);
  view.setUint16(32, VOICE_WAV_CHANNELS * VOICE_WAV_BITS / 8, true);
  view.setUint16(34, VOICE_WAV_BITS, true);
  writeAscii(view, 36, 'data');
  view.setUint32(40, byteLength, true);
  const samples = new Int16Array(buffer, 44, byteLength / 2);
  let offset = 0;
  for (const chunk of chunks) {
    samples.set(chunk, offset);
    offset += chunk.length;
  }
  return buffer;
}

self.onmessage = event => {
  const message = event.data || {};
  try {
    if (message.type === 'start') {
      reset();
      recording = true;
      return;
    }
    if (message.type === 'pcm') {
      if (!recording) throw new Error('audio arrived before recording started');
      if (limitReached) return;
      const chunk = pcmChunk(message.samples);
      if (chunk.byteLength % 2) {
        fail('Voice recording is limited to 20 MiB.');
        return;
      }
      const remaining = Math.min(
        VOICE_MAX_SAMPLES - byteLength / 2,
        (VOICE_MAX_WAV_BYTES - 44 - byteLength) / 2,
      );
      if (chunk.length > remaining) {
        if (remaining > 0) {
          const clipped = chunk.subarray(0, remaining);
          chunks.push(clipped);
          byteLength += clipped.byteLength;
        }
        limitReached = true;
        self.postMessage({ type: 'limit' });
        return;
      }
      chunks.push(chunk);
      byteLength += chunk.byteLength;
      return;
    }
    if (message.type === 'finish') {
      if (!recording) throw new Error('audio finished before recording started');
      const wav = buildWav();
      reset();
      self.postMessage({ type: 'wav', buffer: wav }, [wav]);
    }
  } catch (error) {
    fail(error.message || 'The browser could not prepare the recording.');
  }
};
