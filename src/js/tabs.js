export function setupTabs() {
  const tabs = document.querySelectorAll(".tab");
  const panels = {
    broadcast: document.querySelector("#panel-broadcast"),
    watch: document.querySelector("#panel-watch"),
  };

  function switchTo(name) {
    tabs.forEach((t) => {
      const isTarget = t.dataset.tab === name;
      t.classList.toggle("is-active", isTarget);
      t.setAttribute("aria-selected", isTarget ? "true" : "false");
    });
    Object.entries(panels).forEach(([panelName, panel]) => {
      panel.classList.toggle("is-active", panelName === name);
    });
  }

  tabs.forEach((tab) => {
    tab.addEventListener("click", () => switchTo(tab.dataset.tab));
  });

  // Cross-links elsewhere in the page (e.g. "quer transmitir sua tela?" on
  // the Assistir tab) — any element with this attribute switches tabs the
  // same way clicking the tab itself would.
  document.querySelectorAll("[data-switch-tab]").forEach((el) => {
    el.addEventListener("click", () => switchTo(el.dataset.switchTab));
  });
}
