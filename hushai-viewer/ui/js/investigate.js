// Investigate page (Gotham G4): browse the entity graph the worker materializes —
// per-entity edges grouped by relationship, cross-camera journey strips, recent
// evidence, shortest-path connections, and the voice↔face binding review queue.
//
// Everything renders from the backend's /v1/graph/* read API (bare {type,id} endpoints);
// display names are joined client-side from the catalog lists, exactly like the graph
// API doc says ("no catalog-join structs yet"). Evidence and journey hops deep-link into
// the player timeline via the same /?device=&t= links the chat citations use.
//
// Identity-binding rule (spec §1.4): the graph only ever PROPOSES a voice↔face pair —
// confirm/reject here is the sole authority, rejection is sticky, nothing auto-merges.

import { el, errorState } from "./dom.js";
import { initTopbar, setLive, setUpdated } from "./nav.js";
import { nsToMs, clockMs, dateLabel } from "./time.js";
import { toast } from "./toast.js";
import {
  getDevices,
  getSpeakers,
  getPersons,
  getPlates,
  getGraphEntity,
  getGraphEntityTimeline,
  getGraphPath,
  getGraphJourneys,
  getGraphBindings,
  confirmGraphBinding,
  rejectGraphBinding,
  sampleAudioUrl,
  sampleFaceUrl,
  samplePlateUrl,
} from "./api.js";

const TYPE_ICON = { person: "👤", speaker: "🎙", plate: "🚗", device: "📷" };
const TYPE_LABEL = { person: "Person", speaker: "Voice", plate: "Plate", device: "Camera" };
// Friendly relationship names for edge groups.
const EDGE_LABEL = {
  co_present: "Seen together with",
  conversed_with: "Talked with",
  arrived_with_vehicle: "Arrives with vehicle",
  same_identity_candidate: "Same-identity candidate",
  visits_place: "Frequents",
};

const state = {
  names: new Map(), // "type:id" -> display label
  entities: [], // [{ref, type, id, name}] for the pickers, grouped by type
  current: null, // "type:id" currently open in the explorer
};

const refOf = (type, id) => `${type}:${id}`;
const nameOf = (type, id) =>
  state.names.get(refOf(type, id)) || `${TYPE_LABEL[type] || type} ${String(id).slice(0, 8)}`;

// Deep link into the player timeline at a moment (the citation-chip pattern).
const playerLink = (deviceId, ns) =>
  `/?device=${encodeURIComponent(deviceId)}&t=${Math.round(nsToMs(ns))}`;

const whenLabel = (ns) => {
  const ms = nsToMs(ns);
  return `${dateLabel(ms)} ${clockMs(ms)}`;
};

// ---- catalogs → name map + pickers ---------------------------------------------------

async function loadCatalogs() {
  // Per-catalog degradation, but a wholesale backend outage must surface (boot's banner) —
  // an empty catalog is legitimate, four rejections are not. allSettled distinguishes them.
  const settled = await Promise.allSettled([getDevices(), getSpeakers(), getPersons(), getPlates()]);
  if (settled.every((s) => s.status === "rejected")) {
    throw new Error(settled[0].reason?.message || "backend unreachable");
  }
  const [devices, speakers, persons, plates] = settled.map((s) =>
    s.status === "fulfilled" ? s.value : [],
  );
  const ents = [];
  for (const p of persons || []) {
    const id = p.person_id;
    const name = p.display_name || `Unidentified ${String(id).slice(0, 8)}`;
    state.names.set(refOf("person", id), name);
    ents.push({ ref: refOf("person", id), type: "person", id, name });
  }
  for (const s of speakers || []) {
    const id = s.speaker_id;
    const name = s.display_name || `Unnamed voice ${String(id).slice(0, 8)}`;
    state.names.set(refOf("speaker", id), name);
    ents.push({ ref: refOf("speaker", id), type: "speaker", id, name });
  }
  for (const pl of plates || []) {
    const id = pl.plate_id;
    const name = pl.display_name || pl.plate_text || `Plate ${String(id).slice(0, 8)}`;
    state.names.set(refOf("plate", id), name);
    ents.push({ ref: refOf("plate", id), type: "plate", id, name });
  }
  for (const d of devices || []) {
    const name = d.displayName || d.id;
    state.names.set(refOf("device", d.id), name);
    ents.push({ ref: refOf("device", d.id), type: "device", id: d.id, name });
  }
  state.entities = ents;
}

