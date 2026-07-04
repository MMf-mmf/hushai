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
import uuid
from pathlib import Path

import requests

import segment_pb2 as pb

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent

# Fixed namespace for hushai-eval's DETERMINISTIC segment/session ids. Used only when
# --segment-id-seed is given: the same (seed, seq) always maps to the same UUID, so a
# regression run re-POSTs byte-identical ids and the eval harness can recompute / read
# them to poll per-segment processing status. Never used on the normal (sidecar) path.
EVAL_NS = uuid.UUID("6f1a7b2c-0000-7000-8000-000000000000")


def seeded_uuid(seed: str, suffix: str) -> bytes:
    """Deterministic 16-byte UUIDv5 from (seed, suffix). Any 16 bytes is a valid pg uuid."""
    return uuid.uuid5(EVAL_NS, f"{seed}:{suffix}").bytes


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
    extra_attrs: dict | None = None,
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
    for k, v in (extra_attrs or {}).items():
        m.attrs[k] = v
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
    ap.add_argument("--capture-start-ns", type=int, default=None,
                    help="FIXED capture_start_unix_nanos for seq 0 (deterministic timestamps for "
                         "hushai-eval). Subsequent segments are base + seq*duration_nanos. When set, "
                         "monotonic_start_nanos is pinned to the same base. Default: live wall clock.")
    ap.add_argument("--segment-id-seed", default=None,
                    help="Derive segment_id + session_id deterministically from this seed "
                         "(UUIDv5). Bypasses the sidecar entirely so a re-run POSTs byte-identical "
                         "ids. Used by hushai-eval for reproducible regression runs.")
    ap.add_argument("--emit-ids", default=None,
                    help="Write a JSON {session_id, device_id, stream_id, segments:[{seq,segment_id}]} "
                         "to this path so the harness can poll per-segment status.")
    ap.add_argument("--body-first", action="store_true", help="send the body part before the manifest")
    ap.add_argument("--bad-token", action="store_true", help="use an invalid token (expect 401)")
    ap.add_argument("--corrupt-body", action="store_true",
                    help="flip a byte in the sent body but not the manifest (expect 422)")
    ap.add_argument("--conflict", action="store_true",
                    help="reuse stored segment_ids with DIFFERENT (self-consistent) bytes (expect 422)")
    ap.add_argument("--cacert", default=os.environ.get("HUSHAI_CACERT"),
                    help="CA bundle to verify the server's TLS cert (e.g. local_dev/certs/ca.crt). "
                         "Use this for an https:// --url with the self-signed LAN CA.")
    ap.add_argument("--insecure", action="store_true",
                    help="skip TLS certificate verification (testing only; prefer --cacert)")
    ap.add_argument("--attr", action="append", default=[], metavar="K=V",
                    help="extra manifest attrs key=value, repeatable — e.g. exercise the ingest "
                         "hint gate without a phone: --attr hint.v=1 --attr hint.audio_peak_rms=0.0001 "
                         "--attr hint.motion_score=0.2")
    args = ap.parse_args()

    extra_attrs: dict = {}
    for kv in args.attr:
        key, sep, value = kv.partition("=")
        if not sep or not key:
            print(f"--attr expects K=V, got {kv!r}", file=sys.stderr)
            return 2
        extra_attrs[key] = value

    # requests `verify`: False to skip, a CA path to pin, or True for the system store.
    verify = False if args.insecure else (args.cacert or True)

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

    # Deterministic mode (hushai-eval): seed-derived session/segment ids, no sidecar.
    deterministic = args.segment_id_seed is not None
    if deterministic:
        state = {"device_id": args.device, "session_id": None, "segment_ids": {}}
        session_id = (bytes.fromhex(args.session) if args.session
                      else seeded_uuid(args.segment_id_seed, "session"))
    else:
        state = load_sidecar(state_file, args.device)
        if args.session:
            state["session_id"] = args.session
        session_id = bytes.fromhex(state["session_id"])
    stream_id = f"{args.device}-muxed"

    token = "totally-invalid-token" if args.bad_token else args.token
    headers = {"Authorization": f"Bearer {token}"}

    duration_ns = args.seg_seconds * 1_000_000_000
    base_wall = args.capture_start_ns if args.capture_start_ns is not None else time.time_ns()
    base_mono = args.capture_start_ns if args.capture_start_ns is not None else time.monotonic_ns()

    print(f"[feed] device={args.device} session={state['session_id']} "
          f"segments={len(seg_paths)} url={args.url}")

    accepted = 0
    results: list[tuple[int, int]] = []  # (sequence, status)
    for seq, seg_path in enumerate(seg_paths):
        body = seg_path.read_bytes()

        # Stable segment_id across re-runs (idempotency). Deterministic mode derives it from
        # the seed; the sidecar path mints once per (device, seq) and persists it.
        key = str(seq)
        if deterministic:
            segment_id = seeded_uuid(args.segment_id_seed, key)
            state["segment_ids"][key] = hexb(segment_id)
        else:
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
            extra_attrs=extra_attrs,
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
            resp = requests.post(args.url, headers=headers, files=parts, timeout=60, verify=verify)
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

    # Persist sidecar (unless we deliberately corrupted/altered the run, or are deterministic).
    if not (deterministic or args.corrupt_body or args.conflict or args.bad_token):
        save_sidecar(state_file, state)

    # Emit the ids the harness needs to poll per-segment processing status.
    if args.emit_ids:
        Path(args.emit_ids).parent.mkdir(parents=True, exist_ok=True)
        Path(args.emit_ids).write_text(json.dumps({
            "session_id": hexb(session_id),
            "device_id": args.device,
            "stream_id": stream_id,
            "capture_start_unix_nanos": base_wall,
            "duration_nanos": duration_ns,
            "segments": [{"seq": int(k), "segment_id": v}
                         for k, v in sorted(state["segment_ids"].items(), key=lambda kv: int(kv[0]))],
        }, indent=2))

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
