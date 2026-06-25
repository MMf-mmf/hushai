package com.hushai.android.capture

import android.annotation.SuppressLint
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import com.hushai.android.util.HushaiLog
import java.util.concurrent.CopyOnWriteArrayList

/**
 * Single owner of the microphone. Reads 16-bit mono PCM at [sampleRate] on a
 * dedicated thread and fans each chunk out to all [PcmSink]s synchronously — one
 * mic, many consumers (AAC encoder + voice assistant). 16 kHz is deliberate: it's
 * what Vosk and whisper both want, and AAC encodes it fine.
 *
 * Sinks are fixed before [start]; the reader reuses one buffer, so sinks must copy
 * what they need within [PcmSink.onPcm].
 */
class MicSource(
    private val sampleRate: Int,
    private val channelCount: Int,
    initialSinks: List<PcmSink>,
) {
    // Thread-safe so the voice assistant can be toggled on/off while the mic runs.
    private val sinks = CopyOnWriteArrayList(initialSinks)

    fun addSink(sink: PcmSink) { sinks.addIfAbsent(sink) }
    fun removeSink(sink: PcmSink) { sinks.remove(sink) }

    private val channelMask =
        if (channelCount == 1) AudioFormat.CHANNEL_IN_MONO else AudioFormat.CHANNEL_IN_STEREO
    private val readChunk: Int
    private val record: AudioRecord

    @Volatile private var running = false
    private var thread: Thread? = null

    init {
        val min = AudioRecord.getMinBufferSize(sampleRate, channelMask, AudioFormat.ENCODING_PCM_16BIT)
        val buffer = if (min > 0) min * 2 else sampleRate * 2
        // ~100 ms read granularity (bounded to the device's min buffer) — small
        // enough for responsive wake-word latency, large enough to be efficient.
        readChunk = minOf(buffer, sampleRate / 10 * 2 * channelCount).coerceAtLeast(min.coerceAtLeast(640))
        @SuppressLint("MissingPermission") // RECORD_AUDIO granted before the service starts
        record = AudioRecord(
            MediaRecorder.AudioSource.MIC,
            sampleRate,
            channelMask,
            AudioFormat.ENCODING_PCM_16BIT,
            buffer,
        )
    }

    fun start() {
        if (record.state != AudioRecord.STATE_INITIALIZED) {
            HushaiLog.error("AudioRecord not initialized (state=${record.state})")
            return
        }
        record.startRecording()
        running = true
        thread = Thread({ loop() }, "hushai-mic").apply { start() }
    }

    private fun loop() {
        val buf = ByteArray(readChunk)
        while (running) {
            val n = record.read(buf, 0, buf.size)
            if (n > 0) {
                for (sink in sinks) {
                    runCatching { sink.onPcm(buf, n) }
                        .onFailure { HushaiLog.error("pcm sink failed", it) }
                }
            }
        }
    }

    /** Stops the reader thread and releases the mic. After this returns, no sink
     *  is being called — safe for sinks to tear down their own resources. */
    fun stop() {
        running = false
        thread?.join(2_000)
        thread = null
        runCatching { record.stop() }
        runCatching { record.release() }
    }
}
