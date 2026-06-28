// SHA-256 of the EXACT bytes that go on the wire, returned as 32 raw bytes. The backend
// re-hashes the uploaded `body` and 422s on mismatch, so this must cover precisely the body
// (the moof+mdat fragment) — not init+body. `crypto.subtle` requires a secure context;
// http://127.0.0.1 (the viewer's default bind) qualifies as secure, as does HTTPS.

export async function sha256(data) {
  // Accept a Blob or a BufferSource (ArrayBuffer / typed-array view). digest() hashes a view's
  // byteOffset..byteLength exactly, which is what lets us hash a subarray of a larger buffer.
  const buf = data instanceof Blob ? await data.arrayBuffer() : data;
  const digest = await crypto.subtle.digest("SHA-256", buf);
  return new Uint8Array(digest);
}
