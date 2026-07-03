// Thin fetch wrappers around the viewer API. Converts the API's Unix-nanosecond
// fields into milliseconds at the boundary so the rest of the UI is ms-only.

import { nsToMs, msToNsStr } from "./time.js";

// When the admin session has lapsed the viewer returns 401 to fetch/XHR/SSE (and 303
// to HTML navigations). Bounce the SPA to the login page on any 401 so the user can
// re-authenticate instead of seeing opaque errors. Returns true if it redirected.
function redirectIfUnauth(res) {
  if (res.status === 401) {
    window.location.href = "/login";
    return true;
  }
  return false;
}

// One retry after 400ms on a network failure (fetch rejects with TypeError) or a gateway
// hiccup (502/503/504). Blind retry is safe here because everything routed through this
// helper is a GET — mutations all use their own fetch wrappers below.
async function fetchWithRetry(url) {
  try {
    const res = await fetch(url);
    if (![502, 503, 504].includes(res.status)) return res;
  } catch (e) {
    if (!(e instanceof TypeError)) throw e;
  }
  await new Promise((r) => setTimeout(r, 400));
  return fetch(url);
}

async function getJson(url) {
  const res = await fetchWithRetry(url);
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`${url} -> ${res.status}`);
  return res.json();
}

export async function getDevices() {
  const { devices } = await getJson("/api/devices");
  return devices.map((d) => ({
    id: d.device_id,
    displayName: d.display_name ?? null,
    sourceKind: d.source_kind,
    earliestMs: d.first_capture_unix_nanos != null ? nsToMs(d.first_capture_unix_nanos) : null,
    latestMs: d.last_capture_unix_nanos != null ? nsToMs(d.last_capture_unix_nanos) : null,
    segmentCount: d.segment_count,
    sessionCount: d.session_count,
    hasVideo: d.has_video,
    hasAudio: d.has_audio,
    hasMuxed: d.has_muxed,
  }));
}

// System dashboard: cameras + background-process status, in one payload. Returned as-is
// (the dashboard renders the server's fields directly; timestamps are RFC3339 strings).
export async function getDashboard() {
  return getJson("/api/dashboard");
}

export async function getTimeline(deviceId, fromMs, toMs) {
  const url = `/api/devices/${encodeURIComponent(deviceId)}/timeline?from=${msToNsStr(
    fromMs,
  )}&to=${msToNsStr(toMs)}`;
  const t = await getJson(url);
  return {
    fromMs: nsToMs(t.from),
    toMs: nsToMs(t.to),
    spans: t.spans.map((s) => ({
      sessionId: s.session_id,
      streamId: s.stream_id,
      kind: s.kind,
      startMs: nsToMs(s.start_unix_nanos),
      endMs: nsToMs(s.end_unix_nanos),
      segmentCount: s.segment_count,
    })),
    coverage: t.coverage.map((c) => ({
      startMs: nsToMs(c.start_unix_nanos),
      endMs: nsToMs(c.end_unix_nanos),
    })),
    sessionBoundariesMs: t.session_boundaries.map(nsToMs),
  };
}

export function masterUrl(deviceId, fromMs, toMs) {
  return `/hls/${encodeURIComponent(deviceId)}/master.m3u8?from=${msToNsStr(
    fromMs,
  )}&to=${msToNsStr(toMs)}`;
}

// AI detections (bounding boxes) for the Detections overlay. The server groups them by
// sampled-frame timestamp; we convert ns->ms and keep bbox as raw original-frame pixels
// (the overlay scales against video.videoWidth/videoHeight).
export async function getDetections(deviceId, fromMs, toMs) {
  const url = `/api/devices/${encodeURIComponent(deviceId)}/detections?from=${msToNsStr(
    fromMs,
  )}&to=${msToNsStr(toMs)}`;
  const d = await getJson(url);
  return {
    truncated: !!d.truncated,
    frames: (d.frames ?? []).map((f) => ({
      tMs: nsToMs(f.t_unix_nanos),
      boxes: (f.detections ?? []).map((x) => ({
        kind: x.kind,
        label: x.label ?? null,
        personId: x.person_id ?? null,
        bbox: x.bbox,
        score: x.det_score ?? null,
      })),
    })),
  };
}

