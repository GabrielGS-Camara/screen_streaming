// The "Assistir" tab's ViewModel: joins a session by pairing code, wires
// the resulting video/audio streams up to the view (the <img>, the audio
// graph, fullscreen/volume controls), and resets everything when the
// broadcaster stops or the user leaves. Whoever is watching still needs to
// be told the broadcaster's address (one of the ones shown on the
// "Transmitir" tab) and paste it in — persisted in localStorage so it
// isn't retyped every time.
import { invoke, listenTauriEvent } from "./tauri.js";
import { setupAudioPlayback } from "./audio-playback.js";

const WATCH_SIGNALING_ADDR_KEY = "screen-streaming:watch-signaling-addr";

export function setupWatch() {
  const signalingInput = document.querySelector("#watch-signaling-addr");
  const codeInput = document.querySelector("#pairing-code");
  const connectButton = document.querySelector("#connect-watch");
  const statusEl = document.querySelector("#watch-status");
  const placeholder = document.querySelector("#video-placeholder");
  const video = document.querySelector("#remote-video");
  const videoFrame = document.querySelector("#watch-video-frame");
  const controls = document.querySelector("#watch-controls");
  const fullscreenButton = document.querySelector("#watch-fullscreen");
  const stopWatchingButton = document.querySelector("#stop-watching");
  const volumeRow = document.querySelector("#watch-volume-row");
  const volumeSlider = document.querySelector("#watch-volume");
  let stopAudioPlayback = null;
  let lastNonZeroVolume = volumeSlider.value;

  async function toggleFullscreen() {
    try {
      if (document.fullscreenElement) {
        await document.exitFullscreen();
      } else {
        await videoFrame.requestFullscreen();
      }
    } catch (err) {
      console.error("fullscreen failed:", err);
    }
  }

  function toggleMute() {
    if (volumeSlider.disabled) return; // no audio to mute in this session
    if (Number(volumeSlider.value) > 0) {
      lastNonZeroVolume = volumeSlider.value;
      volumeSlider.value = "0";
    } else {
      volumeSlider.value = lastNonZeroVolume || "100";
    }
    // `setupAudioPlayback` listens for this same event to update the
    // GainNode — dispatching it here keeps this the single source of
    // truth instead of duplicating the gain-update logic.
    volumeSlider.dispatchEvent(new Event("input"));
  }

  fullscreenButton.addEventListener("click", toggleFullscreen);

  // F/M/Esc — only while actually watching something, and never while the
  // user is typing into a field (the address/code inputs live on this same
  // tab).
  document.addEventListener("keydown", (event) => {
    if (controls.hidden) return;
    const tag = event.target?.tagName;
    if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;

    switch (event.key.toLowerCase()) {
      case "f":
        event.preventDefault();
        toggleFullscreen();
        break;
      case "m":
        event.preventDefault();
        toggleMute();
        break;
      case "escape":
        // The Fullscreen API already exits on Esc natively in every
        // browser engine WebView2 is built on — this is just a safety net
        // in case that native behavior isn't triggered for some reason.
        if (document.fullscreenElement) {
          document.exitFullscreen().catch(() => {});
        }
        break;
    }
  });

  try {
    const saved = localStorage.getItem(WATCH_SIGNALING_ADDR_KEY);
    if (saved) signalingInput.value = saved;
  } catch {
    // Private window / blocked storage — just skip persistence.
  }
  signalingInput.addEventListener("change", () => {
    try {
      localStorage.setItem(WATCH_SIGNALING_ADDR_KEY, signalingInput.value.trim());
    } catch {
      // Ignore — not essential to functioning.
    }
  });

  function setStatus(text, kind) {
    statusEl.textContent = text;
    statusEl.className = kind ? `status is-${kind}` : "status";
  }

  function resetWatchUI(message) {
    video.hidden = true;
    video.src = "";
    placeholder.hidden = false;
    controls.hidden = true;
    volumeRow.hidden = true;
    volumeSlider.disabled = false;
    volumeSlider.title = "";
    connectButton.disabled = false;
    stopAudioPlayback?.();
    stopAudioPlayback = null;
    setStatus(message);
  }

  connectButton.addEventListener("click", async () => {
    const signalingAddr = signalingInput.value.trim();
    const code = codeInput.value.trim();
    if (!signalingAddr || !code) {
      setStatus("Informe o servidor de sinalização e o código.", "error");
      return;
    }

    connectButton.disabled = true;
    setStatus("Conectando…");

    try {
      await invoke("join_signaling_session", { signalingAddr, code });
      setStatus("Conectado — aguardando vídeo…");

      const urls = await invoke("start_watching");
      video.src = urls.video;
      video.hidden = false;
      placeholder.hidden = true;
      controls.hidden = false;

      // Always shown once connected (even with no audio track) rather than
      // only appearing/disappearing based on whether this particular
      // broadcast has audio — a control that pops in and out is more
      // surprising than one that's just disabled when it doesn't apply.
      volumeRow.hidden = false;
      if (urls.audio) {
        volumeSlider.disabled = false;
        volumeSlider.title = "";
        stopAudioPlayback = setupAudioPlayback(urls.audio, volumeSlider);
      } else {
        volumeSlider.disabled = true;
        volumeSlider.title = "Esta transmissão não tem áudio.";
      }

      setStatus("Recebendo transmissão.", "success");
    } catch (err) {
      setStatus("Falha ao conectar — veja o console.", "error");
      console.error("watch connect failed:", err);
      connectButton.disabled = false;
    }
  });

  stopWatchingButton.addEventListener("click", async () => {
    stopWatchingButton.disabled = true;
    try {
      await invoke("stop_watching");
    } catch (err) {
      console.error("stop_watching failed:", err);
    } finally {
      stopWatchingButton.disabled = false;
      resetWatchUI("Você saiu da transmissão.");
    }
  });

  // Fired by the backend when the broadcaster stops (or their connection
  // drops) — resets the UI on its own instead of leaving a dead <img> up
  // with no way to tell it's not receiving anything anymore.
  listenTauriEvent?.("watch-ended", () => {
    if (controls.hidden) return; // already reset locally, nothing to do
    resetWatchUI("A transmissão foi encerrada pelo transmissor.");
  });
}
