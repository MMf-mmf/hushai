// Shared confirmation dialog for destructive actions. `confirmAction({...})` returns a Promise that
// resolves true on confirm, false on cancel/Esc/backdrop. Impact text (counts/size) is passed in by
// the caller — it already has the loaded usage rows, so no extra round-trip.
//
// Type-to-confirm: pass `requireText` (e.g. the device name) and the confirm button stays disabled
// until the user types it exactly. Used for the irreversible whole-device / entire-history deletes;
// single-date and range deletes confirm without typing.
//
// Self-contained: the modal DOM is created lazily and appended to <body>, so any page can import this
// without adding markup. One dialog at a time (a new call cancels a pending one).

import { trapFocus } from "./modal.js";

let modal, card, titleEl, msgEl, typeWrap, typeLabel, typeInput, okBtn, cancelBtn, xBtn;
let resolver = null;
let needText = null;
let restoreTo = null; // whatever had focus when the dialog opened

function build() {
  if (modal) return;
  modal = document.createElement("div");
  modal.className = "modal confirm-modal";
  modal.hidden = true;
  modal.innerHTML = `
    <div class="modal-card confirm-card">
      <div class="modal-head">
        <span class="lbl confirm-title"></span>
        <span class="spacer"></span>
        <button type="button" class="confirm-x" title="Cancel">✕</button>
      </div>
      <div class="modal-body">
        <p class="confirm-msg"></p>
        <label class="confirm-type" hidden>
          <span class="confirm-type-label muted small"></span>
          <input type="text" class="confirm-type-input" autocomplete="off" spellcheck="false" />
        </label>
      </div>
      <div class="confirm-actions">
        <button type="button" class="ghost confirm-cancel">Cancel</button>
        <button type="button" class="danger confirm-ok"></button>
      </div>
    </div>`;
  document.body.appendChild(modal);
  modal.setAttribute("role", "dialog");
  modal.setAttribute("aria-modal", "true");

  card = modal.querySelector(".confirm-card");
  titleEl = modal.querySelector(".confirm-title");
  msgEl = modal.querySelector(".confirm-msg");
  typeWrap = modal.querySelector(".confirm-type");
  typeLabel = modal.querySelector(".confirm-type-label");
  typeInput = modal.querySelector(".confirm-type-input");
  okBtn = modal.querySelector(".confirm-ok");
  cancelBtn = modal.querySelector(".confirm-cancel");
  xBtn = modal.querySelector(".confirm-x");
  titleEl.id = "confirm-title";
  modal.setAttribute("aria-labelledby", titleEl.id);

  cancelBtn.addEventListener("click", () => settle(false));
  xBtn.addEventListener("click", () => settle(false));
  okBtn.addEventListener("click", () => {
    if (!okBtn.disabled) settle(true);
  });
  modal.addEventListener("click", (e) => {
    if (e.target === modal) settle(false); // backdrop dismiss
  });
  typeInput.addEventListener("input", refreshOk);
  // On the modal (focus is trapped inside), not the document. Enter must NOT confirm
  // when focus sits on Cancel/✕ — their own native activation handles those.
  modal.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      settle(false);
    } else if (e.key === "Enter" && !okBtn.disabled && e.target !== cancelBtn && e.target !== xBtn) {
      settle(true);
    } else {
      trapFocus(card, e);
    }
  });
}

function refreshOk() {
  okBtn.disabled = needText != null && typeInput.value.trim() !== needText;
}

function settle(val) {
  modal.hidden = true;
  const r = resolver;
  resolver = null;
  if (restoreTo?.isConnected) restoreTo.focus();
  restoreTo = null;
  if (r) r(val);
}

/** Returns true if the confirm dialog is currently open (so callers can pause background refresh). */
export function confirmOpen() {
  return !!modal && !modal.hidden;
}

export function confirmAction({
  title = "Are you sure?",
  message = "",
  confirmLabel = "Delete",
  requireText = null,
} = {}) {
  build();
  if (resolver) settle(false); // cancel any pending dialog

  titleEl.textContent = title;
  msgEl.textContent = message;
  okBtn.textContent = confirmLabel;
  needText = requireText && String(requireText).trim() ? String(requireText) : null;

  if (needText) {
    typeWrap.hidden = false;
    typeLabel.textContent = `Type “${needText}” to confirm`;
  } else {
    typeWrap.hidden = true;
  }
  typeInput.value = "";
  refreshOk();

  restoreTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  modal.hidden = false;
  setTimeout(() => (needText ? typeInput : okBtn).focus(), 0);

  return new Promise((resolve) => {
    resolver = resolve;
  });
}
