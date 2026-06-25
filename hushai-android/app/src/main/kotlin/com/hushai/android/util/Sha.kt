package com.hushai.android.util

import okio.ByteString
import okio.ByteString.Companion.toByteString
import java.io.File
import java.security.MessageDigest

object Sha {
    /** SHA-256 over a file's exact bytes, streamed so a large body never loads whole. */
    fun sha256(file: File): ByteString {
        val md = MessageDigest.getInstance("SHA-256")
        file.inputStream().use { input ->
            val buf = ByteArray(64 * 1024)
            while (true) {
                val n = input.read(buf)
                if (n < 0) break
                md.update(buf, 0, n)
            }
        }
        return md.digest().toByteString()
    }

    fun sha256(data: ByteArray): ByteString =
        MessageDigest.getInstance("SHA-256").digest(data).toByteString()
}
