// People settings: name the faces discovered in your recordings, recognize one by sight from a
// sample-face crop, and merge duplicate faces the matcher over-split. This is the web twin of
// the Android "People" screen and the visual sibling of the Voices modal. All calls go to
// hushai-backend's /v1/persons* surface via the viewer proxy (which injects the device bearer).

import {
  getPersons, sampleFaceUrl, renamePerson, mergePerson,
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

function personLabel(name, id) {
  return name || `Unidentified person (${String(id).slice(0, 8)})`;
}

function isIdentified(p) {
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

// A collapsed-by-default disclosure ("Known people (N)") wrapping its cards. Already-identified
// people are tucked away so opening the modal surfaces the unidentified ones (the faces you
// actually need to name); the known list grows over time and isn't useful on every open.
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

// Short "last seen" from the most recent of up to 3 sighting timestamps (ns).
function lastSeenLabel(sightings) {
  if (!sightings || !sightings.length) return "no recent sightings";
  const latestMs = Math.max(...sightings.map(nsToMs));
  return `last seen ${new Date(latestMs).toLocaleString()}`;
}

// One person: face crop + name (editable) + sample count / last-seen, with Save and "merge into".
function personCard(p, others, ctx) {
  const card = div("voice-card person-card");

  const face = document.createElement("img");
  face.className = "person-face";
  face.alt = personLabel(p.display_name, p.person_id);
  face.loading = "lazy";
  face.src = sampleFaceUrl(p.person_id);
  // If no decodable face crop exists yet, hide the broken-image icon.
  face.addEventListener("error", () => face.classList.add("is-missing"));
  card.appendChild(face);

  const info = div("person-info");
  info.appendChild(div("voice-name", personLabel(p.display_name, p.person_id)));
  // Distinct appearances, not raw per-frame face templates (n_samples over-counts a short clip).
  // Fall back to n_samples only if talking to an older backend that doesn't send n_sightings.
  const sightings = p.n_sightings != null ? p.n_sightings : p.n_samples;
  info.appendChild(
    div("voice-meta", `${sightings} sighting${sightings === 1 ? "" : "s"} · ${lastSeenLabel(p.sample_sighting_unix_nanos)}`),
  );

  const actions = div("voice-actions");
  const input = document.createElement("input");
  input.type = "text";
  input.placeholder = "Name this person";
  input.value = p.display_name || "";
  const save = document.createElement("button");
  save.type = "button";
  save.textContent = "Save";
  save.addEventListener("click", async () => {
    const name = input.value.trim();
    if (!name) return;
    save.disabled = true;
    try {
      await renamePerson(p.person_id, name);
      await ctx.reload();
    } catch {
      save.disabled = false;
      ctx.flash("Couldn't save that name (Refresh and try again).");
    }
  });
  actions.append(input, save);

  // ⭐ Watch — "Person of Interest": alert whenever this person is seen. Toggles the backend
  // watchlist (which auto-manages a scoped alert rule). `ctx.watched` maps person_id → watch_id.
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
  actions.appendChild(watchBtn);

  // "Merge into" — fold this face into another person (same human, split across ids).
  if (others.length) {
    const merge = document.createElement("select");
    merge.title = "Merge this person into…";
    const def = document.createElement("option");
    def.value = "";
    def.textContent = "Merge into…";
    merge.appendChild(def);
    for (const o of others) {
      const opt = document.createElement("option");
      opt.value = o.person_id;
      opt.textContent = personLabel(o.display_name, o.person_id);
      merge.appendChild(opt);
    }
    merge.addEventListener("change", async () => {
      const into = merge.value;
      if (!into) return;
      merge.disabled = true;
      try {
        await mergePerson(p.person_id, into);
        await ctx.reload();
      } catch {
        merge.disabled = false;
        ctx.flash("Couldn't merge those people (Refresh and try again).");
      }
    });
    actions.appendChild(merge);
  }

  info.appendChild(actions);
  card.appendChild(info);
  return card;
}

function render(container, persons, ctx) {
  container.replaceChildren();

  if (!persons.length) {
    container.appendChild(
      note("No people discovered yet. Faces appear here once the vision pipeline processes video segments."),
    );
    return;
  }

  const cardFor = (p) => personCard(p, persons.filter((o) => o.person_id !== p.person_id), ctx);

  // The known (named) people collapse into a closed disclosure; the still-unidentified faces —
  // the ones you open this modal to name — stay shown.
  const known = persons.filter(isIdentified);
  const unknown = persons.filter((p) => !isIdentified(p));

  if (known.length) {
    container.appendChild(collapsibleSection("Known people", known.length, known.map(cardFor)));
  } else {
    container.appendChild(sectionTitle("Known people", 0));
    container.appendChild(note("No faces identified yet — name one below to build your known-people list."));
  }

  if (unknown.length) {
    container.appendChild(sectionTitle("Unidentified people", unknown.length));
    unknown.forEach((p) => container.appendChild(cardFor(p)));
  }
}

function boot() {
  const openBtn = document.getElementById("btnPeople");
  const modal = document.getElementById("peopleModal");
  const closeBtn = document.getElementById("peopleClose");
  const refreshBtn = document.getElementById("peopleRefresh");
  const body = document.getElementById("peopleBody");
  if (!openBtn || !modal || !body) return;

  const ctx = {
    reload: () => load(),
    watched: new Map(), // person_id -> watch_id (refreshed each load)
    flash(msg) {
      const n = note(msg);
      body.prepend(n);
      setTimeout(() => n.remove(), 4000);
    },
  };

  async function load() {
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
    render(body, persons || [], ctx);
  }

  function open() {
    modal.hidden = false;
    load();
  }
  function close() {
    modal.hidden = true;
  }

  openBtn.addEventListener("click", open);
  if (closeBtn) closeBtn.addEventListener("click", close);
  if (refreshBtn) refreshBtn.addEventListener("click", load);
  modal.addEventListener("click", (e) => {
    if (e.target === modal) close(); // click the backdrop to dismiss
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !modal.hidden) close();
  });
}

boot();
