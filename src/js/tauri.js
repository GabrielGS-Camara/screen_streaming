// Thin bridge to the Tauri backend — every other module talks to Rust only
// through these two exports, never `window.__TAURI__` directly. Both are
// `undefined` when the page runs outside Tauri (e.g. the static preview
// used to iterate on layout without recompiling the app), so callers use
// them defensively (`invoke?.(...)`, or expect the promise to reject).
export const invoke = window.__TAURI__?.core?.invoke;
export const listenTauriEvent = window.__TAURI__?.event?.listen;