// AI processing status for the scrub-bar ribbons. Two lanes (audio = transcription +
// speaker + sentiment; vision = faces + objects), each a list of coalesced status
// intervals with output counts. Converts ns->ms at the boundary.
export async function getProcessing(deviceId, fromMs, toMs) {
  const url = `/api/devices/${encodeURIComponent(deviceId)}/processing?from=${msToNsStr(
    fromMs,
  )}&to=${msToNsStr(toMs)}`;
  const p = await getJson(url);
  const audio = (p.audio ?? []).map((i) => ({
    startMs: nsToMs(i.start_unix_nanos),
    endMs: nsToMs(i.end_unix_nanos),
    status: i.status,
    lastError: i.last_error ?? null,
    sentences: i.sentences ?? 0,
    speakers: i.speakers ?? 0,
  }));
  const vision = (p.vision ?? []).map((i) => ({
    startMs: nsToMs(i.start_unix_nanos),
    endMs: nsToMs(i.end_unix_nanos),
    status: i.status,
    lastError: i.last_error ?? null,
    faces: i.faces ?? 0,
    objects: i.objects ?? 0,
  }));
  return {
    audio,
    vision,
    audioSummary: p.audio_summary ?? null,
    visionSummary: p.vision_summary ?? null,
    truncated: !!p.truncated,
  };
}

// Sentiment coverage for the scrub-bar mood ribbon: coalesced positive/neutral/negative
// runs (segment-grain server-side; see src/sentiment.rs). ns->ms at the boundary.
export async function getSentiment(deviceId, fromMs, toMs) {
  const url = `/api/devices/${encodeURIComponent(deviceId)}/sentiment?from=${msToNsStr(
    fromMs,
  )}&to=${msToNsStr(toMs)}`;
  const p = await getJson(url);
  return {
    intervals: (p.intervals ?? []).map((i) => ({
      startMs: nsToMs(i.start_unix_nanos),
      endMs: nsToMs(i.end_unix_nanos),
      sentiment: i.sentiment,
      segments: i.segments ?? 0,
    })),
    truncated: !!p.truncated,
  };
}

// ---- chat over recordings (proxied to hushai-rag at /v1/rag/*) ----------------

export async function getAgents() {
  return getJson("/v1/rag/agents");
}

export async function getSessionMessages(sessionId) {
  return getJson(`/v1/rag/chat/sessions/${encodeURIComponent(sessionId)}/messages`);
}

// Saved conversations, newest first: [{session_id, agent_id, title, created_at, updated_at}].
export async function getSessions() {
  return getJson("/v1/rag/chat/sessions");
}

// Local TTS (Kokoro). Returns a WAV blob for `new Audio(URL.createObjectURL(blob))`;
// throws with status 503 in the message when the engine isn't loaded.
export async function synthesizeSpeech(text) {
  const res = await fetch("/v1/tts", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ text }),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`tts -> ${res.status}`);
  return res.blob();
}

// ---- speaker admin (proxied to hushai-backend at /v1/speakers*) -----------------
// The viewer proxy routes these to hushai-backend and injects the device bearer, so the
// browser calls them on the same origin with no token. See src/proxy.rs.

export async function getSpeakers() {
  return getJson("/v1/speakers");
}

export async function getSpeakerDuplicates() {
  return getJson("/v1/speakers/duplicates");
}

// Re-run voice clustering server-side (semantic pass / deep neural pass). Both are
// long-running POSTs; callers should disable their trigger while awaiting.
export async function reclusterSpeakers() {
  const res = await fetch("/v1/speakers/recluster", { method: "POST" });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`recluster -> ${res.status}`);
  return res.json().catch(() => ({}));
}

