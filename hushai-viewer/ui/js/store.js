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

/// Hand a question to the AI chat pane: `{ text }`. The omni-search palette's
/// "Ask: <query>" row publishes; chat-pane.js subscribes (stages the text in its
/// composer and sends when idle). Pages without a chat pane navigate to /?ask= instead.
export function chatAsk(text) {
  emit("chatAsk", { text });
}

// The pull-model twin of seekToCitation: chat asks "what is the viewer showing right now?"
// without coupling to app.js internals. app.js registers a provider at init; the chat pane
// pulls it per send so the backend can scope deictic questions ("who was speaking in this
// clip") to the on-screen camera + playhead.
let playbackProvider = null;

/// app.js registers `fn() -> { deviceId, playheadMs } | null` (null = nothing playing).
export function setPlaybackProvider(fn) {
  playbackProvider = fn;
}

/// The current playback context, or null (no provider / nothing playing / provider threw).
export function playbackContext() {
  if (!playbackProvider) return null;
  try {
    return playbackProvider() ?? null;
  } catch {
    return null;
  }
}
