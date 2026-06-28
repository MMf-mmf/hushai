// Minimal protobuf-wire WRITER — just enough to encode hushai.v1.SegmentManifest, with no
// library and no build step. We mirror prost/proto3 semantics: default-valued scalars
// (0, false, "", empty bytes) are omitted; the fixed-width identity/hash fields are always
// present (they are never default). All uint64/fixed64 values go through BigInt because the
// nanosecond clocks exceed Number.MAX_SAFE_INTEGER and the backend rejects values > i64::MAX.
//
// Field numbers/types track hushai-backend/proto/hushai/v1/segment.proto. The reference
// encoder this matches byte-for-byte is local_dev/feed_segments.py.

class Writer {
  constructor() {
    this.parts = []; // Uint8Array chunks, concatenated by finish()
    this.len = 0;
  }
  _push(arr) {
    this.parts.push(arr);
    this.len += arr.length;
  }
  varint(value) {
    let v = BigInt(value);
    if (v < 0n) throw new Error("protobuf varint must be non-negative");
    const out = [];
    do {
      let byte = Number(v & 0x7fn);
      v >>= 7n;
      if (v > 0n) byte |= 0x80;
      out.push(byte);
    } while (v > 0n);
    this._push(Uint8Array.from(out));
    return this;
  }
  tag(field, wire) {
    return this.varint((BigInt(field) << 3n) | BigInt(wire));
  }
  // wire 0 — uint64 / enum. Omits the field when the value is 0 (proto3 default).
  uint64(field, value) {
    const v = BigInt(value);
    if (v === 0n) return this;
    return this.tag(field, 0).varint(v);
  }
  // wire 0 — bool. Omits when false (proto3 default).
  bool(field, value) {
    if (!value) return this;
    return this.tag(field, 0).varint(1n);
  }
  // wire 1 — fixed64, little-endian 8 bytes.
  fixed64(field, value) {
    let v = BigInt(value);
    if (v === 0n) return this;
    this.tag(field, 1);
    const bytes = new Uint8Array(8);
    for (let i = 0; i < 8; i++) {
      bytes[i] = Number(v & 0xffn);
      v >>= 8n;
    }
    this._push(bytes);
    return this;
  }
  // wire 2 — length-delimited bytes. Omits when empty (proto3 default).
  bytes(field, u8) {
    if (!u8 || u8.length === 0) return this;
    this.tag(field, 2).varint(u8.length);
    this._push(u8);
    return this;
  }
  // wire 2 — length-delimited UTF-8 string. Omits when empty.
  string(field, str) {
    if (str == null || str === "") return this;
    return this.bytes(field, new TextEncoder().encode(str));
  }
  // wire 2 — embedded message (already-encoded submessage bytes), length-prefixed.
  message(field, subBytes) {
    this.tag(field, 2).varint(subBytes.length);
    this._push(subBytes);
    return this;
  }
  finish() {
    const out = new Uint8Array(this.len);
    let off = 0;
    for (const p of this.parts) {
      out.set(p, off);
      off += p.length;
    }
    return out;
  }
}

// MediaType enum values from segment.proto.
export const MediaType = { AUDIO: 1, VIDEO: 2, MUXED: 3 };

// Encode a SegmentManifest. `m` carries already-prepared values (BigInt for the ns fields,
// Uint8Array for the byte fields). Returns the serialized manifest as a Uint8Array.
export function encodeSegmentManifest(m) {
  const w = new Writer();
  w.bytes(1, m.segmentId); //                bytes  segment_id
  w.string(2, m.deviceId); //                string device_id
  w.string(3, m.streamId); //                string stream_id
  w.bytes(4, m.sessionId); //                bytes  session_id
  w.uint64(5, m.sequence); //                uint64 sequence
  w.string(6, m.sourceKind); //              string source_kind
  w.uint64(7, m.mediaType); //               MediaType media_type (varint enum)
  w.string(8, m.codec); //                   string codec
  w.string(9, m.container); //               string container
  w.bytes(10, m.codecInitData); //           bytes  codec_init_data
  w.fixed64(11, m.captureStartUnixNanos); // fixed64 capture_start_unix_nanos
  w.uint64(12, m.monotonicStartNanos); //    uint64 monotonic_start_nanos
  w.uint64(13, m.durationNanos); //          uint64 duration_nanos
  w.bytes(14, m.contentSha256); //           bytes  content_sha256
  w.uint64(15, m.byteLen); //                uint64 byte_len
  w.bool(16, m.gapBefore); //                bool   gap_before
  // map<string,string> attrs = 17 — wire-encoded as a repeated message{ key=1; value=2 }.
  if (m.attrs) {
    for (const [k, v] of Object.entries(m.attrs)) {
      const e = new Writer();
      e.string(1, k);
      e.string(2, v);
      w.message(17, e.finish());
    }
  }
  return w.finish();
}
