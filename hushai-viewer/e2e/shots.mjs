// Regenerate the README screenshots from a running viewer, in REAL Google Chrome.
//
// Chromium is not an option: it has no H.264/AAC, so the HLS remux never decodes and every
// shot of the player comes out blank. Same reason run.mjs pins `puppeteer-core` + an explicit
// executablePath.
//
// Prereqs:
//   1. A stack running against a DEMO database with auth disabled — never a real install, since
//      every one of these images gets committed:
//        createdb hushai_demo
//        DATABASE_URL="postgres://$USER@localhost:5432/hushai_demo" \
//        BLOB_DIR="$PWD/local_dev/.demo_work/blobs" \
//        VISION_MOTION_SKIP_ENABLED=false VIEWER_AUTH_DISABLED=true \
//          ./local_dev/run_stack.sh
//   2. Demo footage injected and processed: ./local_dev/build_demo.sh
//   3. npm i in this directory.
//
// TWO PASSES, and the order matters:
//
//   SHOTS_PASS=player node shots.mjs          # 01–04, with NO live feeder running
//   ./local_dev/demo_live_feed.sh &           # then, with the cameras replaying live:
//   SHOTS_PASS=live node shots.mjs ; kill %1  # 05–06
//
// They cannot share a pass. The `live` pass needs demo_live_feed.sh, because the dashboard and the
// camera wall classify a camera purely on upload recency (<15s live, <5m idle) and a dataset left
// alone for five minutes screenshots as three OFFLINE cameras. But that same feeder moves the
// camera's `latestMs` to now, and the player can only navigate the last VIEWER_MAX_WINDOW_NANOS
// (6h) of footage — so with the feeder running, every frame of the demo's own footage falls
// outside the reachable window and the player shots silently land on the live tail instead: one
// 2-second segment every few seconds, drawn as a picket fence. See REACHABLE_MS below.
//
// Each shot is skipped rather than faked when its prerequisite is missing, and the script exits
// non-zero if any shot it was asked for could not be produced — a silently-empty screenshot is
// worse than a missing one, because it ends up in the README looking like a broken feature.

import puppeteer from "puppeteer-core";
import { mkdir, writeFile } from "node:fs/promises";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import path from "node:path";

const execFileP = promisify(execFile);

const VIEWER_URL = process.env.VIEWER_URL ?? "http://127.0.0.1:8070";
const CHROME =
  process.env.CHROME ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const OUT = process.env.SHOTS_DIR ?? path.resolve(import.meta.dirname, "../../docs/img");
// Captured at 2x then downscaled, so the images stay crisp on a retina display without
// committing 3-megapixel PNGs.
const WIDTH = Number(process.env.SHOTS_WIDTH ?? 1440);
const HEIGHT = Number(process.env.SHOTS_HEIGHT ?? 900);
const FINAL_WIDTH = Number(process.env.SHOTS_FINAL_WIDTH ?? 1600);

// Which pass to capture. `player` = the four shots that drive the timeline and must run with no
// live feeder; `live` = the two status pages that need one. Unset captures everything, which is
// only correct when nothing is feeding and the demo's own footage is still the newest on the
// stack. See the two-pass note at the top of this file.
const PASSES = {
  player: ["01-timeline", "02-detections", "03-chat", "04-investigate"],
  live: ["05-dashboard", "06-events"],
};
const PASS = process.env.SHOTS_PASS ?? "";
if (PASS && !PASSES[PASS]) {
  console.error(`SHOTS_PASS must be one of: ${Object.keys(PASSES).join(", ")}`);
  process.exit(2);
}
const wanted = PASS ? new Set(PASSES[PASS]) : null;

const results = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function until(page, fn, { timeout = 15000, step = 200, arg } = {}) {
  const t0 = Date.now();
  for (;;) {
    const v = arg === undefined ? await page.evaluate(fn) : await page.evaluate(fn, arg);
    if (v) return v;
    if (Date.now() - t0 > timeout) throw new Error(`timed out waiting for ${fn}`);
    await sleep(step);
  }
}

await mkdir(OUT, { recursive: true });

const browser = await puppeteer.launch({
  executablePath: CHROME,
  headless: "new",
  args: [
    "--autoplay-policy=no-user-gesture-required",
    "--mute-audio",
    // The web-capture modal asks for a camera; without a fake device it renders a
    // permission error instead of a preview.
    "--use-fake-ui-for-media-stream",
    "--use-fake-device-for-media-stream",
    "--force-device-scale-factor=2",
    "--hide-scrollbars",
  ],
});

