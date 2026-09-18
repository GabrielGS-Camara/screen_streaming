// The "Assistir" tab's ViewModel: joins a session by pairing code, wires
// the resulting video/audio streams up to the view (the <img>, the audio
// graph, fullscreen/PiP/volume controls), and resets everything when the
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
  const pipButton = document.querySelector("#watch-pip");
  const stopWatchingButton = document.querySelector("#stop-watching");
  const volumeRow = document.querySelector("#watch-volume-row");
  const volumeSlider = document.querySelector("#watch-volume");
  let stopAudioPlayback = null;

  fullscreenButton.addEventListener("click", async () => {
    try {
      if (document.fullscreenElement) {
        await document.exitFullscreen();
      } else {
        await videoFrame.requestFullscreen();
      }
    } catch (err) {
      console.error("fullscreen failed:", err);
    }
  });

  // A real OS-level Tauri window (see `open_pip_window` in lib.rs), not the
  // browser's Document Picture-in-Picture API — that API silently broke
  // the video the first time this was tried (moving the live <img> into
  // its own separate browsing context killed the in-flight
  // multipart/x-mixed-replace connection), and stayed unreliable on real
  // WebView2 even after working around that. A plain Tauri window pointed
  // at its own tiny page (pip.html) sidesteps all of that.
  pipButton.addEventListener("click", async () => {
    if (!video.src) {
      setStatus("Conecte-se a uma transmissão antes de abrir o picture-in-picture.", "error");
      return;
    }
    try {
      await invoke("open_pip_window");
    } catch (err) {
      setStatus("Falha ao abrir picture-in-picture — veja o console.", "error");
      console.error("open_pip_window failed:", err);
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

      if (urls.audio) {
        volumeRow.hidden = false;
        stopAudioPlayback = setupAudioPlayback(urls.audio, volumeSlider);
      } else {
        volumeRow.hidden = true;
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
