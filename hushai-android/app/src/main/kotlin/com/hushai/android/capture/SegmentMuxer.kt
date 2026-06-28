package com.hushai.android.capture

import android.media.MediaCodec
import android.media.MediaFormat
import android.media.MediaMuxer
import android.os.SystemClock
import com.hushai.android.util.Sha
import com.hushai.android.util.Uuid7
import okio.ByteString
import java.io.ByteArrayOutputStream
import java.io.File
import java.nio.ByteBuffer

/**
 * One fresh [MediaMuxer] per segment (contract §5.1, §5.4): a self-contained
 * small MP4 (own moov+mdat) that decodes standalone. Snapshots the segment's RAW
 * device clocks at construction (§5.5), accumulates samples, then on [finish]
 * computes the exact SHA-256 + byte length over the finalized file and mints the
 * immutable [Segment] (its segment_id minted ONCE here).
 */
class SegmentMuxer(
    private val file: File,
    private val streamId: String,
    private val mediaTypeValue: Int,
    private val codec: String,
    format: MediaFormat,
    private val codecInitData: ByteString,
    // null = live capture: snapshot the real device clocks at segment start (default,
    // byte-identical to the original behavior). A non-null clock (manual import) maps
    // the segment's first-sample PTS onto the file's ORIGINAL capture time.
    private val clock: SegmentClock? = null,
) {
    private val muxer = MediaMuxer(file.absolutePath, MediaMuxer.OutputFormat.MUXER_OUTPUT_MPEG_4)
    private val trackIndex = muxer.addTrack(format)
    private var started = false
    private var firstPtsUs = -1L
    private var lastPtsUs = 0L
    private var sampleCount = 0

    // Raw, uncorrected device clocks at segment start (§5.5). Never NTP-corrected.
    // Used only for the live path (clock == null).
    private val captureWallNanos = System.currentTimeMillis() * 1_000_000L
    private val monotonicNanos = SystemClock.elapsedRealtimeNanos()

    fun start() {
        muxer.start()
        started = true
    }

    fun write(buffer: ByteBuffer, info: MediaCodec.BufferInfo) {
        if (!started || info.size <= 0) return
        // MediaMuxer reads from the buffer's current position/limit — set them to the
        // sample region the BufferInfo describes (the canonical MediaCodec→MediaMuxer
        // pattern), or the written bytes can be wrong on some devices.
        buffer.position(info.offset)
        buffer.limit(info.offset + info.size)
        if (firstPtsUs < 0) firstPtsUs = info.presentationTimeUs
        lastPtsUs = info.presentationTimeUs
        muxer.writeSampleData(trackIndex, buffer, info)
        sampleCount++
    }

    fun hasSamples(): Boolean = sampleCount > 0

    /** Finalize into an immutable [Segment]. Caller assigns the monotonic [sequence]. */
    fun finish(sequence: Long): Segment {
        muxer.stop()
        muxer.release()
        val durationUs = (lastPtsUs - firstPtsUs).coerceAtLeast(0)
        val startPts = firstPtsUs.coerceAtLeast(0)
        return Segment(
            segmentId = Uuid7.bytes(),
            streamId = streamId,
            sequence = sequence,
            file = file,
            byteLen = file.length(),
            contentSha256 = Sha.sha256(file),
            mediaTypeValue = mediaTypeValue,
            codec = codec,
            container = "mp4",
            codecInitData = codecInitData,
            captureStartUnixNanos = clock?.captureWallNanos(startPts) ?: captureWallNanos,
            monotonicStartNanos = clock?.monotonicNanos() ?: monotonicNanos,
            durationNanos = durationUs * 1000L,
            gapBefore = false, // service stamps this from the retry buffer's gap flag
        )
    }

    fun abort() {
        runCatching { muxer.stop() }
        runCatching { muxer.release() }
        file.delete()
    }

    /**
     * Maps a segment's first-sample PTS onto a wall-clock capture time. The live path
     * uses the muxer's construction-time snapshot (clock == null); manual import injects
     * an [com.hushai.android.capture.imports.ImportClock] so recovered footage keeps its
     * ORIGINAL timeline position on the backend (which orders by capture time).
     */
    interface SegmentClock {
        fun captureWallNanos(firstPtsUs: Long): Long
        fun monotonicNanos(): Long
    }

    companion object {
        /** Concatenate csd-0 (+ csd-1 for H.264) as the standalone decoder init. */
        fun extractCsd(format: MediaFormat): ByteString {
            val out = ByteArrayOutputStream()
            for (key in listOf("csd-0", "csd-1")) {
                val bb = if (format.containsKey(key)) format.getByteBuffer(key) else null
                if (bb != null) {
                    val dup = bb.duplicate()
                    val arr = ByteArray(dup.remaining())
                    dup.get(arr)
                    out.write(arr)
                }
            }
            return ByteString.of(*out.toByteArray())
        }
    }
}
