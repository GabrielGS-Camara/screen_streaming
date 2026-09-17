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

// ──────────────────────────────────  Init  ────────────────────────────────

window.addEventListener("DOMContentLoaded", () => {
  setupTabs();
  setupSourcePicker();
  setupCaptureTest();
});
