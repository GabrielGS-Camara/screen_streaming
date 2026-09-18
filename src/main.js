const invoke = window.__TAURI__?.core?.invoke;

// ─────────────────────────────────── Tabs ────────────────────────────────

function setupTabs() {
  const tabs = document.querySelectorAll(".tab");
  const panels = {
    broadcast: document.querySelector("#panel-broadcast"),
    watch: document.querySelector("#panel-watch"),
  };

  tabs.forEach((tab) => {
    tab.addEventListener("click", () => {
      tabs.forEach((t) => {
        t.classList.toggle("is-active", t === tab);
        t.setAttribute("aria-selected", t === tab ? "true" : "false");
      });
      Object.entries(panels).forEach(([name, panel]) => {
        panel.classList.toggle("is-active", name === tab.dataset.tab);
      });
    });
  });
}

// ────────────────────────────── Source picker ────────────────────────────

function setupSourcePicker() {
  const windowPicker = document.querySelector("#window-picker");
  const windowSelect = document.querySelector("#window-select");
  const refreshButton = document.querySelector("#refresh-windows");
  const sourceRadios = document.querySelectorAll('input[name="source-type"]');

  async function loadWindows() {
    windowSelect.innerHTML = '<option value="">Carregando janelas…</option>';
    try {
      const windows = await invoke("list_capturable_windows");
      if (windows.length === 0) {
        windowSelect.innerHTML = '<option value="">Nenhuma janela encontrada</option>';
        return;
      }
      windowSelect.innerHTML = "";
      for (const win of windows) {
        const option = document.createElement("option");
        option.value = win.title;
        option.textContent = `${win.title} — ${win.process_name}`;
        windowSelect.appendChild(option);
      }
    } catch (err) {
      windowSelect.innerHTML = '<option value="">Falha ao listar janelas</option>';
      console.error("list_capturable_windows failed:", err);
    }
  }

  sourceRadios.forEach((radio) => {
    radio.addEventListener("change", () => {
      const isWindow = document.querySelector('input[name="source-type"]:checked').value === "window";
      windowPicker.hidden = !isWindow;
      if (isWindow && windowSelect.options.length === 0) {
        loadWindows();
      }
    });
  });

  refreshButton.addEventListener("click", loadWindows);

  // Pre-load in the background so the dropdown isn't empty the moment the
  // user switches to "janela específica".
  loadWindows();
}

// ───────────────────────────── Capture test ──────────────────────────────

function getSelectedSource() {
  const type = document.querySelector('input[name="source-type"]:checked').value;
  if (type === "window") {
    return { type, title: document.querySelector("#window-select").value };
  }
  return { type };
}

function setupCaptureTest() {
  const button = document.querySelector("#test-capture");
  const statusEl = document.querySelector("#test-capture-status");
  const resultsEl = document.querySelector("#test-capture-results");

  button.addEventListener("click", async () => {
    const source = getSelectedSource();
    if (source.type === "window" && !source.title) {
      statusEl.textContent = "Escolha uma janela primeiro.";
      statusEl.className = "status is-error";
      return;
    }

    button.disabled = true;
    statusEl.textContent = "Capturando por 3 segundos…";
    statusEl.className = "status";
    resultsEl.hidden = true;

    try {
      const stats =
        source.type === "window"
          ? await invoke("benchmark_window_capture", { titleContains: source.title })
          : await invoke("benchmark_capture");

      document.querySelector("#stat-fps").textContent = stats.fps.toFixed(1);
      document.querySelector("#stat-resolution").textContent = `${stats.width}×${stats.height}`;
      document.querySelector("#stat-frames").textContent = stats.frame_count;
      document.querySelector("#stat-duration").textContent = `${stats.elapsed_secs.toFixed(2)}s`;
      resultsEl.hidden = false;

      statusEl.textContent = "Captura concluída.";
      statusEl.className = "status is-success";
    } catch (err) {
      statusEl.textContent = "Falha ao capturar — veja o console.";
      statusEl.className = "status is-error";
      console.error("capture test failed:", err);
    } finally {
      button.disabled = false;
    }
  });
}

// ─────────────────────────────── Clipboard ────────────────────────────────

async function copyToClipboard(text, button) {
  try {
    await navigator.clipboard.writeText(text);
    const original = button.textContent;
    button.textContent = "Copiado!";
    setTimeout(() => {
      button.textContent = original;
    }, 1500);
  } catch (err) {
    console.error("clipboard write failed:", err);
  }
}

// ────────────────────────────────  Transmitir  ────────────────────────────
// The broadcaster's own machine runs the signaling server (see
// start_hosting_session in lib.rs) — nothing to type in here, just addresses
// and a code to copy and send to whoever is going to watch.