const page = await browser.newPage();
await page.setViewport({ width: WIDTH, height: HEIGHT, deviceScaleFactor: 2 });
page.on("dialog", (d) => d.dismiss().catch(() => {}));

async function shot(name, fn) {
  if (wanted && !wanted.has(name)) return; // not in this pass — leave the committed file alone
  const file = path.join(OUT, `${name}.png`);
  try {
    const r = await fn();
    if (r && r.skip) {
      results.push({ name, status: "SKIP", why: r.skip });
      return;
    }
    // Let the last paint settle — CSS transitions and canvas repaints both land a frame late.
    await sleep(600);
    const buf = await page.screenshot({ type: "png" });
    await writeFile(file, buf);
    // `sips` ships with macOS; elsewhere the 2x image is committed as-is.
    try {
      await execFileP("sips", ["-Z", String(FINAL_WIDTH), file], { timeout: 30_000 });
    } catch {
      /* not macOS, or sips unavailable — keep the full-resolution capture */
    }
    results.push({ name, status: "OK" });
  } catch (e) {
    results.push({ name, status: "FAIL", why: e?.message ?? String(e) });
  }
}

// A unix-nanos query parameter, built as a STRING. Nanosecond timestamps are ~1.8e18, well past
// Number.MAX_SAFE_INTEGER, so `ms * 1e6` silently rounds; pasting the zeroes on keeps every digit.
const ns = (ms) => `${Math.floor(ms)}000000`;

// Open the player ON a given instant and prove it got there. The `?device=&t=` deep link alone is
// not enough to rely on: the page can finish loading, decode a frame, and still be sitting at the
// live edge (auto-refresh follows new footage, and demo_live_feed.sh supplies some every few
// seconds), which silently produces a shot whose recorded track is empty because the view window
// and the loaded data are days apart. So: deep-link, then re-seek, then assert both the playhead
// and the coverage the scrub bar is about to draw.
async function parkAt(ms, { fitMs = 120_000 } = {}) {
  await page.goto(`${VIEWER_URL}/?device=${encodeURIComponent(deviceId)}&t=${Math.floor(ms)}`, {
    waitUntil: "domcontentloaded",
  });
  await until(page, () => window.viewerDebug?.state?.device != null, { timeout: 25000 });
  await page.evaluate(
    ({ ms, fitMs }) => {
      window.viewerDebug.timeline.fit(ms - fitMs, ms + fitMs);
      window.viewerDebug.seekTo(ms, { play: true });
    },
    { ms, fitMs },
  );
  await until(
    page,
    ({ ms }) =>
      document.getElementById("video")?.videoWidth > 0 &&
      Math.abs(window.viewerDebug.currentMs() - ms) < 10_000 &&
      (window.viewerDebug.timeline.coverage ?? []).length > 0,
    { timeout: 30000, arg: { ms } },
  );
  // Pause on a decoded frame: a playing video can screenshot mid-buffer as a black frame.
  await page.evaluate(() => document.getElementById("video")?.pause());
}

// Clear transient UI that would either date the screenshot or hang a tooltip over it.
async function clearChrome() {
  await page.evaluate(() => {
    const t = document.getElementById("toast");
    if (t) t.textContent = "";
    const p = document.getElementById("tlPreview");
    if (p) p.hidden = true;
  });
}

// Every recorded span for a camera, across its whole retention. `window.viewerDebug.timeline
// .coverage` is NOT a substitute: it only holds the window the player has loaded, which at boot
// is the newest footage. With demo_live_feed.sh running that window is the live tail — one
// 2-second segment every few seconds — so anything chosen from it draws as a picket fence and
// makes the recorder look like it drops three quarters of its input.
// The timeline route clamps each request to 6h (hushai-viewer/src/routes.rs), hence the walk.
async function recordedSpans(deviceId, earliestMs, latestMs) {
  const SIX_H = 6 * 3600_000;
  const out = [];
  for (let s = earliestMs; s < latestMs; s += SIX_H) {
    const e = Math.min(s + SIX_H, latestMs);
    const r = await fetch(
      `${VIEWER_URL}/api/devices/${encodeURIComponent(deviceId)}/timeline` +
        `?from=${ns(s)}&to=${ns(e)}`,
    );
    if (!r.ok) continue;
    const { coverage = [] } = await r.json();
    for (const c of coverage) {
      out.push([
        Math.floor(c.start_unix_nanos / 1e6),
        Math.floor(c.end_unix_nanos / 1e6),
      ]);
    }
  }
  return out;
}

