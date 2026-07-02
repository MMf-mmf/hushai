// Shared toast notifications: one bottom-center stack, screen-reader announced,
// bounded to a few at once (new toasts push the oldest out instead of piling up).
// Replaces native alert() and the per-page flash() implementations.
//
// The player viewport has its own positional #toast (inside the video); this module
// is for page-level notices on every page.

import { el } from "./dom.js";

const MAX_VISIBLE = 3;
let stack = null;

function ensureStack() {
  if (stack) return stack;
  stack = el("div", { class: "toast-stack", role: "status", "aria-live": "polite" });
  document.body.appendChild(stack);
  return stack;
}

/** Show a toast. kind: "info" | "success" | "error". Returns the element. */
export function toast(message, { kind = "info", ms = 4000 } = {}) {
  const host = ensureStack();
  while (host.children.length >= MAX_VISIBLE) host.firstChild.remove();
  const item = el("div", { class: `toast-item is-${kind}`, text: message });
  host.appendChild(item);
  const t = setTimeout(() => dismiss(), ms);
  function dismiss() {
    clearTimeout(t);
    if (!item.isConnected) return;
    item.classList.add("leaving");
    // Matches the CSS transition; remove immediately under reduced motion.
    const linger = matchMedia("(prefers-reduced-motion: reduce)").matches ? 0 : 180;
    setTimeout(() => item.remove(), linger);
  }
  item.addEventListener("click", dismiss);
  return item;
}
