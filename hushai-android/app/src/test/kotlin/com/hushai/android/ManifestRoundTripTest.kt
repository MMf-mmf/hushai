package com.hushai.android

import com.hushai.android.capture.Segment
import com.hushai.android.capture.SegmentManifestBuilder
import com.hushai.android.config.DeviceIdentity
import com.hushai.android.util.Uuid7
import hushai.v1.MediaType
import hushai.v1.SegmentManifest
import okio.ByteString
import okio.ByteString.Companion.decodeHex
import okio.ByteString.Companion.toByteString
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

class ManifestRoundTripTest {

    private fun sampleSegment(): Segment = Segment(
        segmentId = "ffeeddccbbaa99887766554433221100".decodeHex(),
        streamId = "cam0-video",
        sequence = 7,
        file = File("/dev/null"),
        byteLen = 123,
        contentSha256 = ByteArray(32) { it.toByte() }.toByteString(),
        mediaTypeValue = MediaType.VIDEO.value,
        codec = "h264",
        container = "mp4",
        codecInitData = "00000167abcd".decodeHex(),
        captureStartUnixNanos = 1_700_000_000_000_000_000L,
        monotonicStartNanos = 42L,
        durationNanos = 2_000_000_000L,
        gapBefore = false,
    )

    @Test
    fun roundTrip_preserves_fields_and_16byte_ids() {
        val identity = DeviceIdentity(
            deviceId = "cam0",
            sessionId = "00112233445566778899aabbccddeeff".decodeHex(),
        )
        val bytes = SegmentManifestBuilder.build(sampleSegment(), identity)
        val decoded = SegmentManifest.ADAPTER.decode(bytes)

        // 16-byte raw ids and 32-byte sha — the backend rejects any other length (400).
        assertEquals(16, decoded.segment_id.size)
        assertEquals(16, decoded.session_id.size)
        assertEquals(32, decoded.content_sha256.size)

        assertEquals("cam0", decoded.device_id)
        assertEquals("cam0-video", decoded.stream_id)
        assertEquals(7L, decoded.sequence)
        assertEquals(MediaType.VIDEO, decoded.media_type)
        assertEquals("h264", decoded.codec)
        assertEquals("mp4", decoded.container)
        assertEquals(1_700_000_000_000_000_000L, decoded.capture_start_unix_nanos)
        assertEquals(2_000_000_000L, decoded.duration_nanos)
        assertEquals("android_app", decoded.source_kind)
        assertEquals(false, decoded.gap_before)
    }

    @Test
    fun uuid7_is_16_bytes_with_version_and_variant() {
        val id: ByteString = Uuid7.bytes(0x0192abcd1234L)
        assertEquals(16, id.size)
        assertEquals(0x70, id[6].toInt() and 0xF0) // version 7 nibble
        assertEquals(0x80, id[8].toInt() and 0xC0) // variant 0b10
        // 48-bit big-endian millis prefix preserved.
        assertEquals(0x01.toByte(), id[0])
        assertEquals(0x92.toByte(), id[1])
    }

    @Test
    fun content_hint_attrs_survive_roundtrip_alongside_client_tag() {
        val identity = DeviceIdentity("cam0", "00112233445566778899aabbccddeeff".decodeHex())
        val hinted = sampleSegment().copy(
            attrs = mapOf(
                "hint.v" to "1",
                "hint.audio_rms" to "0.002310",
                "hint.audio_peak_rms" to "0.004700",
            ),
        )
        val decoded = SegmentManifest.ADAPTER.decode(SegmentManifestBuilder.build(hinted, identity))

        // Hints ride along AND the fixed client tag is preserved (it wins any collision).
        assertEquals("hushai-android", decoded.attrs["client"])
        assertEquals("1", decoded.attrs["hint.v"])
        assertEquals("0.002310", decoded.attrs["hint.audio_rms"])
        assertEquals("0.004700", decoded.attrs["hint.audio_peak_rms"])

        // A hint-less segment still carries exactly the legacy attrs (fail-open on the server).
        val plain = SegmentManifest.ADAPTER.decode(
            SegmentManifestBuilder.build(sampleSegment(), identity)
        )
        assertEquals(mapOf("client" to "hushai-android"), plain.attrs)
    }

    @Test
    fun device_and_stream_ids_must_be_nonempty() {
        // Mirror of the backend's EmptyField(400) guard — keep these populated.
        val identity = DeviceIdentity("cam0", "00112233445566778899aabbccddeeff".decodeHex())
        val decoded = SegmentManifest.ADAPTER.decode(
            SegmentManifestBuilder.build(sampleSegment(), identity)
        )
        assertTrue(decoded.device_id.isNotEmpty())
        assertTrue(decoded.stream_id.isNotEmpty())
    }
}
