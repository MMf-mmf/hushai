package com.hushai.android.capture

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import com.hushai.android.util.HushaiLog
import hushai.v1.MediaType
import okio.ByteString
import java.io.File
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong

/**
 * AAC-LC audio encoder fed by pushed mic PCM (a [PcmSink]; the mic is owned by
 * [MicSource], shared with the voice assistant). A second, INDEPENDENT stream
 * (`cam0-audio`) with its own monotonic sequence (§5.3). AAC frames are each
 * independently decodable, so segments are cut purely on elapsed PTS (~2s); the
 * AudioSpecificConfig (csd-0) is attached to every segment.
 *
 * Threading: [onPcm] (the shared mic reader thread) only copies the PCM into a
 * bounded queue and returns immediately. A dedicated [pump] thread does the codec
 * feed/drain and segment finalization (which includes file I/O + SHA-256) — so the
 * mic reader thread is never blocked and the *other* mic consumer (the voice
 * assistant) can't be starved. [stop] must only be called after the mic has
 * stopped (no further onPcm); [CaptureService] guarantees this by stopping
 * [MicSource] first.
 */
class AudioEncoder(
    private val segmentDir: File,
    private val sampleRate: Int,
    private val channelCount: Int,
    bitRate: Int,
    private val segmentDurationUs: Long,
    private val sequencer: AtomicLong,
    private val onSegment: (Segment) -> Unit,
) : PcmSink {
    private val codec = MediaCodec.createEncoderByType(MIME)
    private val info = MediaCodec.BufferInfo()
    private val bytesPerFrame = 2 * channelCount
    private val queue = ArrayBlockingQueue<ByteArray>(QUEUE_CAPACITY)

    @Volatile private var running = false
    private var pumpThread: Thread? = null

    private var outputFormat: MediaFormat? = null
    private var csd: ByteString = ByteString.EMPTY
    private var current: SegmentMuxer? = null
    private var segmentStartPtsUs = -1L
    private var totalFramesRead = 0L
    private var fileCounter = 0

    init {
        val format = MediaFormat.createAudioFormat(MIME, sampleRate, channelCount).apply {
            setInteger(MediaFormat.KEY_AAC_PROFILE, MediaCodecInfo.CodecProfileLevel.AACObjectLC)
            setInteger(MediaFormat.KEY_BIT_RATE, bitRate)
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, MAX_INPUT_SIZE)
        }
        codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
    }

    fun start() {
        codec.start()
        running = true
        pumpThread = Thread({ pump() }, "hushai-audio-enc").apply { start() }
    }

    fun stop() {
        running = false
        pumpThread?.join(2_000)
        pumpThread = null
        finalizeCurrent()
        runCatching { codec.stop() }
        runCatching { codec.release() }
    }

    /** Mic thread: copy + enqueue only (drop oldest under overload) — never blocks. */
    override fun onPcm(data: ByteArray, length: Int) {
        if (!running) return
        val copy = data.copyOf(length)
        if (!queue.offer(copy)) {
            queue.poll()
            queue.offer(copy)
        }
    }

    /** Dedicated thread: all codec + file work happens here, off the mic thread. */
    private fun pump() {
        while (running) {
            val chunk = queue.poll(100, TimeUnit.MILLISECONDS) ?: continue
            feed(chunk)
        }
    }

    private fun feed(data: ByteArray) {
        var offset = 0
        while (offset < data.size) {
            val inIndex = codec.dequeueInputBuffer(TIMEOUT_US)
            if (inIndex < 0) {
                drainOutput()
                continue
            }
            val inBuf = codec.getInputBuffer(inIndex) ?: continue
            inBuf.clear()
            val chunk = minOf(inBuf.capacity(), data.size - offset)
            inBuf.put(data, offset, chunk)
            val ptsUs = totalFramesRead * 1_000_000L / sampleRate
            totalFramesRead += chunk / bytesPerFrame
            codec.queueInputBuffer(inIndex, 0, chunk, ptsUs, 0)
            offset += chunk
            drainOutput()
        }
    }

    private fun drainOutput() {
        while (true) {
            val index = codec.dequeueOutputBuffer(info, 0)
            when {
                index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    outputFormat = codec.outputFormat
                    csd = SegmentMuxer.extractCsd(codec.outputFormat)
                }
                index >= 0 -> {
                    val buffer = codec.getOutputBuffer(index)
                    if (buffer != null) handleEncoded(buffer, info)
                    codec.releaseOutputBuffer(index, false)
                }
                else -> return // INFO_TRY_AGAIN_LATER
            }
        }
    }

    private fun handleEncoded(buffer: java.nio.ByteBuffer, info: MediaCodec.BufferInfo) {
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
        val format = outputFormat ?: return
        val ptsUs = info.presentationTimeUs

        if (current == null) {
            openSegment(format, ptsUs)
        } else if (ptsUs - segmentStartPtsUs >= segmentDurationUs) {
            finalizeCurrent()
            openSegment(format, ptsUs)
        }
        current?.write(buffer, info)
    }

    private fun openSegment(format: MediaFormat, startPtsUs: Long) {
        val file = File(segmentDir, "audio-${fileCounter++}.mp4")
        val muxer = SegmentMuxer(file, STREAM_ID, MediaType.AUDIO.value, "aac", format, csd)
        muxer.start()
        current = muxer
        segmentStartPtsUs = startPtsUs
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
        }.onFailure { HushaiLog.error("audio segment finalize failed", it) }
    }

    companion object {
        const val MIME = MediaFormat.MIMETYPE_AUDIO_AAC
        const val STREAM_ID = "cam0-audio"
        private const val TIMEOUT_US = 10_000L
        private const val MAX_INPUT_SIZE = 8192
        // ~3s of 100ms chunks — absorbs finalize/SHA-256 latency without dropping.
        private const val QUEUE_CAPACITY = 32
    }
}
