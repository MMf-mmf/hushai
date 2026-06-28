// UUIDv7 as raw 16 bytes — wire-identical to feed_segments.uuid7_bytes() and the Android
// Uuid7 helper, so segment/session ids round-trip across every capture client. The 48-bit
// unix-ms timestamp prefix is what makes ids time-ordered (the backend relies on this for
// idempotency/ordering), so DO NOT substitute crypto.randomUUID() — that is a v4 (random) id.

export function uuidv7Bytes(ms = Date.now()) {
  const b = new Uint8Array(16);
  // 48-bit big-endian unix-millisecond timestamp in bytes 0..5.
  const t = BigInt(ms);
  b[0] = Number((t >> 40n) & 0xffn);
  b[1] = Number((t >> 32n) & 0xffn);
  b[2] = Number((t >> 24n) & 0xffn);
  b[3] = Number((t >> 16n) & 0xffn);
  b[4] = Number((t >> 8n) & 0xffn);
  b[5] = Number(t & 0xffn);
  crypto.getRandomValues(b.subarray(6)); // 10 random bytes
  b[6] = (b[6] & 0x0f) | 0x70; // version 7
  b[8] = (b[8] & 0x3f) | 0x80; // variant 0b10
  return b;
}

export function toHex(bytes) {
  let s = "";
  for (const x of bytes) s += x.toString(16).padStart(2, "0");
  return s;
}