function fillPicker(select, { placeholder }) {
  select.replaceChildren(el("option", { value: "", text: placeholder }));
  for (const type of ["person", "speaker", "plate", "device"]) {
    const group = state.entities.filter((e) => e.type === type);
    if (!group.length) continue;
    const og = el("optgroup", { label: `${TYPE_ICON[type]} ${TYPE_LABEL[type]}s` });
    for (const e of group.sort((a, b) => a.name.localeCompare(b.name))) {
      og.appendChild(el("option", { value: e.ref, text: e.name }));
    }
    select.appendChild(og);
  }
}

// ---- entity explorer ------------------------------------------------------------------

// A clickable entity chip that re-focuses the explorer on that entity.
function entityChip(type, id) {
  return el("button", {
    class: "entity-chip",
    type: "button",
    "data-ref": refOf(type, id),
    text: `${TYPE_ICON[type] || "▸"} ${nameOf(type, id)}`,
    title: `${TYPE_LABEL[type] || type} — open in the explorer`,
    onclick: () => openEntity(refOf(type, id)),
  });
}

async function openEntity(ref) {
  const card = document.getElementById("entityCard");
  const select = document.getElementById("entitySelect");
  if (select.value !== ref) select.value = ref;
  state.current = ref;
  const [type, ...idParts] = ref.split(":");
  const id = idParts.join(":");
  card.replaceChildren(el("div", { class: "empty", text: "Loading…" }));

  let page;
  try {
    page = await getGraphEntity(type, id);
  } catch (e) {
    if (state.current !== ref) return; // user moved on; don't clobber the newer card
    card.replaceChildren(errorState(`Couldn't load the entity page: ${e.message}`, () => openEntity(ref)));
    return;
  }
  // Journeys (person/plate only) + recent evidence load best-effort in parallel.
  const [journeys, timeline] = await Promise.all([
    type === "person" || type === "plate"
      ? getGraphJourneys(ref, 10).catch(() => [])
      : Promise.resolve([]),
    getGraphEntityTimeline(type, id, 40).catch(() => null),
  ]);
  if (state.current !== ref) return; // user moved on while we were loading

  const frag = [];

  // Header: who/what + a one-line baseline summary when the graph has one.
  const head = el(
    "div",
    { class: "entity-head" },
    el("span", { class: "entity-title", text: `${TYPE_ICON[type]} ${nameOf(type, id)}` }),
    el("span", { class: "count-chip", text: TYPE_LABEL[type] || type }),
  );
  const b = page.baseline;
  if (b && typeof b.visits_in_window === "number") {
    const dwell =
      typeof b.dwell_p50_secs === "number" && b.dwell_p50_secs > 0
        ? ` · typical stay ~${Math.max(1, Math.round(b.dwell_p50_secs / 60))}m`
        : "";
    head.appendChild(
      el("span", {
        class: "muted small",
        text: `seen ${b.visits_in_window}× in the last ${b.window_days ?? 30} days${dwell}`,
      }),
    );
  }
  frag.push(head);

  // Relationships, grouped by edge type. Endpoints are bare ids — join names locally.
  const groups = page.edges_by_type || {};
  const kinds = Object.keys(groups);
  if (!kinds.length) {
    frag.push(el("div", { class: "empty", text: "No relationships materialized yet — the graph learns as footage folds in." }));
  }
  for (const kind of kinds) {
    const rows = groups[kind] || [];
    if (!rows.length) continue;
    const list = el("div", { class: "edge-list" });
    for (const edge of rows) {
      const other =
        edge.src && refOf(edge.src.type, edge.src.id) !== ref ? edge.src : edge.dst;
      if (!other) continue;
      const meta = [];
      if (typeof edge.observation_count === "number") meta.push(`${edge.observation_count}×`);
      if (typeof edge.confidence === "number") meta.push(`conf ${edge.confidence.toFixed(2)}`);
      if (edge.status) meta.push(edge.status);
      if (edge.last_seen_unix_nanos != null) meta.push(`last ${whenLabel(edge.last_seen_unix_nanos)}`);
      list.appendChild(
        el(
          "div",
          { class: "edge-row" },
          entityChip(other.type, other.id),
          el("span", { class: "muted small", text: meta.join(" · ") }),
        ),
      );
    }
    frag.push(
      el(
        "div",
        { class: "edge-group" },
        el("div", { class: "edge-kind", text: EDGE_LABEL[kind] || kind }),
        list,
      ),
    );
  }

  // Journey timeline strips: ordered camera hops, each hop a deep link into the player.
  if (journeys && journeys.length) {
    const wrap = el("div", { class: "journey-list" }, el("div", { class: "edge-kind", text: "Cross-camera journeys" }));
    for (const j of journeys) {
      const strip = el("div", { class: "journey-strip", "data-journey": j.journey_id || "" });
      (j.hops || []).forEach((hop, i) => {
        if (i > 0) strip.appendChild(el("span", { class: "journey-arrow", text: "→" }));
        strip.appendChild(
          el("a", {
            class: "journey-hop",
            href: playerLink(hop.device_id, hop.arrive_ns),
            title: `${nameOf("device", hop.device_id)} · ${whenLabel(hop.arrive_ns)} — open in the player`,
            text: `📷 ${nameOf("device", hop.device_id)} ${clockMs(nsToMs(hop.arrive_ns))}`,
          }),
        );
      });
      strip.appendChild(
        el("span", {
          class: "muted small journey-meta",
          text: `${dateLabel(nsToMs(j.started_at_unix_nanos))} · ${j.status}`,
        }),
      );
      wrap.appendChild(strip);
    }
    frag.push(wrap);
  }

  // Recent evidence: merged events timeline; every row jumps the player to the moment.
  const items = timeline?.items || [];
  if (items.length) {
    const list = el("div", { class: "evidence-list" }, el("div", { class: "edge-kind", text: "Recent evidence" }));
    for (const it of items.slice(0, 20)) {
      list.appendChild(
        el(
          "a",
          {
            class: "evidence-row",
            href: playerLink(it.device_id, it.start_unix_nanos),
            title: "Open in the player",
          },
          el("span", { class: "muted small", text: whenLabel(it.start_unix_nanos) }),
          el("span", { text: it.label || it.event_type }),
          el("span", { class: "muted small", text: nameOf("device", it.device_id) }),
        ),
      );
    }
    frag.push(list);
  }

  // The 0024 narrative profile, tucked away but at hand.
  if (page.profile_text) {
    frag.push(
      el(
        "details",
        { class: "entity-profile" },
        el("summary", { text: "Profile notes" }),
        el("div", { class: "muted", text: page.profile_text }),
      ),
    );
  }

  card.replaceChildren(...frag);
}

