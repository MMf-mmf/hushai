// Plates settings: name the license plates discovered in your recordings, recognize one from a
// rectified-plate crop, merge duplicates the OCR over-split, and SEARCH the catalog by text
// ("when did I see plate ABC123"). The vehicle twin of the People modal. All calls go to
// hushai-backend's /v1/plates* surface via the viewer proxy (which injects the device bearer).
// The per-plate cards and the Named/Unidentified/Archived section layout are shared with
// Voices/People via entity-card.js; unlike those two, the search here is server-side
// (plate text lives in the catalog index, not just display names).

import {
  getPlates, searchPlates, samplePlateUrl, renamePlate, mergePlate,
  archivePlate, unarchivePlate,
  getWatchlist, addWatch, removeWatch,
} from "../api.js";
import { nsToMs } from "../time.js";
import { entityCard, entitySections } from "./entity-card.js";
import { wireModal } from "../modal.js";
import { toast } from "../toast.js";

function note(text) {
  const el = document.createElement("div");
  el.className = "chat-note";
  el.textContent = text;
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

function lastSeenLabel(sightings) {
  if (!sightings || !sightings.length) return "no recent sightings";
  const latestMs = Math.max(...sightings.map(nsToMs));
  return `last seen ${new Date(latestMs).toLocaleString()}`;
}

// ⭐ Watch — "Plate of Interest": alert whenever this plate is seen (auto-managed alert rule).
// Stays available on archived cards so a watched+archived plate can still be unwatched.
function watchButton(p, ctx) {
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
  return watchBtn;
}

// One plate: rectified crop + the plate string + an editable display name + sightings/last-seen,
// with Save, Watch, and "merge into", built on the shared entity card (archived plates get the
// reduced Watch + Restore card).
function plateCard(p, others, ctx) {
  const crop = document.createElement("img");
  crop.className = "person-face plate-crop";
  crop.alt = plateLabel(p);
  crop.loading = "lazy";
  crop.src = samplePlateUrl(p.plate_id);
  crop.addEventListener("error", () => crop.classList.add("is-missing"));

  const sightings = p.n_sightings != null ? p.n_sightings : p.n_samples;

  return entityCard({
    entity: p,
    cardClass: "voice-card person-card",
    media: crop,
    label: plateLabel(p),
    sublabel: [
      // Show the raw plate string under a human name so you can confirm the OCR read.
      ...(isNamed(p) && p.plate_text ? [{ class: "voice-meta muted small", text: p.plate_text }] : []),
      `${sightings} sighting${sightings === 1 ? "" : "s"} · ${lastSeenLabel(p.sample_sighting_unix_nanos)}`,
    ],
    archived: !!p.archived,
    onRename: async (name) => {
      await renamePlate(p.plate_id, name);
      await ctx.reload();
    },
    renamePlaceholder: "Name this plate (e.g. “Mom’s car”)",
    renameValue: p.display_name || "",
    extra: watchButton(p, ctx),
    mergeOptions: others.map((o) => ({ value: o.plate_id, label: plateLabel(o) })),
    mergeTitle: "Merge this plate into…",
    onMerge: async (into) => {
      await mergePlate(p.plate_id, into);
      await ctx.reload();
    },
    onArchive: async () => {
      await archivePlate(p.plate_id);
      await ctx.reload();
    },
    onRestore: async () => {
      await unarchivePlate(p.plate_id);
      await ctx.reload();
    },
    archiveTitle: "Move to Archived — hides it from these lists; recordings are unaffected.",
    restoreTitle: "Bring this plate back into the active lists",
    errors: {
      rename: "Couldn't save that name (Refresh and try again).",
      merge: "Couldn't merge those plates (Refresh and try again).",
      archive: "Couldn't disregard that plate (Refresh and try again).",
      restore: "Couldn't restore that plate (Refresh and try again).",
    },
    flash: ctx.flash,
  });
}

function render(container, plates, ctx) {
  container.replaceChildren();
  const q = ctx.query;

  if (!plates.length) {
    container.appendChild(
      note(q
        ? `No plates match “${q}”.`
        : "No plates discovered yet. License plates appear here once the vision pipeline processes video of vehicles."),
    );
    return;
  }

  // The shared three-group layout: named plates collapse closed; the still-unnamed plates
  // stay open; disregarded plates sink into a closed "Archived" disclosure at the bottom and
  // are never offered as merge targets. A text search still surfaces an archived plate (under
  // Archived) — the right answer to "did I disregard ABC123?". Section empty-states are
  // suppressed while searching (an empty group just means no matches there).
  const archivedList = plates.filter((p) => p.archived);
  const active = plates.filter((p) => !p.archived);
  const cardFor = (p) => plateCard(p, active.filter((o) => o.plate_id !== p.plate_id), ctx);
  const named = active.filter(isNamed);
  const unknown = active.filter((p) => !isNamed(p));

  for (const el of entitySections({
    known: {
      title: "Named plates",
      cards: named.map(cardFor),
      emptyText: q ? null : "No plates named yet — name one below to build your named-plates list.",
    },
    unknown: {
      title: "Unidentified plates",
      cards: unknown.map(cardFor),
      emptyText: q ? null : "No unnamed plates — every plate has a name.",
    },
    archived: {
      title: "Archived",
      cards: archivedList.map((p) => plateCard(p, [], ctx)),
    },
  })) {
    container.appendChild(el);
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
    // Operation failures ("couldn't save/merge/…") surface as page-level error toasts.
    flash: (msg) => toast(msg, { kind: "error" }),
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

  // Shared modal behavior (backdrop click, Escape, focus trap + restore) lives in modal.js;
  // opening always starts from the unfiltered list.
  const m = wireModal(modal, { onOpen: () => load("") });

  openBtn.addEventListener("click", m.open);
  if (closeBtn) closeBtn.addEventListener("click", m.close);
  if (refreshBtn) refreshBtn.addEventListener("click", () => load(ctx.query));
  if (searchBox) {
    let t;
    searchBox.addEventListener("input", () => {
      clearTimeout(t);
      t = setTimeout(() => load(searchBox.value.trim()), 250);
    });
  }
}

boot();
