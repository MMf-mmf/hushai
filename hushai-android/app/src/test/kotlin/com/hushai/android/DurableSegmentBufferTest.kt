package com.hushai.android

import com.hushai.android.capture.DurableSegmentBuffer
import com.hushai.android.capture.Segment
import com.hushai.android.config.DeviceIdentity
import com.hushai.android.util.Sha
import com.hushai.android.util.Uuid7
import okio.ByteString
import okio.ByteString.Companion.decodeHex
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.nio.file.Files

class DurableSegmentBufferTest {

    private val identity = DeviceIdentity(
        deviceId = "test-device",
        sessionId = "00112233445566778899aabbccddeeff".decodeHex(),
    )

    private fun root(): File = Files.createTempDirectory("dsb").toFile()

    private fun newBuffer(root: File, maxBytes: Long, verifySha: Boolean = false) =
        DurableSegmentBuffer(
            segmentDir = File(root, "segments"),
            quarantineDir = File(root, "quarantine"),
            maxBytes = maxBytes,
            minFreeBytesFloor = 0L, // disable the device-free floor in tests
            identity = identity,
            verifyShaOnRecover = verifySha,
        )

    private fun seg(root: File, stream: String, seq: Long, size: Int, captureNanos: Long = seq): Segment {
        val incoming = File(root, "incoming").apply { mkdirs() }
        val f = File(incoming, "scratch-$stream-$seq.mp4")
        f.writeBytes(ByteArray(size) { seq.toByte() })
        return Segment(
            segmentId = Uuid7.bytes(),
            streamId = stream,
            sequence = seq,
            file = f,
            byteLen = size.toLong(),
            contentSha256 = Sha.sha256(f),
            mediaTypeValue = 2,
            codec = "h264",
            container = "mp4",
            codecInitData = ByteString.EMPTY,
            captureStartUnixNanos = captureNanos,
            monotonicStartNanos = 0,
            durationNanos = 0,
            gapBefore = false,
        )
    }

    @Test
    fun offer_persists_body_and_sidecar() {
        val root = root()
        val buf = newBuffer(root, maxBytes = 1_000_000)
        buf.offer(seg(root, "cam0-video", 0, 100))
        assertEquals(1, buf.size())
        assertEquals(100L, buf.byteSize())
        val files = File(root, "segments").listFiles()!!.filter { it.isFile }
        assertEquals(1, files.count { it.name.endsWith(".mp4") })
        assertEquals(1, files.count { it.name.endsWith(".manifest") })
    }

    @Test
    fun overflow_by_bytes_drops_oldest_and_flags_gap() {
        val root = root()
        // Cap fits two 100-byte segments but not three.
        val buf = newBuffer(root, maxBytes = 250)
        buf.offer(seg(root, "cam0-video", 0, 100))
        buf.offer(seg(root, "cam0-video", 1, 100))
        assertEquals(2, buf.size())
        buf.offer(seg(root, "cam0-video", 2, 100)) // evicts seq 0
        assertEquals(2, buf.size())
        assertEquals(200L, buf.byteSize())

        // The next surviving segment on that stream must declare a gap, exactly once.
        assertTrue(buf.consumeGap("cam0-video"))
        assertFalse(buf.consumeGap("cam0-video"))
    }

    @Test
    fun accepted_remove_and_quarantine_delete_both_files() {
        val root = root()
        val buf = newBuffer(root, maxBytes = 1_000_000)
        buf.offer(seg(root, "cam0-audio", 0, 50))
        buf.offer(seg(root, "cam0-audio", 1, 50))

        val head = buf.peek()!!
        buf.remove(head)
        assertFalse(head.body.exists())
        assertFalse(head.sidecar.exists())
        assertEquals(1, buf.size())

        val bad = buf.peek()!!
        buf.quarantine(bad)
        assertFalse(bad.body.exists())
        assertFalse(bad.sidecar.exists())
        assertEquals(0, buf.size())
        assertTrue(File(root, "quarantine").listFiles()!!.isNotEmpty())
    }

    @Test
    fun recover_rebuilds_queue_ordered_by_capture_time() {
        val root = root()
        val buf = newBuffer(root, maxBytes = 1_000_000)
        // Offer out of capture-time order.
        buf.offer(seg(root, "cam0-video", 2, 30, captureNanos = 300))
        buf.offer(seg(root, "cam0-video", 0, 30, captureNanos = 100))
        buf.offer(seg(root, "cam0-video", 1, 30, captureNanos = 200))

        // A fresh buffer over the same dir recovers all three, oldest-first.
        val recovered = newBuffer(root, maxBytes = 1_000_000)
        assertEquals(3, recovered.recover())
        assertEquals(3, recovered.size())
        assertEquals(100L, recovered.oldestUnixNanos())
        assertEquals(90L, recovered.byteSize())
        assertEquals(100L, recovered.peek()!!.captureStartUnixNanos)
    }

    @Test
    fun recover_reaps_orphan_body_dangling_sidecar_and_tmp() {
        val root = root()
        val buf = newBuffer(root, maxBytes = 1_000_000)
        buf.offer(seg(root, "cam0-video", 0, 40)) // one valid pair

        val segDir = File(root, "segments")
        File(segDir, "orphan.mp4").writeBytes(ByteArray(10))           // body, no sidecar
        File(segDir, "dangling.manifest").writeBytes(ByteArray(10))    // sidecar, no body
        File(segDir, "halfwrite.manifest.tmp").writeBytes(ByteArray(5)) // incomplete write

        val recovered = newBuffer(root, maxBytes = 1_000_000)
        assertEquals(1, recovered.recover())
        val left = segDir.listFiles()!!.filter { it.isFile }.map { it.name }
        assertFalse(left.contains("orphan.mp4"))
        assertFalse(left.contains("dangling.manifest"))
        assertFalse(left.contains("halfwrite.manifest.tmp"))
    }

    @Test
    fun recover_drops_truncated_body_and_flags_gap() {
        val root = root()
        val buf = newBuffer(root, maxBytes = 1_000_000)
        val s = seg(root, "cam0-audio", 0, 80)
        buf.offer(s)

        // Truncate the durable body so its length no longer matches the manifest.
        val body = File(root, "segments").listFiles()!!.first { it.name.endsWith(".mp4") }
        body.writeBytes(ByteArray(3))

        val recovered = newBuffer(root, maxBytes = 1_000_000)
        assertEquals(0, recovered.recover())
        assertEquals(0, recovered.size())
        assertTrue("truncated body should flag a gap on its stream", recovered.consumeGap("cam0-audio"))
    }

    @Test
    fun recovered_entry_carries_manifest_bytes() {
        val root = root()
        val buf = newBuffer(root, maxBytes = 1_000_000)
        buf.offer(seg(root, "cam0-video", 0, 60))
        val recovered = newBuffer(root, maxBytes = 1_000_000)
        recovered.recover()
        val head = recovered.peek()
        assertNotNull(head)
        assertTrue(head!!.manifestBytes.isNotEmpty())
        assertEquals("cam0-video", head.streamId)
    }
}
