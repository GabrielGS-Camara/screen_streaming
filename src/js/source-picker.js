// The "Fonte" card, shared by the Transmitir tab: whole monitor vs. one
// specific window, with a filterable list fed by the real
// `list_capturable_windows` command. `selectedWindowTitle` is the small
// bit of state broadcast.js also needs (to know what to send when
// starting/applying) — read it via `getSelectedSource()` rather than
// reaching into this module's internals.
import { invoke } from "./tauri.js";

let selectedWindowTitle = "";

export function getSelectedSource() {
  const type = document.querySelector('input[name="source-type"]:checked').value;
  if (type === "window") {
    return { type, title: selectedWindowTitle };
  }
  return { type };
}

export function setupSourcePicker() {
  const windowPicker = document.querySelector("#window-picker");
  const windowList = document.querySelector("#window-list");
  const windowFilter = document.querySelector("#window-filter");
  const refreshButton = document.querySelector("#refresh-windows");
  const sourceRadios = document.querySelectorAll('input[name="source-type"]');
  let windows = [];
  let loaded = false;

  function renderWindowList() {
    const query = windowFilter.value.trim().toLowerCase();
    const matches = query
      ? windows.filter(
          (w) => w.title.toLowerCase().includes(query) || w.process_name.toLowerCase().includes(query),
        )
      : windows;

    windowList.innerHTML = "";
    if (matches.length === 0) {
      windowList.innerHTML = `<p class="card-hint">${loaded ? "Nenhuma janela encontrada." : "Carregando janelas…"}</p>`;
      return;
    }

    for (const win of matches) {
      const option = document.createElement("button");
      option.type = "button";
      option.className = "window-option";
      option.setAttribute("role", "radio");
      option.setAttribute("aria-checked", String(win.title === selectedWindowTitle));
      option.classList.toggle("is-selected", win.title === selectedWindowTitle);

      const title = document.createElement("span");
      title.className = "window-option-title";
      title.textContent = win.title;

      const process = document.createElement("span");
      process.className = "window-option-process";
      process.textContent = win.process_name;

      option.append(title, process);
      option.addEventListener("click", () => {
        selectedWindowTitle = win.title;
        renderWindowList();
        document.dispatchEvent(new Event("source-selection-changed"));
      });
      windowList.appendChild(option);
    }
  }

  async function loadWindows() {
    loaded = false;
    renderWindowList();
    try {
      windows = await invoke("list_capturable_windows");
      loaded = true;
      if (!windows.some((w) => w.title === selectedWindowTitle)) {
        selectedWindowTitle = windows[0]?.title ?? "";
      }
      renderWindowList();
    } catch (err) {
      loaded = true;
      windowList.innerHTML = '<p class="card-hint">Falha ao listar janelas — veja o console.</p>';
      console.error("list_capturable_windows failed:", err);
    }
  }

  sourceRadios.forEach((radio) => {
    radio.addEventListener("change", () => {
      const isWindow = document.querySelector('input[name="source-type"]:checked').value === "window";
      windowPicker.hidden = !isWindow;
      if (isWindow && !loaded) {
        loadWindows();
      }
    });
  });

  windowFilter.addEventListener("input", renderWindowList);
  refreshButton.addEventListener("click", loadWindows);

  // Pre-load in the background so the list isn't empty the moment the user
  // switches to "janela específica".
  loadWindows();
}
