package com.hushai.android

import com.hushai.android.capture.RetryBuffer
import com.hushai.android.capture.Segment
import okio.ByteString
import okio.ByteString.Companion.toByteString
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.nio.file.Files

class RetryBufferTest {

    private fun seg(stream: String, seq: Long): Segment {
        val f = File.createTempFile("seg-$stream-$seq", ".mp4").apply { writeBytes(byteArrayOf(seq.toByte())) }
        return Segment(
            segmentId = ByteArray(16) { seq.toByte() }.toByteString(),
            streamId = stream,
            sequence = seq,
            file = f,
            byteLen = 1,
            contentSha256 = ByteArray(32).toByteString(),
            mediaTypeValue = 2,
            codec = "h264",
            container = "mp4",
            codecInitData = ByteString.EMPTY,
            captureStartUnixNanos = 0,
            monotonicStartNanos = 0,
            durationNanos = 0,
            gapBefore = false,
        )
    }

    @Test
    fun overflow_drops_oldest_and_flags_gap_on_that_stream() {
        val qdir = Files.createTempDirectory("q").toFile()
        val buf = RetryBuffer(maxEntries = 2, quarantineDir = qdir)

        val s0 = seg("cam0-video", 0)
        val s1 = seg("cam0-video", 1)
        val s2 = seg("cam0-video", 2) // forces drop of s0

        buf.offer(s0); buf.offer(s1)
        assertEquals(2, buf.size())
        buf.offer(s2)
        assertEquals(2, buf.size())
        assertFalse("oldest body deleted on overflow", s0.file.exists())

        // The next surviving segment on that stream must declare a gap, exactly once.
        assertTrue(buf.consumeGap("cam0-video"))
        assertFalse(buf.consumeGap("cam0-video"))
        assertFalse(buf.consumeGap("cam0-audio"))
    }

    @Test
    fun accepted_removal_deletes_body_and_quarantine_moves_it() {
        val qdir = Files.createTempDirectory("q").toFile()
        val buf = RetryBuffer(maxEntries = 8, quarantineDir = qdir)

        val good = seg("cam0-audio", 0)
        val bad = seg("cam0-audio", 1)
        buf.offer(good); buf.offer(bad)

        assertEquals(good, buf.peek())
        buf.remove(good)
        assertFalse(good.file.exists())

        buf.quarantine(bad)
        assertFalse(bad.file.exists())
        assertEquals(0, buf.size())
        assertTrue(qdir.listFiles()!!.isNotEmpty())
    }
}
