// Reads the "Qualidade" card's selectors, and owns the audio device picker
// (populated from the real `list_audio_devices` command, same pattern
// source-picker.js uses for the window list) — shown only once "Transmitir
// áudio do sistema" is on, since it's meaningless otherwise.
import { invoke } from "./tauri.js";

export function getSelectedQuality() {
  return {
    resolutionHeight: Number(document.querySelector("#resolution-select").value),
    // 0 is the "Ilimitado" option — matches quality::UNLIMITED_FPS on the
    // Rust side exactly, so it can be sent straight through unchanged.
    fps: Number(document.querySelector("#fps-select").value),
    audio: document.querySelector("#audio-toggle").checked,
    // "" (the "Padrão do sistema" option) becomes `null` — capture from
    // whatever the system's current default output device is.
    audioDeviceId: document.querySelector("#audio-device-select").value || null,
    boostPerformance: document.querySelector("#boost-performance-toggle").checked,
  };
}

export function setupQuality() {
  const audioToggle = document.querySelector("#audio-toggle");
  const deviceRow = document.querySelector("#audio-device-row");
  const deviceSelect = document.querySelector("#audio-device-select");
  let devicesLoaded = false;

  async function loadAudioDevices() {
    if (devicesLoaded) return;
    devicesLoaded = true;
    try {
      const devices = await invoke("list_audio_devices");
      for (const device of devices) {
        const option = document.createElement("option");
        option.value = device.id;
        option.textContent = device.name;
        deviceSelect.appendChild(option);
      }
    } catch (err) {
      console.error("list_audio_devices failed:", err);
    }
  }

  audioToggle.addEventListener("change", () => {
    deviceRow.hidden = !audioToggle.checked;
    if (audioToggle.checked) loadAudioDevices();
  });
}
