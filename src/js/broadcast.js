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

export function setupBroadcast() {
  const startButton = document.querySelector("#start-broadcast");
  const applySettingsButton = document.querySelector("#apply-broadcast-settings");
  const stopButton = document.querySelector("#stop-broadcast");
  const statusEl = document.querySelector("#broadcast-status");
  const shareSection = document.querySelector("#broadcast-share");
  const addrList = document.querySelector("#broadcast-addr-list");
  const codeEl = document.querySelector("#broadcast-code");
  const copyCodeButton = document.querySelector("#copy-broadcast-code");
  const maxViewersSelect = document.querySelector("#max-viewers-select");
  const viewerCountDisplay = document.querySelector("#viewer-count-display");

  // "" (the "Ilimitado" option) becomes `null` — no cap — on the Rust side.
  function getMaxViewers() {
    const value = maxViewersSelect.value;
    return value === "" ? null : Number(value);
  }

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
      'input[name="source-type"], #resolution-select, #fps-select, #audio-toggle, #boost-performance-toggle',
    )
    .forEach((el) => el.addEventListener("change", refreshApplyButtonState));
  document.addEventListener("source-selection-changed", refreshApplyButtonState);

  function setStatus(text, kind) {
    statusEl.textContent = text;
    statusEl.className = kind ? `status is-${kind}` : "status";
  }

  function renderAddresses(addresses) {
    addrList.innerHTML = "";
    if (addresses.length === 0) {
      addrList.innerHTML =
        '<p class="card-hint">Nenhum endereço de rede encontrado — só quem estiver nesta mesma máquina vai conseguir conectar.</p>';
      return;
    }
    for (const addr of addresses) {
      const row = document.createElement("div");
      row.className = "field-row";

      const input = document.createElement("input");
      input.type = "text";
      input.className = "text-input";
      input.readOnly = true;
      input.value = addr;

      const copyButton = document.createElement("button");
      copyButton.type = "button";
      copyButton.className = "btn btn-ghost";
      copyButton.textContent = "Copiar";
      copyButton.addEventListener("click", () => copyToClipboard(addr, copyButton));

      row.appendChild(input);
      row.appendChild(copyButton);
      addrList.appendChild(row);
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
