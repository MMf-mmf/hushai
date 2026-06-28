package com.hushai.android

import com.hushai.android.assistant.WavPcm
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import java.io.ByteArrayOutputStream

class WavPcmTest {

    /** Build a canonical 44-byte-header mono 16-bit WAV, like the backend returns. */
    private fun wav(sampleRate: Int, pcm: ByteArray): ByteArray {
        val out = ByteArrayOutputStream()
        fun str(s: String) = out.write(s.toByteArray(Charsets.US_ASCII))
        fun le32(v: Int) = out.write(byteArrayOf(
            (v and 0xFF).toByte(), ((v ushr 8) and 0xFF).toByte(),
            ((v ushr 16) and 0xFF).toByte(), ((v ushr 24) and 0xFF).toByte(),
        ))
        fun le16(v: Int) = out.write(byteArrayOf((v and 0xFF).toByte(), ((v ushr 8) and 0xFF).toByte()))
        val byteRate = sampleRate * 1 * 2
        str("RIFF"); le32(36 + pcm.size); str("WAVE")
        str("fmt "); le32(16); le16(1); le16(1); le32(sampleRate); le32(byteRate); le16(2); le16(16)
        str("data"); le32(pcm.size); out.write(pcm)
        return out.toByteArray()
    }

    @Test fun parsesCanonicalMono16k() {
        val pcm = byteArrayOf(1, 0, 2, 0, 3, 0, 4, 0) // 4 frames
        val parsed = WavPcm.parse(wav(24000, pcm))!!
        assertEquals(24000, parsed.sampleRate)
        assertEquals(1, parsed.channels)
        assertEquals(2, parsed.frameBytes)
        assertArrayEquals(pcm, parsed.pcm16le)
    }

    @Test fun rejectsTooShortOrNonRiff() {
        assertNull(WavPcm.parse(ByteArray(10)))
        val bogus = ByteArray(44) { 0 }
        assertNull(WavPcm.parse(bogus)) // no RIFF/WAVE tags
    }

    @Test fun toleratesTrailingTruncationByClampingData() {
        val pcm = byteArrayOf(1, 0, 2, 0, 3, 0, 4, 0)
        val full = wav(24000, pcm)
        // Claim 8 data bytes but drop the last 2 — parser must clamp, not overrun.
        val truncated = full.copyOfRange(0, full.size - 2)
        val parsed = WavPcm.parse(truncated)!!
        assertEquals(6, parsed.pcm16le.size)
    }
}
