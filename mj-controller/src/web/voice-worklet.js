// Capture microphone samples without putting audio conversion on the viewer's
// main thread. The processor sends small transferable PCM chunks to the
// browser Worker, which owns the larger WAV buffer.
const VOICE_SAMPLE_RATE = 16000;
const VOICE_PCM_CHUNK_SAMPLES = 2048;

function pcm16(value) {
  const clipped = Math.max(-1, Math.min(1, value));
  return clipped < 0 ? Math.round(clipped * 0x8000) : Math.round(clipped * 0x7fff);
}

class VoiceCaptureProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.sourceStep = sampleRate / VOICE_SAMPLE_RATE;
    this.input = new Float32Array(0);
    this.cursor = 0;
    this.pending = [];
    this.stopped = false;
    this.port.onmessage = event => {
      if (event.data?.type === 'flush') this.flush();
    };
  }

  append(samples) {
    const merged = new Float32Array(this.input.length + samples.length);
    merged.set(this.input);
    merged.set(samples, this.input.length);
    this.input = merged;
  }

  emit(finalChunk) {
    const output = [];
    const limit = finalChunk ? this.input.length : this.input.length - 1;
    while (this.cursor < limit) {
      const index = Math.floor(this.cursor);
      const fraction = this.cursor - index;
      const next = this.input[index + 1] ?? this.input[index];
      output.push(this.input[index] + (next - this.input[index]) * fraction);
      this.cursor += this.sourceStep;
    }

    if (finalChunk) {
      this.input = new Float32Array(0);
      this.cursor = 0;
    } else {
      // At least one source sample remains for boundary interpolation. Clamp
      // the consumed prefix: for a downsample step larger than one, the
      // cursor can advance beyond the current quantum before the next one
      // arrives.
      const consumed = Math.min(
        Math.floor(this.cursor),
        Math.max(0, this.input.length - 1),
      );
      if (consumed > 0) {
        this.input = this.input.slice(consumed);
        this.cursor -= consumed;
      }
    }
    this.pending.push(...output);
    const count = finalChunk
      ? this.pending.length
      : Math.floor(this.pending.length / VOICE_PCM_CHUNK_SAMPLES) * VOICE_PCM_CHUNK_SAMPLES;
    if (!count) return;
    const pcm = new Int16Array(count);
    for (let index = 0; index < count; index += 1) pcm[index] = pcm16(this.pending[index]);
    this.pending = this.pending.slice(count);
    this.port.postMessage({ type: 'pcm', samples: pcm }, [pcm.buffer]);
  }

  flush() {
    if (this.stopped) return;
    this.emit(true);
    this.stopped = true;
    this.port.postMessage({ type: 'flushed' });
  }

  process(inputs, outputs) {
    if (this.stopped) {
      for (const channel of outputs[0] || []) channel.fill(0);
      return false;
    }
    const channels = inputs[0];
    if (channels?.length && channels[0]?.length) {
      const frameCount = channels[0].length;
      const mono = new Float32Array(frameCount);
      for (let frame = 0; frame < frameCount; frame += 1) {
        let total = 0;
        let count = 0;
        for (const channel of channels) {
          if (frame < channel.length) {
            total += channel[frame];
            count += 1;
          }
        }
        mono[frame] = count ? total / count : 0;
      }
      this.append(mono);
    }

    this.emit(false);

    // Keep the graph alive while ensuring microphone audio is never played
    // back to the user.
    for (const channel of outputs[0] || []) channel.fill(0);
    return true;
  }
}

registerProcessor('voice-capture-processor', VoiceCaptureProcessor);
