export function getSelectedQuality() {
  return {
    resolutionHeight: Number(document.querySelector("#resolution-select").value),
    // 0 is the "Ilimitado" option — matches quality::UNLIMITED_FPS on the
    // Rust side exactly, so it can be sent straight through unchanged.
    fps: Number(document.querySelector("#fps-select").value),
    audio: document.querySelector("#audio-toggle").checked,
    boostPerformance: document.querySelector("#boost-performance-toggle").checked,
  };
}
