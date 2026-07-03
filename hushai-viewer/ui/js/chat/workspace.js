// Chat workspace bootstrap: one unified chat. The server's "auto" assistant classifies each
// message and dispatches to the right capability (recordings / reflection / people / objects /
// plates), so there are no agent tabs to pick — you just ask. The pane deep-links into the one
// shared video player via the store, so chat and scrubbing feel unified.

import { ChatPane } from "./chat-pane.js";
// Omni-search palette ("/" or Cmd/Ctrl+K) — a side-effect import that installs its own
// hotkeys + modal. Viewer page only for now; other pages adopt it with this same line.
import "../search/omni.js";

const AUTO_AGENT = {
  id: "auto",
  name: "Assistant",
  description: "Ask anything about your recordings — it figures out where to look.",
};

function boot() {
  const pickerEl = document.getElementById("agentPicker");
  const panesEl = document.getElementById("chatPanes");
  if (!panesEl) return;
  // No tabs in unified mode — hide the picker row if it's present in the markup.
  if (pickerEl) pickerEl.style.display = "none";

  const pane = new ChatPane(panesEl, AUTO_AGENT);
  pane.root.style.display = "flex";
}

boot();
