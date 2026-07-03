// Voices settings: name the speakers discovered in your recordings, play a sample to
// recognize a voice by ear, and merge duplicate voices the matcher over-split. This is the
// web counterpart of the Android "Voices" screen. All calls go to hushai-backend's
// /v1/speakers* surface via the viewer proxy (which injects the device bearer).
//
// The standard per-speaker cards and the Known/Unidentified/Archived section layout are
// shared with People/Plates via entity-card.js; the duplicate-suggestion cards and the
// unattributed-cluster naming flow below are voices-only and stay bespoke.

import {
  getSpeakers,
  getSpeakerDuplicates,
  getUnattributed,
  nameUnattributed,
  sampleAudioUrl,
  unattributedSampleAudioUrl,
  renameSpeaker,
  mergeSpeaker,
  mergeSpeakerGroup,
  archiveSpeaker,
  unarchiveSpeaker,
  reclusterSpeakers,
  reclusterSpeakersDeep,
} from "../api.js";
import { entityCard, entitySections, matchesQuery } from "./entity-card.js";
import { wireModal } from "../modal.js";
import { toast } from "../toast.js";
import { confirmAction } from "../confirm.js";

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

function speakerLabel(name, id) {
  return name || `Unknown speaker (${String(id).slice(0, 8)})`;
}

function isIdentified(sp) {
  return sp.display_name != null && String(sp.display_name).trim() !== "";
}

// One speaker: name + sample count + sample utterances, with rename, play, and "merge into",
// built on the shared entity card (archived voices get the reduced Play + Restore card).
function speakerCard(sp, others, ctx) {
  const play = document.createElement("button");
  play.type = "button";
  play.textContent = "Play sample";
  play.addEventListener("click", () => ctx.play(sp.speaker_id));

  return entityCard({
    entity: sp,
    cardClass: "voice-card",
    label: speakerLabel(sp.display_name, sp.speaker_id),
    sublabel: [
      `${sp.n_samples} samples`,
      ...(sp.sample_utterances || []).map((utt) => ({ class: "voice-utt", text: `“${utt}”` })),
    ],
    archived: !!sp.archived,
    onRename: async (name) => {
      await renameSpeaker(sp.speaker_id, name);
      await ctx.reload();
    },
    renamePlaceholder: "Name this voice",
    renameValue: sp.display_name || "",
    extra: play,
    mergeOptions: others.map((o) => ({
      value: o.speaker_id,
      label: speakerLabel(o.display_name, o.speaker_id),
    })),
    onMerge: async (into) => {
      await mergeSpeaker(sp.speaker_id, into);
      await ctx.reload();
    },
    onArchive: async () => {
      await archiveSpeaker(sp.speaker_id);
      await ctx.reload();
    },
    onRestore: async () => {
      await unarchiveSpeaker(sp.speaker_id);
      await ctx.reload();
    },
    archiveTitle: "Move to Archived — hides it from these lists; recordings are unaffected.",
    errors: {
      rename: "Couldn't save that name.",
      merge: "Couldn't merge those voices.",
      archive: "Couldn't disregard that voice.",
      restore: "Couldn't restore that voice.",
    },
    flash: ctx.flash,
  });
}

// One suggested duplicate group: the matcher thinks these are the same person. (Bespoke to
// voices — not an entity card.)
function dupGroupCard(group, ctx) {
  const card = div(group.name_conflict ? "dup-card conflict" : "dup-card");
  const survivor = (group.members || []).find((m) => m.speaker_id === group.suggested_into);
  const survivorLabel = speakerLabel(survivor && survivor.display_name, group.suggested_into);
  const confidence = group.max_distance <= 0.2 ? "Very likely" : "Likely";
  card.appendChild(div("voice-name", `Possible duplicate of ${survivorLabel}`));
  card.appendChild(
    div("voice-meta", `${confidence} the same person · ${(group.members || []).length} voices`),
  );

  for (const m of group.members || []) {
    card.appendChild(div("voice-meta", `${speakerLabel(m.display_name, m.speaker_id)} · ${m.n_samples} samples`));
    const first = (m.sample_utterances || [])[0];
    if (first) card.appendChild(div("voice-utt", `“${first}”`));
    const play = document.createElement("button");
    play.type = "button";
    play.textContent = "Play sample";
    play.addEventListener("click", () => ctx.play(m.speaker_id));
    card.appendChild(play);
  }

  if (group.name_conflict) {
    card.appendChild(
      div("voice-meta", "These voices carry different names — rename them to match, then merge below."),
    );
  } else {
    const merge = document.createElement("button");
    merge.type = "button";
    merge.textContent = "Merge group";
    merge.addEventListener("click", async () => {
      merge.disabled = true;
      try {
        await mergeSpeakerGroup(group.suggested_into, (group.members || []).map((m) => m.speaker_id));
        await ctx.reload();
      } catch {
        merge.disabled = false;
        ctx.flash("Couldn't merge that group.");
      }
    });
    card.appendChild(merge);
  }
  return card;
}

