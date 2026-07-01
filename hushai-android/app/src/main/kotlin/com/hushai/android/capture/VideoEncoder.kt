package com.hushai.android.capture

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Bundle
import android.util.Size
import android.view.Surface
import com.hushai.android.util.HushaiLog
import hushai.v1.MediaType
import okio.ByteString
import java.io.File
import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicLong

/**
 * H.264 video encoder fed by a Camera2 [inputSurface]. Produces ~2s,
 * independently-decodable MP4 segments: it requests an IDR near each boundary
 * (KEY_I_FRAME_INTERVAL + PARAMETER_KEY_REQUEST_SYNC_FRAME) and CUTS only on an
 * actual `BUFFER_FLAG_KEY_FRAME` (never mid-GOP, contract §5.4). Each segment is
 * a fresh [SegmentMuxer]; the boundary keyframe becomes the next segment's first
 * sample. The encoder's CSD (SPS/PPS) is captured once and attached to every
 * segment so each body decodes standalone.
 */
class VideoEncoder(
    private val segmentDir: File,
    val size: Size,
    bitRate: Int,
    private val frameRate: Int,
    private val segmentDurationUs: Long,
    private val sequencer: AtomicLong,
    private val onSegment: (Segment) -> Unit,
    // Returns the current upright-rotation hint (0/90/180/270) for the NEXT segment, sampled at each
    // ~2s boundary so rotating the phone mid-capture self-corrects. Default 0 = no rotation.
    private val rotationProvider: () -> Int = { 0 },
) {
    private val codec = MediaCodec.createEncoderByType(MIME)
    val inputSurface: Surface

    @Volatile private var running = false
    private var drainThread: Thread? = null

    private var outputFormat: MediaFormat? = null
    private var csd: ByteString = ByteString.EMPTY
    private var current: SegmentMuxer? = null
    private var segmentStartPtsUs = -1L
    private var lastSyncRequestPtsUs = Long.MIN_VALUE
    private var fileCounter = 0

    init {
        val format = MediaFormat.createVideoFormat(MIME, size.width, size.height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, bitRate)
            setInteger(MediaFormat.KEY_FRAME_RATE, frameRate)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2) // ~2s GOP -> a keyframe to cut on
            // Discourage B-frames: their out-of-order PTS would make MediaMuxer reject
            // samples. Hardware realtime encoders honor this; a no-op on older APIs.
            setInteger(MediaFormat.KEY_LATENCY, 1)
        }
        // configure()/createInputSurface() can throw on a device-specific config rejection
        // (size comes from CameraController.select(), not a trusted constant). Release the native
        // MediaCodec before rethrowing, else it leaks against the global codec pool on every
        // START_STICKY restart attempt.
        try {
            codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            inputSurface = codec.createInputSurface()
        } catch (e: Exception) {
            runCatching { codec.release() }
            throw e
        }
    }

    fun start() {
        codec.start()
        running = true
        drainThread = Thread({ drainLoop() }, "hushai-video-enc").apply { start() }
    }

    /** Finalize the in-flight segment and tear down (clean Stop). */
    fun stop() {
        running = false
        drainThread?.join(2_000)
        finalizeCurrent()
        runCatching { codec.stop() }
        runCatching { codec.release() }
        runCatching { inputSurface.release() }
    }

    private fun drainLoop() {
        val info = MediaCodec.BufferInfo()
        while (running) {
            val index = try {
                codec.dequeueOutputBuffer(info, TIMEOUT_US)
            } catch (e: IllegalStateException) {
                HushaiLog.error("video dequeue failed", e); break
            }
            when {
                index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    outputFormat = codec.outputFormat
                    csd = SegmentMuxer.extractCsd(codec.outputFormat)
                }
                index >= 0 -> {
                    val buffer = codec.getOutputBuffer(index)
                    // Guard handleEncoded (opens a SegmentMuxer, which can throw on a muxer/format
                    // reject): a throw would otherwise kill this drain thread and silently stop
                    // encoding. Log + drop the sample; releaseOutputBuffer still runs below.
                    if (buffer != null) runCatching { handleEncoded(buffer, info) }
                        .onFailure { HushaiLog.error("video handleEncoded failed; dropping sample", it) }
                    codec.releaseOutputBuffer(index, false)
                }
                // INFO_TRY_AGAIN_LATER: nothing ready; loop.
            }
        }
    }

    private fun handleEncoded(buffer: ByteBuffer, info: MediaCodec.BufferInfo) {
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
        val format = outputFormat ?: return // must know the muxer track format first

        val isKeyFrame = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
        val ptsUs = info.presentationTimeUs

        if (current == null) {
            if (!isKeyFrame) return // a segment must START on a keyframe
            openSegment(format, ptsUs)
        } else if (isKeyFrame && ptsUs - segmentStartPtsUs >= segmentDurationUs) {
            finalizeCurrent()
            openSegment(format, ptsUs)
        }

        current?.write(buffer, info)
        requestSyncFrameNearBoundary(ptsUs)
    }

    private fun openSegment(format: MediaFormat, startPtsUs: Long) {
        val file = File(segmentDir, "video-${fileCounter++}.mp4")
        // Sample the device orientation now so this segment records upright for the phone's CURRENT
        // pose (a mid-capture rotation self-corrects on the next segment).
        val rotation = runCatching { rotationProvider() }.getOrDefault(0)
        val muxer = SegmentMuxer(
            file, STREAM_ID, MediaType.VIDEO.value, "h264", format, csd,
            orientationHintDegrees = rotation,
        )
        muxer.start()
        current = muxer
        segmentStartPtsUs = startPtsUs
        lastSyncRequestPtsUs = startPtsUs
    }

    private fun finalizeCurrent() {
        val muxer = current ?: return
        current = null
        if (!muxer.hasSamples()) {
            muxer.abort()
            return
        }
        runCatching {
            onSegment(muxer.finish(sequencer.getAndIncrement()))
        }.onFailure { HushaiLog.error("video segment finalize failed", it) }
    }

    /** Nudge an IDR slightly before the boundary so the cut lands near ~2s. */
    private fun requestSyncFrameNearBoundary(ptsUs: Long) {
        if (ptsUs - lastSyncRequestPtsUs >= segmentDurationUs - SYNC_LEAD_US) {
            lastSyncRequestPtsUs = ptsUs
            runCatching {
                codec.setParameters(Bundle().apply {
                    putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0)
                })
            }
        }
    }

    companion object {
        const val MIME = MediaFormat.MIMETYPE_VIDEO_AVC
        const val STREAM_ID = "cam0-video"
        private const val TIMEOUT_US = 10_000L
        private const val SYNC_LEAD_US = 250_000L // request IDR ~250ms before boundary
    }
}
