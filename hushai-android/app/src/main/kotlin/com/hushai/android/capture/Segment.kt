package com.hushai.android.capture

import okio.ByteString
import java.io.File

/**
 * A finalized, immutable segment ready to upload — the unit of the camera→backend
 * contract (§2). [file] is a self-contained MP4 (own moov+mdat) that decodes
 * standalone given [codec]/[container]/[codecInitData]. All identity/timing/
 * integrity fields map 1:1 onto hushai.v1.SegmentManifest.
 *
 * [segmentId] is minted ONCE when the segment is created and reused on every
 * retry (§5.2). The bytes and id never change once this object exists (§5.1).
 */
data class Segment(
    val segmentId: ByteString,        // 16-byte UUIDv7
    val streamId: String,             // "cam0-video" | "cam0-audio"
    val sequence: Long,               // monotonic per (streamId, sessionId), from 0
    val file: File,                   // the opaque body on disk
    val byteLen: Long,                // exact length of file
    val contentSha256: ByteString,    // 32-byte SHA-256 of file
    val mediaTypeValue: Int,          // hushai.v1.MediaType numeric (VIDEO=2, AUDIO=1)
    val codec: String,                // "h264" | "aac"
    val container: String,            // "mp4"
    val codecInitData: ByteString,    // SPS/PPS (video) or AudioSpecificConfig (audio)
    val captureStartUnixNanos: Long,  // raw device wall clock, uncorrected (§5.5)
    val monotonicStartNanos: Long,    // raw elapsedRealtimeNanos, uncorrected
    val durationNanos: Long,
    val gapBefore: Boolean,           // true iff captured data was dropped just before this (§5.8)
    // Per-segment session override. null = "use the live capture session" (the
    // identity the drain loop carries). Imported files set a FRESH session here so
    // their sequence space is independent of live capture (Workstream 3).
    val sessionId: ByteString? = null,
)
