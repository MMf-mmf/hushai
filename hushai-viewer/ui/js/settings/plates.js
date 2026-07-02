// Plates settings: name the license plates discovered in your recordings, recognize one from a
// rectified-plate crop, merge duplicates the OCR over-split, and SEARCH the catalog by text
// ("when did I see plate ABC123"). The vehicle twin of the People modal. All calls go to
// hushai-backend's /v1/plates* surface via the viewer proxy (which injects the device bearer).

import {
  getPlates, searchPlates, samplePlateUrl, renamePlate, mergePlate,
  archivePlate, unarchivePlate,
  getWatchlist, addWatch, removeWatch,
} from "../api.js";
import { nsToMs } from "../time.js";

function note(text) {
  const el = document.createElement("div");
  el.className = "chat-note";
  el.textContent = text;
  return el;
}

function div(className, text) {
  const el = document.createElement("div");
  if (className) el.className = className;
  if (text != null) el.textContent = text;
  return el;
}

// Unnamed plates are labelled by their (voted) string; fall back to the id when text is absent.
function plateLabel(p) {
  if (p.display_name && String(p.display_name).trim()) return p.display_name.trim();
  if (p.plate_text && String(p.plate_text).trim()) return p.plate_text.trim();
  return `Unreadable plate (${String(p.plate_id).slice(0, 8)})`;
}

function isNamed(p) {
  return p.display_name != null && String(p.display_name).trim() !== "";
}

function sectionTitle(text, count) {
  const el = div("voice-section-title");
  el.textContent = text;
  const c = document.createElement("span");
  c.className = "muted small";
  c.textContent = `  (${count})`;
  el.appendChild(c);
  return el;
}

function collapsibleSection(text, count, cardEls) {
  const details = document.createElement("details");
  details.className = "voice-section";
  const summary = document.createElement("summary");
  summary.className = "voice-section-title";
  summary.textContent = text;
  const c = document.createElement("span");
  c.className = "muted small";
  c.textContent = `  (${count})`;
  summary.appendChild(c);
  details.appendChild(summary);
  const body = div("voice-section-body");
  for (const el of cardEls) body.appendChild(el);
  details.appendChild(body);
  return details;
}

function lastSeenLabel(sightings) {
  if (!sightings || !sightings.length) return "no recent sightings";
  const latestMs = Math.max(...sightings.map(nsToMs));
  return `last seen ${new Date(latestMs).toLocaleString()}`;
}

// One plate: rectified crop + the plate string + an editable display name + sightings/last-seen,
// with Save and "merge into". An archived (disregarded) plate renders a reduced card: Watch
// toggle + Restore only — Watch stays available so a watched+archived plate can be unwatched.
function plateCard(p, others, ctx) {
  const card = div("voice-card person-card");

  const crop = document.createElement("img");
  crop.className = "person-face plate-crop";
  crop.alt = plateLabel(p);
  crop.loading = "lazy";
  crop.src = samplePlateUrl(p.plate_id);
  crop.addEventListener("error", () => crop.classList.add("is-missing"));
  card.appendChild(crop);

  const info = div("person-info");
  info.appendChild(div("voice-name", plateLabel(p)));
  // Show the raw plate string under a human name so you can confirm the OCR read.
  if (isNamed(p) && p.plate_text) {
    info.appendChild(div("voice-meta muted small", p.plate_text));
  }
  const sightings = p.n_sightings != null ? p.n_sightings : p.n_samples;
  info.appendChild(
    div("voice-meta", `${sightings} sighting${sightings === 1 ? "" : "s"} · ${lastSeenLabel(p.sample_sighting_unix_nanos)}`),
  );

  const actions = div("voice-actions");
  if (!p.archived) {
    const input = document.createElement("input");
    input.type = "text";
    input.placeholder = "Name this plate (e.g. “Mom’s car”)";
    input.value = p.display_name || "";
    const save = document.createElement("button");
    save.type = "button";
    save.textContent = "Save";
    save.addEventListener("click", async () => {
      const name = input.value.trim();
      if (!name) return;
      save.disabled = true;
      try {
        await renamePlate(p.plate_id, name);
        await ctx.reload();
      } catch {
        save.disabled = false;
        ctx.flash("Couldn't save that name (Refresh and try again).");
      }
    });
    actions.append(input, save);
  }

  // ⭐ Watch — "Plate of Interest": alert whenever this plate is seen (auto-managed alert rule).
  const watchId = ctx.watched.get(p.plate_id);
  const watchBtn = document.createElement("button");
  watchBtn.type = "button";
  watchBtn.className = watchId ? "watch-on" : "watch-off";
  watchBtn.textContent = watchId ? "★ Watching" : "☆ Watch";
  watchBtn.title = watchId
    ? "Stop watching (remove from Plates of Interest)"
    : "Watch — alert me whenever this plate is seen";
  watchBtn.addEventListener("click", async () => {
    watchBtn.disabled = true;
    try {
      if (watchId) await removeWatch(watchId);
      else await addWatch("plate", p.plate_id);
      await ctx.reload();
    } catch {
      watchBtn.disabled = false;
      ctx.flash("Couldn't update the watchlist (Refresh and try again).");
    }
  });
  actions.appendChild(watchBtn);

  if (!p.archived && others.length) {
    const merge = document.createElement("select");
    merge.title = "Merge this plate into…";
    const def = document.createElement("option");
    def.value = "";
    def.textContent = "Merge into…";
    merge.appendChild(def);
    for (const o of others) {
      const opt = document.createElement("option");
      opt.value = o.plate_id;
      opt.textContent = plateLabel(o);
      merge.appendChild(opt);
    }
    merge.addEventListener("change", async () => {
      const into = merge.value;
      if (!into) return;
      merge.disabled = true;
      try {
        await mergePlate(p.plate_id, into);
        await ctx.reload();
      } catch {
        merge.disabled = false;
        ctx.flash("Couldn't merge those plates (Refresh and try again).");
      }
    });
    actions.appendChild(merge);
  }

  const toggle = document.createElement("button");
  toggle.type = "button";
  toggle.textContent = p.archived ? "Restore" : "Disregard";
  toggle.title = p.archived
    ? "Bring this plate back into the active lists"
    : "Move to Archived — hides it from these lists; recordings are unaffected.";
  toggle.addEventListener("click", async () => {
    toggle.disabled = true;
    try {
      if (p.archived) await unarchivePlate(p.plate_id);
      else await archivePlate(p.plate_id);
      await ctx.reload();
    } catch {
      toggle.disabled = false;
      ctx.flash(p.archived
        ? "Couldn't restore that plate (Refresh and try again)."
        : "Couldn't disregard that plate (Refresh and try again).");
    }
  });
  actions.appendChild(toggle);

  info.appendChild(actions);
  card.appendChild(info);
  return card;
}