// How far back the PLAYER can actually be driven. `app.js refetchTimeline` asks for the camera's
// whole range, but `get_timeline` clamps any window wider than VIEWER_MAX_WINDOW_NANOS (6h) to its
// most recent slice — `clamp_window` returns `(to - max, to)`. The scrub bar therefore only ever
// holds the last 6h of coverage, and `snapToCovered` drags a seek outside it forward to the live
// edge. Asking for an older instant does not fail loudly; it silently photographs the wrong
// moment with an empty recorded track, so candidates have to come from this tail.
const REACHABLE_MS = Number(process.env.SHOTS_REACHABLE_HOURS ?? 6) * 3600_000;
const reachable = (spans, latestMs) => spans.filter(([, e]) => e > latestMs - REACHABLE_MS);

// Park the playhead on an instant that provably has a decodable video frame, so no shot lands on a
// correctly-empty stretch of the timeline. Candidates are the midpoints of the longest reachable
// spans, newest first among equals — the hero has to show a continuous timeline, and the newest
// footage is where the scrub bar is guaranteed to have data. The live edge stays as a last resort
// for a stack that has only ever seen a trickle. The thumb.jpg probe is the same one run.mjs uses.
async function findVideoMs(deviceId, spans, latestMs) {
  const cands = reachable(spans, latestMs)
    .sort((a, b) => b[1] - b[0] - (a[1] - a[0]) || b[0] - a[0])
    .slice(0, 12)
    .map(([s, e]) => (s + e) / 2);
  if (latestMs) cands.push(latestMs - 10_000);
  for (const ms of cands) {
    const r = await fetch(
      `${VIEWER_URL}/api/devices/${encodeURIComponent(deviceId)}/thumb.jpg?t=${ns(ms)}`,
    );
    if (r.status === 200) {
      await r.arrayBuffer();
      return ms;
    }
  }
  return null;
}

// Which camera the player shots use. Prefer a vehicle/plate camera over a face camera: the
// demo's face footage is built from public-domain portraits of real (identifiable) people, and
// a published screenshot of a surveillance product is not the place for someone's face, even a
// public-domain one. Override with SHOTS_DEVICE to force a specific camera.
const PREFER_DEVICE = process.env.SHOTS_DEVICE ?? "";
const PREFER_MATCH = /driveway|vehicle|car|garage|plate/i;

// Scan the recorded spans through the very endpoint the overlay itself uses and return the instant
// carrying the most boxes. Reusing the hero's instant instead lands on whatever frame happened to
// decode first, which is usually a single lonely box — a screenshot that undersells the feature.
// Bounded to the longest SCAN_SPANS spans so a long-running install does not scan its whole disk.
const SCAN_SPANS = 24;
async function findRichestDetectionMs(deviceId, spans, latestMs) {
  let best = null;
  // Same reachability constraint as findVideoMs: an instant the player cannot be driven to is
  // useless here, however many boxes it carries.
  const biggest = reachable(spans, latestMs)
    .sort((a, b) => b[1] - b[0] - (a[1] - a[0]) || b[0] - a[0])
    .slice(0, SCAN_SPANS);
  for (const [s, e] of biggest) {
    const to = Math.min(e, s + 6 * 3600_000);
    const r = await fetch(
      `${VIEWER_URL}/api/devices/${encodeURIComponent(deviceId)}/detections` +
        `?from=${ns(s)}&to=${ns(to)}`,
    );
    if (!r.ok) continue;
    const { frames = [] } = await r.json();
    for (const f of frames) {
      const n = f.detections?.length ?? 0;
      if (n && (!best || n > best.n)) best = { n, ms: Math.floor(f.t_unix_nanos / 1e6) };
    }
  }
  return best?.ms ?? null;
}

let deviceId = null;
let spans = [];
let latestMs = null;

