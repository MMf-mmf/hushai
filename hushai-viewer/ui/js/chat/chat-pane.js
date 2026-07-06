// One conversation with one agent: message list + composer + streaming render. The
// session id is persisted in sessionStorage — PER TAB, on purpose — so a reload restores
// the conversation but a new tab starts fresh and two tabs can never share (or clobber)
// one session pointer, which used to bleed history across "different" chats. The server
// holds the actual history + citations; older conversations stay one click away in 🕘.
//
// Compose extras: a camera scope, a "Thorough" (exhaustive retrieval) toggle, and a 🕘
// conversation-history dropdown over the server's saved sessions. Completed answers grow
// hover actions (copy/download as Markdown with citation deep-links, read-aloud TTS);
// failed turns keep the question and offer a Retry. The omni-search palette hands
// questions in via the store's `chatAsk` event (same page) or `/?ask=` (cross-page).

import { streamChat, getSessionMessages, getSessions, getDevices, synthesizeSpeech } from "../api.js";
import { playbackContext, on } from "../store.js";
import { renderCitation } from "./citation.js";
import { nsToMs, localDateInput } from "../time.js";
import { toast } from "../toast.js";

const SESSION_KEY = (agentId) => `hushai.chat.session.${agentId}`;
const THOROUGH_KEY = "hushai.chat.thorough";
// Don't auto-restore a conversation whose last message is older than this — a stale
// morning session reappearing in the afternoon reads as "random history from another
// chat". Explicitly opening it from the 🕘 dropdown still works (guard bypassed).
const STALE_RESTORE_MS = 60 * 60_000;
const PLACEHOLDER =
  "Ask anything about your recordings — who you saw, what was said, things or plates on camera, or how you've been. Answers cite the moment; click a citation to jump the video there.";

// "5m ago" / "3h ago" / "2d ago" for the session list; older sessions show the date.
function relTime(iso) {
  const ms = Date.parse(iso);
  if (!isFinite(ms)) return "";
  const d = Date.now() - ms;
  if (d < 60_000) return "just now";
  const m = Math.floor(d / 60_000);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const days = Math.floor(h / 24);
  if (days < 7) return `${days}d ago`;
  return new Date(ms).toLocaleDateString();
}

export class ChatPane {
  constructor(container, agent) {
    this.agent = agent;
    // Legacy cleanup: the pointer used to live in localStorage (origin-wide, shared across
    // tabs). Drop it rather than adopt it — adopting would resurrect the cross-tab bleed once.
    localStorage.removeItem(SESSION_KEY(agent.id));
    this.sessionId = sessionStorage.getItem(SESSION_KEY(agent.id)) || null;
    this.busy = false;
    // Which camera to scope answers to. null = search across all cameras.
    this.scope = null;
    // Read-aloud state: one audio at a time; a 503 (engine not loaded) disables TTS
    // for the whole page session so every bubble's 🔊 goes quiet together.
    this._ttsBtns = new Set();
    this._ttsDead = false;
    this._audio = null;
    this._audioUrl = null;
    this._audioBtn = null;
    this.histDrop = null;
    this._build(container);
    this._loadScopeOptions();
    this._ready = this.sessionId ? this._restore() : Promise.resolve();

    // Omni-search "Ask the AI" (same page): fill the composer and send when idle.
    on("chatAsk", (e) => this._onAsk(e.detail?.text));
    // Cross-page handoff: /?ask=<question> sends on arrival, then cleans the URL so a
    // reload doesn't re-ask. Runs after restore so history renders above the new turn.
    const params = new URLSearchParams(location.search);
    const ask = params.get("ask");
    if (ask) {
      params.delete("ask");
      const qs = params.toString();
      history.replaceState(null, "", location.pathname + (qs ? `?${qs}` : "") + location.hash);
      this._ready.then(() => this._onAsk(ask));
    }
  }

