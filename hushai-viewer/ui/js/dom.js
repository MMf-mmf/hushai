// Shared DOM helpers. One `el()` for every page (supersedes the per-page copies that
// drifted apart), plus the escape helper and the standard empty/error blocks.
//
// `el()` deliberately has NO `html:` prop — all server/user text goes through
// `text:`/`textContent` so a rendering path can't become an XSS sink. If you truly
// need markup, build it from nested el() calls.

/** Minimal DOM builder. `text` sets textContent (XSS-safe); `on*` adds a listener;
 *  booleans toggle attributes (`hidden: false` removes). Children may be passed
 *  variadically or as (nested) arrays; null/undefined children are skipped. */
export function el(tag, props = {}, ...rest) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v == null) continue;
    if (k === "class") node.className = v;
    else if (k === "text") node.textContent = v;
    else if (k.startsWith("on") && typeof v === "function") node.addEventListener(k.slice(2), v);
    else if (typeof v === "boolean") node.toggleAttribute(k, v);
    else node.setAttribute(k, v);
  }
  for (const kid of rest.flat(Infinity)) {
    if (kid != null) node.append(kid);
  }
  return node;
}

/** HTML-escape for the rare spot that must assemble markup strings. Prefer el(). */
export const esc = (s) =>
  String(s).replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c],
  );

/** The standard error block for a failed section load: message + optional Retry. */
export function errorState(message, onRetry) {
  return el(
    "div",
    { class: "empty is-error" },
    el("div", { class: "err-msg", text: message }),
    onRetry ? el("button", { class: "ghost", type: "button", text: "Retry", onclick: onRetry }) : null,
  );
}

/** The standard "nothing here" block. */
export function emptyState(message) {
  return el("div", { class: "empty muted", text: message });
}