async function openBusiestCamera() {
  await page.goto(`${VIEWER_URL}/`, { waitUntil: "domcontentloaded" });
  await until(page, () => window.viewerDebug?.state?.device != null, { timeout: 25000 });
  const id = await page.evaluate(
    ({ forced, matchSrc }) => {
      const re = new RegExp(matchSrc, "i");
      const devs = (window.viewerDebug.state.devices ?? []).filter(
        (d) => d.segmentCount > 0 && d.latestMs,
      );
      if (!devs.length) return null;
      if (forced) return devs.find((d) => d.id === forced)?.id ?? null;
      const byFootage = [...devs].sort((a, b) => b.segmentCount - a.segmentCount);
      const preferred = byFootage.find((d) => re.test(d.displayName ?? d.name ?? d.id));
      return (preferred ?? byFootage[0]).id;
    },
    { forced: PREFER_DEVICE, matchSrc: PREFER_MATCH.source },
  );
  if (id) {
    await page.goto(`${VIEWER_URL}/?device=${encodeURIComponent(id)}`, {
      waitUntil: "domcontentloaded",
    });
    await until(page, () => window.viewerDebug?.state?.device != null, { timeout: 25000 });
  }
  return id;
}

// ---------------------------------------------------------------- 01 · the NVR timeline (hero)
let videoMs = null;
await shot("01-timeline", async () => {
  deviceId = await openBusiestCamera();
  if (!deviceId) return { skip: "no device with footage — run ./local_dev/build_demo.sh first" };
  const range = await page.evaluate(() => {
    const d = window.viewerDebug?.state?.device;
    return d ? { earliestMs: d.earliestMs, latestMs: d.latestMs } : null;
  });
  latestMs = range?.latestMs ?? null;
  if (!range?.earliestMs || !range?.latestMs) return { skip: "camera reports no recorded range" };
  spans = await recordedSpans(deviceId, range.earliestMs, range.latestMs);
  videoMs = await findVideoMs(deviceId, spans, range.latestMs);
  if (videoMs == null) return { skip: "no video-bearing instant found" };
  await parkAt(videoMs, { fitMs: 120_000 });
  await clearChrome();
});

// ---------------------------------------------------------------- 02 · detections overlay
await shot("02-detections", async () => {
  if (videoMs == null) return { skip: "no video-bearing instant found" };
  const detMs = (await findRichestDetectionMs(deviceId, spans, latestMs)) ?? videoMs;
  // Re-enter on the detection instant rather than overlaying boxes on whatever frame shot 01 left
  // behind: the overlay is fetched for the loaded window, so a stale frame under fresh boxes is
  // exactly the kind of quietly-wrong screenshot that is worse than none.
  await parkAt(detMs, { fitMs: 90_000 });
  await page.evaluate(() => window.viewerDebug.setMode(true));
  // The overlay only paints once the detections for the loaded window have arrived.
  await until(
    page,
    () => {
      const c = document.getElementById("detOverlay");
      return c && c.classList.contains("show") && c.width > 0;
    },
    { timeout: 20000 },
  );
  await clearChrome();
  await sleep(1500); // give the fetch + first paint a beat
});

// ---------------------------------------------------------------- 03 · RAG chat with citations
await shot("03-chat", async () => {
  await page.evaluate(() => window.viewerDebug.setMode(false));
  await until(page, () => !!window.chatDebug, { timeout: 20000 });
  const sel = "#chatPanes .chat-pane:not([style*='none']) .chat-input textarea";
  await page.click(sel);
  // A question the demo dialogue actually answers (see local_dev/build_demo.sh).
  await page.type(sel, process.env.SHOTS_CHAT_Q ?? "What did the courier say about the parcels?");
  await page.keyboard.press("Enter");
  // Wait for the turn to FINISH, not merely to start. The obvious "assistant text is longer than
  // N characters" test fires on the first streamed tokens and photographs a half-written
  // sentence. The pane re-enables its own Send button in the `finally` of `_send`
  // (chat-pane.js:536), so a live submit button is the one reliable end-of-turn signal in the
  // DOM. A local 7B answering over a hundred-odd citations is not quick.
  const answered = await until(
    page,
    () => {
      const pane = [...document.querySelectorAll("#chatPanes .chat-pane")].find(
        (p) => p.style.display !== "none",
      );
      const btn = pane?.querySelector(".chat-input button[type='submit']");
      const msg = pane?.querySelector(".chat-msg.assistant .chat-text");
      if (!btn || btn.disabled) return false; // still streaming
      return !!msg && msg.textContent.trim().length > 40;
    },
    { timeout: 240_000 },
  ).catch(() => false);
  if (!answered) return { skip: "the assistant produced no answer in time" };
  await page.evaluate(() => {
    const pane = [...document.querySelectorAll("#chatPanes .chat-pane")].find(
      (p) => p.style.display !== "none",
    );
    pane?.querySelector(".chat-msg.user")?.scrollIntoView({ block: "start" });
    // Then pin it. The pane keeps calling `_scroll()` after the answer is complete — the hover
    // action row, the read-aloud control and the retry affordance each append to the message and
    // re-anchor to the bottom — so an unpinned log slides back down before the shutter and the
    // shot is a wall of chips again. A no-op setter is only safe because the page is about to be
    // photographed and discarded.
    const log = pane?.querySelector(".chat-log");
    if (log) {
      Object.defineProperty(log, "scrollTop", {
        configurable: true,
        get() {
          return 0;
        },
        set() {},
      });
    }
  });
  await sleep(300);
});