export async function reclusterSpeakersDeep() {
  const res = await fetch("/v1/speakers/recluster-deep", { method: "POST" });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`deep recluster -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// URL for a speaker's 2s sample-audio clip — used directly as an <audio> src (the proxy
// adds the bearer), so no fetch-to-blob dance is needed.
export function sampleAudioUrl(id) {
  return `/v1/speakers/${encodeURIComponent(id)}/sample-audio`;
}

// Here and in every mutation wrapper below: `res.json().catch(() => ({}))` runs after the res.ok check, so it only tolerates a legitimately empty 200 body — it does not swallow errors.
export async function renameSpeaker(id, displayName) {
  const res = await fetch(`/v1/speakers/${encodeURIComponent(id)}`, {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ display_name: displayName }),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`rename -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// Fold `loserId` into `intoId` (no name-conflict guard — used for explicit "merge into").
export async function mergeSpeaker(loserId, intoId) {
  const res = await fetch(`/v1/speakers/${encodeURIComponent(loserId)}/merge`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ into: intoId }),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`merge -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// Fold a whole suggested duplicate group into `intoId` atomically.
export async function mergeSpeakerGroup(intoId, memberIds) {
  const res = await fetch("/v1/speakers/merge-group", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ into: intoId, members: memberIds }),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`merge-group -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// Disregard/restore a catalog entity (display-level archive: it moves to the "Archived"
// section in the modal; matching, RAG, and watchlist behavior are unaffected).
async function postJson(url, label) {
  const res = await fetch(url, { method: "POST" });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`${label} -> ${res.status}`);
  return res.json().catch(() => ({}));
}

export function archiveSpeaker(id) {
  return postJson(`/v1/speakers/${encodeURIComponent(id)}/archive`, "archive speaker");
}

export function unarchiveSpeaker(id) {
  return postJson(`/v1/speakers/${encodeURIComponent(id)}/unarchive`, "unarchive speaker");
}

// Candidate voices clustered from audio the matcher left unattributed (speaker_id NULL).
export async function getUnattributed() {
  return getJson("/v1/speakers/unattributed");
}

// URL for a still-unattributed cluster's sample clip. These clusters have no speaker_id yet,
// so we key on a segment_id from the cluster (used directly as an <audio> src; the proxy adds
// the bearer). Lets a user hear a candidate voice before naming it.
export function unattributedSampleAudioUrl(segmentId) {
  return `/v1/speakers/unattributed/sample-audio?segment_id=${encodeURIComponent(segmentId)}`;
}

// Mint a NEW named speaker from a cluster's segments (claims the still-unattributed ones).
export async function nameUnattributed(displayName, segmentIds) {
  const res = await fetch("/v1/speakers/unattributed/name", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ display_name: displayName, segment_ids: segmentIds }),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`name-unattributed -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// ---- person (face) admin (proxied to hushai-backend at /v1/persons*) ------------
// The visual twin of the speaker surface: list/name/merge the faces discovered in recordings.
// The viewer proxy routes these to hushai-backend and injects the device bearer.

export async function getPersons() {
  return getJson("/v1/persons");
}

// URL for a person's representative cropped face — used directly as an <img> src (the proxy
// adds the bearer), so no fetch-to-blob dance is needed. Mirrors sampleAudioUrl for voices.
export function sampleFaceUrl(id) {
  return `/v1/persons/${encodeURIComponent(id)}/sample-face`;
}

export async function renamePerson(id, displayName) {
  const res = await fetch(`/v1/persons/${encodeURIComponent(id)}`, {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ display_name: displayName }),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`rename person -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// Fold `loserId` into `intoId` (two ids that are the same person).
