// People settings: name the faces discovered in your recordings, recognize one by sight from a
// sample-face crop, and merge duplicate faces the matcher over-split. This is the web twin of
// the Android "People" screen and the visual sibling of the Voices modal. All calls go to
// hushai-backend's /v1/persons* surface via the viewer proxy (which injects the device bearer).

import { getPersons, sampleFaceUrl, renamePerson, mergePerson } from "../api.js";
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
  info.appendChild(
    div("voice-meta", `${p.n_samples} sighting${p.n_samples === 1 ? "" : "s"} · ${lastSeenLabel(p.sample_sighting_unix_nanos)}`),
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

  const addCard = (p) =>
    container.appendChild(personCard(p, persons.filter((o) => o.person_id !== p.person_id), ctx));

  // Named people first, then the still-unidentified faces waiting to be named.
  const known = persons.filter(isIdentified);
  const unknown = persons.filter((p) => !isIdentified(p));

  container.appendChild(sectionTitle("Known people", known.length));
  if (known.length) {
    known.forEach(addCard);
  } else {
    container.appendChild(note("No faces identified yet — name one below to build your known-people list."));
  }

  if (unknown.length) {
    container.appendChild(sectionTitle("Unidentified people", unknown.length));
    unknown.forEach(addCard);
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
    flash(msg) {
      const n = note(msg);
      body.prepend(n);
      setTimeout(() => n.remove(), 4000);
    },
  };

  async function load() {
    body.replaceChildren(note("Loading people…"));
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
