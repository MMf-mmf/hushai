// Shared topbar for the dashboard-style pages (Events / System / Files). One place
// renders the brand, the nav links (current page omitted), the live-status chip and
// the logout button, so the pages can't drift apart. The main viewer keeps its
// bespoke topbar (device picker, day nav, clock) and shares only wireLogout().

import { el } from "./dom.js";

const SECTIONS = [
  { key: "viewer", href: "/", icon: "‹", label: "Viewer", title: "Back to the viewer" },
  { key: "cameras", href: "/cameras.html", icon: "📷", label: "Cameras", title: "All cameras at a glance" },
  { key: "events", href: "/events.html", icon: "🔔", label: "Events", title: "Alerts & event feed" },
  { key: "system", href: "/dashboard.html", icon: "▦", label: "System", title: "System dashboard" },
  { key: "files", href: "/manage.html", icon: "🗄", label: "Files", title: "Manage devices & footage" },
];

let liveDot = null;
let updatedEl = null;

export function wireLogout(btn) {
  btn.addEventListener("click", () => {
    fetch("/logout", { method: "POST" }).finally(() => (location.href = "/login"));
  });
}

/** Render the shared topbar into `<header class="topbar" data-topbar>`.
 *  `section` names the current page (its nav link is omitted; its name shows in the brand). */
export function initTopbar({ section }) {
  const host = document.querySelector("[data-topbar]");
  if (!host) return;
  const current = SECTIONS.find((s) => s.key === section);
  const logout = el("button", { id: "btnLogout", class: "ghost", title: "Log out", text: "⎋" });
  wireLogout(logout);
  liveDot = el("span", { id: "liveDot", class: "dot" });
  updatedEl = el("span", { id: "generatedAt", class: "muted small", text: "connecting…" });
  host.replaceChildren(
    el(
      "div",
      { class: "brand" },
      el("span", { class: "logo", text: "◉" }),
      " HUSHAI ",
      el("span", { class: "muted", text: current ? current.label.toLowerCase() : "" }),
    ),
    ...SECTIONS.filter((s) => s.key !== section).map((s) =>
      el("a", { class: "navlink", href: s.href, title: s.title }, `${s.icon} ${s.label}`),
    ),
    el("span", { class: "spacer" }),
    el("div", { class: "clock dash-updated" }, liveDot, updatedEl),
    logout,
  );
}

/** Light the live dot (green) or dim it. */
export function setLive(ok) {
  liveDot?.classList.toggle("live", !!ok);
}

/** Set the "updated …" chip text. */
export function setUpdated(text) {
  if (updatedEl) updatedEl.textContent = text;
}