// One candidate voice clustered from unattributed audio: sample utterances + a name field.
// Naming it mints a speaker and claims the cluster's segments. (Bespoke to voices — there's
// no entity id yet, so this isn't an entity card.)
function unattributedCard(cluster, ctx) {
  const card = div("voice-card");
  const confidence = cluster.max_distance <= 0.2 ? "Very likely one person" : "Likely one person";
  card.appendChild(div("voice-name", "Unrecognized voice"));
  card.appendChild(div("voice-meta", `${confidence} · ${cluster.n_segments} clip(s)`));
  for (const utt of cluster.sample_utterances || []) {
    card.appendChild(div("voice-utt", `“${utt}”`));
  }

  const actions = div("voice-actions");
  // Hear the candidate before naming it (no speaker_id yet → play a clip by segment_id).
  const sampleSegment = (cluster.segment_ids || [])[0];
  const play = document.createElement("button");
  play.type = "button";
  play.textContent = "▶ Play";
  play.disabled = !sampleSegment;
  play.addEventListener("click", () => {
    if (sampleSegment) ctx.playUrl(unattributedSampleAudioUrl(sampleSegment));
  });
  const input = document.createElement("input");
  input.type = "text";
  input.placeholder = "Name this voice";
  const save = document.createElement("button");
  save.type = "button";
  save.textContent = "Save";
  save.addEventListener("click", async () => {
    const name = input.value.trim();
    if (!name) return;
    save.disabled = true;
    try {
      await nameUnattributed(name, cluster.segment_ids || []);
      await ctx.reload();
    } catch {
      save.disabled = false;
      ctx.flash("Couldn't name that voice (it may have been attributed since — Refresh).");
    }
  });
  actions.append(play, input, save);
  card.appendChild(actions);
  return card;
}

function render(container, speakers, dups, unattributed, ctx) {
  container.replaceChildren();
  const q = ctx.query;

  // The bespoke helper sections only make sense on the unfiltered list — a name search
  // filters the speaker catalog below, so tuck these away while a query is active.
  if (!q) {
    // Candidate voices the matcher couldn't auto-create (chronically marginal audio): name one
    // to add it to your voices. This is the web twin of the Android "Identify new voices" section.
    if (unattributed.length) {
      const head = div("voice-card");
      head.appendChild(div("voice-section-title", "Identify new voices"));
      head.appendChild(
        div(
          "voice-meta",
          `${unattributed.length} voice(s) heard in recordings but not yet linked to anyone. Name one to add it.`,
        ),
      );
      container.appendChild(head);
      for (const c of unattributed) container.appendChild(unattributedCard(c, ctx));
    }

    const mergeable = dups.filter((g) => !g.name_conflict);
    if (dups.length) {
      const head = div("voice-card");
      head.appendChild(div("voice-section-title", "Clean up voices"));
      head.appendChild(
        div(
          "voice-meta",
          `${dups.length} possible duplicate group(s) — these look like one person split across several voices.`,
        ),
      );
      if (mergeable.length > 1) {
        const all = document.createElement("button");
        all.type = "button";
        all.textContent = `Merge all (${mergeable.length})`;
        all.addEventListener("click", async () => {
          all.disabled = true;
          try {
            for (const g of mergeable) {
              await mergeSpeakerGroup(g.suggested_into, g.members.map((m) => m.speaker_id));
            }
            await ctx.reload();
          } catch {
            all.disabled = false;
            ctx.flash("Some groups couldn't be merged.");
          }
        });
        head.appendChild(all);
      }
      container.appendChild(head);
      for (const g of dups) container.appendChild(dupGroupCard(g, ctx));
    }
  }

  if (!speakers.length) {
    container.appendChild(note("No speakers discovered yet."));
    return;
  }

  const list = q
    ? speakers.filter((sp) => matchesQuery(q, sp.display_name, sp.speaker_id))
    : speakers;
  if (!list.length) {
    container.appendChild(note(`No voices match “${q}”.`));
    return;
  }

  // The shared three-group layout: known (named) voices collapse closed; the
  // still-unidentified voices — the ones you open this modal to name — stay open;
  // disregarded voices sink into a closed "Archived" disclosure at the bottom (they keep
  // matching new audio; they're just out of the labeling to-do list) and are never offered
  // as merge targets. Merge targets always come from the full active list, not the filtered
  // view — a search must not shrink where a voice can be merged into.
  const archived = speakers.filter((sp) => sp.archived);
  const active = speakers.filter((sp) => !sp.archived);
  const cardFor = (sp) => speakerCard(sp, active.filter((o) => o.speaker_id !== sp.speaker_id), ctx);
  const shown = (arr) => (q ? arr.filter((sp) => list.includes(sp)) : arr);
  const known = shown(active.filter(isIdentified));
  const unknown = shown(active.filter((sp) => !isIdentified(sp)));

  for (const el of entitySections({
    known: {
      title: "Known voices",
      cards: known.map(cardFor),
      emptyText: q ? null : "No voices identified yet — name one below to build your known-voices list.",
    },
    unknown: {
      title: "Unidentified voices",
      cards: unknown.map(cardFor),
      emptyText: q ? null : "No unidentified voices — every voice has a name.",
    },
    archived: {
      title: "Archived",
      cards: shown(archived).map((sp) => speakerCard(sp, [], ctx)),
    },
  })) {
    container.appendChild(el);
  }
}

