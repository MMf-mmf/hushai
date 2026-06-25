#!/usr/bin/env bash
#
# export_capture.sh — reassemble the per-~2s segments the Android client uploaded
# back into a single playable file, so you can watch/listen to a capture.
#
# The backend stores each segment as a standalone, content-addressed MP4 blob
# (own moov+mdat). This script pulls one stream's blobs for one session, orders
# them by `sequence`, and concatenates them with ffmpeg (-c copy, no re-encode).
#
# Usage:
#   export_capture.sh [STREAM] [SESSION_PREFIX] [OUT_FILE]
#     STREAM         cam0-video (default) | cam0-audio
#     SESSION_PREFIX 8+ hex chars of session_id; default = the session with the
#                    MOST segments for that stream
#     OUT_FILE       output path; default local_dev/captures/<stream>-<session>.<ext>
#
# Env: DATABASE_URL (default postgres://localhost/hushai)
#
# Examples:
#   ./export_capture.sh                       # newest/biggest video session -> mp4
#   ./export_capture.sh cam0-audio            # its audio -> m4a
#   ./export_capture.sh cam0-video 019efbfe   # a specific session
set -euo pipefail

DB="${DATABASE_URL:-postgres://localhost/hushai}"
STREAM="${1:-cam0-video}"
PREFIX="${2:-}"
OUT="${3:-}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUTDIR="$SCRIPT_DIR/captures"
mkdir -p "$OUTDIR"

# Resolve the full session_id.
if [[ -n "$PREFIX" ]]; then
  SESSION="$(psql "$DB" -tAc "SELECT session_id FROM segments WHERE stream_id='$STREAM' AND session_id::text LIKE '$PREFIX%' GROUP BY session_id ORDER BY count(*) DESC LIMIT 1;")"
else
  SESSION="$(psql "$DB" -tAc "SELECT session_id FROM segments WHERE stream_id='$STREAM' GROUP BY session_id ORDER BY count(*) DESC LIMIT 1;")"
fi
[[ -n "$SESSION" ]] || { echo "no segments for stream '$STREAM'"; exit 1; }

case "$STREAM" in *audio*) EXT=m4a;; *) EXT=mp4;; esac
OUT="${OUT:-$OUTDIR/${STREAM}-${SESSION:0:8}.$EXT}"

# Ordered blob paths for this (stream, session) -> ffmpeg concat list (bash 3.2 safe).
LIST="$(mktemp)"
trap 'rm -f "$LIST"' EXIT
n=0
while IFS= read -r uri; do
  [[ -n "$uri" ]] || continue
  path="${uri#file://}"
  [[ -f "$path" ]] || { echo "missing blob: $path" >&2; continue; }
  printf "file '%s'\n" "$path" >> "$LIST"
  n=$((n + 1))
done < <(psql "$DB" -tAc "SELECT blob_uri FROM segments WHERE stream_id='$STREAM' AND session_id='$SESSION' ORDER BY sequence;")
echo "stream=$STREAM session=${SESSION:0:8} segments=$n -> $OUT"
[[ "$n" -gt 0 ]] || { echo "no blobs"; exit 1; }

# Concat demuxer offsets each segment's timestamps after the previous one, so
# stream-copy yields a continuous, playable file.
ffmpeg -y -loglevel error -f concat -safe 0 -i "$LIST" -c copy "$OUT"
echo "wrote $(du -h "$OUT" | cut -f1) -> $OUT"
ffprobe -v error -show_entries format=duration,format_name -show_entries stream=codec_name,codec_type,width,height -of default=noprint_wrappers=1 "$OUT"
echo "open with: open \"$OUT\""
