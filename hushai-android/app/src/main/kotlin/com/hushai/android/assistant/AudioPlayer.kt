package com.hushai.android.assistant

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.os.SystemClock
import com.hushai.android.util.HushaiLog

/**
 * Plays the 16-bit PCM WAV returned by the backend `/v1/tts` through an
 * [AudioTrack] (the spoken assistant answer). Blocking: [play] returns only once
 * the audio has finished (or failed), so the caller can then resume listening.
 *
 * Replaces the old on-device `android.speech.tts.TextToSpeech` — synthesis now
 * happens on the backend; the phone only plays the audio.
 */
class AudioPlayer {
    @Volatile private var current: AudioTrack? = null
    @Volatile private var cancelled = false

    /** Decode and play `wav` synchronously. Returns true if it played to the end. */
    fun play(wav: ByteArray): Boolean {
        val audio = WavPcm.parse(wav) ?: run {
            HushaiLog.warn("TTS audio could not be parsed (${wav.size} bytes)")
            return false
        }
        val channelMask =
            if (audio.channels >= 2) AudioFormat.CHANNEL_OUT_STEREO else AudioFormat.CHANNEL_OUT_MONO
        val minBuf = AudioTrack.getMinBufferSize(
            audio.sampleRate,
            channelMask,
            AudioFormat.ENCODING_PCM_16BIT,
        )
        if (minBuf <= 0) {
            HushaiLog.warn("AudioTrack.getMinBufferSize failed ($minBuf) sr=${audio.sampleRate}")
            return false
        }

        cancelled = false
        val track = AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_ASSISTANT)
                    .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                    .build()
            )
            .setAudioFormat(
                AudioFormat.Builder()
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .setSampleRate(audio.sampleRate)
                    .setChannelMask(channelMask)
                    .build()
            )
            .setBufferSizeInBytes(maxOf(minBuf, 64 * 1024))
            .setTransferMode(AudioTrack.MODE_STREAM)
            .build()
        current = track

        return try {
            track.play()
            val data = audio.pcm16le
            var offset = 0
            while (offset < data.size && !cancelled) {
                // Blocking write in MODE_STREAM: returns when the buffer accepts more.
                val n = track.write(data, offset, data.size - offset)
                if (n < 0) {
                    HushaiLog.error("AudioTrack.write error $n")
                    return false
                }
                offset += n
            }
            if (cancelled) false else drain(track, data.size / audio.frameBytes, audio.sampleRate)
        } catch (e: Exception) {
            HushaiLog.error("audio playback failed", e)
            false
        } finally {
            runCatching { track.stop() }
            runCatching { track.release() }
            current = null
        }
    }

    /** Stop any in-flight playback (used on teardown). */
    fun stop() {
        cancelled = true
        runCatching { current?.pause(); current?.flush(); current?.stop() }
    }

    /** Wait until all written frames have actually played out (bounded). */
    private fun drain(track: AudioTrack, totalFrames: Int, sampleRate: Int): Boolean {
        val playMs = totalFrames * 1000L / sampleRate
        val deadline = SystemClock.elapsedRealtime() + playMs + 1500
        while (!cancelled &&
            track.playbackHeadPosition < totalFrames &&
            SystemClock.elapsedRealtime() < deadline
        ) {
            Thread.sleep(20)
        }
        return !cancelled
    }
}
