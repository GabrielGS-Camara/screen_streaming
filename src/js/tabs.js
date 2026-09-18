export function setupTabs() {
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