export async function mergePerson(loserId, intoId) {
  const res = await fetch(`/v1/persons/${encodeURIComponent(loserId)}/merge`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ into: intoId }),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`merge person -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// Disregard/restore a face (display-level archive; see archiveSpeaker).
export function archivePerson(id) {
  return postJson(`/v1/persons/${encodeURIComponent(id)}/archive`, "archive person");
}

export function unarchivePerson(id) {
  return postJson(`/v1/persons/${encodeURIComponent(id)}/unarchive`, "unarchive person");
}

// ---- license plates (ALPR) — the vehicle twin of persons, proxied to hushai-backend ----
// List/search/name/merge the license plates discovered in recordings, and pull a representative
// rectified-plate crop. Identity is the plate STRING (matched by normalized text), so a search box
// over the text is the natural lookup ("when did I see plate ABC123").

export async function getPlates() {
  return getJson("/v1/plates");
}

// Fuzzy text search over the plate catalog (exact + trigram). Returns the same shape as getPlates.
export async function searchPlates(q) {
  return getJson(`/v1/plates/search?q=${encodeURIComponent(q)}`);
}

// URL for a plate's representative rectified crop — used directly as an <img> src (proxy adds the
// bearer). Mirrors sampleFaceUrl for faces.
export function samplePlateUrl(id) {
  return `/v1/plates/${encodeURIComponent(id)}/sample-crop`;
}

export async function renamePlate(id, displayName) {
  const res = await fetch(`/v1/plates/${encodeURIComponent(id)}`, {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ display_name: displayName }),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`rename plate -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// Fold `loserId` into `intoId` (OCR variance split one plate into two ids).
export async function mergePlate(loserId, intoId) {
  const res = await fetch(`/v1/plates/${encodeURIComponent(loserId)}/merge`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ into: intoId }),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`merge plate -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// Disregard/restore a plate (display-level archive; see archiveSpeaker).
export function archivePlate(id) {
  return postJson(`/v1/plates/${encodeURIComponent(id)}/archive`, "archive plate");
}

export function unarchivePlate(id) {
  return postJson(`/v1/plates/${encodeURIComponent(id)}/unarchive`, "unarchive plate");
}

// ---- device management (proxied to hushai-backend at /v1/devices*) --------------
// Rename a device, read its per-day footage usage, set a keep-last-N-days retention policy, and
// delete footage (a day / several days) or a whole device. Proxied to hushai-backend (the bearer is
// injected server-side). Export is a viewer route (it owns ffmpeg) — see exportUrl below.

export async function getManagedDevices() {
  const list = await getJson("/v1/devices");
  return list.map((d) => ({
    id: d.device_id,
    displayName: d.display_name ?? null,
    sourceKind: d.source_kind,
    segmentCount: d.segment_count,
    sessionCount: d.session_count,
    bytes: d.logical_bytes,
    earliestMs: d.first_capture_unix_nanos != null ? nsToMs(d.first_capture_unix_nanos) : null,
    latestMs: d.last_capture_unix_nanos != null ? nsToMs(d.last_capture_unix_nanos) : null,
    retentionDays: d.retention_days ?? null,
    hasVideo: d.has_video,
    hasAudio: d.has_audio,
    hasMuxed: d.has_muxed,
  }));
}

// Per-day footage breakdown in the caller's local tz (server buckets by that tz).
export async function getDeviceUsage(deviceId, tz) {
  const url = `/v1/devices/${encodeURIComponent(deviceId)}/usage?tz=${encodeURIComponent(tz)}`;
  const days = await getJson(url);
  return days.map((d) => ({
    date: d.day, // YYYY-MM-DD (local)
    segmentCount: d.segment_count,
    bytes: d.logical_bytes,
    startMs: nsToMs(d.first_capture_unix_nanos),
    endMs: nsToMs(d.last_capture_unix_nanos),
  }));
}

export async function renameDevice(deviceId, displayName) {
  const res = await fetch(`/v1/devices/${encodeURIComponent(deviceId)}`, {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ display_name: displayName }),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`rename device -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// `days` is a positive integer to keep the last N days, or null to clear the policy (keep forever).
