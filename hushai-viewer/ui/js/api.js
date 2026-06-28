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

async function getJson(url) {
  const res = await fetch(url);
  if (redirectIfUnauth(res)) throw new Error("unauthorized");
  if (!res.ok) throw new Error(`${url} -> ${res.status}`);
  return res.json();
}

export async function getDevices() {
  const { devices } = await getJson("/api/devices");
  return devices.map((d) => ({
    id: d.device_id,
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

// ---- chat over recordings (proxied to hushai-rag at /v1/rag/*) ----------------

export async function getAgents() {
  return getJson("/v1/rag/agents");
}

export async function getSessionMessages(sessionId) {
  return getJson(`/v1/rag/chat/sessions/${encodeURIComponent(sessionId)}/messages`);
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

// URL for a speaker's 2s sample-audio clip — used directly as an <audio> src (the proxy
// adds the bearer), so no fetch-to-blob dance is needed.
export function sampleAudioUrl(id) {
  return `/v1/speakers/${encodeURIComponent(id)}/sample-audio`;
}

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

// POST a chat turn and stream the answer as Server-Sent Events. `onEvent({event, data})`
// is called per SSE frame: `session` {session_id, agent_id}, `sources` [Source...],
// `token` {delta}, `done` {message_id}, or `error` {message}. EventSource can't POST a
// body, so we read the streaming fetch response and parse SSE frames by hand.
export async function streamChat({ sessionId, agentId, message, filters }, onEvent) {
  const res = await fetch("/v1/rag/chat", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "text/event-stream" },
    body: JSON.stringify({
      session_id: sessionId ?? null,
      agent_id: agentId ?? null,
      message,
      filters: filters ?? null,
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
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    // SSE events are separated by a blank line. Tolerate both \n\n and \r\n\r\n.
    let sep;
    while ((sep = nextFrameBreak(buf)) !== null) {
      const frame = buf.slice(0, sep.idx);
      buf = buf.slice(sep.idx + sep.len);
      const ev = parseFrame(frame);
      if (ev) onEvent(ev);
    }
  }
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
