// One modal behavior for every dialog (Voices/People/Plates/Capture + confirm):
// backdrop click + Escape close, Tab stays trapped inside, and focus returns to
// whatever opened the dialog. Pages keep their own markup (.modal > .modal-card);
// this only wires behavior + ARIA.

let labelSeq = 0;

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]):not([type="hidden"]), ' +
  'select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function focusables(root) {
  return [...root.querySelectorAll(FOCUSABLE)].filter(
    (n) => n.offsetParent !== null || n === document.activeElement,
  );
}

/** Handle a Tab keydown inside `container`, wrapping focus at the edges.
 *  Exported so confirm.js shares the exact same trap. */
export function trapFocus(container, e) {
  if (e.key !== "Tab") return;
  const items = focusables(container);
  if (!items.length) {
    e.preventDefault();
    return;
  }
  const first = items[0];
  const last = items[items.length - 1];
  const active = document.activeElement;
  if (e.shiftKey && (active === first || !container.contains(active))) {
    e.preventDefault();
    last.focus();
  } else if (!e.shiftKey && (active === last || !container.contains(active))) {
    e.preventDefault();
    first.focus();
  }
}

/** Wire open/close/trap/restore onto an existing `.modal` element.
 *  Returns { open, close, isOpen }. `initialFocus` may be a selector or element. */
export function wireModal(modalEl, { onOpen, onClose, initialFocus } = {}) {
  const card = modalEl.querySelector(".modal-card") || modalEl;
  let restoreTo = null;

  modalEl.setAttribute("role", "dialog");
  modalEl.setAttribute("aria-modal", "true");
  const label = modalEl.querySelector(".modal-head .lbl");
  if (label) {
    if (!label.id) label.id = `modal-label-${++labelSeq}`;
    modalEl.setAttribute("aria-labelledby", label.id);
  }

  modalEl.addEventListener("click", (e) => {
    if (e.target === modalEl) close(); // backdrop dismiss
  });
  modalEl.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.stopPropagation(); // don't also trigger page-level Escape handling
      close();
    } else {
      trapFocus(card, e);
    }
  });

  function open() {
    if (!modalEl.hidden) return;
    restoreTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    modalEl.hidden = false;
    onOpen?.();
    const target =
      (typeof initialFocus === "string" ? modalEl.querySelector(initialFocus) : initialFocus) ||
      focusables(card)[0] ||
      card;
    // Defer so the browser has laid the modal out (focus() on display:none is a no-op).
    setTimeout(() => target.focus?.(), 0);
  }

  function close() {
    if (modalEl.hidden) return;
    modalEl.hidden = true;
    onClose?.();
    if (restoreTo?.isConnected) restoreTo.focus();
    restoreTo = null;
  }

  return { open, close, isOpen: () => !modalEl.hidden };
}
