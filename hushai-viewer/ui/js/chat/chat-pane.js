// One conversation with one agent: message list + composer + streaming render. The
// session id is persisted in localStorage so the conversation restores on reload (the
// server holds the actual history + citations).

import { streamChat, getSessionMessages, getDevices } from "../api.js";
import { renderCitation } from "./citation.js";

const SESSION_KEY = (agentId) => `hushai.chat.session.${agentId}`;
const PLACEHOLDER =
  "Ask a question about your recordings. Answers cite the moment — click a citation to jump the video there.";

export class ChatPane {
  constructor(container, agent) {
    this.agent = agent;
    this.sessionId = localStorage.getItem(SESSION_KEY(agent.id)) || null;
    this.busy = false;
    // Which camera to scope answers to. null = search across all cameras.
    this.scope = null;
    this._build(container);
    this._loadScopeOptions();
    if (this.sessionId) this._restore();
  }

  _build(container) {
    this.root = document.createElement("div");
    this.root.className = "chat-pane";

    // Toolbar: pick a camera to scope answers to, and start a fresh conversation.
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
    const spacer = document.createElement("span");
    spacer.className = "spacer";
    this.newBtn = document.createElement("button");
    this.newBtn.type = "button";
    this.newBtn.className = "chat-new";
    this.newBtn.textContent = "New chat";
    this.newBtn.title = "Clear this conversation and start fresh";
    this.newBtn.addEventListener("click", () => this.reset());
    toolbar.append(this.scopeSelect, spacer, this.newBtn);

    this.log = document.createElement("div");
    this.log.className = "chat-log";
    this.placeholder = this._note(PLACEHOLDER);
    this.log.appendChild(this.placeholder);

    const form = document.createElement("form");
    form.className = "chat-input";
    this.input = document.createElement("textarea");
    this.input.rows = 1;
    this.input.placeholder = "Ask about your recordings…";
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
      opt.textContent = d.id;
      this.scopeSelect.appendChild(opt);
    }
  }

  // Clear this conversation and start a new one. The next message opens a fresh
  // server session (session_id: null); the old session stays retrievable server-side.
  reset() {
    if (this.busy) return;
    this.sessionId = null;
    localStorage.removeItem(SESSION_KEY(this.agent.id));
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

  async _restore() {
    let messages;
    try {
      messages = await getSessionMessages(this.sessionId);
    } catch {
      // Stale/forgotten session id — start fresh.
      localStorage.removeItem(SESSION_KEY(this.agent.id));
      this.sessionId = null;
      return;
    }
    if (!messages || !messages.length) return;
    for (const m of messages) {
      const { msg, text } = this._bubble(m.role === "assistant" ? "assistant" : "user");
      text.textContent = m.content;
      if (m.role === "assistant") this._renderCitations(msg, m.sources);
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

  async _send(message) {
    this.busy = true;
    this.sendBtn.disabled = true;

    const user = this._bubble("user");
    user.text.textContent = message;

    const assistant = this._bubble("assistant");
    let gotToken = false;

    try {
      const filters = this.scope ? { device_id: this.scope } : null;
      await streamChat(
        { sessionId: this.sessionId, agentId: this.agent.id, message, filters },
        (ev) => {
          switch (ev.event) {
            case "session":
              if (ev.data?.session_id) {
                this.sessionId = ev.data.session_id;
                localStorage.setItem(SESSION_KEY(this.agent.id), this.sessionId);
              }
              break;
            case "sources":
              this._renderCitations(assistant.msg, ev.data);
              break;
            case "token":
              gotToken = true;
              assistant.text.textContent += ev.data?.delta ?? "";
              this._scroll();
              break;
            case "error":
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
      assistant.msg.classList.remove("assistant");
      assistant.msg.classList.add("error");
      assistant.text.textContent = "Chat failed: " + e.message;
    } finally {
      this.busy = false;
      this.sendBtn.disabled = false;
      this.input.focus();
    }
  }
}
