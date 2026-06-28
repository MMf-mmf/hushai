// Turn one raw recorded fragment + session identity into a finalized, uploadable segment:
// the serialized SegmentManifest protobuf + the body bytes. This is the JS analog of the
// Android SegmentManifestBuilder / feed_segments.build_manifest. We declare container="fmp4",
// codec="h264+aac" (or "aac" for audio-only), media_type=MUXED (or AUDIO), and carry the
// fMP4 init segment as codec_init_data on every manifest.

import { encodeSegmentManifest, MediaType } from "./protobuf.js";
import { sha256 } from "./crypto.js";
import { uuidv7Bytes } from "./uuid.js";

const NS_PER_MS = 1_000_000n;

export async function buildSegment({ raw, session, audioOnly, gapBefore, attrs }) {
  const body = raw.body; // Uint8Array (the moof+mdat fragment that goes on the wire)
  const contentSha256 = await sha256(body);
  // Mint the segment_id ONCE, here; the queue reuses these exact manifest bytes on every
  // retry, so re-POSTs are idempotent (the backend returns 200 for a duplicate segment_id).
  const segmentId = uuidv7Bytes(raw.captureStartUnixMs);
  const sequence = session.nextSequence();

  const manifestBytes = encodeSegmentManifest({
    segmentId,
    deviceId: session.deviceId,
    streamId: session.streamId,
    sessionId: session.sessionId,
    sequence,
    sourceKind: "web_browser",
    mediaType: audioOnly ? MediaType.AUDIO : MediaType.MUXED,
    codec: audioOnly ? "aac" : "h264+aac",
    container: "fmp4",
    codecInitData: raw.init,
    captureStartUnixNanos: BigInt(Math.round(raw.captureStartUnixMs)) * NS_PER_MS,
    monotonicStartNanos: BigInt(Math.max(0, Math.round(raw.monotonicStartMs))) * NS_PER_MS,
    durationNanos: BigInt(Math.max(0, Math.round(raw.durationMs))) * NS_PER_MS,
    contentSha256,
    byteLen: body.length,
    gapBefore: !!gapBefore,
    attrs: { client: "hushai-web", ...(attrs || {}) },
  });

  return {
    segmentId,
    sequence,
    streamId: session.streamId,
    manifestBytes,
    body,
    byteLen: body.length,
    resends: 0,
  };
}