// ---- connections (shortest evidence path) ----------------------------------------------

async function findPath() {
  const from = document.getElementById("pathFrom").value;
  const to = document.getElementById("pathTo").value;
  const out = document.getElementById("pathResult");
  if (!from || !to || from === to) {
    out.replaceChildren(el("div", { class: "empty", text: "Pick two different entities." }));
    return;
  }
  out.replaceChildren(el("div", { class: "empty", text: "Searching…" }));
  let res;
  try {
    res = await getGraphPath(from, to);
  } catch (e) {
    out.replaceChildren(errorState(`Path query failed: ${e.message}`, findPath));
    return;
  }
  if (!res.found) {
    out.replaceChildren(
      el("div", { class: "empty", text: "No connection within 4 hops — they may simply never cross paths." }),
    );
    return;
  }
  const strip = el("div", { class: "journey-strip path-strip" });
  (res.path || []).forEach((ref, i) => {
    if (i > 0) strip.appendChild(el("span", { class: "journey-arrow", text: "→" }));
    const [t, ...rest] = ref.split(":");
    strip.appendChild(entityChip(t, rest.join(":")));
  });
  out.replaceChildren(
    el("div", { class: "muted small", text: `Connected in ${res.hops} hop${res.hops === 1 ? "" : "s"}:` }),
    strip,
  );
}

// ---- binding review queue ---------------------------------------------------------------

function bindingSide(type, id) {
  const side = el(
    "div",
    { class: "binding-side" },
    el("div", { class: "binding-name", text: `${TYPE_ICON[type] || ""} ${nameOf(type, id)}` }),
  );
  if (type === "speaker") {
    const audio = document.createElement("audio");
    audio.controls = true;
    audio.preload = "none";
    audio.src = sampleAudioUrl(id);
    side.appendChild(audio);
  } else if (type === "person") {
    const img = document.createElement("img");
    img.className = "binding-face";
    img.loading = "lazy";
    img.alt = "sample face";
    img.src = sampleFaceUrl(id);
    side.appendChild(img);
  } else if (type === "plate") {
    const img = document.createElement("img");
    img.className = "binding-face";
    img.loading = "lazy";
    img.alt = "plate crop";
    img.src = samplePlateUrl(id);
    side.appendChild(img);
  }
  return side;
}

