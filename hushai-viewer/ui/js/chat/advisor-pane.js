// The Ahithophel advisor as a chat pane: same composer/history/streaming chrome as
// ChatPane, but a genuinely different protocol — its own endpoints (/v1/advisor/*), its
// own SSE vocabulary (phase heartbeats, follow-up `questions` rounds, grounding
// `chapters`), and no camera scope / playback / thorough. Hence a subclass overriding
// the four agent seams, not branches inside ChatPane._send.
//
// A consult turn runs 30–90 s and must never look silent: a phase pill goes up the
// moment the turn is sent ("⟳ gathering…") and each `phase` heartbeat relabels it; it
// comes down on the first token, on a questions round, or on error.

import { ChatPane } from "./chat-pane.js";
import { streamAdvisorChat, getAdvisorSessions, getAdvisorSessionMessages } from "../api.js";

export class AdvisorPane extends ChatPane {
  async _fetchSessions() {
    const sessions = await getAdvisorSessions();
    // Map to the row shape the shared 🕘 dropdown renders.
    return (sessions || []).map((s) => ({
      session_id: s.session_id,
      title: s.title,
      updated_at: s.updated_at,
      agent_id: "advisor",
    }));
  }

  async _fetchMessages(sessionId) {
    return getAdvisorSessionMessages(sessionId);
  }

  // Slim body on purpose: the advisor takes no filters/playback/exhaustive.
  _stream(payload, onEvent) {
    return streamAdvisorChat({ sessionId: payload.sessionId, message: payload.message }, onEvent);
  }

  _errorText(e) {
    // The advisor 409s a second send while a turn is still streaming (busy-session guard).
    if (/-> 409\b/.test(e?.message || "")) {
      return "The advisor is still thinking about the previous turn — Retry when it finishes.";
    }
    return super._errorText(e);
  }

  _onSendStart(ctx) {
    this.input.placeholder = this.agent.composerPlaceholder || "Ask anything…";
    this._setPhasePill(ctx, "gathering");
  }

  _handleEvent(ev, ctx) {
    const { assistant } = ctx;
    switch (ev.event) {
      case "session":
        if (ev.data?.session_id) {
          this.sessionId = ev.data.session_id;
          sessionStorage.setItem(`hushai.chat.session.${this.agent.id}`, this.sessionId);
        }
        break;
      case "phase":
        this.lastPhase = ev.data?.phase || "";
        this._setPhasePill(ctx, this.lastPhase);
        break;
      case "questions":
        this._renderQuestions(ctx, ev.data || {});
        break;
      case "chapters":
        this._renderChapters(ctx, ev.data || {});
        break;
      case "token":
        ctx.gotToken = true;
        this._clearPhasePill(ctx);
        assistant.text.textContent += ev.data?.delta ?? "";
        this._scroll();
        break;
      case "error":
        ctx.errored = true;
        this._clearPhasePill(ctx);
        assistant.msg.classList.remove("assistant");
        assistant.msg.classList.add("error");
        assistant.text.textContent = ev.data?.message || "Something went wrong.";
        break;
      case "done":
      default:
        break;
    }
  }

  // A Yenta gathering round: the turn legitimately ends with questions, not an answer.
  // One free-text reply answers the whole round (the service contract) — no quick-reply
  // buttons, just a composer placeholder flip until the next send.
  _renderQuestions(ctx, d) {
    ctx.terminal = true;
    this._clearPhasePill(ctx);
    const head = document.createElement("div");
    head.className = "chat-questions-head";
    head.textContent = `I need a bit more detail (round ${d.round ?? 1} of 2):`;
    const ol = document.createElement("ol");
    for (const q of d.questions || []) {
      const li = document.createElement("li");
      li.textContent = q;
      ol.appendChild(li);
    }
    ctx.assistant.text.append(head, ol);
    this.input.placeholder = "Answer the questions above…";
    this._scroll();
  }

  // Grounding chips ([ch. 12] Reciprocity) above the streaming text. The routing sets
  // CONVERGE, so each `chapters` event REPLACES the row rather than appending.
  _renderChapters(ctx, d) {
    if (ctx.chaptersRow) ctx.chaptersRow.remove();
    ctx.chaptersRow = this._chapterChips(d.chapters || []);
    ctx.assistant.msg.insertBefore(ctx.chaptersRow, ctx.assistant.text);
    this._scroll();
  }

  _chapterChips(chapters) {
    const row = document.createElement("div");
    row.className = "chat-cites chat-chapters";
    for (const ch of chapters) {
      const chip = document.createElement("span");
      chip.className = "chat-citation chapter-chip";
      chip.textContent = `[ch. ${ch.no}]${ch.title ? ` ${ch.title}` : ""}`;
      chip.title = ch.title || `Chapter ${ch.no}`;
      row.appendChild(chip);
    }
    return row;
  }

  // Restore fidelity: live questions render as an <ol>; restored transcripts render the
  // persisted numbered `content` string (accepted difference — never parsed back).
  _renderRestored(m) {
    const isAssistant = m.role === "assistant";
    const { msg, text } = this._bubble(isAssistant ? "assistant" : "user");
    text.textContent = m.content;
    if (m.kind === "final_answer") {
      if (Array.isArray(m.chapters) && m.chapters.length) {
        msg.insertBefore(this._chapterChips(m.chapters), text);
      }
      this._addActions(msg, m.content || "", []);
    }
  }
}
