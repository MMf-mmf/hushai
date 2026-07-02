// Voices settings: name the speakers discovered in your recordings, play a sample to
// recognize a voice by ear, and merge duplicate voices the matcher over-split. This is the
// web counterpart of the Android "Voices" screen. All calls go to hushai-backend's
// /v1/speakers* surface via the viewer proxy (which injects the device bearer).

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
} from "../api.js";

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

// A section heading with a count, e.g. "Known voices (3)".
function sectionTitle(text, count) {
  const el = div("voice-section-title");
  el.textContent = text;
  const c = document.createElement("span");
  c.className = "muted small";
  c.textContent = `  (${count})`;
  el.appendChild(c);
  return el;
}

// A collapsed-by-default disclosure ("Known voices (N)") wrapping its cards. Already-named voices
// are tucked away so opening the modal surfaces the unidentified ones (the voices you actually
// need to name); the known list grows over time and isn't useful on every open.
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

// One speaker: name + sample count + sample utterances, with rename, play, and "merge into".
// An archived (disregarded) voice renders a reduced card: play + Restore only — renaming and
// merging are noise for something the user chose to tuck away.
function voiceCard(sp, others, ctx) {
  const card = div("voice-card");
  card.appendChild(div("voice-name", speakerLabel(sp.display_name, sp.speaker_id)));
  card.appendChild(div("voice-meta", `${sp.n_samples} samples`));
  for (const utt of sp.sample_utterances || []) {
    card.appendChild(div("voice-utt", `“${utt}”`));
  }

  const actions = div("voice-actions");

  if (sp.archived) {
    const play = document.createElement("button");
    play.type = "button";
    play.textContent = "Play sample";
    play.addEventListener("click", () => ctx.play(sp.speaker_id));
    const restore = document.createElement("button");
    restore.type = "button";
    restore.textContent = "Restore";
    restore.addEventListener("click", async () => {
      restore.disabled = true;
      try {
        await unarchiveSpeaker(sp.speaker_id);
        await ctx.reload();
      } catch {
        restore.disabled = false;
        ctx.flash("Couldn't restore that voice.");
      }
    });
    actions.append(play, restore);
    card.appendChild(actions);
    return card;
  }

  const input = document.createElement("input");
  input.type = "text";
  input.placeholder = "Name this voice";
  input.value = sp.display_name || "";
  const save = document.createElement("button");
  save.type = "button";
  save.textContent = "Save";
  save.addEventListener("click", async () => {
    const name = input.value.trim();
    if (!name) return;
    save.disabled = true;
    try {
      await renameSpeaker(sp.speaker_id, name);
      await ctx.reload();
    } catch {
      save.disabled = false;
      ctx.flash("Couldn't save that name.");
    }
  });
  actions.append(input, save);

  const play = document.createElement("button");
  play.type = "button";
  play.textContent = "Play sample";
  play.addEventListener("click", () => ctx.play(sp.speaker_id));
  actions.appendChild(play);

  if (others.length) {
    const merge = document.createElement("select");
    const head = document.createElement("option");
    head.value = "";
    head.textContent = "Merge into…";
    merge.appendChild(head);
    for (const o of others) {
      const opt = document.createElement("option");
      opt.value = o.speaker_id;
      opt.textContent = speakerLabel(o.display_name, o.speaker_id);
      merge.appendChild(opt);
    }
    merge.addEventListener("change", async () => {
      const into = merge.value;
      if (!into) return;
      merge.disabled = true;
      try {
        await mergeSpeaker(sp.speaker_id, into);
        await ctx.reload();
      } catch {
        merge.disabled = false;
        ctx.flash("Couldn't merge those voices.");
      }
    });
    actions.appendChild(merge);
  }

  const disregard = document.createElement("button");
  disregard.type = "button";
  disregard.textContent = "Disregard";
  disregard.title = "Move to Archived — hides it from these lists; recordings are unaffected.";
  disregard.addEventListener("click", async () => {
    disregard.disabled = true;
    try {
      await archiveSpeaker(sp.speaker_id);
      await ctx.reload();
    } catch {
      disregard.disabled = false;
      ctx.flash("Couldn't disregard that voice.");
    }
  });
  actions.appendChild(disregard);

  card.appendChild(actions);
  return card;
}

// One suggested duplicate group: the matcher thinks these are the same person.
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
// Naming it mints a speaker and claims the cluster's segments.
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

  if (!speakers.length) {
    container.appendChild(note("No speakers discovered yet."));
    return;
  }

  // The known (named) voices collapse into a closed disclosure; the still-unidentified voices —
  // the ones you open this modal to name — stay shown. Disregarded voices sink into a closed
  // "Archived" disclosure at the bottom (they keep matching new audio; they're just out of the
  // labeling to-do list) and are never offered as merge targets.
  const archived = speakers.filter((sp) => sp.archived);
  const active = speakers.filter((sp) => !sp.archived);
  const known = active.filter(isIdentified);
  const unknown = active.filter((sp) => !isIdentified(sp));
  const cardFor = (sp) => voiceCard(sp, active.filter((o) => o.speaker_id !== sp.speaker_id), ctx);

  if (known.length) {
    container.appendChild(collapsibleSection("Known voices", known.length, known.map(cardFor)));
  } else {
    container.appendChild(sectionTitle("Known voices", 0));
    container.appendChild(note("No voices identified yet — name one below to build your known-voices list."));
  }

  if (unknown.length) {
    container.appendChild(sectionTitle("Unidentified voices", unknown.length));
    unknown.forEach((sp) => container.appendChild(cardFor(sp)));
  }

  if (archived.length) {
    container.appendChild(
      collapsibleSection("Archived", archived.length, archived.map((sp) => voiceCard(sp, [], ctx))),
    );
  }
}

function boot() {
  const openBtn = document.getElementById("btnSettings");
  const modal = document.getElementById("settingsModal");
  const closeBtn = document.getElementById("settingsClose");
  const refreshBtn = document.getElementById("voicesRefresh");
  const body = document.getElementById("voicesBody");
  if (!openBtn || !modal || !body) return;

  // One shared <audio> for sample playback; the proxy attaches the bearer to the src.
  const audio = new Audio();
  const ctx = {
    play(id) {
      audio.src = sampleAudioUrl(id);
      audio.play().catch(() => {});
    },
    playUrl(url) {
      audio.src = url;
      audio.play().catch(() => {});
    },
    reload: () => load(),
    flash(msg) {
      const n = note(msg);
      body.prepend(n);
      setTimeout(() => n.remove(), 4000);
    },
  };

  async function load() {
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
    render(body, speakers || [], dups || [], unattributed || [], ctx);
  }

  function open() {
    modal.hidden = false;
    load();
  }
  function close() {
    modal.hidden = true;
    audio.pause();
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
