package com.hushai.android.assistant

/**
 * Minimal RIFF/WAVE parser for the 16-bit PCM the backend's `/v1/tts` returns.
 *
 * Pure Kotlin (no Android APIs) so it's unit-testable on the JVM. Walks the chunk
 * list rather than assuming a fixed 44-byte header, so it tolerates extra chunks.
 */
data class WavPcm(val pcm16le: ByteArray, val sampleRate: Int, val channels: Int) {
    /** Bytes per sample frame (channels × 2 for 16-bit). */
    val frameBytes: Int get() = channels * 2

    // data class with a ByteArray: provide value-based equals/hashCode for tests.
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other !is WavPcm) return false
        return sampleRate == other.sampleRate &&
            channels == other.channels &&
            pcm16le.contentEquals(other.pcm16le)
    }

    override fun hashCode(): Int =
        (pcm16le.contentHashCode() * 31 + sampleRate) * 31 + channels

    companion object {
        /** Parse a 16-bit PCM WAV. Returns null if malformed or not 16-bit PCM. */
        fun parse(wav: ByteArray): WavPcm? {
            if (wav.size < 44) return null
            if (!tag(wav, 0, "RIFF") || !tag(wav, 8, "WAVE")) return null

            var sampleRate = 0
            var channels = 0
            var bits = 0
            var dataOff = -1
            var dataLen = 0

            var off = 12
            while (off + 8 <= wav.size) {
                val id = String(wav, off, 4, Charsets.US_ASCII)
                val size = le32(wav, off + 4)
                val body = off + 8
                if (size < 0) break
                when (id) {
                    "fmt " -> if (body + 16 <= wav.size) {
                        channels = le16(wav, body + 2)
                        sampleRate = le32(wav, body + 4)
                        bits = le16(wav, body + 14)
                    }
                    "data" -> {
                        // Record even if the declared length over-runs the buffer; the
                        // actual bytes are clamped below (tolerates a truncated tail).
                        dataOff = body
                        dataLen = size
                    }
                }
                if (dataOff >= 0) break
                // Advance to the next word-aligned chunk; stop if it runs past the buffer.
                val next = body.toLong() + size.toLong() + (size and 1).toLong()
                if (next > wav.size) break
                off = next.toInt()
            }

            if (dataOff < 0 || sampleRate <= 0 || bits != 16 || channels <= 0) return null
            val end = minOf(dataOff.toLong() + dataLen.toLong(), wav.size.toLong()).toInt()
            if (end <= dataOff) return null
            return WavPcm(wav.copyOfRange(dataOff, end), sampleRate, channels)
        }

        private fun tag(b: ByteArray, at: Int, s: String): Boolean {
            if (at + s.length > b.size) return false
            for (i in s.indices) if (b[at + i].toInt() != s[i].code) return false
            return true
        }

        private fun le16(b: ByteArray, at: Int): Int =
            (b[at].toInt() and 0xFF) or ((b[at + 1].toInt() and 0xFF) shl 8)

        private fun le32(b: ByteArray, at: Int): Int =
            (b[at].toInt() and 0xFF) or
                ((b[at + 1].toInt() and 0xFF) shl 8) or
                ((b[at + 2].toInt() and 0xFF) shl 16) or
                ((b[at + 3].toInt() and 0xFF) shl 24)
    }
}
