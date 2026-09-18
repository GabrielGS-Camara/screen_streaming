// Entry point: wires up each tab's independent module once the DOM is
// ready. See src/js/ for the actual view-model logic — this file only
// orchestrates startup order, it shouldn't grow any feature logic of its
// own.
import { setupTabs } from "./js/tabs.js";
import { setupSourcePicker } from "./js/source-picker.js";
import { setupQuality } from "./js/quality.js";
import { setupBroadcast } from "./js/broadcast.js";
import { setupWatch } from "./js/watch.js";

window.addEventListener("DOMContentLoaded", () => {
  setupTabs();
  setupSourcePicker();
  setupQuality();
  setupBroadcast();
  setupWatch();
});
