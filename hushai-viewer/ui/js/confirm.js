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

let modal, titleEl, msgEl, typeWrap, typeLabel, typeInput, okBtn, cancelBtn;
let resolver = null;
let needText = null;

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

  titleEl = modal.querySelector(".confirm-title");
  msgEl = modal.querySelector(".confirm-msg");
  typeWrap = modal.querySelector(".confirm-type");
  typeLabel = modal.querySelector(".confirm-type-label");
  typeInput = modal.querySelector(".confirm-type-input");
  okBtn = modal.querySelector(".confirm-ok");
  cancelBtn = modal.querySelector(".confirm-cancel");

  cancelBtn.addEventListener("click", () => settle(false));
  modal.querySelector(".confirm-x").addEventListener("click", () => settle(false));
  okBtn.addEventListener("click", () => {
    if (!okBtn.disabled) settle(true);
  });
  modal.addEventListener("click", (e) => {
    if (e.target === modal) settle(false); // backdrop dismiss
  });
  typeInput.addEventListener("input", refreshOk);
  document.addEventListener("keydown", (e) => {
    if (modal.hidden) return;
    if (e.key === "Escape") settle(false);
    else if (e.key === "Enter" && !okBtn.disabled) settle(true);
  });
}

function refreshOk() {
  okBtn.disabled = needText != null && typeInput.value.trim() !== needText;
}

function settle(val) {
  modal.hidden = true;
  const r = resolver;
  resolver = null;
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

  modal.hidden = false;
  setTimeout(() => (needText ? typeInput : okBtn).focus(), 0);

  return new Promise((resolve) => {
    resolver = resolve;
  });
}
