// A citation chip: a retrieved source (segment_id, device_id, start_unix_nanos, text,
// plus the backend-resolved speaker_name + human time_label). Clicking it deep-links the
// video timeline to that moment via the shared store.

import { nsToMs, clockMs, dateLabel } from "../time.js";
import { seekToCitation } from "../store.js";

export function renderCitation(source, index) {
  const el = document.createElement("button");
  el.className = "chat-citation";
  el.type = "button";
  const ms = nsToMs(source.start_unix_nanos);
  const snippet = (source.text || "").trim();
  // The backend humanizes speaker + time once, so the chip matches the spoken/written
  // answer. Fall back gracefully for citations persisted before those fields existed.
  const speaker = source.speaker_name || "Someone (not yet identified)";
  const when = source.time_label || clockMs(ms);

  const idx = document.createElement("span");
  idx.className = "cite-idx";
  idx.textContent = `[${index}]`;
  const who = document.createElement("span");
  who.className = "cite-speaker";
  who.textContent = speaker;
  const time = document.createElement("span");
  time.className = "cite-time";
  time.textContent = when;
  const text = document.createElement("span");
  text.className = "cite-text";
  text.textContent = snippet;

  el.append(idx, who, time, text);
  el.title = `${speaker} · ${when}\n${dateLabel(ms)} ${clockMs(ms)} · ${source.device_id}\n${snippet}`;
  el.addEventListener("click", () => seekToCitation({ deviceId: source.device_id, ms }));
  return el;
}
