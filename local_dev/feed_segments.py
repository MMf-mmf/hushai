#!/usr/bin/env python3
"""
feed_segments.py — replay a video file into the Hushai backend as conforming
MUXED fMP4 segments, exercising the camera→backend contract v0.1.0.

Pipeline:
  1. ffmpeg splits the input into a shared fMP4 init segment (both tracks muxed)
     plus N independently-decodable ~2s media segments, each starting on a
     keyframe (HLS fMP4 muxer). The init bytes become `codec_init_data`.
  2. For each media segment we build a hushai.v1.SegmentManifest: a UUIDv7
     `segment_id` minted ONCE (persisted to a sidecar so re-runs are idempotent),
     a per-run `session_id`, an incrementing `sequence`, dual device clocks,
     SHA-256 + byte_len over the EXACT body bytes, and source_kind="file_replay".
  3. POST multipart/form-data (`manifest` + `body`) with a Bearer token.

Examples:
  python feed_segments.py --device cam-A
  python feed_segments.py --device cam-A            # re-run -> all idempotent 200s
  python feed_segments.py --device cam-A --conflict # reused ids, different bytes -> 422
  python feed_segments.py --device cam-A --corrupt-body   # body != manifest sha -> 422
  python feed_segments.py --device cam-A --bad-token      # -> 401
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys
import time
from pathlib import Path

import requests

import segment_pb2 as pb

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent


def uuid7_bytes() -> bytes:
    """16-byte RFC-9562 UUIDv7 (Python 3.13 has no uuid.uuid7())."""
    ts_ms = time.time_ns() // 1_000_000
    b = bytearray(ts_ms.to_bytes(6, "big") + os.urandom(10))
    b[6] = (b[6] & 0x0F) | 0x70  # version 7
    b[8] = (b[8] & 0x3F) | 0x80  # variant 10
    return bytes(b)


def hexb(b: bytes) -> str:
    return b.hex()


def split_video(video: Path, work: Path, seg_seconds: int) -> tuple[Path, list[Path]]:
    """Run ffmpeg HLS fMP4 to produce init.mp4 + seg_*.m4s. Returns (init, [segments])."""
    work.mkdir(parents=True, exist_ok=True)
    init = work / "init.mp4"
    existing = sorted(work.glob("seg_*.m4s"))
    if init.exists() and existing:
        return init, existing

    # Clear any stale output so segment numbering is deterministic.
    for f in work.glob("seg_*.m4s"):
        f.unlink()
    if init.exists():
        init.unlink()

    cmd = [
        "ffmpeg", "-y", "-i", str(video),
        "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p",
        "-c:a", "aac",
        # Force a keyframe at every segment boundary so each segment decodes standalone.
        "-force_key_frames", f"expr:gte(t,n_forced*{seg_seconds})",
        "-hls_time", str(seg_seconds),
        "-hls_segment_type", "fmp4",
        "-hls_fmp4_init_filename", "init.mp4",
        "-hls_segment_filename", "seg_%05d.m4s",
        "-hls_list_size", "0",
        "-hls_playlist_type", "vod",
        "index.m3u8",
    ]
    print(f"[ffmpeg] splitting {video.name} into ~{seg_seconds}s MUXED fMP4 segments ...")
    proc = subprocess.run(cmd, cwd=work, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr[-2000:])
        raise SystemExit(f"ffmpeg failed (exit {proc.returncode})")

    segs = sorted(work.glob("seg_*.m4s"))
    if not init.exists() or not segs:
        raise SystemExit("ffmpeg produced no init/segments")
    return init, segs


def load_sidecar(path: Path, device: str) -> dict:
    if path.exists():
        return json.loads(path.read_text())
    return {"device_id": device, "session_id": hexb(uuid7_bytes()), "segment_ids": {}}


def save_sidecar(path: Path, state: dict) -> None:
    path.write_text(json.dumps(state, indent=2))


def build_manifest(
    *,
    segment_id: bytes,
    session_id: bytes,
    device: str,
    stream_id: str,
    sequence: int,
    body: bytes,
    init_bytes: bytes,
    capture_ns: int,
    monotonic_ns: int,
    duration_ns: int,
) -> bytes:
    m = pb.SegmentManifest()
    m.segment_id = segment_id
    m.device_id = device
    m.stream_id = stream_id
    m.session_id = session_id
    m.sequence = sequence
    m.source_kind = "file_replay"
    m.media_type = pb.MUXED
    m.codec = "h264+aac"
    m.container = "fmp4"
    m.codec_init_data = init_bytes
    m.capture_start_unix_nanos = capture_ns
    m.monotonic_start_nanos = monotonic_ns
    m.duration_nanos = duration_ns
    m.content_sha256 = hashlib.sha256(body).digest()
    m.byte_len = len(body)
    m.gap_before = False
    m.attrs["feeder"] = "feed_segments.py"
    return m.SerializeToString()


def main() -> int:
    ap = argparse.ArgumentParser(description="Feed a video to the Hushai backend as segments.")
    ap.add_argument("--device", default="cam-A", help="device_id")
    ap.add_argument("--session", default=None, help="hex(16-byte) session_id override")
    ap.add_argument("--url", default="http://localhost:8080/v1/segments")
    ap.add_argument("--token", default=os.environ.get("DEVICE_TOKEN", "dev-secret-token"))
    ap.add_argument("--video", default=str(REPO_ROOT / "IMG_7256.mp4"))
    ap.add_argument("--seg-seconds", type=int, default=2)
    ap.add_argument("--work-dir", default=None, help="ffmpeg output dir (default: scratch per video)")
    ap.add_argument("--state-file", default=None, help="sidecar JSON for stable segment_ids")
    ap.add_argument("--limit", type=int, default=None, help="only send the first N segments")
    ap.add_argument("--body-first", action="store_true", help="send the body part before the manifest")
    ap.add_argument("--bad-token", action="store_true", help="use an invalid token (expect 401)")
    ap.add_argument("--corrupt-body", action="store_true",
                    help="flip a byte in the sent body but not the manifest (expect 422)")
    ap.add_argument("--conflict", action="store_true",
                    help="reuse stored segment_ids with DIFFERENT (self-consistent) bytes (expect 422)")
    args = ap.parse_args()

    video = Path(args.video).resolve()
    if not video.exists():
        raise SystemExit(f"video not found: {video}")

    work = Path(args.work_dir).resolve() if args.work_dir else (
        SCRIPT_DIR / ".feed_work" / video.stem
    )
    state_file = Path(args.state_file).resolve() if args.state_file else (
        SCRIPT_DIR / ".feed_work" / f"sidecar-{args.device}.json"
    )
    state_file.parent.mkdir(parents=True, exist_ok=True)

    init_path, seg_paths = split_video(video, work, args.seg_seconds)
    init_bytes = init_path.read_bytes()
    if args.limit is not None:
        seg_paths = seg_paths[: args.limit]

    state = load_sidecar(state_file, args.device)
    if args.session:
        state["session_id"] = args.session
    session_id = bytes.fromhex(state["session_id"])
    stream_id = f"{args.device}-muxed"

    token = "totally-invalid-token" if args.bad_token else args.token
    headers = {"Authorization": f"Bearer {token}"}

    duration_ns = args.seg_seconds * 1_000_000_000
    base_wall = time.time_ns()
    base_mono = time.monotonic_ns()

    print(f"[feed] device={args.device} session={state['session_id']} "
          f"segments={len(seg_paths)} url={args.url}")

    accepted = 0
    results: list[tuple[int, int]] = []  # (sequence, status)
    for seq, seg_path in enumerate(seg_paths):
        body = seg_path.read_bytes()

        # Stable segment_id across re-runs (idempotency); minted once per (device, seq).
        key = str(seq)
        if key not in state["segment_ids"]:
            state["segment_ids"][key] = hexb(uuid7_bytes())
        segment_id = bytes.fromhex(state["segment_ids"][key])

        if args.conflict:
            # Same segment_id, DIFFERENT but self-consistent bytes -> server must 422.
            body = body + b"\x00conflict"

        manifest = build_manifest(
            segment_id=segment_id,
            session_id=session_id,
            device=args.device,
            stream_id=stream_id,
            sequence=seq,
            body=body,
            init_bytes=init_bytes,
            capture_ns=base_wall + seq * duration_ns,
            monotonic_ns=base_mono + seq * duration_ns,
            duration_ns=duration_ns,
        )

        sent_body = body
        if args.corrupt_body:
            # Corrupt the bytes on the wire only; manifest still describes the clean body.
            sent_body = bytearray(body)
            sent_body[0] ^= 0xFF
            sent_body = bytes(sent_body)

        parts = [
            ("manifest", ("manifest", manifest, "application/x-protobuf")),
            ("body", ("body", sent_body, "application/octet-stream")),
        ]
        if args.body_first:
            parts.reverse()

        try:
            resp = requests.post(args.url, headers=headers, files=parts, timeout=60)
            status = resp.status_code
        except requests.RequestException as e:
            print(f"  seq={seq:>3} ERROR {e}")
            results.append((seq, -1))
            continue

        results.append((seq, status))
        if status == 200:
            accepted += 1
        sha = hashlib.sha256(body).hexdigest()[:12]
        print(f"  seq={seq:>3} sha={sha} bytes={len(body):>7} -> {status}")

    # Persist sidecar (unless we deliberately corrupted/altered the run).
    if not (args.corrupt_body or args.conflict or args.bad_token):
        save_sidecar(state_file, state)

    print(f"\n[summary] {accepted}/{len(seg_paths)} segments accepted (200)")
    distinct = sorted({s for _, s in results})
    print(f"[summary] status codes seen: {distinct}")
    print(f"[summary] sidecar: {state_file}")

    # Exit non-zero if the happy path didn't fully succeed.
    if not (args.corrupt_body or args.conflict or args.bad_token):
        return 0 if accepted == len(seg_paths) else 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
