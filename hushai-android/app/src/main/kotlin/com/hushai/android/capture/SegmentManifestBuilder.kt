package com.hushai.android.capture

import com.hushai.android.config.DeviceIdentity
import hushai.v1.MediaType
import hushai.v1.SegmentManifest

/**
 * Serializes a finalized [Segment] into hushai.v1.SegmentManifest wire bytes —
 * the IDENTICAL message the backend decodes with prost (contract §8). `bytes`
 * fields stay 16/32 raw bytes; uint64/fixed64 surface as Long.
 *
 * `source_kind` is descriptive only; the backend never branches on it (§7).
 */
object SegmentManifestBuilder {
    const val SOURCE_KIND = "android_app"

    fun build(segment: Segment, identity: DeviceIdentity): ByteArray {
        val manifest = SegmentManifest(
            segment_id = segment.segmentId,
            device_id = identity.deviceId,
            stream_id = segment.streamId,
            session_id = identity.sessionId,
            sequence = segment.sequence,
            source_kind = SOURCE_KIND,
            media_type = MediaType.fromValue(segment.mediaTypeValue)
                ?: MediaType.MEDIA_TYPE_UNSPECIFIED,
            codec = segment.codec,
            container = segment.container,
            codec_init_data = segment.codecInitData,
            capture_start_unix_nanos = segment.captureStartUnixNanos,
            monotonic_start_nanos = segment.monotonicStartNanos,
            duration_nanos = segment.durationNanos,
            content_sha256 = segment.contentSha256,
            byte_len = segment.byteLen,
            gap_before = segment.gapBefore,
            // Per-segment content hints (Segment.attrs) ride along; the fixed client tag wins
            // on any key collision by coming last.
            attrs = segment.attrs + mapOf("client" to "hushai-android"),
        )
        return manifest.encode()
    }
}