export async function setRetention(deviceId, days) {
  const res = await fetch(`/v1/devices/${encodeURIComponent(deviceId)}/retention`, {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ retention_days: days }),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`set retention -> ${res.status}`);
  return res.json().catch(() => ({}));
}

export async function deleteDevice(deviceId) {
  const res = await fetch(`/v1/devices/${encodeURIComponent(deviceId)}`, { method: "DELETE" });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`delete device -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// Delete one local-day bucket (server re-derives the day boundaries in `tz`, so the deleted set
// exactly matches what getDeviceUsage reported — no boundary bleed).
export async function deleteFootageDay(deviceId, tz, day) {
  const url = `/v1/devices/${encodeURIComponent(deviceId)}/footage?tz=${encodeURIComponent(
    tz,
  )}&day=${encodeURIComponent(day)}`;
  const res = await fetch(url, { method: "DELETE" });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`delete footage -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// Delete several days in one request; returns a per-day result array.
export async function bulkDeleteFootageDays(deviceId, tz, days) {
  const res = await fetch(`/v1/devices/${encodeURIComponent(deviceId)}/footage/bulk-delete`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ tz, days }),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`bulk delete -> ${res.status}`);
  return res.json().catch(() => []);
}

// A normal `<a download>` href — the viewer streams an MP4 and the session cookie authorizes it.
export function exportUrl(deviceId, fromMs, toMs, kind = "muxed") {
  return `/api/devices/${encodeURIComponent(deviceId)}/export.mp4?from=${msToNsStr(
    fromMs,
  )}&to=${msToNsStr(toMs)}&kind=${encodeURIComponent(kind)}`;
}

// Still frames (src/stills.rs): used directly as <img> src. thumbUrl is the timeline
// hover preview at wall-clock `ms` (quantize the caller side so the browser cache hits);
// posterUrl is the device's newest frame for camera-grid tiles.
export function thumbUrl(deviceId, ms) {
  return `/api/devices/${encodeURIComponent(deviceId)}/thumb.jpg?t=${msToNsStr(ms)}`;
}

export function posterUrl(deviceId) {
  return `/api/devices/${encodeURIComponent(deviceId)}/poster.jpg`;
}

// POST a chat turn and stream the answer as Server-Sent Events. `onEvent({event, data})`
// is called per SSE frame: `session` {session_id, agent_id}, `sources` [Source...],
// `token` {delta}, `done` {message_id}, or `error` {message}. EventSource can't POST a
// body, so we read the streaming fetch response and parse SSE frames by hand.
// `playback` = the viewer's live {device_id, playhead_unix_nanos} so the server can scope
// deictic questions ("who was speaking in this clip") to the open video; null when idle.
export async function streamChat(
  { sessionId, agentId, message, filters, playback, exhaustive },
  onEvent,
) {
  const res = await fetch("/v1/rag/chat", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "text/event-stream" },
    body: JSON.stringify({
      session_id: sessionId ?? null,
      agent_id: agentId ?? null,
      message,
      filters: filters ?? null,
      playback: playback ?? null,
      // "Thorough" toggle: exhaustive speaker attribution instead of semantic top-k.
      exhaustive: exhaustive ?? null,
      // The user's live local UTC offset (seconds) so spoken times ("today at 4:06 PM") match
      // their clock. getTimezoneOffset() is minutes-behind-UTC with inverted sign → negate.
      tz_offset_secs: -new Date().getTimezoneOffset() * 60,
    }),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok || !res.body) {
    const text = await res.text().catch(() => "");
    throw new Error(`chat -> ${res.status} ${text}`.trim());
  }
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  let skipped = 0; // frames with real content but no usable data lines (malformed)
  const feed = (frame) => {
    const ev = parseFrame(frame);
    if (ev) onEvent(ev);
    // Pure keep-alive comment frames are legitimate SSE — only count the rest.
    else if (frame.split(/\r?\n/).some((l) => l && !l.startsWith(":"))) skipped += 1;
  };
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    // SSE events are separated by a blank line. Tolerate both \n\n and \r\n\r\n.
    let sep;
    while ((sep = nextFrameBreak(buf)) !== null) {
      const frame = buf.slice(0, sep.idx);
      buf = buf.slice(sep.idx + sep.len);
      feed(frame);
    }
  }
  buf += decoder.decode(); // flush any buffered multi-byte tail
  if (buf.trim()) feed(buf); // a final frame may arrive without its trailing blank line
  if (skipped) console.warn(`streamChat: skipped ${skipped} malformed SSE frame(s)`);
}

