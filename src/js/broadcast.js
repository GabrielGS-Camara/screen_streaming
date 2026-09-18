// The "Transmitir" tab's ViewModel: reads the Fonte/Qualidade form state
// (via source-picker.js / quality.js), drives the broadcast lifecycle
// commands, and updates this tab's own view (status text, share section,
// button states) in response. The broadcaster's own machine runs the
// signaling server (see start_hosting_session in lib.rs) — nothing to type
// in here, just addresses and a code to copy and send to whoever is going
// to watch.
import { invoke, listenTauriEvent } from "./tauri.js";
import { copyToClipboard } from "./clipboard.js";
import { getSelectedSource } from "./source-picker.js";
import { getSelectedQuality } from "./quality.js";

// Remembers the last source/quality/max-viewers actually used, so the form
// doesn't reset to defaults every time the app restarts. Deliberately
// doesn't try to restore the exact *window* selected (the list of open
// windows is transient — it might not even exist next time) or the exact
// *audio device* (its `<select>` is only populated once audio is turned
// on, asynchronously, so it wouldn't be ready yet when settings are
// restored) — just the source *type*, the last window's title as a search
// filter pre-fill, and everything else as-is.
const SAVED_SETTINGS_KEY = "screen-streaming:last-broadcast-settings";

// The max-viewers slider only has 5 real stops (not a free-form range) —
// this is the index-to-value mapping for it. `null` means "Ilimitado" (no
// cap) on the Rust side.
const MAX_VIEWERS_STEPS = [1, 2, 5, 10, null];

function saveBroadcastSettings(source, quality, maxViewers) {
  try {
    localStorage.setItem(SAVED_SETTINGS_KEY, JSON.stringify({ source, quality, maxViewers }));
  } catch {
    // Ignore — not essential to functioning.
  }
}

function restoreBroadcastSettings() {
  let saved;
  try {
    const raw = localStorage.getItem(SAVED_SETTINGS_KEY);
    if (!raw) return;
    saved = JSON.parse(raw);
  } catch {
    return;
  }

  if (saved.source?.type === "window") {
    const windowRadio = document.querySelector('input[name="source-type"][value="window"]');
    windowRadio.checked = true;
    windowRadio.dispatchEvent(new Event("change"));
    if (saved.source.title) {
      const filter = document.querySelector("#window-filter");
      filter.value = saved.source.title;
      filter.dispatchEvent(new Event("input"));
    }
  }

  const q = saved.quality;
  if (q) {
    if (q.resolutionHeight) document.querySelector("#resolution-select").value = String(q.resolutionHeight);
    if (q.fps !== undefined) document.querySelector("#fps-select").value = String(q.fps);
    document.querySelector("#boost-performance-toggle").checked = !!q.boostPerformance;
    const audioToggle = document.querySelector("#audio-toggle");
    audioToggle.checked = !!q.audio;
    audioToggle.dispatchEvent(new Event("change"));
  }

  if (saved.maxViewers !== undefined) {
    const stepIndex = MAX_VIEWERS_STEPS.indexOf(saved.maxViewers);
    if (stepIndex !== -1) {
      const slider = document.querySelector("#max-viewers-slider");
      slider.value = String(stepIndex);
      slider.dispatchEvent(new Event("input"));
    }
  }
}

