// Capture identity: a stable per-browser device_id, plus a per-run session with its own
// stream_id and monotonic sequence counter. Mirrors how the Android client and
// feed_segments.py name themselves to the source-agnostic backend.

import { uuidv7Bytes, toHex } from "./uuid.js";

const DEVICE_KEY = "hushai.capture.device_id";

// Stable across reloads so the NVR shows one "web-…" device per browser profile, not a new
// one each session. (device_id must be non-empty — the backend rejects empty ids.)
export function getDeviceId() {
  let id = localStorage.getItem(DEVICE_KEY);
  if (!id) {
    id = "web-" + toHex(uuidv7Bytes());
    localStorage.setItem(DEVICE_KEY, id);
  }
  return id;
}

// One capture run. Fresh session_id; sequence resets to 0; stream_id distinguishes the
// muxed vs audio-only mode (the backend processes both transcription and vision on MUXED).
export class Session {
  constructor(deviceId, audioOnly) {
    this.deviceId = deviceId;
    this.sessionId = uuidv7Bytes();
    const short = deviceId.replace(/^web-/, "").slice(0, 8);
    this.streamId = `web-${short}-${audioOnly ? "audio" : "muxed"}`;
    this.sequence = 0;
  }
  nextSequence() {
    return this.sequence++;
  }
}