function nextFrameBreak(buf) {
  const a = buf.indexOf("\n\n");
  const b = buf.indexOf("\r\n\r\n");
  if (a < 0 && b < 0) return null;
  if (b < 0 || (a >= 0 && a < b)) return { idx: a, len: 2 };
  return { idx: b, len: 4 };
}

function parseFrame(frame) {
  let event = "message";
  const data = [];
  for (const raw of frame.split(/\r?\n/)) {
    if (!raw || raw.startsWith(":")) continue; // blank or keep-alive comment
    if (raw.startsWith("event:")) event = raw.slice(6).trim();
    else if (raw.startsWith("data:")) data.push(raw.slice(5).replace(/^ /, ""));
  }
  if (!data.length) return null;
  let payload = data.join("\n");
  try {
    payload = JSON.parse(payload);
  } catch {
    /* leave as string */
  }
  return { event, data: payload };
}

// ---- events & alerts (proxied to hushai-backend at /v1/events* and /v1/alert-rules*) ----------
// The proactive VSaaS layer: the materialized event stream, the in-app notification feed (with
// acknowledge), and alert-rule CRUD. ns->ms at the boundary like the rest of this module. The proxy
// injects the device bearer, so the browser calls these on the same origin with no token.

function eventFromApi(e) {
  return {
    id: e.event_id,
    deviceId: e.device_id ?? null,
    type: e.event_type,
    severity: e.severity,
    subjectType: e.subject_type ?? null,
    subjectId: e.subject_id ?? null,
    subjectLabel: e.subject_label ?? null,
    segmentId: e.segment_id ?? null,
    startMs: nsToMs(e.start_unix_nanos),
    endMs: nsToMs(e.end_unix_nanos),
    score: e.score ?? null,
    metadata: e.metadata ?? {},
    createdMs: nsToMs(e.created_unix_nanos),
  };
}

export async function getEvents({
  deviceId,
  eventType,
  severity,
  subjectType,
  subjectId,
  sinceMs,
  untilMs,
  limit,
} = {}) {
  const p = new URLSearchParams();
  if (deviceId) p.set("device_id", deviceId);
  if (eventType) p.set("event_type", eventType);
  if (severity) p.set("severity", severity);
  if (subjectType) p.set("subject_type", subjectType);
  if (subjectId) p.set("subject_id", subjectId);
  if (sinceMs) p.set("since_unix_nanos", msToNsStr(sinceMs));
  if (untilMs) p.set("until_unix_nanos", msToNsStr(untilMs));
  if (limit) p.set("limit", String(limit));
  const rows = await getJson(`/v1/events?${p.toString()}`);
  return rows.map(eventFromApi);
}

