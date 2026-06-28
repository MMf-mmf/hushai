// Tiny shared event bus so the chat UI can drive the video player WITHOUT coupling to
// app.js internals (or touching the verified player.js / timeline.js). The only
// cross-module action today is "seek the video to a citation's moment (switching device
// if needed)". app.js subscribes; chat citation chips publish.

const bus = new EventTarget();

export function on(type, fn) {
  bus.addEventListener(type, fn);
  return () => bus.removeEventListener(type, fn);
}

export function emit(type, detail) {
  bus.dispatchEvent(new CustomEvent(type, { detail }));
}

/// Ask the player to jump to a chat citation: `{ deviceId, ms }` (ms = wall-clock).
export function seekToCitation({ deviceId, ms }) {
  emit("seekToCitation", { deviceId, ms });
}