function boot() {
  const openBtn = document.getElementById("btnSettings");
  const modal = document.getElementById("settingsModal");
  const closeBtn = document.getElementById("settingsClose");
  const refreshBtn = document.getElementById("voicesRefresh");
  const searchBox = document.getElementById("voicesSearch");
  const body = document.getElementById("voicesBody");
  if (!openBtn || !modal || !body) return;

  // One shared <audio> for sample playback; the proxy attaches the bearer to the src.
  const audio = new Audio();
  const ctx = {
    query: "",
    play(id) {
      audio.src = sampleAudioUrl(id);
      audio.play().catch(() => {});
    },
    playUrl(url) {
      audio.src = url;
      audio.play().catch(() => {});
    },
    reload: () => load(),
    // Operation failures ("couldn't save/merge/…") surface as page-level error toasts.
    flash: (msg) => toast(msg, { kind: "error" }),
  };

  // Last-fetched lists, kept so the search box can re-filter client-side without refetching.
  let cache = { speakers: [], dups: [], unattributed: [] };
  const rerender = () => render(body, cache.speakers, cache.dups, cache.unattributed, ctx);

  async function load() {
    ctx.query = searchBox ? searchBox.value.trim() : "";
    body.replaceChildren(note("Loading voices…"));
    let speakers, dups, unattributed;
    try {
      [speakers, dups, unattributed] = await Promise.all([
        getSpeakers(),
        getSpeakerDuplicates(),
        getUnattributed(),
      ]);
    } catch {
      body.replaceChildren(
        note(
          "Couldn't reach the speaker service. Check that hushai-backend is running and the " +
            "viewer's BACKEND_TOKEN / DEVICE_TOKEN is set, then Refresh.",
        ),
      );
      return;
    }
    cache = { speakers: speakers || [], dups: dups || [], unattributed: unattributed || [] };
    rerender();
  }

  // Server-side reclustering, surfaced in the modal head (inserted dynamically so the
  // markup stays untouched): a fast semantic pass, and — behind a confirm — the slow
  // deep neural pass over every voiceprint. Both are long-running POSTs: disable both
  // triggers while one runs, then reload the list to show the regrouped voices.
  const reclusterBtn = document.createElement("button");
  reclusterBtn.type = "button";
  reclusterBtn.textContent = "Recluster";
  reclusterBtn.title = "Re-run voice clustering to regroup duplicate voices (fast pass)";
  const deepBtn = document.createElement("button");
  deepBtn.type = "button";
  deepBtn.textContent = "Deep recluster";
  deepBtn.title = "Re-run neural clustering over every voiceprint (slow, thorough)";
  const head = modal.querySelector(".modal-head");
  if (head && closeBtn) {
    head.insertBefore(reclusterBtn, closeBtn);
    head.insertBefore(deepBtn, closeBtn);
  }

  async function runRecluster(btn, fn, doneMsg) {
    reclusterBtn.disabled = true;
    deepBtn.disabled = true;
    const prev = btn.textContent;
    btn.textContent = "Working…";
    try {
      await fn();
      toast(doneMsg, { kind: "success" });
      await load();
    } catch {
      toast("Recluster failed — check that hushai-backend is running.", { kind: "error" });
    } finally {
      reclusterBtn.disabled = false;
      deepBtn.disabled = false;
      btn.textContent = prev;
    }
  }

  reclusterBtn.addEventListener("click", () =>
    runRecluster(reclusterBtn, reclusterSpeakers, "Recluster complete"),
  );
  deepBtn.addEventListener("click", async () => {
    const ok = await confirmAction({
      title: "Deep recluster",
      message:
        "Re-runs neural clustering over every voiceprint. This can take a while and may regroup existing voices.",
      confirmLabel: "Run deep recluster",
    });
    if (ok) runRecluster(deepBtn, reclusterSpeakersDeep, "Deep recluster complete");
  });

  // Shared modal behavior (backdrop click, Escape, focus trap + restore) lives in modal.js;
  // opening (re)loads the list, closing stops any playing sample.
  const m = wireModal(modal, {
    onOpen: load,
    onClose: () => audio.pause(),
  });

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