function setupBroadcast() {
  const startButton = document.querySelector("#start-broadcast");
  const stopButton = document.querySelector("#stop-broadcast");
  const statusEl = document.querySelector("#broadcast-status");
  const shareSection = document.querySelector("#broadcast-share");
  const addrList = document.querySelector("#broadcast-addr-list");
  const codeEl = document.querySelector("#broadcast-code");
  const copyCodeButton = document.querySelector("#copy-broadcast-code");
  const previewToggle = document.querySelector("#toggle-broadcast-preview");
  const previewFrame = document.querySelector("#broadcast-preview-frame");
  const previewImg = document.querySelector("#broadcast-preview-img");
  let previewOn = false;

  function resetPreview() {
    previewOn = false;
    previewFrame.hidden = true;
    previewImg.src = "";
    previewToggle.textContent = "Ver prévia";
  }

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

    const resolutionHeight = Number(document.querySelector("#resolution-select").value);
    const fps = Number(document.querySelector("#fps-select").value);
    const audio = document.querySelector("#audio-toggle").checked;

    startButton.disabled = true;
    shareSection.hidden = true;
    setStatus("Iniciando servidor de sinalização…");

    try {
      const { code, addresses } = await invoke("start_hosting_session");
      codeEl.textContent = code;
      renderAddresses(addresses);
      shareSection.hidden = false;
      setStatus("Aguardando alguém entrar com o código…");
      stopButton.hidden = false;

      await invoke("wait_for_peer", {
        windowTitle: source.type === "window" ? source.title : null,
        resolutionHeight,
        fps,
        audio,
      });

      setStatus("Conectado — transmitindo.", "success");
    } catch (err) {
      setStatus("Falha ao transmitir — veja o console.", "error");
      console.error("broadcast failed:", err);
      stopButton.hidden = true;
      shareSection.hidden = true;
      startButton.disabled = false;
    }
  });

  copyCodeButton.addEventListener("click", () => copyToClipboard(codeEl.textContent, copyCodeButton));

  previewToggle.addEventListener("click", async () => {
    previewToggle.disabled = true;
    try {
      if (previewOn) {
        await invoke("stop_broadcast_preview");
        resetPreview();
      } else {
        const url = await invoke("start_broadcast_preview");
        previewImg.src = url;
        previewFrame.hidden = false;
        previewToggle.textContent = "Ocultar prévia";
        previewOn = true;
      }
    } catch (err) {
      console.error("broadcast preview toggle failed:", err);
    } finally {
      previewToggle.disabled = false;
    }
  });

  stopButton.addEventListener("click", async () => {
    stopButton.disabled = true;
    try {
      await invoke("stop_broadcast");
    } catch (err) {
      console.error("stop_broadcast failed:", err);
    } finally {
      stopButton.hidden = true;
      stopButton.disabled = false;
      startButton.disabled = false;
      shareSection.hidden = true;
      resetPreview();
      setStatus("Transmissão encerrada.");
    }
  });
}

// ─────────────────────────────────  Assistir  ─────────────────────────────
// Whoever is watching still needs to be told the broadcaster's address (one
// of the ones shown on the "Transmitir" tab) and paste it in — persisted in
// localStorage so it isn't retyped every time.

const WATCH_SIGNALING_ADDR_KEY = "screen-streaming:watch-signaling-addr";

function setupWatch() {
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

  // The classic <video>.requestPictureInPicture() API doesn't apply here —
  // the stream is an <img> (see video_preview.rs for why), not a <video>
  // element. The newer Document Picture-in-Picture API works with any
  // content, which is exactly what's needed, but it's only in fairly
  // recent Chromium — feature-detect and just disable the button instead
  // of breaking on an older WebView2 runtime.
  if ("documentPictureInPicture" in window) {
    pipButton.addEventListener("click", async () => {
      try {
        const pipWindow = await window.documentPictureInPicture.requestWindow({
          width: 480,
          height: 270,
        });
        const style = pipWindow.document.createElement("style");
        style.textContent = `
          html, body { margin: 0; height: 100%; background: #000; }
          img { display: block; width: 100%; height: 100%; object-fit: contain; }
        `;
        pipWindow.document.head.append(style);

        const originalParent = video.parentElement;
        pipWindow.document.body.append(video);

        // The user picks the window's size themselves via its own resize
        // handles — requestWindow's width/height above is only the
        // starting size.
        pipWindow.addEventListener(
          "pagehide",
          () => {
            originalParent.append(video);
          },
          { once: true },
        );
      } catch (err) {
        console.error("picture-in-picture failed:", err);
      }
    });
  } else {
    pipButton.disabled = true;
    pipButton.title = "Picture-in-picture não é suportado nesta versão do WebView2.";
  }

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

      const previewUrl = await invoke("start_watching");
      video.src = previewUrl;
      video.hidden = false;
      placeholder.hidden = true;
      controls.hidden = false;
      setStatus("Recebendo transmissão.", "success");
    } catch (err) {
      setStatus("Falha ao conectar — veja o console.", "error");
      console.error("watch connect failed:", err);
    } finally {
      connectButton.disabled = false;
    }
  });
}

// ──────────────────────────────────  Init  ────────────────────────────────

window.addEventListener("DOMContentLoaded", () => {
  setupTabs();
  setupSourcePicker();
  setupCaptureTest();
  setupBroadcast();
  setupWatch();
});
