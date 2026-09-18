// AudioWorklet processor that plays back interleaved stereo Float32 PCM
// chunks pushed to it via postMessage, in order, as they arrive over the
// WebSocket set up in main.js's `setupAudioPlayback`. Runs on the audio
// rendering thread (not the main thread), which is the only way Web Audio
// lets you feed samples in with real-time-safe, glitch-free timing.
//
// Missing data (network hasn't delivered the next chunk yet) is filled
// with silence rather than blocking — a brief gap is far less jarring than
// stalling the whole audio graph.
class PcmPlayerProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.queue = [];
    this.readFrame = 0; // next interleaved-frame index to read from queue[0]
    this.port.onmessage = (event) => {
      this.queue.push(event.data);
    };
  }

  process(_inputs, outputs) {
    const [left, right] = outputs[0];
    for (let i = 0; i < left.length; i++) {
      const chunk = this.queue[0];
      if (!chunk) {
        left[i] = 0;
        right[i] = 0;
        continue;
      }
      left[i] = chunk[this.readFrame * 2];
      right[i] = chunk[this.readFrame * 2 + 1];
      this.readFrame++;
      if (this.readFrame * 2 >= chunk.length) {
        this.queue.shift();
        this.readFrame = 0;
      }
    }
    return true;
  }
}

registerProcessor("pcm-player", PcmPlayerProcessor);
