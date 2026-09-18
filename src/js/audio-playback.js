// Plays the interleaved-stereo-i16-PCM-over-WebSocket audio stream served
// by `audio_preview.rs` through an AudioWorklet (see
// pcm-player-worklet.js), with a GainNode for the volume slider.
export function setupAudioPlayback(wsUrl, volumeSlider) {
  const audioContext = new AudioContext({ sampleRate: 48000 });
  const gainNode = audioContext.createGain();
  gainNode.gain.value = Number(volumeSlider.value) / 100;
  gainNode.connect(audioContext.destination);

  const onVolumeInput = () => {
    gainNode.gain.value = Number(volumeSlider.value) / 100;
  };
  volumeSlider.addEventListener("input", onVolumeInput);

  let stopped = false;
  let ws = null;

  audioContext.audioWorklet
    .addModule("pcm-player-worklet.js")
    .then(() => {
      if (stopped) return;
      const playerNode = new AudioWorkletNode(audioContext, "pcm-player", {
        outputChannelCount: [2],
      });
      playerNode.connect(gainNode);

      ws = new WebSocket(wsUrl);
      ws.binaryType = "arraybuffer";
      ws.onmessage = (event) => {
        const interleaved = new Int16Array(event.data);
        const floats = new Float32Array(interleaved.length);
        for (let i = 0; i < interleaved.length; i++) {
          floats[i] = interleaved[i] / 32768;
        }
        playerNode.port.postMessage(floats, [floats.buffer]);
      };
      ws.onerror = (err) => console.error("audio websocket error:", err);
    })
    .catch((err) => console.error("failed to load pcm-player-worklet:", err));

  // Autoplay policies suspend new AudioContexts unless resumed from a user
  // gesture — this call happens inside the "Conectar" click handler's own
  // call chain, so it counts.
  audioContext.resume().catch((err) => console.error("audioContext.resume failed:", err));

  // Returns a `stop()` to tear the whole thing down (called on
  // disconnect/leave, or before setting up a fresh one for a new
  // connection).
  return () => {
    stopped = true;
    volumeSlider.removeEventListener("input", onVolumeInput);
    ws?.close();
    audioContext.close().catch(() => {});
  };
}