  _build(container) {
    this.root = document.createElement("div");
    this.root.className = "chat-pane";

    // Toolbar: camera scope, exhaustive toggle, conversation history, fresh conversation.
    const toolbar = document.createElement("div");
    toolbar.className = "chat-toolbar";
    this.scopeSelect = document.createElement("select");
    this.scopeSelect.className = "chat-scope";
    this.scopeSelect.title = "Limit answers to one camera, or search across all of them";
    const optAll = document.createElement("option");
    optAll.value = "all";
    optAll.textContent = "All cameras";
    this.scopeSelect.appendChild(optAll);
    this.scopeSelect.addEventListener("change", () => {
      this.scope = this.scopeSelect.value === "all" ? null : this.scopeSelect.value;
    });

    // "Thorough" = the backend's exhaustive retrieval mode (list everything, not top-k).
    const thorough = document.createElement("label");
    thorough.className = "chat-thorough";
    thorough.title = "Exhaustive: list everything (needs a person filter or name in the question)";
    this.thoroughCb = document.createElement("input");
    this.thoroughCb.type = "checkbox";
    this.thoroughCb.checked = localStorage.getItem(THOROUGH_KEY) === "1";
    this.thoroughCb.addEventListener("change", () => {
      localStorage.setItem(THOROUGH_KEY, this.thoroughCb.checked ? "1" : "0");
    });
    thorough.append(this.thoroughCb, document.createTextNode("Thorough"));

    const spacer = document.createElement("span");
    spacer.className = "spacer";

    // 🕘 conversation history: dropdown over the server's saved sessions.
    this.histWrap = document.createElement("span");
    this.histWrap.className = "chat-hist-wrap";
    this.histBtn = document.createElement("button");
    this.histBtn.type = "button";
    this.histBtn.className = "ghost chat-hist-btn";
    this.histBtn.textContent = "🕘";
    this.histBtn.title = "Conversation history";
    this.histBtn.setAttribute("aria-haspopup", "true");
    this.histBtn.setAttribute("aria-expanded", "false");
    this.histBtn.addEventListener("click", () => this._toggleHistory());
    this.histWrap.appendChild(this.histBtn);

    this.newBtn = document.createElement("button");
    this.newBtn.type = "button";
    this.newBtn.className = "chat-new";
    this.newBtn.textContent = "New chat";
    this.newBtn.title = "Clear this conversation and start fresh";
    this.newBtn.addEventListener("click", () => this.reset());
    toolbar.append(this.scopeSelect, thorough, spacer, this.histWrap, this.newBtn);

    this.log = document.createElement("div");
    this.log.className = "chat-log";
    this.placeholder = this._note(PLACEHOLDER);
    this.log.appendChild(this.placeholder);

    const form = document.createElement("form");
    form.className = "chat-input";
    this.input = document.createElement("textarea");
    this.input.rows = 1;
    this.input.placeholder = "Ask anything…";
    this.sendBtn = document.createElement("button");
    this.sendBtn.type = "submit";
    this.sendBtn.textContent = "Send";
    form.append(this.input, this.sendBtn);

    form.addEventListener("submit", (e) => {
      e.preventDefault();
      this._submit();
    });
    this.input.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        this._submit();
      }
    });
    this.input.addEventListener("input", () => this._autosize());

    this.root.append(toolbar, this.log, form);
    container.appendChild(this.root);
  }

  // Populate the camera scope dropdown from the same device list the player uses.
  // Best-effort: on failure the dropdown just keeps the "All cameras" option.
  async _loadScopeOptions() {
    let devices;
    try {
      devices = await getDevices();
    } catch {
      return;
    }
    for (const d of devices) {
      const opt = document.createElement("option");
      opt.value = d.id;
      // Prefer the operator-assigned friendly name (matches the player's camera picker).
      opt.textContent = d.displayName || d.id;
      this.scopeSelect.appendChild(opt);
    }
  }

  // Clear this conversation and start a new one. The next message opens a fresh
  // server session (session_id: null); the old session stays retrievable server-side.
  reset() {
    if (this.busy) return;
    this.sessionId = null;
    sessionStorage.removeItem(SESSION_KEY(this.agent.id));
    this.placeholder = this._note(PLACEHOLDER);
    this.log.replaceChildren(this.placeholder);
    this.input.value = "";
    this._autosize();
    this.input.focus();
  }

  _autosize() {
    this.input.style.height = "auto";
    this.input.style.height = Math.min(this.input.scrollHeight, 120) + "px";
  }

  _note(text) {
    const el = document.createElement("div");
    el.className = "chat-note";
    el.textContent = text;
    return el;
  }

  _clearPlaceholder() {
    if (this.placeholder && this.placeholder.parentNode) {
      this.placeholder.remove();
      this.placeholder = null;
    }
  }

  _bubble(role) {
    this._clearPlaceholder();
    const msg = document.createElement("div");
    msg.className = `chat-msg ${role}`;
    const text = document.createElement("div");
    text.className = "chat-text";
    msg.appendChild(text);
    this.log.appendChild(msg);
    this._scroll();
    return { msg, text };
  }

  _scroll() {
    this.log.scrollTop = this.log.scrollHeight;
  }

  _renderCitations(parentMsg, sources) {
    if (!sources || !sources.length) return;
    const wrap = document.createElement("div");
    wrap.className = "chat-cites";
    sources.forEach((s, i) => wrap.appendChild(renderCitation(s, i + 1)));
    parentMsg.appendChild(wrap);
    this._scroll();
  }

  // ---- conversation history (🕘 dropdown) --------------------------------------

  _toggleHistory() {
    if (this.histDrop) this._closeHistory();
    else this._openHistory();
  }

  async _openHistory() {
    const dd = document.createElement("div");
    dd.className = "chat-sessions";
    this.histDrop = dd;
    this.histBtn.setAttribute("aria-expanded", "true");

    // "New chat" first — the dropdown doubles as the conversation switcher.
    const fresh = document.createElement("button");
    fresh.type = "button";
    fresh.className = "chat-session-row";
    const freshTitle = document.createElement("span");
    freshTitle.className = "sess-title";
    freshTitle.textContent = "＋ New chat";
    fresh.appendChild(freshTitle);
    fresh.addEventListener("click", () => {
      this._closeHistory();
      this.reset();
    });
    dd.appendChild(fresh);

    const loading = this._note("Loading conversations…");
    dd.appendChild(loading);
    this.histWrap.appendChild(dd);

    // Outside-click / Escape close. Escape is captured so nothing else reacts to it.
    this._histDocClick = (e) => {
      if (dd.contains(e.target) || e.target === this.histBtn) return;
      this._closeHistory();
    };
    this._histDocKey = (e) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        this._closeHistory();
        this.histBtn.focus();
      }
    };
    document.addEventListener("click", this._histDocClick);
    document.addEventListener("keydown", this._histDocKey, true);

    let sessions;
    try {
      sessions = await getSessions();
    } catch {
      if (this.histDrop === dd) loading.textContent = "Couldn't load conversations.";
      return;
    }
    if (this.histDrop !== dd) return; // closed while loading
    loading.remove();
    if (!sessions || !sessions.length) {
      dd.appendChild(this._note("No saved conversations yet."));
      return;
    }
    for (const s of sessions) dd.appendChild(this._sessionRow(s));
  }

  _sessionRow(s) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "chat-session-row" + (s.session_id === this.sessionId ? " is-current" : "");
    const title = document.createElement("span");
    title.className = "sess-title";
    title.textContent = (s.title || "").trim() || "Untitled conversation";
    const meta = document.createElement("span");
    meta.className = "sess-meta";
    const when = document.createElement("span");
    when.textContent = relTime(s.updated_at);
    const agent = document.createElement("span");
    agent.className = "sess-agent";
    agent.textContent = s.agent_id || "auto";
    meta.append(when, agent);
    btn.append(title, meta);
    btn.addEventListener("click", () => this._openSession(s));
    return btn;
  }

  _closeHistory() {
    if (!this.histDrop) return;
    this.histDrop.remove();
    this.histDrop = null;
    this.histBtn.setAttribute("aria-expanded", "false");
    if (this._histDocClick) document.removeEventListener("click", this._histDocClick);
    if (this._histDocKey) document.removeEventListener("keydown", this._histDocKey, true);
    this._histDocClick = this._histDocKey = null;
  }

  async _openSession(s) {
    if (this.busy) return;
    this._closeHistory();
    if (s.session_id === this.sessionId) return;
    this.sessionId = s.session_id;
    sessionStorage.setItem(SESSION_KEY(this.agent.id), this.sessionId);
    this.placeholder = this._note("Loading conversation…");
    this.log.replaceChildren(this.placeholder);
    await this._restore(true);
    // An empty (or failed-to-load) session falls back to the standard placeholder.
    if (!this.log.querySelector(".chat-msg")) {
      this.placeholder = this._note(PLACEHOLDER);
      this.log.replaceChildren(this.placeholder);
    }
    this.input.focus();
  }

  // `explicitOpen` = the user picked this session from the 🕘 dropdown, so restore it
  // regardless of age; the staleness guard only applies to silent on-load auto-restores.
  async _restore(explicitOpen = false) {
    let messages;
    try {
      messages = await getSessionMessages(this.sessionId);
    } catch {
      // Stale/forgotten session id — start fresh.
      sessionStorage.removeItem(SESSION_KEY(this.agent.id));
      this.sessionId = null;
      return;
    }
    if (!messages || !messages.length) return;
    if (!explicitOpen) {
      const last = Date.parse(messages[messages.length - 1]?.created_at);
      if (isFinite(last) && Date.now() - last > STALE_RESTORE_MS) {
        sessionStorage.removeItem(SESSION_KEY(this.agent.id));
        this.sessionId = null;
        return;
      }
    }
    for (const m of messages) {
      const { msg, text } = this._bubble(m.role === "assistant" ? "assistant" : "user");
      text.textContent = m.content;
      if (m.role === "assistant") {
        this._renderCitations(msg, m.sources);
        this._addActions(msg, m.content || "", m.sources || []);
      }
    }
    this._scroll();
  }

  _submit() {
    const value = this.input.value.trim();
    if (!value || this.busy) return;
    this.input.value = "";
    this._autosize();
    this._send(value);
  }

  // The omni palette (or /?ask=) handed us a question: stage it in the composer and
  // send immediately unless a stream is mid-flight (then it stays staged, ready to send).
  _onAsk(text) {
    const value = (text || "").trim();
    if (!value) return;
    this.input.value = value;
    this._autosize();
    if (this.busy) {
      this.input.focus();
      return;
    }
    this._submit();
  }

  async _send(message) {
    this.busy = true;
    this.sendBtn.disabled = true;

    const user = this._bubble("user");
    user.text.textContent = message;

    const assistant = this._bubble("assistant");
    let gotToken = false;
    let errored = false;
    let sources = [];

    try {
      const filters = this.scope ? { device_id: this.scope } : null;
      // What the viewer is showing right now (camera + wall-clock playhead), so the server can
      // scope deictic questions ("who was speaking in this clip") to the open video. Sent every
      // turn; the server only consults it for deictic questions, and an explicit scope above
      // still wins for the device. Null when nothing is playing.
      const pb = playbackContext();
      const playback = pb
        ? { device_id: pb.deviceId, playhead_unix_nanos: Math.round(pb.playheadMs * 1e6) }
        : null;
      await streamChat(
        {
          sessionId: this.sessionId,
          agentId: this.agent.id,
          message,
          filters,
          playback,
          exhaustive: this.thoroughCb.checked ? true : null,
        },
        (ev) => {
          switch (ev.event) {
            case "session":
              if (ev.data?.session_id) {
                this.sessionId = ev.data.session_id;
                sessionStorage.setItem(SESSION_KEY(this.agent.id), this.sessionId);
              }
              break;
            case "sources":
              sources = Array.isArray(ev.data) ? ev.data : [];
              this._renderCitations(assistant.msg, sources);
              break;
            case "token":
              gotToken = true;
              assistant.text.textContent += ev.data?.delta ?? "";
              this._scroll();
              break;
            case "error":
              errored = true;
              assistant.msg.classList.remove("assistant");
              assistant.msg.classList.add("error");
              assistant.text.textContent = ev.data?.message || "Something went wrong.";
              break;
            case "done":
            default:
              break;
          }
        },
      );
      if (!gotToken && !assistant.text.textContent) {
        assistant.text.textContent = "(no answer)";
      }
    } catch (e) {
      errored = true;
      assistant.msg.classList.remove("assistant");
      assistant.msg.classList.add("error");
      assistant.text.textContent = "Chat failed: " + e.message;
    } finally {
      // A failed turn keeps the question and offers a one-click resend; a completed
      // answer grows its hover actions (copy / download / read aloud).
      if (errored) this._addRetry(assistant.msg, message);
      else this._addActions(assistant.msg, assistant.text.textContent, sources);
      this.busy = false;
      this.sendBtn.disabled = false;
      this.input.focus();
    }
  }

  _addRetry(msg, message) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "chat-retry";
    btn.textContent = "Retry";
    btn.title = "Send this message again";
    btn.addEventListener("click", () => {
      if (this.busy) return;
      btn.remove();
      this._send(message);
    });
    msg.appendChild(btn);
    this._scroll();
  }

  // ---- per-answer actions (copy / download / read aloud) ------------------------

  _addActions(msg, text, sources) {
    if (!text || !text.trim() || text === "(no answer)") return;
    const wrap = document.createElement("div");
    wrap.className = "chat-actions";
    const mk = (glyph, title) => {
      const b = document.createElement("button");
      b.type = "button";
      b.textContent = glyph;
      b.title = title;
      wrap.appendChild(b);
      return b;
    };

    const copy = mk("⧉", "Copy answer + sources");
    copy.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(this._exportMarkdown(text, sources));
        toast("Copied");
      } catch {
        toast("Copy failed — clipboard unavailable.", { kind: "error" });
      }
    });

    const dl = mk("⬇", "Download as Markdown");
    dl.addEventListener("click", () => {
      const blob = new Blob([this._exportMarkdown(text, sources)], { type: "text/markdown" });
      const a = document.createElement("a");
      a.href = URL.createObjectURL(blob);
      a.download = `hushai-chat-${localDateInput(Date.now())}.md`;
      document.body.appendChild(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(a.href), 5000);
    });

    const speak = mk("🔊", "Read aloud");
    this._ttsBtns.add(speak);
    if (this._ttsDead) {
      speak.disabled = true;
      speak.title = "Voice engine not loaded";
    }
    speak.addEventListener("click", () => this._speak(speak, text));

    msg.appendChild(wrap);
  }

  // Answer + "Sources:" list as Markdown; each source carries the /?device=&t= deep link
  // (the same one the citation chips seek to), so a pasted answer stays verifiable.
  _exportMarkdown(text, sources) {
    const lines = [text.trim()];
    if (sources && sources.length) {
      lines.push("", "Sources:");
      sources.forEach((s, i) => {
        const ms = Math.round(nsToMs(s.start_unix_nanos));
        const who = s.speaker_name || "Someone (not yet identified)";
        const when = s.time_label || new Date(ms).toLocaleString();
        const link = `${location.origin}/?device=${encodeURIComponent(s.device_id)}&t=${ms}`;
        lines.push(`${i + 1}. ${who} · ${when} — ${link}`);
      });
    }
    return lines.join("\n") + "\n";
  }

  // Read one answer aloud via the local TTS engine. Clicking the playing bubble's ⏹
  // stops it; starting another bubble stops the first. A 503 ("engine not loaded")
  // disables read-aloud for the rest of the page session.
  async _speak(btn, text) {
    if (this._ttsDead) return;
    if (this._audioBtn === btn && this._audio) {
      this._stopAudio();
      return;
    }
    this._stopAudio();
    btn.disabled = true; // no double-fetch while synthesis is in flight
    try {
      const blob = await synthesizeSpeech(text);
      const url = URL.createObjectURL(blob);
      const audio = new Audio(url);
      this._audio = audio;
      this._audioUrl = url;
      this._audioBtn = btn;
      btn.textContent = "⏹";
      btn.title = "Stop";
      audio.addEventListener("ended", () => this._stopAudio());
      audio.addEventListener("error", () => this._stopAudio());
      await audio.play();
    } catch (e) {
      this._stopAudio();
      if (String(e?.message || "").includes("503")) {
        this._ttsDead = true;
        for (const b of this._ttsBtns) {
          b.disabled = true;
          b.title = "Voice engine not loaded";
        }
        toast("Voice engine not loaded", { kind: "error" });
      } else {
        toast("Couldn't read that aloud.", { kind: "error" });
      }
    } finally {
      btn.disabled = this._ttsDead;
    }
  }

  _stopAudio() {
    if (this._audio) this._audio.pause();
    if (this._audioUrl) {
      URL.revokeObjectURL(this._audioUrl);
      this._audioUrl = null;
    }
    if (this._audioBtn) {
      this._audioBtn.textContent = "🔊";
      this._audioBtn.title = "Read aloud";
    }
    this._audio = null;
    this._audioBtn = null;
  }
}
