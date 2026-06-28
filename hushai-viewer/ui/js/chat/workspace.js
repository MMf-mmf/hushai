// Chat workspace bootstrap: load the selectable agents, render the picker, and host one
// chat pane per agent (created lazily, toggled by the active tab). All panes deep-link
// into the one shared video player via the store, so chat and scrubbing feel unified.

import { getAgents } from "../api.js";
import { renderAgentPicker } from "./agent-picker.js";
import { ChatPane } from "./chat-pane.js";

async function boot() {
  const pickerEl = document.getElementById("agentPicker");
  const panesEl = document.getElementById("chatPanes");
  if (!pickerEl || !panesEl) return;

  let agents = [];
  try {
    agents = await getAgents();
  } catch {
    /* fall through to the default below */
  }
  if (!Array.isArray(agents) || !agents.length) {
    // Backend unreachable: still show a usable default tab (the chat call will surface
    // its own error if the service is truly down).
    agents = [{ id: "recordings", name: "Recordings", description: "Ask about your recordings." }];
  }

  const panes = new Map();
  let activeId = agents[0].id;

  function activate(id) {
    activeId = id;
    renderAgentPicker(pickerEl, agents, activeId, activate);
    if (!panes.has(id)) {
      const agent = agents.find((a) => a.id === id) ?? agents[0];
      panes.set(id, new ChatPane(panesEl, agent));
    }
    for (const [pid, pane] of panes) {
      pane.root.style.display = pid === id ? "flex" : "none";
    }
  }

  activate(activeId);
}

boot();
