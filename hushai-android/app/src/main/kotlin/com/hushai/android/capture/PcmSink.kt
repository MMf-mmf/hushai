package com.hushai.android.capture

/**
 * A consumer of raw mic PCM (16-bit little-endian, mono). [MicSource] reads the
 * microphone once and pushes each chunk to every registered sink synchronously,
 * so two consumers (the AAC segment encoder and the voice-assistant pipeline) can
 * share one [android.media.AudioRecord] — Android won't reliably hand two
 * AudioRecords concurrent mic data on API 26.
 *
 * `onPcm` is called on [MicSource]'s reader thread; the same `data` array is reused
 * across calls, so a sink must consume (copy/encode) it before returning and must
 * not retain the reference.
 */
interface PcmSink {
    fun onPcm(data: ByteArray, length: Int)
}
