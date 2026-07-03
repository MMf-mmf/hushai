// People settings: name the faces discovered in your recordings, recognize one by sight from a
// sample-face crop, search the catalog by name, and merge duplicate faces the matcher
// over-split. This is the web twin of the Android "People" screen and the visual sibling of
// the Voices modal. All calls go to hushai-backend's /v1/persons* surface via the viewer
// proxy (which injects the device bearer). The per-person cards and the
// Known/Unidentified/Archived section layout are shared with Voices/Plates via entity-card.js.

import {
  getPersons, sampleFaceUrl, renamePerson, mergePerson,
  archivePerson, unarchivePerson,
  getWatchlist, addWatch, removeWatch,
} from "../api.js";
import { nsToMs } from "../time.js";
import { entityCard, entitySections, matchesQuery } from "./entity-card.js";
import { wireModal } from "../modal.js";
import { toast } from "../toast.js";

function note(text) {
  const el = document.createElement("div");
  el.className = "chat-note";
  el.textContent = text;
  return el;
}

function personLabel(name, id) {
  return name || `Unidentified person (${String(id).slice(0, 8)})`;
}

function isIdentified(p) {
  return p.display_name != null && String(p.display_name).trim() !== "";
}

// Short "last seen" from the most recent of up to 3 sighting timestamps (ns).
function lastSeenLabel(sightings) {
  if (!sightings || !sightings.length) return "no recent sightings";
  const latestMs = Math.max(...sightings.map(nsToMs));
  return `last seen ${new Date(latestMs).toLocaleString()}`;
}

// ⭐ Watch — "Person of Interest": alert whenever this person is seen. Toggles the backend
// watchlist (which auto-manages a scoped alert rule). `ctx.watched` maps person_id → watch_id.
// Stays available on archived cards so a watched+archived person can still be unwatched.
function watchButton(p, ctx) {
  const watchId = ctx.watched.get(p.person_id);
  const watchBtn = document.createElement("button");
  watchBtn.type = "button";
  watchBtn.className = watchId ? "watch-on" : "watch-off";
  watchBtn.textContent = watchId ? "★ Watching" : "☆ Watch";
  watchBtn.title = watchId
    ? "Stop watching (remove from People of Interest)"
    : "Watch — alert me whenever this person is seen";
  watchBtn.addEventListener("click", async () => {
    watchBtn.disabled = true;
    try {
      if (watchId) await removeWatch(watchId);
      else await addWatch("person", p.person_id);
      await ctx.reload();
    } catch {
      watchBtn.disabled = false;
      ctx.flash("Couldn't update the watchlist (Refresh and try again).");
    }
  });
  return watchBtn;
}

// One person: face crop + name (editable) + sighting count / last-seen, with Save, Watch, and
// "merge into", built on the shared entity card (archived people get the reduced
// Watch + Restore card).
function personCard(p, others, ctx) {
  const face = document.createElement("img");
  face.className = "person-face";
  face.alt = personLabel(p.display_name, p.person_id);
  face.loading = "lazy";
  face.src = sampleFaceUrl(p.person_id);
  // If no decodable face crop exists yet, hide the broken-image icon.
  face.addEventListener("error", () => face.classList.add("is-missing"));

  // Distinct appearances, not raw per-frame face templates (n_samples over-counts a short clip).
  // Fall back to n_samples only if talking to an older backend that doesn't send n_sightings.
  const sightings = p.n_sightings != null ? p.n_sightings : p.n_samples;

  return entityCard({
    entity: p,
    cardClass: "voice-card person-card",
    media: face,
    label: personLabel(p.display_name, p.person_id),
    sublabel: `${sightings} sighting${sightings === 1 ? "" : "s"} · ${lastSeenLabel(p.sample_sighting_unix_nanos)}`,
    archived: !!p.archived,
    onRename: async (name) => {
      await renamePerson(p.person_id, name);
      await ctx.reload();
    },
    renamePlaceholder: "Name this person",
    renameValue: p.display_name || "",
    extra: watchButton(p, ctx),
    mergeOptions: others.map((o) => ({
      value: o.person_id,
      label: personLabel(o.display_name, o.person_id),
    })),
    mergeTitle: "Merge this person into…",
    onMerge: async (into) => {
      await mergePerson(p.person_id, into);
      await ctx.reload();
    },
    onArchive: async () => {
      await archivePerson(p.person_id);
      await ctx.reload();
    },
    onRestore: async () => {
      await unarchivePerson(p.person_id);
      await ctx.reload();
    },
    archiveTitle: "Move to Archived — hides it from these lists; recordings are unaffected.",
    restoreTitle: "Bring this person back into the active lists",
    errors: {
      rename: "Couldn't save that name (Refresh and try again).",
      merge: "Couldn't merge those people (Refresh and try again).",
      archive: "Couldn't disregard that person (Refresh and try again).",
      restore: "Couldn't restore that person (Refresh and try again).",
    },
    flash: ctx.flash,
  });
}