export function setupBroadcast() {
  const startButton = document.querySelector("#start-broadcast");
  const applySettingsButton = document.querySelector("#apply-broadcast-settings");
  const stopButton = document.querySelector("#stop-broadcast");
  const statusEl = document.querySelector("#broadcast-status");
  const shareSection = document.querySelector("#broadcast-share");
  const addrList = document.querySelector("#broadcast-addr-list");
  const codeEl = document.querySelector("#broadcast-code");
  const copyCodeButton = document.querySelector("#copy-broadcast-code");
  const copyAllButton = document.querySelector("#copy-broadcast-all");
  const maxViewersSlider = document.querySelector("#max-viewers-slider");
  const maxViewersValue = document.querySelector("#max-viewers-value");
  const viewerCountDisplay = document.querySelector("#viewer-count-display");
  let lastAddresses = [];

  restoreBroadcastSettings();

  function getMaxViewers() {
    return MAX_VIEWERS_STEPS[Number(maxViewersSlider.value)];
  }

  function refreshMaxViewersLabel() {
    const value = getMaxViewers();
    maxViewersValue.textContent = value === null ? "Ilimitado" : String(value);
  }
  maxViewersSlider.addEventListener("input", refreshMaxViewersLabel);
  refreshMaxViewersLabel();

  function renderViewerCount(count) {
    viewerCountDisplay.hidden = false;
    if (count === 0) {
      viewerCountDisplay.textContent = "Ninguém assistindo ainda.";
    } else if (count === 1) {
      viewerCountDisplay.textContent = "1 pessoa assistindo.";
    } else {
      viewerCountDisplay.textContent = `${count} pessoas assistindo.`;
    }
  }

  // What's actually live right now (set on start/apply success), vs. what's
  // currently selected in the form — the Apply button stays visible the
  // whole time we're broadcasting, but only enabled once they differ, so it
  // can't be clicked to "apply" a no-op restart.
  let activeSettings = null;

  function settingsEqual(a, b) {
    return (
      a.source.type === b.source.type &&
      a.source.title === b.source.title &&
      a.quality.resolutionHeight === b.quality.resolutionHeight &&
      a.quality.fps === b.quality.fps &&
      a.quality.audio === b.quality.audio &&
      a.quality.audioDeviceId === b.quality.audioDeviceId &&
      a.quality.boostPerformance === b.quality.boostPerformance
    );
  }

  function refreshApplyButtonState() {
    if (!activeSettings) return;
    const current = { source: getSelectedSource(), quality: getSelectedQuality() };
    applySettingsButton.disabled = settingsEqual(current, activeSettings);
  }

  document
    .querySelectorAll(
      'input[name="source-type"], #resolution-select, #fps-select, #audio-toggle, #audio-device-select, #boost-performance-toggle',
    )
    .forEach((el) => el.addEventListener("change", refreshApplyButtonState));
  document.addEventListener("source-selection-changed", refreshApplyButtonState);

  function setStatus(text, kind) {
    statusEl.textContent = text;
    statusEl.className = kind ? `status is-${kind}` : "status";
  }

  function renderAddresses(addresses) {
    lastAddresses = addresses;
    addrList.innerHTML = "";
    if (addresses.length === 0) {
      addrList.innerHTML =
        '<p class="card-hint">Nenhum endereço de rede encontrado — só quem estiver nesta mesma máquina vai conseguir conectar.</p>';
      return;
    }
    for (const { label, url } of addresses) {
      const item = document.createElement("div");

      // The label is the adapter's own Windows name (Wi-Fi, Ethernet,
      // "Radmin VPN", etc.) — real info from the OS, not a guess about
      // which address is "the VPN one", so it's shown as-is.
      const labelEl = document.createElement("span");
      labelEl.className = "label";
      labelEl.textContent = label;

      const row = document.createElement("div");
      row.className = "field-row";

      const input = document.createElement("input");
      input.type = "text";
      input.className = "text-input";
      input.readOnly = true;
      input.value = url;

      const copyButton = document.createElement("button");
      copyButton.type = "button";
      copyButton.className = "btn btn-ghost";
      copyButton.textContent = "Copiar";
      copyButton.addEventListener("click", () => copyToClipboard(url, copyButton));

      row.appendChild(input);
      row.appendChild(copyButton);
      item.appendChild(labelEl);
      item.appendChild(row);
      addrList.appendChild(item);
    }
  }

  startButton.addEventListener("click", async () => {
    const source = getSelectedSource();
    if (source.type === "window" && !source.title) {
      setStatus("Escolha uma janela primeiro.", "error");
      return;
    }
    const quality = getSelectedQuality();

    startButton.disabled = true;
    shareSection.hidden = true;
    setStatus("Iniciando servidor de sinalização…");

    try {
      const { code, addresses } = await invoke("start_hosting_session", { maxViewers: getMaxViewers() });
      codeEl.textContent = code;
      renderAddresses(addresses);
      shareSection.hidden = false;
      setStatus("Aguardando alguém entrar com o código…");
      stopButton.hidden = false;

      await invoke("wait_for_peer", {
        windowTitle: source.type === "window" ? source.title : null,
        ...quality,
      });

      activeSettings = { source, quality };
      saveBroadcastSettings(source, quality, getMaxViewers());
      applySettingsButton.hidden = false;
      applySettingsButton.disabled = true;
      setStatus("Conectado — transmitindo.", "success");
    } catch (err) {
      setStatus("Falha ao transmitir — veja o console.", "error");
      console.error("broadcast failed:", err);
      stopButton.hidden = true;
      applySettingsButton.hidden = true;
      shareSection.hidden = true;
      startButton.disabled = false;
    }
  });

  copyCodeButton.addEventListener("click", () => copyToClipboard(codeEl.textContent, copyCodeButton));

  copyAllButton.addEventListener("click", () => {
    const message = [
      "Entra na minha transmissão pelo Screen Streaming!",
      `Código: ${codeEl.textContent}`,
      "",
      "Endereço (use o que você conseguir alcançar):",
      ...lastAddresses.map(({ label, url }) => `${label}: ${url}`),
    ].join("\n");
    copyToClipboard(message, copyAllButton);
  });

  // Changing a select/radio while already transmitting doesn't do anything
  // by itself — the button only enables once the form actually differs from
  // what's live (see refreshApplyButtonState), and the user has to click it
  // to restart the capture/encode pipeline with the new values, so
  // adjusting several settings in a row doesn't restart the stream once per
  // click.
  applySettingsButton.addEventListener("click", async () => {
    const source = getSelectedSource();
    if (source.type === "window" && !source.title) {
      setStatus("Escolha uma janela primeiro.", "error");
      return;
    }
    const quality = getSelectedQuality();

    applySettingsButton.disabled = true;
    try {
      await invoke("apply_broadcast_settings", {
        windowTitle: source.type === "window" ? source.title : null,
        ...quality,
      });
      activeSettings = { source, quality };
      saveBroadcastSettings(source, quality, getMaxViewers());
      setStatus("Alterações aplicadas — transmitindo.", "success");
    } catch (err) {
      setStatus("Falha ao aplicar alterações — veja o console.", "error");
      console.error("apply_broadcast_settings failed:", err);
      refreshApplyButtonState();
    }
  });

  function resetBroadcastUI(message) {
    stopButton.hidden = true;
    stopButton.disabled = false;
    applySettingsButton.hidden = true;
    applySettingsButton.disabled = true;
    activeSettings = null;
    startButton.disabled = false;
    shareSection.hidden = true;
    viewerCountDisplay.hidden = true;
    setStatus(message);
  }

  stopButton.addEventListener("click", async () => {
    stopButton.disabled = true;
    try {
      await invoke("stop_broadcast");
    } catch (err) {
      console.error("stop_broadcast failed:", err);
    } finally {
      resetBroadcastUI("Transmissão encerrada.");
    }
  });

  // Fired by the backend when the broadcast itself ends — "Parar
  // transmissão" here, or the embedded signaling server connection
  // dropping. A viewer leaving on its own does *not* fire this anymore:
  // with multiple people able to watch the same code, the broadcast keeps
  // running for whoever else is still there (see "viewer-count-changed"
  // below for tracking who's still watching).
  listenTauriEvent?.("broadcast-ended", () => {
    if (stopButton.hidden) return; // already stopped locally, nothing to do
    resetBroadcastUI("A transmissão foi encerrada.");
  });

  // Fired once right away when transmission starts, then every time
  // someone joins or leaves — keeps the "N pessoas assistindo" line live
  // without polling.
  listenTauriEvent?.("viewer-count-changed", (event) => {
    if (stopButton.hidden) return; // not broadcasting (stale event race)
    renderViewerCount(event.payload);
  });
}