function render(container, plates, ctx) {
  container.replaceChildren();

  if (!plates.length) {
    container.appendChild(
      note(ctx.query
        ? `No plates match “${ctx.query}”.`
        : "No plates discovered yet. License plates appear here once the vision pipeline processes video of vehicles."),
    );
    return;
  }

  // Disregarded plates sink into a closed "Archived" disclosure at the bottom and are never
  // offered as merge targets. A text search still surfaces an archived plate (under Archived) —
  // the right answer to "did I disregard ABC123?".
  const archivedList = plates.filter((p) => p.archived);
  const active = plates.filter((p) => !p.archived);
  const cardFor = (p) => plateCard(p, active.filter((o) => o.plate_id !== p.plate_id), ctx);
  const named = active.filter(isNamed);
  const unknown = active.filter((p) => !isNamed(p));

  if (named.length) {
    container.appendChild(collapsibleSection("Named plates", named.length, named.map(cardFor)));
  }
  if (unknown.length) {
    container.appendChild(sectionTitle("Unidentified plates", unknown.length));
    unknown.forEach((p) => container.appendChild(cardFor(p)));
  }
  if (archivedList.length) {
    container.appendChild(
      collapsibleSection("Archived", archivedList.length, archivedList.map((p) => plateCard(p, [], ctx))),
    );
  }
}

function boot() {
  const openBtn = document.getElementById("btnPlates");
  const modal = document.getElementById("platesModal");
  const closeBtn = document.getElementById("platesClose");
  const refreshBtn = document.getElementById("platesRefresh");
  const searchBox = document.getElementById("platesSearch");
  const body = document.getElementById("platesBody");
  if (!openBtn || !modal || !body) return;

  const ctx = {
    query: "",
    reload: () => load(ctx.query),
    watched: new Map(), // plate_id -> watch_id (refreshed each load)
    flash(msg) {
      const n = note(msg);
      body.prepend(n);
      setTimeout(() => n.remove(), 4000);
    },
  };

  async function load(query) {
    ctx.query = query || "";
    body.replaceChildren(note(ctx.query ? `Searching “${ctx.query}”…` : "Loading plates…"));
    // Watchlist is best-effort: a failure must not block listing plates.
    try {
      const wl = await getWatchlist();
      ctx.watched = new Map(
        (wl || []).filter((w) => w.subject_type === "plate").map((w) => [w.subject_id, w.watch_id]),
      );
    } catch {
      ctx.watched = new Map();
    }
    let plates;
    try {
      plates = ctx.query ? await searchPlates(ctx.query) : await getPlates();
    } catch {
      body.replaceChildren(
        note(
          "Couldn't reach the plates service. Check that hushai-backend is running and the " +
            "viewer's BACKEND_TOKEN / DEVICE_TOKEN is set, then Refresh.",
        ),
      );
      return;
    }
    render(body, plates || [], ctx);
  }

  function open() {
    modal.hidden = false;
    load("");
  }
  function close() {
    modal.hidden = true;
  }

  openBtn.addEventListener("click", open);
  if (closeBtn) closeBtn.addEventListener("click", close);
  if (refreshBtn) refreshBtn.addEventListener("click", () => load(ctx.query));
  if (searchBox) {
    let t;
    searchBox.addEventListener("input", () => {
      clearTimeout(t);
      t = setTimeout(() => load(searchBox.value.trim()), 250);
    });
  }
  modal.addEventListener("click", (e) => {
    if (e.target === modal) close();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !modal.hidden) close();
  });
}

boot();
