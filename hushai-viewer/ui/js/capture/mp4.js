// Fragmented-MP4 box parsing — just enough to split a MediaRecorder mp4 blob into its init
// segment (ftyp+moov: everything before the first media fragment) and the media fragment(s)
// (moof+mdat...). The backend's "fmp4" path prepends codec_init_data (= the init segment)
// before handing the body to ffmpeg, so we ship the init once per manifest and the
// moof+mdat as the upload body — exactly the shape local_dev/feed_segments.py produces.

const latin1 = new TextDecoder("latin1");

// Yield {type, start, end} for each top-level box in `view` (a DataView). Boxes are
// [u32 size][4-char type][payload]; size==1 means a 64-bit largesize follows the type,
// size==0 means "to end of buffer". Bails on truncated/garbage input rather than guessing.
function* boxes(view) {
  let off = 0;
  while (off + 8 <= view.byteLength) {
    let size = view.getUint32(off); // big-endian
    const type = latin1.decode(
      new Uint8Array(view.buffer, view.byteOffset + off + 4, 4),
    );
    let header = 8;
    if (size === 1) {
      if (off + 16 > view.byteLength) break;
      const hi = view.getUint32(off + 8);
      const lo = view.getUint32(off + 12);
      size = hi * 2 ** 32 + lo;
      header = 16;
    } else if (size === 0) {
      size = view.byteLength - off;
    }
    if (size < header || off + size > view.byteLength) break;
    yield { type, start: off, end: off + size };
    off += size;
  }
}

// Offset where the first media fragment begins (the first `moof`, or a `styp` immediately
// preceding it). Returns -1 if the buffer is init-only (no fragment yet). 0 means the buffer
// starts with a fragment and carries no init prefix.
export function firstFragmentOffset(u8) {
  const view = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  let stypStart = -1;
  for (const box of boxes(view)) {
    if (box.type === "styp") {
      if (stypStart < 0) stypStart = box.start;
    } else if (box.type === "moof") {
      return stypStart >= 0 ? stypStart : box.start;
    } else {
      stypStart = -1; // any non-styp box clears a pending styp prefix
    }
  }
  return -1;
}

// Split a self-contained fMP4 (ftyp+moov+moof+mdat...) into { init, body } byte views.
// Returns null when there is no init prefix or no fragment to send.
export function splitFragment(u8) {
  const off = firstFragmentOffset(u8);
  if (off <= 0) return null;
  return { init: u8.subarray(0, off), body: u8.subarray(off) };
}