function render(container, persons, ctx) {
  container.replaceChildren();
  const q = ctx.query;

  if (!persons.length) {
    container.appendChild(
      note("No people discovered yet. Faces appear here once the vision pipeline processes video segments."),
    );
    return;
  }

  const list = q
    ? persons.filter((p) => matchesQuery(q, p.display_name, p.person_id))
    : persons;
  if (!list.length) {
    container.appendChild(note(`No people match “${q}”.`));
    return;
  }

  // The shared three-group layout: known (named) people collapse closed; the
  // still-unidentified faces — the ones you open this modal to name — stay open;
  // disregarded people sink into a closed "Archived" disclosure at the bottom and are never
  // offered as merge targets. Merge targets always come from the full active list, not the
  // filtered view — a search must not shrink where a person can be merged into.
  const archived = persons.filter((p) => p.archived);
  const active = persons.filter((p) => !p.archived);
  const cardFor = (p) => personCard(p, active.filter((o) => o.person_id !== p.person_id), ctx);
  const shown = (arr) => (q ? arr.filter((p) => list.includes(p)) : arr);
  const known = shown(active.filter(isIdentified));
  const unknown = shown(active.filter((p) => !isIdentified(p)));

  for (const el of entitySections({
    known: {
      title: "Known people",
      cards: known.map(cardFor),
      emptyText: q ? null : "No faces identified yet — name one below to build your known-people list.",
    },
    unknown: {
      title: "Unidentified people",
      cards: unknown.map(cardFor),
      emptyText: q ? null : "No unidentified people — every face has a name.",
    },
    archived: {
      title: "Archived",
      cards: shown(archived).map((p) => personCard(p, [], ctx)),
    },
  })) {
    container.appendChild(el);
  }
}

function boot() {
  const openBtn = document.getElementById("btnPeople");
  const modal = document.getElementById("peopleModal");
  const closeBtn = document.getElementById("peopleClose");
  const refreshBtn = document.getElementById("peopleRefresh");
  const searchBox = document.getElementById("peopleSearch");
  const body = document.getElementById("peopleBody");
  if (!openBtn || !modal || !body) return;

  const ctx = {
    query: "",
    reload: () => load(),
    watched: new Map(), // person_id -> watch_id (refreshed each load)
    // Operation failures ("couldn't save/merge/…") surface as page-level error toasts.
    flash: (msg) => toast(msg, { kind: "error" }),
  };

  // Last-fetched list, kept so the search box can re-filter client-side without refetching.
  let cache = [];
  const rerender = () => render(body, cache, ctx);

  async function load() {
    ctx.query = searchBox ? searchBox.value.trim() : "";
    body.replaceChildren(note("Loading people…"));
    // Watchlist is best-effort: a failure here must not block listing people.
    try {
      const wl = await getWatchlist();
      ctx.watched = new Map(
        (wl || []).filter((w) => w.subject_type === "person").map((w) => [w.subject_id, w.watch_id]),
      );
    } catch {
      ctx.watched = new Map();
    }
    let persons;
    try {
      persons = await getPersons();
    } catch {
      body.replaceChildren(
        note(
          "Couldn't reach the people service. Check that hushai-backend is running and the " +
            "viewer's BACKEND_TOKEN / DEVICE_TOKEN is set, then Refresh.",
        ),
      );
      return;
    }
    cache = persons || [];
    rerender();
  }

  // Shared modal behavior (backdrop click, Escape, focus trap + restore) lives in modal.js;
  // opening (re)loads the list.
  const m = wireModal(modal, { onOpen: load });

  openBtn.addEventListener("click", m.open);
  if (closeBtn) closeBtn.addEventListener("click", m.close);
  if (refreshBtn) refreshBtn.addEventListener("click", load);
  // Client-side name/id filter (plates' search is server-side; this one just re-filters
  // the cached list), debounced so typing doesn't re-render per keystroke.
  if (searchBox) {
    let t;
    searchBox.addEventListener("input", () => {
      clearTimeout(t);
      t = setTimeout(() => {
        ctx.query = searchBox.value.trim();
        rerender();
      }, 250);
    });
  }
}

boot();
