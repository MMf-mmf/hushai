// Shared per-entity card + section scaffolding for the three catalog modals
// (Voices / People / Plates). The three modals grew ~identical cards — name input
// + Save, "Merge into…" select, Disregard/Restore — as three drifting copies; this
// module is the single implementation. Each modal keeps its own media element
// (audio Play button, face crop, plate crop) and any bespoke controls (Watch star),
// passing them in via `media` / `extra`. Voices' duplicate-suggestion and
// unattributed-cluster cards are NOT entity cards and stay bespoke in voices.js.
//
// DOM classes are the ones the modal CSS already styles (.voice-card, .person-card,
// .voice-name, …) so extraction changes no pixels; pass `cardClass` per modal.

import { el, emptyState } from "../dom.js";

// One meta line under the entity name. Accepts a bare string (default .voice-meta)
// or { text, class } for the odd line (voices' italic .voice-utt utterances,
// plates' raw-OCR .voice-meta.muted.small string).
function metaLine(line) {
  const spec = typeof line === "string" ? { text: line } : line;
  return el("div", { class: spec.class || "voice-meta", text: spec.text });
}

/**
 * Build one catalog-entity card.
 *
 * An archived (disregarded) entity renders a reduced card — `extra` controls +
 * Restore only; renaming and merging are noise for something the user tucked away.
 *
 * @param {object} opts
 * @param {object} opts.entity          raw record (callbacks usually close over it; kept for callers that share one handler)
 * @param {string} opts.label           .voice-name heading
 * @param {string|object|Array} opts.sublabel  meta line(s): string | {text,class} | array of those
 * @param {Element} [opts.media]        media element (face/plate <img>); its presence switches to the row layout (.person-info column)
 * @param {boolean} [opts.archived]     render the reduced archived card
 * @param {(name:string)=>Promise} [opts.onRename]  save handler (do the API call + reload inside); presence renders input+Save
 * @param {string} [opts.renamePlaceholder]
 * @param {string} [opts.renameValue]   current name to prefill
 * @param {Array<{value:string,label:string}>} [opts.mergeOptions]  merge targets (never offered on archived cards)
 * @param {string} [opts.mergeLabel]    head option text
 * @param {string} [opts.mergeTitle]    tooltip on the select
 * @param {(into:string)=>Promise} [opts.onMerge]
 * @param {()=>Promise} [opts.onArchive]  Disregard handler (active cards)
 * @param {()=>Promise} [opts.onRestore]  Restore handler (archived cards)
 * @param {string} [opts.archiveTitle]
 * @param {string} [opts.restoreTitle]
 * @param {Element|Element[]} [opts.extra]  module-specific action controls (Play sample, Watch star) — placed after name controls, before merge
 * @param {object} [opts.errors]        per-action toast text: { rename, merge, archive, restore }
 * @param {(msg:string)=>void} [opts.flash]  error-toast sink (ctx.flash)
 * @param {string} [opts.cardClass]     "voice-card" (voices) | "voice-card person-card" (people/plates)
 * @returns {Element}
 */
