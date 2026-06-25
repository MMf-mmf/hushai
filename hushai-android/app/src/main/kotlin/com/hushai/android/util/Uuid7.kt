package com.hushai.android.util

import okio.ByteString
import okio.ByteString.Companion.toByteString
import java.security.SecureRandom

/**
 * RFC-9562 UUIDv7 as 16 raw bytes — 48-bit big-endian Unix-millis prefix + 74
 * random bits, version 7, variant 0b10. Byte-for-byte the same layout the
 * verified reference client emits (local_dev/feed_segments.py:uuid7_bytes), so
 * segment_id/session_id round-trip identically on both sides of the proto.
 */
object Uuid7 {
    private val rng = SecureRandom()

    fun bytes(unixMillis: Long): ByteString {
        val b = ByteArray(16)
        b[0] = (unixMillis ushr 40).toByte()
        b[1] = (unixMillis ushr 32).toByte()
        b[2] = (unixMillis ushr 24).toByte()
        b[3] = (unixMillis ushr 16).toByte()
        b[4] = (unixMillis ushr 8).toByte()
        b[5] = unixMillis.toByte()
        val rand = ByteArray(10)
        rng.nextBytes(rand)
        System.arraycopy(rand, 0, b, 6, 10)
        b[6] = ((b[6].toInt() and 0x0F) or 0x70).toByte() // version 7
        b[8] = ((b[8].toInt() and 0x3F) or 0x80).toByte() // variant 0b10
        return b.toByteString()
    }

    fun bytes(): ByteString = bytes(System.currentTimeMillis())
}
