package com.hushai.android.capture.imports

import android.content.Context
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaExtractor
import android.media.MediaFormat
import android.os.Bundle
import com.hushai.android.capture.Segment
import com.hushai.android.capture.SegmentMuxer
import com.hushai.android.util.HushaiLog
import com.hushai.android.util.Uuid7
import hushai.v1.MediaType
import okio.ByteString
import java.io.File
import java.nio.ByteBuffer
import kotlin.math.ceil

/**
 * Converts ONE picked audio/video file into the SAME 2-second MP4 segments the live
 * encoders produce, then hands each to [onSegment] (which routes it into the durable
 * buffer + uploader). Always decode → re-encode through fresh AAC / H.264 codecs:
 * this normalizes the whole source-codec matrix and RE-ESTABLISHES the 2 s
 * keyframe-aligned GOP that arbitrary files lack — exactly what the backend workers
 * expect. Segments keep the file's ORIGINAL capture time via [ImportClock], carry a
 * fresh per-file session, and the first of each stream declares `gap_before`.
 *
 * Runs on a single import worker thread (see ImportManager); decode + encode are
 * driven synchronously here. [backpressure] is invoked between segments so the import
 * can't outrun the uploader and evict live footage from the shared cap.
 */
class ImportPipeline(
    private val context: Context,
    private val request: ImportRequest,
    private val segmentDir: File,
    private val segmentDurationUs: Long,
    private val onSegment: (Segment) -> Unit,
    private val backpressure: () -> Unit,
    private val isCancelled: () -> Boolean,
    private val onProgress: (done: Int, total: Int) -> Unit,
) {
    private val fileSessionId: ByteString = Uuid7.bytes()
    private var fileCounter = 0
    private var emitted = 0
    private var totalEstimate = 0

    fun run(): ImportResult {
        val probe = MediaProbe.probe(context, request.uri).getOrElse {
            return ImportResult.Failed(it.message ?: "unreadable file")
        }
        val clock = ImportClock(probe.captureUnixNanos)
        val perTrack = ceil(probe.durationUs.toDouble() / segmentDurationUs).toInt().coerceAtLeast(1)
        totalEstimate = perTrack * ((if (probe.hasAudio) 1 else 0) + (if (probe.hasVideo) 1 else 0))
        onProgress(0, totalEstimate)

        try {
            if (probe.hasAudio) {
                val r = transcodeAudio(clock)
                if (r != null) return r
            }
            if (probe.hasVideo) {
                val r = transcodeVideo(clock)
                if (r != null) return r
            }
        } catch (e: Exception) {
            HushaiLog.error("import failed for ${request.displayName}", e)
            return ImportResult.Failed(e.message ?: "import error")
        }
        return if (isCancelled()) ImportResult.Cancelled else ImportResult.Completed
    }

    // --- AUDIO: decode -> PCM -> AAC encode -> 2s segments ---

    private fun transcodeAudio(clock: SegmentMuxer.SegmentClock): ImportResult? {
        val extractor = MediaExtractor()
        var decoder: MediaCodec? = null
        var encoder: MediaCodec? = null
        try {
            extractor.setDataSource(context, request.uri, null)
            val track = selectTrack(extractor, "audio/") ?: return null
            extractor.selectTrack(track)
            val srcFormat = extractor.getTrackFormat(track)
            val srcMime = srcFormat.getString(MediaFormat.KEY_MIME)
                ?: return ImportResult.Failed("audio track has no codec type")

            val sampleRate = srcFormat.optInt(MediaFormat.KEY_SAMPLE_RATE, 44_100)
            val channels = srcFormat.optInt(MediaFormat.KEY_CHANNEL_COUNT, 1).coerceIn(1, 2)

            decoder = MediaCodec.createDecoderByType(srcMime).apply {
                configure(srcFormat, null, null, 0)
                start()
            }
            val encFormat = MediaFormat.createAudioFormat(AUDIO_MIME, sampleRate, channels).apply {
                setInteger(MediaFormat.KEY_AAC_PROFILE, MediaCodecInfo.CodecProfileLevel.AACObjectLC)
                setInteger(MediaFormat.KEY_BIT_RATE, AUDIO_BITRATE)
                setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, AUDIO_MAX_INPUT)
            }
            encoder = MediaCodec.createEncoderByType(AUDIO_MIME).apply {
                configure(encFormat, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
                start()
            }

            val streamId = "${IMPORT_PREFIX}${request.streamIndex}-audio"
            return pumpTranscode(
                streamId = streamId,
                mediaTypeValue = MediaType.AUDIO.value,
                codecName = "aac",
                clock = clock,
                extractor = extractor,
                decoder = decoder,
                encoder = encoder,
                renderToSurface = false,
            )
        } finally {
            runCatching { decoder?.stop() }; runCatching { decoder?.release() }
            runCatching { encoder?.stop() }; runCatching { encoder?.release() }
            runCatching { extractor.release() }
        }
    }

    // --- VIDEO: decode -> encoder input Surface -> H.264 encode -> 2s segments ---

    private fun transcodeVideo(clock: SegmentMuxer.SegmentClock): ImportResult? {
        val extractor = MediaExtractor()
        var decoder: MediaCodec? = null
        var encoder: MediaCodec? = null
        try {
            extractor.setDataSource(context, request.uri, null)
            val track = selectTrack(extractor, "video/") ?: return null
            extractor.selectTrack(track)
            val srcFormat = extractor.getTrackFormat(track)
            val srcMime = srcFormat.getString(MediaFormat.KEY_MIME)
                ?: return ImportResult.Failed("video track has no codec type")

            val width = srcFormat.optInt(MediaFormat.KEY_WIDTH, 0).let { it - (it % 2) }
            val height = srcFormat.optInt(MediaFormat.KEY_HEIGHT, 0).let { it - (it % 2) }
            if (width <= 0 || height <= 0) return ImportResult.Failed("unknown video dimensions")
            val frameRate = srcFormat.optInt(MediaFormat.KEY_FRAME_RATE, 30).coerceAtLeast(1)

            val encFormat = MediaFormat.createVideoFormat(VIDEO_MIME, width, height).apply {
                setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
                setInteger(MediaFormat.KEY_BIT_RATE, VIDEO_BITRATE)
                setInteger(MediaFormat.KEY_FRAME_RATE, frameRate)
                setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
                setInteger(MediaFormat.KEY_LATENCY, 1)
            }
            encoder = MediaCodec.createEncoderByType(VIDEO_MIME).apply {
                configure(encFormat, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            }
            val inputSurface = encoder.createInputSurface()
            encoder.start()
            decoder = MediaCodec.createDecoderByType(srcMime).apply {
                configure(srcFormat, inputSurface, null, 0)
                start()
            }

            val streamId = "${IMPORT_PREFIX}${request.streamIndex}-video"
            return pumpTranscode(
                streamId = streamId,
                mediaTypeValue = MediaType.VIDEO.value,
                codecName = "h264",
                clock = clock,
                extractor = extractor,
                decoder = decoder,
                encoder = encoder,
                renderToSurface = true,
            )
        } finally {
            runCatching { decoder?.stop() }; runCatching { decoder?.release() }
            runCatching { encoder?.stop() }; runCatching { encoder?.release() }
            runCatching { extractor.release() }
        }
    }

    /**
     * Shared decode→encode→segment pump. For audio, decoded PCM is re-fed to the AAC
     * encoder's input buffers; for video, decoded frames are rendered onto the encoder's
     * input Surface (so [renderToSurface]) and EOS is signalled via the surface.
     */
    private fun pumpTranscode(
        streamId: String,
        mediaTypeValue: Int,
        codecName: String,
        clock: SegmentMuxer.SegmentClock,
        extractor: MediaExtractor,
        decoder: MediaCodec,
        encoder: MediaCodec,
        renderToSurface: Boolean,
    ): ImportResult? {
        val decInfo = MediaCodec.BufferInfo()
        val encInfo = MediaCodec.BufferInfo()

        var current: SegmentMuxer? = null
        var segStartPts = -1L
        var encFormat: MediaFormat? = null
        var csd: ByteString = ByteString.EMPTY
        var seq = 0L
        var firstEmitted = true
        var lastSyncReqPts = Long.MIN_VALUE

        var sawInputEos = false
        var sawDecoderEos = false

        fun openSegment(format: MediaFormat, startPts: Long) {
            val file = File(segmentDir, "import-$streamId-${fileCounter++}.mp4")
            val muxer = SegmentMuxer(file, streamId, mediaTypeValue, codecName, format, csd, clock)
            muxer.start()
            current = muxer
            segStartPts = startPts
            lastSyncReqPts = startPts
        }

        fun finalizeCurrent() {
            val muxer = current ?: return
            current = null
            if (!muxer.hasSamples()) {
                muxer.abort()
                return
            }
            val raw = muxer.finish(seq++)
            onSegment(raw.copy(sessionId = fileSessionId, gapBefore = firstEmitted))
            firstEmitted = false
            emitted++
            onProgress(emitted, totalEstimate)
            backpressure() // never outrun the uploader / overflow the shared cap
        }

        fun handleEncoded(buffer: ByteBuffer, info: MediaCodec.BufferInfo) {
            if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                if (csd.size == 0) {
                    val arr = ByteArray(info.size)
                    buffer.position(info.offset)
                    buffer.get(arr, 0, info.size)
                    csd = ByteString.of(*arr)
                }
                return
            }
            if (info.size <= 0) return
            val format = encFormat ?: return
            val pts = info.presentationTimeUs
            val isKey = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0

            if (current == null) {
                // Video must start on a keyframe; audio frames are all independent.
                if (renderToSurface && !isKey) return
                openSegment(format, pts)
            } else {
                val due = pts - segStartPts >= segmentDurationUs
                val canCut = if (renderToSurface) isKey && due else due
                if (canCut) {
                    finalizeCurrent()
                    openSegment(format, pts)
                }
            }
            current?.write(buffer, info)

            // Nudge an IDR shortly before the next boundary (video only).
            if (renderToSurface && pts - lastSyncReqPts >= segmentDurationUs - SYNC_LEAD_US) {
                lastSyncReqPts = pts
                runCatching {
                    encoder.setParameters(Bundle().apply {
                        putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0)
                    })
                }
            }
        }

        // Returns true when the encoder has emitted end-of-stream.
        fun drainEncoder(): Boolean {
            while (true) {
                val idx = encoder.dequeueOutputBuffer(encInfo, 0)
                when {
                    idx == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                        encFormat = encoder.outputFormat
                        csd = SegmentMuxer.extractCsd(encoder.outputFormat)
                    }
                    idx >= 0 -> {
                        val eos = encInfo.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0
                        val buf = encoder.getOutputBuffer(idx)
                        if (buf != null) handleEncoded(buf, encInfo)
                        encoder.releaseOutputBuffer(idx, false)
                        if (eos) {
                            finalizeCurrent()
                            return true
                        }
                    }
                    else -> return false // INFO_TRY_AGAIN_LATER
                }
            }
        }

        fun feedEncoderAudio(pcm: ByteArray, pts: Long, eos: Boolean) {
            // One pass for a non-empty buffer (split across input buffers as needed);
            // a single zero-length EOS buffer when pcm is empty. EOS is set only on the
            // final chunk so we never queue two end-of-stream markers.
            var offset = 0
            do {
                var inIdx = encoder.dequeueInputBuffer(TIMEOUT_US)
                while (inIdx < 0) {
                    drainEncoder() // make room, then retry
                    inIdx = encoder.dequeueInputBuffer(TIMEOUT_US)
                }
                val inBuf = encoder.getInputBuffer(inIdx) ?: return
                inBuf.clear()
                val chunk = minOf(inBuf.capacity(), pcm.size - offset).coerceAtLeast(0)
                if (chunk > 0) inBuf.put(pcm, offset, chunk)
                val last = offset + chunk >= pcm.size
                val flags = if (eos && last) MediaCodec.BUFFER_FLAG_END_OF_STREAM else 0
                encoder.queueInputBuffer(inIdx, 0, chunk, pts, flags)
                offset += chunk
            } while (offset < pcm.size)
        }

        var encoderDone = false
        while (!encoderDone) {
            if (isCancelled()) {
                runCatching { current?.abort() }
                return ImportResult.Cancelled
            }

            // 1. Feed the decoder from the extractor.
            if (!sawInputEos) {
                val inIdx = decoder.dequeueInputBuffer(TIMEOUT_US)
                if (inIdx >= 0) {
                    val inBuf = decoder.getInputBuffer(inIdx)
                    val size = if (inBuf != null) extractor.readSampleData(inBuf, 0) else -1
                    if (size < 0) {
                        decoder.queueInputBuffer(inIdx, 0, 0, 0, MediaCodec.BUFFER_FLAG_END_OF_STREAM)
                        sawInputEos = true
                    } else {
                        decoder.queueInputBuffer(inIdx, 0, size, extractor.sampleTime, 0)
                        extractor.advance()
                    }
                }
            }

            // 2. Drain the decoder -> feed/signal the encoder.
            val outIdx = decoder.dequeueOutputBuffer(decInfo, TIMEOUT_US)
            if (outIdx >= 0) {
                val eos = decInfo.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0
                if (renderToSurface) {
                    // Render decoded frames to the encoder surface at the SOURCE pts so
                    // 2 s cuts land correctly; never render the empty EOS buffer.
                    if (decInfo.size > 0) {
                        decoder.releaseOutputBuffer(outIdx, decInfo.presentationTimeUs * 1000)
                    } else {
                        decoder.releaseOutputBuffer(outIdx, false)
                    }
                } else {
                    if (decInfo.size > 0) {
                        val outBuf = decoder.getOutputBuffer(outIdx)
                        if (outBuf != null) {
                            val pcm = ByteArray(decInfo.size)
                            outBuf.position(decInfo.offset)
                            outBuf.get(pcm)
                            feedEncoderAudio(pcm, decInfo.presentationTimeUs, false)
                        }
                    }
                    decoder.releaseOutputBuffer(outIdx, false)
                }
                if (eos && !sawDecoderEos) {
                    sawDecoderEos = true
                    if (renderToSurface) encoder.signalEndOfInputStream()
                    else feedEncoderAudio(ByteArray(0), decInfo.presentationTimeUs, true)
                }
            }

            // 3. Drain the encoder -> muxer (cut into 2s segments).
            encoderDone = drainEncoder()
        }
        return null
    }

    private fun selectTrack(extractor: MediaExtractor, prefix: String): Int? {
        for (i in 0 until extractor.trackCount) {
            val mime = extractor.getTrackFormat(i).getString(MediaFormat.KEY_MIME) ?: continue
            if (mime.startsWith(prefix)) return i
        }
        return null
    }

    private fun MediaFormat.optInt(key: String, default: Int): Int =
        if (containsKey(key)) getInteger(key) else default

    companion object {
        const val IMPORT_PREFIX = "import-"
        private const val AUDIO_MIME = MediaFormat.MIMETYPE_AUDIO_AAC
        private const val VIDEO_MIME = MediaFormat.MIMETYPE_VIDEO_AVC
        private const val AUDIO_BITRATE = 96_000
        private const val AUDIO_MAX_INPUT = 16_384
        private const val VIDEO_BITRATE = 4_000_000
        private const val TIMEOUT_US = 10_000L
        private const val SYNC_LEAD_US = 250_000L
    }
}