export async function getEventFeed({ status, limit } = {}) {
  const p = new URLSearchParams();
  if (status) p.set("status", status);
  if (limit) p.set("limit", String(limit));
  const rows = await getJson(`/v1/events/feed?${p.toString()}`);
  return rows.map((d) => ({
    deliveryId: d.delivery_id,
    ruleId: d.rule_id ?? null,
    eventId: d.event_id ?? null,
    channel: d.channel,
    status: d.status,
    deviceId: d.device_id ?? null,
    eventType: d.event_type ?? null,
    severity: d.severity ?? null,
    subjectLabel: d.subject_label ?? null,
    createdMs: nsToMs(d.created_unix_nanos),
    // The underlying event's footage moment (for the deep-link); null if the event was purged.
    eventStartMs: d.event_start_unix_nanos != null ? nsToMs(d.event_start_unix_nanos) : null,
    acknowledged: !!d.acknowledged,
  }));
}

export async function ackDelivery(deliveryId) {
  const res = await fetch(`/v1/events/feed/${encodeURIComponent(deliveryId)}/ack`, { method: "POST" });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`ack -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// Alert rules are returned/sent in the backend's raw snake_case shape (the rule editor round-trips
// the whole object on PATCH, so we don't remap fields the way the ns->ms readers above do).
export async function getAlertRules() {
  return getJson("/v1/alert-rules");
}

export async function createAlertRule(body) {
  const res = await fetch("/v1/alert-rules", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`create rule -> ${res.status} ${await res.text().catch(() => "")}`.trim());
  return res.json();
}

export async function updateAlertRule(id, body) {
  const res = await fetch(`/v1/alert-rules/${encodeURIComponent(id)}`, {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`update rule -> ${res.status} ${await res.text().catch(() => "")}`.trim());
  return res.json();
}

export async function deleteAlertRule(id) {
  const res = await fetch(`/v1/alert-rules/${encodeURIComponent(id)}`, { method: "DELETE" });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`delete rule -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// ---- watchlist ("of interest", proxied to hushai-backend /v1/watchlist) -------------------------
// Mark a person/plate of interest → the backend auto-manages an alert rule so any sighting alerts.

export async function getWatchlist() {
  return getJson("/v1/watchlist");
}

export async function addWatch(subjectType, subjectId, reason = null) {
  const res = await fetch("/v1/watchlist", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ subject_type: subjectType, subject_id: subjectId, reason }),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`watch -> ${res.status}`);
  return res.json();
}

export async function removeWatch(watchId) {
  const res = await fetch(`/v1/watchlist/${encodeURIComponent(watchId)}`, { method: "DELETE" });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`unwatch -> ${res.status}`);
  return res.json().catch(() => ({}));
}

export async function updateWatch(watchId, { reason, enabled } = {}) {
  const body = {};
  if (reason !== undefined) body.reason = reason;
  if (enabled !== undefined) body.enabled = enabled;
  const res = await fetch(`/v1/watchlist/${encodeURIComponent(watchId)}`, {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`update watch -> ${res.status}`);
  return res.json().catch(() => ({}));
}

// ---- audit log (proxied to hushai-backend /v1/audit; append-only, read here) --------------------

export async function getAudit({ actor, action, targetType, targetId, sinceMs, limit } = {}) {
  const p = new URLSearchParams();
  if (actor) p.set("actor", actor);
  if (action) p.set("action", action);
  if (targetType) p.set("target_type", targetType);
  if (targetId) p.set("target_id", targetId);
  if (sinceMs) p.set("since_unix_nanos", msToNsStr(sinceMs));
  if (limit) p.set("limit", String(limit));
  const rows = await getJson(`/v1/audit?${p.toString()}`);
  return rows.map((r) => ({
    id: r.audit_id ?? r.id ?? null,
    tsMs: nsToMs(r.ts_unix_nanos),
    actor: r.actor ?? null,
    ip: r.actor_ip ?? null, // backend field is actor_ip (audit.rs AuditRow)
    action: r.action ?? null,
    targetType: r.target_type ?? null,
    targetId: r.target_id ?? null,
    method: r.method ?? null,
    path: r.path ?? null,
    status: r.status ?? null,
    detail: r.detail ?? null,
  }));
}
