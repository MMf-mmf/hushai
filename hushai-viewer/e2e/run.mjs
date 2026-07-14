// End-to-end sweep of the viewer UI in REAL Google Chrome (Chromium lacks H.264/AAC,
// so `puppeteer` proper won't play the footage — see README "Browser playback").
//
// Prereqs: the viewer running with auth disabled (no VIEWER_ADMIN_PASSWORD) against a DB
// with at least one video-bearing device (`local_dev/feed_segments.py` seeds one), and
// `npm i` in this directory. Then: `node run.mjs`.
//
// Scenarios degrade honestly: a check whose fixture prerequisite is missing (no events
// seeded, audio-only device) reports SKIP with the reason instead of a false PASS.
// Any native alert()/confirm() anywhere fails the run — the UI must never use them.

import puppeteer from "puppeteer-core";

const VIEWER_URL = process.env.VIEWER_URL ?? "http://127.0.0.1:8070";
const CHROME =
  process.env.CHROME ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";

const results = [];
const pass = (name) => results.push({ name, status: "PASS" });
const fail = (name, why) => results.push({ name, status: "FAIL", why });
const skip = (name, why) => results.push({ name, status: "SKIP", why });

async function check(name, fn) {
  try {
    const r = await fn();
    if (r && r.skip) skip(name, r.skip);
    else pass(name);
  } catch (e) {
    fail(name, e?.message ?? String(e));
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Wait until fn() (evaluated in the page) is truthy, with a timeout.
async function until(page, fn, { timeout = 8000, step = 150 } = {}) {
  const t0 = Date.now();
  for (;;) {
    const v = await page.evaluate(fn);
    if (v) return v;
    if (Date.now() - t0 > timeout) throw new Error(`timed out waiting for ${fn}`);
    await sleep(step);
  }
}

const browser = await puppeteer.launch({
  executablePath: CHROME,
  headless: "new",
  args: ["--autoplay-policy=no-user-gesture-required", "--mute-audio"],
});

let dialogSeen = null;
const page = await browser.newPage();
await page.setViewport({ width: 1280, height: 860 });
// The whole UI must be free of native dialogs; seeing one fails the run at the end.
page.on("dialog", (d) => {
  dialogSeen = `${d.type()}: ${d.message()}`;
  d.dismiss().catch(() => {});
});

// --- pre-auth stylesheet -----------------------------------------------------------
await check("styles.css is public (no session cookie)", async () => {
  const res = await fetch(`${VIEWER_URL}/styles.css`);
  if (res.status !== 200) throw new Error(`GET /styles.css -> ${res.status}`);
  const ct = res.headers.get("content-type") ?? "";
  if (!ct.includes("text/css")) throw new Error(`content-type ${ct}`);
});

// --- viewer boots + plays ----------------------------------------------------------
await page.goto(`${VIEWER_URL}/`, { waitUntil: "domcontentloaded" });
await check("viewer boots (debug handle, device selected)", async () => {
  await until(page, () => window.viewerDebug?.state?.device != null, { timeout: 15000 });
});

const hasVideo = await page.evaluate(() => {
  const d = window.viewerDebug?.state?.device;
  return !!(d && (d.hasVideo || d.hasMuxed));
});

// A device can have audio-only stretches: coverage alone doesn't imply video there.
// Find one wall-clock instant that provably has a video frame (thumb.jpg -> 200) so
// the preview/export checks aim at real video, not at a correctly-empty spot.
async function findVideoMs() {
  const cands = await page.evaluate(() => {
    const s = window.viewerDebug.state;
    const cov = window.viewerDebug.timeline.coverage;
    const out = [];
    if (s.device?.latestMs) out.push(s.device.latestMs - 10_000);
    for (const c of [...cov].reverse().slice(0, 8)) out.push((c.startMs + c.endMs) / 2);
    return { id: s.device.id, cands: out };
  });
  for (const ms of cands.cands) {
    const t = `${Math.floor(ms)}000000`; // ms -> ns
    const r = await fetch(`${VIEWER_URL}/api/devices/${encodeURIComponent(cands.id)}/thumb.jpg?t=${t}`);
    if (r.status === 200) {
      await r.arrayBuffer();
      return ms;
    }
  }
  return null;
}
const videoMs = hasVideo ? await findVideoMs() : null;

await check("video decodes (real H.264 frame)", async () => {
  if (!hasVideo) return { skip: "selected device has no video" };
  await until(page, () => document.getElementById("video")?.videoWidth > 0, { timeout: 20000 });
});

// --- seek paths ----------------------------------------------------------------------
await check("gap seek: directional toast + optimistic playhead", async () => {
  const r = await page.evaluate(() => {
    const t = window.viewerDebug.timeline;
    const cov = t.coverage;
    if (!cov?.length) return { skip: "no coverage" };
    // Aim 90s past the last covered edge — guaranteed gap.
    const target = cov[cov.length - 1].endMs + 90_000;
    window.viewerDebug.seekTo(target, { play: false });
    return { target, playhead: t.playheadMs, toast: document.getElementById("toast")?.textContent };
  });
  if (r.skip) return r;
  if (!/skipping .*(ahead|back)/.test(r.toast ?? "")) {
    throw new Error(`toast was ${JSON.stringify(r.toast)}`);
  }
  if (r.playhead == null || Math.abs(r.playhead - r.target) < 1000) {
    throw new Error("playhead did not snap optimistically");
  }
});

await check("frame step pauses and moves one frame", async () => {
  if (!hasVideo) return { skip: "no video" };
  await until(page, () => window.viewerDebug.currentMs() > 0, { timeout: 15000 });
  const before = await page.evaluate(() => {
    const v = document.getElementById("video");
    return v.currentTime;
  });
  await page.keyboard.press("Period");
  await sleep(400);
  const after = await page.evaluate(() => {
    const v = document.getElementById("video");
    return { t: v.currentTime, paused: v.paused };
  });
  if (!after.paused) throw new Error("video not paused after frame step");
  const delta = after.t - before;
  if (!(delta > 0 && delta < 0.5)) throw new Error(`currentTime moved ${delta}s`);
});

// --- hover preview -------------------------------------------------------------------
await check("hover preview thumbnail loads", async () => {
  if (!hasVideo) return { skip: "no video" };
  if (videoMs == null) return { skip: "no video-bearing instant found" };
  const box = await page.evaluate((ms) => {
    const t = window.viewerDebug.timeline;
    // Make sure the instant is inside the visible window, then aim the mouse at it.
    if (ms < t.from || ms > t.to) t.fit(ms - 60_000, ms + 60_000);
    const rect = t.canvas.getBoundingClientRect();
    return { x: rect.left + t.xOf(ms), y: rect.top + 50 };
  }, videoMs);
  if (!box) return { skip: "no coverage" };
  await page.mouse.move(box.x, box.y);
  await until(
    page,
    () => {
      const p = document.getElementById("tlPreview");
      return p && !p.hidden && p.querySelector("img")?.naturalWidth > 0;
    },
    { timeout: 10000 },
  );
  await page.mouse.move(10, 10); // move off; preview must hide
  await until(page, () => document.getElementById("tlPreview")?.hidden);
});

// --- events lane + bell --------------------------------------------------------------
await check("event markers land on the lane", async () => {
  const n = await page.evaluate(() => window.viewerDebug.events());
  if (n === 0) return { skip: "no events seeded for this device" };
});

await check("alert bell ack decrements the badge", async () => {
  const before = await page.evaluate(() => {
    const b = document.getElementById("bellBadge");
    return b && !b.hidden ? Number(b.textContent) : 0;
  });
  if (!before) return { skip: "no unacked alerts seeded" };
  await page.click("#bellBtn");
  await page.evaluate(() => {
    [...document.querySelectorAll("button")].find((x) => x.textContent.trim() === "Ack")?.click();
  });
  await until(page, (prev = 0) => {
    const b = document.getElementById("bellBadge");
    const now = b && !b.hidden ? Number(b.textContent) : 0;
    return now < Number(document.body.dataset.e2ePrev ?? Infinity) || now === 0;
  });
});

// --- export mode ----------------------------------------------------------------------
await check("clip export: selection drives the download href", async () => {
  if (!hasVideo) return { skip: "no video" };
  if (videoMs == null) return { skip: "no video-bearing instant found" };
  const r = await page.evaluate((ms) => {
    const dbg = window.viewerDebug;
    dbg.setExportMode?.(true);
    const t = dbg.timeline;
    const from = ms - 10_000;
    const to = ms + 10_000;
    t.setSelection(from, to);
    const a = document.querySelector("#exportBar a[download]");
    return { href: a?.getAttribute("href") ?? "", from, to };
  }, videoMs);
  if (r.skip) return r;
  const u = new URL(r.href, VIEWER_URL);
  const from = Number(u.searchParams.get("from"));
  const to = Number(u.searchParams.get("to"));
  if (Math.round(from / 1e6) !== r.from || Math.round(to / 1e6) !== r.to) {
    throw new Error(`href range ${from}..${to} != selection ${r.from}..${r.to}`);
  }
  const res = await fetch(u, { method: "GET" });
  if (res.status !== 200) throw new Error(`export.mp4 -> ${res.status}`);
  const ct = res.headers.get("content-type") ?? "";
  if (!ct.includes("video/mp4")) throw new Error(`content-type ${ct}`);
  await res.arrayBuffer(); // drain
  await page.evaluate(() => window.viewerDebug.setExportMode?.(false));
});

// --- modal focus restore ---------------------------------------------------------------
await check("modal focus returns to opener on Escape", async () => {
  const ok = await page.evaluate(async () => {
    const opener = document.getElementById("btnSettings"); // ⚙ Voices modal
    if (!opener) return "no opener";
    opener.focus();
    opener.click();
    await new Promise((r) => setTimeout(r, 300));
    const modal = document.querySelector(".modal:not([hidden])");
    if (!modal) return "modal did not open";
    modal.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    await new Promise((r) => setTimeout(r, 100));
    return document.activeElement === opener ? true : `focus on ${document.activeElement?.id || document.activeElement?.tagName}`;
  });
  if (ok !== true) throw new Error(String(ok));
});

// --- omni palette ----------------------------------------------------------------------
await check("omni palette opens on / and closes on Escape", async () => {
  await page.keyboard.press("/");
  await until(page, () => !!document.querySelector(".omni-card input"));
  await page.keyboard.press("Escape");
  await until(page, () => !document.querySelector(".omni-card input:focus"));
});

// --- cameras grid ----------------------------------------------------------------------
await check("cameras grid renders poster tiles", async () => {
  await page.goto(`${VIEWER_URL}/cameras.html`, { waitUntil: "domcontentloaded" });
  await until(page, () => window.camerasDebug?.deviceCount() > 0, { timeout: 15000 });
  if (!hasVideo) return { skip: "no video device for posters" };
  await until(page, () => [...document.querySelectorAll(".cam-tile img")].some((i) => i.naturalWidth > 0), {
    timeout: 15000,
  });
});

// --- alerts center ----------------------------------------------------------------------
await check("alerts center loads (feed + rules + watchlist sections)", async () => {
  await page.goto(`${VIEWER_URL}/events.html`, { waitUntil: "domcontentloaded" });
  await until(page, () => {
    const live = document.getElementById("liveDot");
    return live && live.classList.contains("live");
  }, { timeout: 15000 });
  const missing = await page.evaluate(() =>
    ["feed", "watchlist", "events", "rules"].filter((id) => !document.getElementById(id)),
  );
  if (missing.length) throw new Error(`missing sections: ${missing.join(",")}`);
});

// --- dashboard ---------------------------------------------------------------------------
await check("dashboard loads KPIs + audit section", async () => {
  await page.goto(`${VIEWER_URL}/dashboard.html`, { waitUntil: "domcontentloaded" });
  await until(page, () => document.querySelectorAll("#kpis .kpi").length > 0, { timeout: 15000 });
  if (!(await page.evaluate(() => !!document.getElementById("audit")))) {
    throw new Error("audit section missing");
  }
});

// --- chat slash picker + plugin panes (advisor-v2 seam + Gotham G4) ----------------------
// Back to the viewer page for the composer checks.
await check("slash picker lists the plugin rows in the composer", async () => {
  await page.goto(`${VIEWER_URL}/`, { waitUntil: "domcontentloaded" });
  await until(page, () => !!window.chatDebug, { timeout: 15000 });
  await page.click("#chatPanes .chat-pane:not([style*='none']) .chat-input textarea");
  await page.keyboard.type("/");
  await until(page, () => !!document.querySelector(".slash-pop [data-cmd='advisor']"));
  const cmds = await page.evaluate(() =>
    [...document.querySelectorAll(".slash-pop [data-cmd]")].map((r) => r.dataset.cmd),
  );
  for (const want of ["auto", "advisor", "gotham"]) {
    if (!cmds.includes(want)) throw new Error(`missing row ${want} (got ${cmds.join(",")})`);
  }
  // Composer owns "/" while focused — the omni palette must NOT have opened.
  if (await page.evaluate(() => !!document.querySelector(".omni-card"))) {
    throw new Error("omni palette hijacked the composer's /");
  }
});

await check("selecting /gotham switches to the Detective pane (chip + debug mode)", async () => {
  await page.evaluate(() => {
    document.querySelector(".slash-pop [data-cmd='gotham']")?.dispatchEvent(
      new MouseEvent("mousedown", { bubbles: true, cancelable: true }),
    );
  });
  await until(page, () => window.chatDebug?.mode === "gotham");
  await until(page, () => {
    const pane = [...document.querySelectorAll("#chatPanes .chat-pane")].find(
      (p) => p.style.display !== "none",
    );
    return !!pane?.querySelector(".agent-chip");
  });
});

await check("Detective pane exit (✕) returns to the Assistant", async () => {
  await page.click("#chatPanes .chat-pane:not([style*='none']) .agent-chip-x");
  await until(page, () => window.chatDebug?.mode === "auto");
});

await check("selecting /advisor switches panes; sessions stay isolated per agent", async () => {
  await page.click("#chatPanes .chat-pane:not([style*='none']) .chat-input textarea");
  await page.keyboard.type("/adv");
  await until(page, () => !!document.querySelector(".slash-pop [data-cmd='advisor']"));
  await page.keyboard.press("Enter");
  await until(page, () => window.chatDebug?.mode === "advisor");
  // The advisor pane keys its server session under its own sessionStorage slot.
  const keys = await page.evaluate(() => ({
    advisor: sessionStorage.getItem("hushai.chat.session.advisor"),
    auto: sessionStorage.getItem("hushai.chat.session.auto"),
  }));
  if (keys.advisor && keys.advisor === keys.auto) throw new Error("advisor session bled into auto");
  await page.click("#chatPanes .chat-pane:not([style*='none']) .agent-chip-x");
  await until(page, () => window.chatDebug?.mode === "auto");
});

// Live advisor turn — needs the advisor service up behind the proxy; SKIP (not PASS)
// when absent. A thin opener is the cheapest real turn (one judge call → questions).
await check("advisor thin message → phase pill, then a questions round", async () => {
  const probe = await page.evaluate(async () => {
    try {
      const r = await fetch("/v1/advisor/sessions");
      return r.status;
    } catch {
      return 0;
    }
  });
  if (probe !== 200) return { skip: `advisor absent behind proxy (status ${probe})` };
  await page.click("#chatPanes .chat-pane:not([style*='none']) .chat-input textarea");
  await page.keyboard.type("/adv");
  await until(page, () => !!document.querySelector(".slash-pop [data-cmd='advisor']"));
  await page.keyboard.press("Enter");
  await until(page, () => window.chatDebug?.mode === "advisor");
  await page.type(
    "#chatPanes .chat-pane:not([style*='none']) .chat-input textarea",
    "I walked into my house.",
  );
  await page.keyboard.press("Enter");
  // Phase pill goes up immediately, then the gate turns into a 1–3 item questions list.
  await until(page, () => {
    const pane = [...document.querySelectorAll("#chatPanes .chat-pane")].find(
      (p) => p.style.display !== "none",
    );
    return !!pane?.querySelector(".chat-phase");
  });
  await until(
    page,
    () => {
      const pane = [...document.querySelectorAll("#chatPanes .chat-pane")].find(
        (p) => p.style.display !== "none",
      );
      const items = pane?.querySelectorAll(".chat-msg.assistant .chat-text ol li")?.length || 0;
      return items >= 1 && items <= 3;
    },
    { timeout: 180_000 },
  );
  await page.click("#chatPanes .chat-pane:not([style*='none']) .agent-chip-x");
});

// Live Detective turn — needs GOTHAM_ENABLED + Ollama on the stack; opt-in via
// E2E_GOTHAM_FULL=1 because a tool loop is 30–90 s of LLM time.
await check("Detective turn streams tool steps (E2E_GOTHAM_FULL=1)", async () => {
  if (process.env.E2E_GOTHAM_FULL !== "1") return { skip: "set E2E_GOTHAM_FULL=1 to run" };
  await page.click("#chatPanes .chat-pane:not([style*='none']) .chat-input textarea");
  await page.keyboard.type("/got");
  await until(page, () => !!document.querySelector(".slash-pop [data-cmd='gotham']"));
  await page.keyboard.press("Enter");
  await until(page, () => window.chatDebug?.mode === "gotham");
  await page.type(
    "#chatPanes .chat-pane:not([style*='none']) .chat-input textarea",
    "How many cameras are there? Use your tools.",
  );
  await page.keyboard.press("Enter");
  await until(
    page,
    () => {
      const pane = [...document.querySelectorAll("#chatPanes .chat-pane")].find(
        (p) => p.style.display !== "none",
      );
      return !!pane?.querySelector(".chat-tools .chat-tool-step");
    },
    { timeout: 180_000 },
  );
  await until(page, () => ["planning", "answering"].includes(window.chatDebug?.lastPhase), {
    timeout: 5000,
  });
  await page.click("#chatPanes .chat-pane:not([style*='none']) .agent-chip-x");
});

// --- investigate page (entity explorer · connections · binding queue) ---------------------
await check("investigate page loads (explorer, connections, bindings sections)", async () => {
  await page.goto(`${VIEWER_URL}/investigate.html`, { waitUntil: "domcontentloaded" });
  await until(page, () => {
    const live = document.getElementById("liveDot");
    return live && live.classList.contains("live");
  }, { timeout: 15000 });
  const missing = await page.evaluate(() =>
    ["entitySelect", "pathFrom", "pathTo", "bindings"].filter((id) => !document.getElementById(id)),
  );
  if (missing.length) throw new Error(`missing sections: ${missing.join(",")}`);
  // The bindings section must resolve to rows or the "awaiting review" empty state —
  // never the error state. errorState() also carries .empty, so exclude .is-error
  // explicitly (a plain .empty match would pass even on a backend failure).
  await until(page, () => {
    const host = document.getElementById("bindings");
    if (!host || host.querySelector(".empty.is-error")) return false;
    return host.querySelector(".binding-card") || host.querySelector(".empty");
  }, { timeout: 15000 });
});

await check("entity explorer renders an entity page with deep-linkable evidence", async () => {
  const picked = await page.evaluate(() => {
    const sel = document.getElementById("entitySelect");
    const first = [...sel.options].find((o) => o.value);
    if (!first) return null;
    sel.value = first.value;
    sel.dispatchEvent(new Event("change"));
    return first.value;
  });
  if (!picked) return { skip: "no catalog entities in this stack" };
  // Wait for the loaded card (.entity-head), NOT the synchronous "Loading…"/.empty
  // placeholder — and fail on the error state. A plain .empty match would resolve
  // instantly against the placeholder and scan an anchor-less card.
  await until(page, () => {
    const card = document.getElementById("entityCard");
    if (!card || card.querySelector(".empty.is-error")) return false;
    if (card.querySelector(".entity-head")) return true;
    // A graph with no edges legitimately shows a non-error .empty note — accept it
    // only once the loading placeholder has been replaced.
    const note = card.querySelector(".empty");
    return note && !/Loading/.test(note.textContent);
  }, { timeout: 15000 });
  // Any evidence/journey link must target the player's /?device=&t= deep-link shape.
  const badLink = await page.evaluate(() =>
    [...document.querySelectorAll("#entityCard a")].find(
      (a) => !/\/\?device=.+&t=\d+/.test(a.getAttribute("href") || ""),
    )?.outerHTML,
  );
  if (badLink) throw new Error(`non-deep-link evidence anchor: ${badLink.slice(0, 120)}`);
});

// --- native-dialog guard (must be last) ---------------------------------------------------
await check("no native alert()/confirm() fired anywhere", async () => {
  if (dialogSeen) throw new Error(dialogSeen);
});

await browser.close();

// --- report -------------------------------------------------------------------------------
let failed = 0;
for (const r of results) {
  const mark = r.status === "PASS" ? "✅" : r.status === "SKIP" ? "⏭️ " : "❌";
  console.log(`${mark} ${r.status.padEnd(4)} ${r.name}${r.why ? ` — ${r.why}` : ""}`);
  if (r.status === "FAIL") failed++;
}
console.log(`\n${results.length} checks: ${results.filter((r) => r.status === "PASS").length} passed, ${results.filter((r) => r.status === "SKIP").length} skipped, ${failed} failed`);
process.exit(failed ? 1 : 0);