// ---------------------------------------------------------------- 04 · investigate / entity graph
await shot("04-investigate", async () => {
  await page.goto(`${VIEWER_URL}/investigate.html`, { waitUntil: "domcontentloaded" });
  await until(page, () => document.getElementById("liveDot")?.classList.contains("live"), {
    timeout: 20000,
  });
  const picked = await page.evaluate(() => {
    const s = document.getElementById("entitySelect");
    const opts = [...(s?.options ?? [])].filter((o) => o.value);
    if (!opts.length) return null;
    // Prefer a vehicle/plate entity over a person, for the same reason the player shots do.
    const pick =
      opts.find((o) => /plate|vehicle|car/i.test(o.textContent || "")) ?? opts[0];
    s.value = pick.value;
    s.dispatchEvent(new Event("change"));
    return pick.value;
  });
  if (!picked) return { skip: "no catalog entities — the perception lanes produced none" };
  await until(
    page,
    () => {
      const card = document.getElementById("entityCard");
      if (!card || card.querySelector(".empty.is-error")) return false;
      if (card.querySelector(".entity-head")) return true;
      const note = card.querySelector(".empty");
      return note && !/Loading/.test(note.textContent);
    },
    { timeout: 20000 },
  );
});

// ---------------------------------------------------------------- 05 · system dashboard
await shot("05-dashboard", async () => {
  await page.goto(`${VIEWER_URL}/dashboard.html`, { waitUntil: "domcontentloaded" });
  await until(page, () => document.querySelectorAll("#kpis .kpi").length > 0, { timeout: 20000 });
  await sleep(1200); // the service/queue panels poll in after first paint
});

// ---------------------------------------------------------------- 06 · events & alert rules
await shot("06-events", async () => {
  await page.goto(`${VIEWER_URL}/events.html`, { waitUntil: "domcontentloaded" });
  await until(page, () => document.getElementById("liveDot")?.classList.contains("live"), {
    timeout: 20000,
  });
  await sleep(1200);
});

// There is deliberately no camera-wall shot. `cameras.html` renders a poster frame per camera,
// and two of the demo's three cameras are built from public-domain PORTRAIT photographs of real,
// identifiable people — so the wall is a grid of faces. The same rule rules out any shot that
// renders a person crop:
//
//   · cameras.html poster tiles                    (whole frame, so a portrait camera = a face)
//   · the Watchlist row for a `person` subject     (events.js `sampleFaceUrl`; build_demo.sh
//                                                   therefore watches the PLATE instead)
//   · the identity-binding review queue further down investigate.html, which renders a
//     `.binding-face` crop per candidate (investigate.js:333) — which is why shot 04 stays
//     scrolled at the entity card and never pages down
//
// If you add a shot, check it against that list first. Everything published here shows the
// vehicle camera, text, or chrome. See docs/screenshots.md.

await browser.close();

// ---------------------------------------------------------------- report
let bad = 0;
for (const r of results) {
  const mark = r.status === "OK" ? "📸" : r.status === "SKIP" ? "⏭️ " : "❌";
  console.log(`${mark} ${r.status.padEnd(4)} ${r.name}${r.why ? ` — ${r.why}` : ""}`);
  if (r.status !== "OK") bad++;
}
console.log(
  `\n${results.length} shots → ${OUT}: ${results.filter((r) => r.status === "OK").length} written, ${bad} not produced`,
);
process.exit(bad ? 1 : 0);
