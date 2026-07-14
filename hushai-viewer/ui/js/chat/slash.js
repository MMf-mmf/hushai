// Slash-command picker: type "/" as the FIRST character of an empty composer and a
// popover of plugin rows opens above it; continued typing filters ("/adv" narrows).
// ↑/↓ highlight, Enter/Tab select, Esc closes leaving the typed text, click selects.
// No conflict with the omni palette — its global "/" hotkey explicitly ignores focused
// form fields (search/omni.js), so the composer owns "/" while focused.
//
// `items`: [{id, icon, name, sub}] — generic registry rows (the workspace owns content).
// `onPick(id)`: called after the composer is cleared; the workspace flips panes.

export function attachSlashPicker(textarea, { items, onPick }) {
  const anchor = textarea.closest(".chat-input") || textarea.parentElement;
  let pop = null;
  let rows = [];
  let hi = 0;
  let docClick = null;

  function close() {
    if (!pop) return;
    pop.remove();
    pop = null;
    rows = [];
    hi = 0;
    if (docClick) {
      document.removeEventListener("mousedown", docClick);
      docClick = null;
    }
  }

  function setHi(i) {
    rows[hi]?.classList.remove("hi");
    hi = i;
    const row = rows[hi];
    if (row) {
      row.classList.add("hi");
      row.scrollIntoView({ block: "nearest" });
    }
  }

  function moveHi(dir) {
    if (!rows.length) return;
    setHi((hi + dir + rows.length) % rows.length);
  }

  function pick(id) {
    close();
    textarea.value = "";
    textarea.dispatchEvent(new Event("input")); // let the composer autosize back down
    onPick(id);
  }

  function render(filter) {
    const q = (filter || "").trim().toLowerCase();
    const matches = items.filter(
      (it) => !q || it.id.toLowerCase().startsWith(q) || it.name.toLowerCase().startsWith(q),
    );
    pop.replaceChildren();
    rows = [];
    hi = 0;
    if (!matches.length) {
      const empty = document.createElement("div");
      empty.className = "slash-empty muted";
      empty.textContent = "No matching plugin.";
      pop.appendChild(empty);
      return;
    }
    for (const it of matches) {
      const row = document.createElement("div");
      row.className = "slash-row";
      row.dataset.cmd = it.id; // e2e hook
      const ico = document.createElement("span");
      ico.className = "slash-ico";
      ico.textContent = it.icon || "▸";
      const label = document.createElement("span");
      label.className = "slash-label";
      label.textContent = it.name;
      row.append(ico, label);
      if (it.sub) {
        const sub = document.createElement("span");
        sub.className = "slash-sub";
        sub.textContent = it.sub;
        row.appendChild(sub);
      }
      // mousedown (not click) so the composer never loses focus mid-selection.
      row.addEventListener("mousedown", (e) => {
        e.preventDefault();
        pick(it.id);
      });
      row.addEventListener("mouseenter", () => setHi(rows.indexOf(row)));
      pop.appendChild(row);
      rows.push(row);
    }
    setHi(0);
  }

  function open() {
    pop = document.createElement("div");
    pop.className = "slash-pop";
    anchor.appendChild(pop);
    render("");
    docClick = (e) => {
      if (pop && !pop.contains(e.target) && e.target !== textarea) close();
    };
    document.addEventListener("mousedown", docClick);
  }

  textarea.addEventListener("input", () => {
    const v = textarea.value;
    if (pop) {
      if (!v.startsWith("/")) close();
      else render(v.slice(1));
    } else if (v === "/") {
      open();
    }
  });

  // Capture-phase on the composer form so Enter selects a row BEFORE the pane's own
  // Enter-submits handler on the textarea can send "/adv" as a chat message.
  anchor.addEventListener(
    "keydown",
    (e) => {
      if (!pop) return;
      if (e.key === "ArrowDown") {
        e.preventDefault();
        moveHi(1);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        moveHi(-1);
      } else if (e.key === "Enter" || e.key === "Tab") {
        const row = rows[hi] ?? rows[0];
        if (row) {
          // A real match: intercept so the pane's Enter-submit doesn't also fire.
          e.preventDefault();
          e.stopPropagation();
          pick(row.dataset.cmd);
        } else {
          // No plugin matches this "/"-prefixed text (e.g. "/etc/hosts …") — close the
          // picker and let Enter fall through so the message actually sends.
          close();
        }
      } else if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        close(); // leaves the typed text in the composer, on purpose
      }
    },
    true,
  );
}