export function entityCard({
  entity = null,
  label,
  sublabel = null,
  media = null,
  archived = false,
  onRename = null,
  renamePlaceholder = "Name",
  renameValue = "",
  mergeOptions = [],
  mergeLabel = "Merge into…",
  mergeTitle = null,
  onMerge = null,
  onArchive = null,
  onRestore = null,
  archiveTitle = null,
  restoreTitle = null,
  extra = null,
  errors = {},
  flash = () => {},
  cardClass = "voice-card",
} = {}) {
  const card = el("div", { class: cardClass });
  // Media-bearing cards (People/Plates) lay out as a row: media | info column.
  const host = media ? el("div", { class: "person-info" }) : card;
  if (media) card.append(media, host);

  host.appendChild(el("div", { class: "voice-name", text: label }));
  for (const line of sublabel == null ? [] : [].concat(sublabel)) {
    host.appendChild(metaLine(line));
  }

  const actions = el("div", { class: "voice-actions" });

  // Name input + Save: disabled while the rename is in flight; on failure re-enable
  // and toast. On success the caller's reload re-renders the whole list, so the
  // button never needs re-enabling here.
  if (!archived && onRename) {
    const input = el("input", { type: "text", placeholder: renamePlaceholder });
    input.value = renameValue || "";
    const save = el("button", { type: "button", text: "Save" });
    save.addEventListener("click", async () => {
      const name = input.value.trim();
      if (!name) return;
      save.disabled = true;
      try {
        await onRename(name, entity);
      } catch {
        save.disabled = false;
        flash(errors.rename || "Couldn't save that name.");
      }
    });
    actions.append(input, save);
  }

  for (const extraEl of [].concat(extra || [])) actions.appendChild(extraEl);

  // "Merge into…" — fold this entity into another (same voice/person/plate, split
  // across ids). Fires on selection without a confirm, exactly as before.
  if (!archived && onMerge && mergeOptions.length) {
    const merge = el("select", mergeTitle ? { title: mergeTitle } : {});
    merge.appendChild(el("option", { value: "", text: mergeLabel }));
    for (const o of mergeOptions) {
      merge.appendChild(el("option", { value: o.value, text: o.label }));
    }
    merge.addEventListener("change", async () => {
      const into = merge.value;
      if (!into) return;
      merge.disabled = true;
      try {
        await onMerge(into, entity);
      } catch {
        merge.disabled = false;
        flash(errors.merge || "Couldn't merge those.");
      }
    });
    actions.appendChild(merge);
  }

  const toggleFn = archived ? onRestore : onArchive;
  if (toggleFn) {
    const title = archived ? restoreTitle : archiveTitle;
    const btn = el("button", {
      type: "button",
      text: archived ? "Restore" : "Disregard",
      ...(title ? { title } : {}),
    });
    btn.addEventListener("click", async () => {
      btn.disabled = true;
      try {
        await toggleFn(entity);
      } catch {
        btn.disabled = false;
        flash(
          archived
            ? errors.restore || "Couldn't restore that."
            : errors.archive || "Couldn't disregard that.",
        );
      }
    });
    actions.appendChild(btn);
  }

  host.appendChild(actions);
  return card;
}

/** A section heading with a count, e.g. "Known voices (3)". */
export function sectionTitle(text, count) {
  return el(
    "div",
    { class: "voice-section-title", text },
    el("span", { class: "muted small", text: `  (${count})` }),
  );
}

/** A disclosure ("Known voices (N)") wrapping its cards; `open` controls the default state. */
export function collapsibleSection(text, count, cardEls, { open = false } = {}) {
  return el(
    "details",
    { class: "voice-section", open },
    el(
      "summary",
      { class: "voice-section-title", text },
      el("span", { class: "muted small", text: `  (${count})` }),
    ),
    el("div", { class: "voice-section-body" }, cardEls),
  );
}

// One group of the unified layout. With cards → a disclosure; empty with an
// emptyText → title + the standard empty-state block; empty without → omitted
// (used for Archived, and for every group while a search filter is active).
function groupEls(group, open) {
  if (!group) return [];
  if (group.cards.length) {
    return [collapsibleSection(group.title, group.cards.length, group.cards, { open })];
  }
  if (group.emptyText) return [sectionTitle(group.title, 0), emptyState(group.emptyText)];
  return [];
}

/**
 * The unified three-group modal-body layout shared by Voices/People/Plates:
 * Known/Named collapses closed (it grows over time and isn't useful on every
 * open), Unidentified stays open (the entities you open the modal to name),
 * Archived collapses closed at the bottom. Each group: { title, cards,
 * emptyText? }. Returns the elements to append, in order.
 */
export function entitySections({ known, unknown, archived }) {
  return [...groupEls(known, false), ...groupEls(unknown, true), ...groupEls(archived, false)];
}

/** Client-side catalog filter: case-insensitive substring over display name or id. */
export function matchesQuery(query, name, id) {
  if (!query) return true;
  const q = query.toLowerCase();
  return (
    String(name || "").toLowerCase().includes(q) || String(id || "").toLowerCase().includes(q)
  );
}
