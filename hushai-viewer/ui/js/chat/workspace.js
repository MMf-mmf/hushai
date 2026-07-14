// Chat workspace bootstrap: one unified chat plus slash-command plugins. The default
// "auto" assistant classifies each message and dispatches to the right capability
// (recordings / reflection / people / objects / plates), so there are no agent tabs to
// pick — you just ask. Typing "/" in an empty composer opens the plugin picker:
// /advisor (book-grounded personal advice, its own service + protocol) and /gotham —
// the Detective (agentic investigation over the same rag endpoint, superset SSE).
// All panes stay constructed + live while switching; each keeps its own server session
// (sessionStorage key `hushai.chat.session.<agent id>`). The pane deep-links into the
// one shared video player via the store, so chat and scrubbing feel unified.

import { ChatPane } from "./chat-pane.js";
import { AdvisorPane } from "./advisor-pane.js";
import { attachSlashPicker } from "./slash.js";
import { on } from "../store.js";
// Omni-search palette ("/" or Cmd/Ctrl+K) — a side-effect import that installs its own
// hotkeys + modal. Viewer page only for now; other pages adopt it with this same line.
import "../search/omni.js";

const AUTO_AGENT = {
  id: "auto",
  icon: "🎥",
  name: "Assistant",
  sub: "your recordings",
  description: "Ask anything about your recordings — it figures out where to look.",
  headSub: "over your recordings",
  acceptsAsk: true,
  placeholder:
    "Ask anything about your recordings — who you saw, what was said, things or plates on camera, or how you've been. Answers cite the moment; click a citation to jump the video there. " +
    'Type "/" for plugins — /advisor gives book-grounded personal advice; /gotham opens the Detective.',
  // Keep the shared rag session list scoped to this pane: Detective conversations get
  // their own pane + history (the advisor's live in a different service entirely).
  sessionFilter: (s) => (s.agent_id || "auto") !== "gotham",
};

const ADVISOR_AGENT = {
  id: "advisor",
  icon: "📖",
  name: "Advisor",
  sub: "book-grounded personal advice",
  headSub: "personal advisor",
  chip: true,
  hideScope: true,
  placeholder:
    "Describe the situation you want advice on. Expect a follow-up question or two, and give it a minute to think — answers are grounded in the book's chapters.",
  composerPlaceholder: "What would you like advice on?",
};

const GOTHAM_AGENT = {
  id: "gotham",
  icon: "🕵️",
  name: "Detective",
  sub: "multi-step investigation with tools",
  headSub: "investigation",
  chip: true,
  hideScope: true,
  placeholder:
    "Ask an investigative question — the Detective plans, calls tools (people, plates, events, the relationship graph), and shows each step. Try: “walk me through what Alice did yesterday”.",
  composerPlaceholder: "Investigate…",
  sessionFilter: (s) => s.agent_id === "gotham",
};

function boot() {
  const pickerEl = document.getElementById("agentPicker");
  const panesEl = document.getElementById("chatPanes");
  if (!panesEl) return;
  // No tabs — mode switching is the slash picker's job; hide the legacy strip.
  if (pickerEl) pickerEl.style.display = "none";
  const headSub = document.querySelector("#chatdock .chatdock-head .muted");

  const registry = [AUTO_AGENT, ADVISOR_AGENT, GOTHAM_AGENT];
  const panes = new Map();
  let mode = "auto";

  const setMode = (id, { focus = true } = {}) => {
    if (!panes.has(id)) return;
    mode = id;
    for (const [pid, pane] of panes) pane.root.style.display = pid === id ? "flex" : "none";
    const agent = registry.find((a) => a.id === id);
    if (headSub && agent?.headSub) headSub.textContent = agent.headSub;
    if (focus) panes.get(id).input.focus();
  };
  for (const agent of registry) {
    if (agent.chip) agent.onExit = () => setMode("auto");
    const Pane = agent.id === "advisor" ? AdvisorPane : ChatPane;
    const pane = new Pane(panesEl, agent);
    pane.root.style.display = "none";
    panes.set(agent.id, pane);
    attachSlashPicker(pane.input, {
      items: registry.map(({ id, icon, name, sub }) => ({ id, icon, name, sub })),
      onPick: setMode,
    });
  }
  // Don't steal page focus on load — the omni palette owns the global "/" hotkey until
  // the user clicks into the composer.
  setMode("auto", { focus: false });

  // Omni-search "Ask the AI" always targets the recordings assistant (only the auto pane
  // listens for chatAsk). If a plugin pane is showing, surface the auto pane first so the
  // answer streams somewhere visible instead of into the hidden default pane.
  on("chatAsk", () => {
    if (mode !== "auto") setMode("auto");
  });

  // e2e/debug hooks (the viewerDebug precedent): current mode, the active pane's server
  // session id, and the last streamed phase (advisor heartbeats / Detective planning).
  window.chatDebug = {
    get mode() {
      return mode;
    },
    get sessionId() {
      return panes.get(mode)?.sessionId ?? null;
    },
    get lastPhase() {
      return panes.get(mode)?.lastPhase ?? null;
    },
  };
}

boot();