async function loadBindings() {
  const status = document.getElementById("bindingStatus").value;
  const host = document.getElementById("bindings");
  const count = document.getElementById("bindingCount");
  host.replaceChildren(el("div", { class: "empty", text: "Loading…" }));
  let rows;
  try {
    rows = await getGraphBindings(status);
  } catch (e) {
    host.replaceChildren(errorState(`Couldn't load bindings: ${e.message}`, loadBindings));
    return;
  }
  count.textContent = String(rows.length);
  if (!rows.length) {
    host.replaceChildren(
      el("div", {
        class: "empty",
        text:
          status === "candidate"
            ? "Nothing awaiting review. An empty queue can be right — ambiguous pairs deliberately never surface."
            : `No ${status} bindings yet.`,
      }),
    );
    return;
  }
  host.replaceChildren(
    ...rows.map((edge) => {
      const counters = edge.metadata?.counters || {};
      const evidence = [
        typeof edge.confidence === "number" ? `confidence ${(edge.confidence * 100).toFixed(0)}%` : null,
        counters.together != null ? `together ${counters.together}×` : null,
        counters.speaker_only != null ? `voice alone ${counters.speaker_only}×` : null,
        counters.person_only != null ? `face alone ${counters.person_only}×` : null,
      ].filter(Boolean);
      const card = el(
        "div",
        { class: "binding-card", "data-edge": edge.edge_id },
        bindingSide(edge.src.type, edge.src.id),
        el("div", { class: "binding-mid" }, el("span", { class: "binding-eq", text: "≟" }), el("span", { class: "muted small", text: evidence.join(" · ") })),
        bindingSide(edge.dst.type, edge.dst.id),
      );
      if (status === "candidate") {
        const act = (fn, verb) => async (ev) => {
          const btn = ev.currentTarget;
          btn.disabled = true;
          try {
            await fn(edge.edge_id);
            toast(`Binding ${verb}.`);
            card.remove();
            count.textContent = String(Math.max(0, Number(count.textContent) - 1));
          } catch (e) {
            btn.disabled = false;
            toast(`Couldn't ${verb === "confirmed" ? "confirm" : "reject"}: ${e.message}`, { kind: "error" });
          }
        };
        card.appendChild(
          el(
            "div",
            { class: "binding-actions" },
            el("button", {
              class: "binding-confirm",
              type: "button",
              text: "✓ Same person",
              "data-action": "confirm-binding",
              onclick: act(confirmGraphBinding, "confirmed"),
            }),
            el("button", {
              class: "binding-reject ghost",
              type: "button",
              text: "✕ Different",
              title: "Sticky: this pair will not be proposed again",
              "data-action": "reject-binding",
              onclick: act(rejectGraphBinding, "rejected"),
            }),
          ),
        );
      }
      return card;
    }),
  );
}

// ---- boot ------------------------------------------------------------------------------

async function boot() {
  initTopbar({ section: "investigate" });
  const banner = document.getElementById("banner");
  try {
    await loadCatalogs();
  } catch (e) {
    banner.className = "dash-banner error";
    banner.textContent = `Couldn't load the catalogs: ${e.message}`;
    setLive(false);
    return;
  }
  banner.remove();
  setLive(true);
  setUpdated("live");

  const entitySelect = document.getElementById("entitySelect");
  fillPicker(entitySelect, { placeholder: "Pick a person, voice, plate or camera…" });
  document.getElementById("entityCount").textContent = String(state.entities.length);
  entitySelect.addEventListener("change", () => {
    if (entitySelect.value) openEntity(entitySelect.value);
  });

  fillPicker(document.getElementById("pathFrom"), { placeholder: "Entity A…" });
  fillPicker(document.getElementById("pathTo"), { placeholder: "Entity B…" });
  document.getElementById("pathGo").addEventListener("click", findPath);

  document.getElementById("bindingStatus").addEventListener("change", loadBindings);
  await loadBindings();

  // Deep link: /investigate.html?entity=type:id opens the explorer on that entity.
  const ref = new URLSearchParams(location.search).get("entity");
  if (ref) openEntity(ref);
}

boot();
