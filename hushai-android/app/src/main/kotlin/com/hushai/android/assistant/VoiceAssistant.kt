package com.hushai.android.assistant

import android.content.Context
import com.hushai.android.capture.PcmSink
import com.hushai.android.net.RagClient
import com.hushai.android.net.TtsClient
import com.hushai.android.util.AssistantBus
import com.hushai.android.util.AssistantPhase
import com.hushai.android.util.HushaiLog
import org.json.JSONObject
import org.vosk.Recognizer
import java.util.Locale
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

/**
 * Offline voice assistant: a [PcmSink] fed by the shared mic. A Vosk recognizer
 * (acoustic + speaker model) runs the loop wake-word → owner voice-verify →
 * question → RAG answer → spoken reply.
 *
 * Threading: [onPcm] (mic thread) only *copies* PCM into a bounded queue; ALL Vosk
 * work — and every recognizer mutation — happens on the single [loop] worker thread,
 * so the recognizer is never touched concurrently. Control requests from other
 * threads (enroll, speak-done) set @Volatile flags the worker picks up. PCM is
 * dropped while THINKING/SPEAKING so the assistant never transcribes its own reply.
 *
 * Spoken answers are synthesized on the backend (`/v1/tts`, [TtsClient]) and played
 * locally via [AudioPlayer]; the fetch+play runs on a dedicated speak thread so the
 * worker loop (and its watchdog) stays responsive, and signals `pendingResume` when
 * done. The phone does no speech synthesis.
 */
class VoiceAssistant(
    private val context: Context,
    private val deviceId: String,
    initialWakeWord: String,
    private val ragClient: RagClient,
    private val ttsClient: TtsClient,
    initialOwnerEmbedding: FloatArray?,
    private val onEnrollComplete: (FloatArray) -> Unit,
) : PcmSink {

    @Volatile var wakeWord: String = SpeakerMath.normalizeWord(initialWakeWord)
        set(value) { field = SpeakerMath.normalizeWord(value) }

    @Volatile private var ownerEmbedding: FloatArray? = initialOwnerEmbedding

    private val queue = ArrayBlockingQueue<ByteArray>(QUEUE_CAPACITY)
    @Volatile private var running = false
    @Volatile private var phase = AssistantPhase.LISTENING
    // @Volatile: created on hushai-va-init, read/torn-down by stop() on hushai-lifecycle.
    @Volatile private var worker: Thread? = null

    @Volatile private var recognizer: Recognizer? = null
    private val audioPlayer = AudioPlayer()
    private val speakExecutor: ExecutorService =
        Executors.newSingleThreadExecutor { r -> Thread(r, "hushai-va-speak") }

    // Cross-thread control flags, acted on by the worker only.
    @Volatile private var pendingEnroll = false
    @Volatile private var pendingResume = false

    private val enrollVectors = ArrayList<FloatArray>()
    private var enrolling = false
    private var awaitDeadlineNanos = 0L
    private var speakDeadlineNanos = 0L
    private var enrollDeadlineNanos = 0L
    private var enrollStartNanos = 0L

    fun start() {
        if (running) return
        running = true
        // Model load is slow (unpack + mmap) — do it off the caller's thread.
        Thread({ init() }, "hushai-va-init").start()
    }

    private fun init() {
        publish { it.copy(enabled = true, phase = AssistantPhase.LISTENING, enrolled = ownerEmbedding != null) }
        val models = try {
            VoiceModels.load(context)
        } catch (e: Exception) {
            HushaiLog.error("vosk model load failed", e)
            running = false // else onPcm keeps enqueuing into a queue no worker drains
            // enabled=false so the UI Switch reflects the dead assistant (not stuck ON).
            publish { it.copy(enabled = false, phase = AssistantPhase.OFF, note = "voice models failed to load") }
            return
        }
        if (!running) return
        val rec = try {
            Recognizer(models.first, SAMPLE_RATE.toFloat(), models.second)
        } catch (e: Exception) {
            HushaiLog.error("recognizer init failed", e)
            running = false
            publish { it.copy(enabled = false, phase = AssistantPhase.OFF, note = "recognizer init failed") }
            return
        }
        // A stop() can land during the slow Vosk/Recognizer init above; if so, close the native
        // Recognizer here instead of leaking it (stop() already ran and saw recognizer==null).
        if (!running) { runCatching { rec.close() }; return }
        recognizer = rec
        publish { it.copy(ready = true) }
        worker = Thread({ loop() }, "hushai-va").apply { start() }
        HushaiLog.info("voice assistant ready (wake='$wakeWord' enrolled=${ownerEmbedding != null})")
    }

    override fun onPcm(data: ByteArray, length: Int) {
        if (!running) return
        // Only capture while listening — never during THINKING/SPEAKING (avoids
        // hearing our own spoken answer and wasting CPU).
        val p = phase
        if (p != AssistantPhase.LISTENING && p != AssistantPhase.AWAIT_QUESTION && p != AssistantPhase.ENROLLING) return
        val copy = data.copyOf(length)
        if (!queue.offer(copy)) {
            queue.poll() // drop oldest to bound latency
            queue.offer(copy)
        }
    }

    private fun loop() {
        val rec = recognizer ?: return
        while (running) {
            if (pendingEnroll) { pendingEnroll = false; beginEnroll(rec) }
            if (pendingResume) { pendingResume = false; resumeListening(rec) }
            val now = System.nanoTime()
            if (phase == AssistantPhase.AWAIT_QUESTION && awaitDeadlineNanos > 0L && now >= awaitDeadlineNanos) {
                awaitDeadlineNanos = 0
                resumeListening(rec, note = "no question heard")
            }
            // Watchdog: never get stuck SPEAKING if synth/playback hangs past the deadline.
            if (phase == AssistantPhase.SPEAKING && speakDeadlineNanos > 0L && now >= speakDeadlineNanos) {
                HushaiLog.warn("speak did not finish in time — resuming")
                resumeListening(rec)
            }
            // Enrollment deadline: finish with whatever voiceprints we have (≥1), else fail.
            if (enrolling && enrollDeadlineNanos > 0L && now >= enrollDeadlineNanos) {
                if (enrollVectors.isNotEmpty()) {
                    finishEnroll()
                } else {
                    enrolling = false
                    enrollDeadlineNanos = 0
                    rec.reset()
                    phase = AssistantPhase.LISTENING
                    publish { it.copy(phase = AssistantPhase.LISTENING, note = "didn't catch your voice — try again, speak clearly") }
                }
            }
            val chunk = queue.poll(150, TimeUnit.MILLISECONDS) ?: continue
            val isFinal = runCatching { rec.acceptWaveForm(chunk, chunk.size) }
                .onFailure { HushaiLog.error("vosk accept failed", it) }
                .getOrDefault(false)
            if (isFinal) {
                // getResult() returns the utterance JSON and RESETS — call it ONCE,
                // then extract both text and the speaker vector from the same string
                // (calling it twice would make the 2nd read empty, dropping `spk`).
                val json = rec.result
                handleFinal(rec, extractText(json), extractSpk(json))
            } else {
                val partial = extractPartial(rec.partialResult)
                if (partial.isNotBlank()) publish { it.copy(lastHeard = partial) }
            }
        }
    }

    private fun handleFinal(rec: Recognizer, text: String, spk: FloatArray?) {
        if (enrolling) { collectEnrollment(spk); return }
        when (phase) {
            AssistantPhase.LISTENING -> {
                val wake = SpeakerMath.tokens(wakeWord)
                if (wake.isEmpty()) return
                val orig = SpeakerMath.tokens(text)
                val lower = orig.map { it.lowercase(Locale.US) }
                val at = SpeakerMath.subsequenceIndex(lower, wake)
                if (at < 0) return
                publish { it.copy(lastHeard = text, note = null) }
                val tail = if (at + wake.size < orig.size) {
                    orig.subList(at + wake.size, orig.size).joinToString(" ")
                } else ""
                if (SpeakerMath.tokens(tail).size >= MIN_QUESTION_WORDS) {
                    // Single utterance: wake + question together — verify on it.
                    if (verifyOwner(spk)) answer(rec, tail) else reject(rec)
                } else {
                    // Bare wake word: wait for the question (a longer, better voice sample).
                    rec.reset()
                    phase = AssistantPhase.AWAIT_QUESTION
                    awaitDeadlineNanos = System.nanoTime() + QUESTION_TIMEOUT_NANOS
                    publish { it.copy(phase = AssistantPhase.AWAIT_QUESTION, note = null) }
                }
            }
            AssistantPhase.AWAIT_QUESTION -> {
                if (text.isBlank()) return
                publish { it.copy(lastHeard = text) }
                if (verifyOwner(spk)) answer(rec, text) else reject(rec)
            }
            else -> {}
        }
    }

    /** Blocks the worker on the RAG call, then speaks the answer (resume on speak-done). */
    private fun answer(rec: Recognizer, question: String) {
        phase = AssistantPhase.THINKING
        // Introspective questions ("how have I been?") route to the reflection agent; the
        // backend scopes them to the owner. Everything else uses the default recordings agent.
        val agentId = ReflectionIntent.agentFor(question)
        publish { it.copy(phase = AssistantPhase.THINKING, lastQuestion = question, note = null) }
        when (val r = ragClient.ask(question, deviceId, agentId)) {
            is RagClient.Result.Answer -> {
                phase = AssistantPhase.SPEAKING
                speakDeadlineNanos = System.nanoTime() + SPEAK_TIMEOUT_NANOS
                publish { it.copy(phase = AssistantPhase.SPEAKING, lastAnswer = r.text) }
                speak(r.text) // resumes LISTENING once playback finishes (pendingResume)
            }
            is RagClient.Result.Error -> {
                publish { it.copy(note = "RAG error: ${r.reason}") }
                resumeListening(rec)
            }
        }
    }

    private fun verifyOwner(spk: FloatArray?): Boolean {
        val owner = ownerEmbedding ?: run {
            publish { it.copy(note = "not enrolled — responding to anyone") }
            return true
        }
        if (spk == null || spk.isEmpty()) {
            publish { it.copy(note = "couldn't capture a voiceprint") }
            return false
        }
        val sim = SpeakerMath.cosine(spk, owner)
        HushaiLog.info("speaker cosine=$sim (threshold=$SPEAKER_THRESHOLD)")
        return sim >= SPEAKER_THRESHOLD
    }

    private fun reject(rec: Recognizer) {
        rec.reset()
        phase = AssistantPhase.LISTENING
        awaitDeadlineNanos = 0
        publish { it.copy(phase = AssistantPhase.LISTENING, note = "speaker not recognized — ignoring") }
    }

    /**
     * Fetch the spoken answer from the backend and play it, off the worker thread so
     * the loop/watchdog keep running. Always sets [pendingResume] (success or not) so
     * the assistant returns to LISTENING; if audio is unavailable the answer text is
     * still shown.
     */
    private fun speak(text: String) {
        speakExecutor.execute {
            val played = runCatching {
                val wav = ttsClient.synthesize(text)
                wav != null && audioPlayer.play(wav)
            }.getOrElse { e -> HushaiLog.error("speak failed", e); false }
            if (!played) HushaiLog.warn("spoken answer unavailable — showing text only")
            pendingResume = true
        }
    }

    private fun resumeListening(rec: Recognizer, note: String? = null) {
        rec.reset()
        queue.clear()
        awaitDeadlineNanos = 0
        speakDeadlineNanos = 0
        phase = AssistantPhase.LISTENING
        publish { it.copy(phase = AssistantPhase.LISTENING, note = note) }
    }

    // --- Enrollment -------------------------------------------------------------

    /** Request enrollment; the worker picks it up (keeps recognizer single-threaded). */
    fun startEnrollment() { pendingEnroll = true }

    private fun beginEnroll(rec: Recognizer) {
        enrollVectors.clear()
        enrolling = true
        val now = System.nanoTime()
        enrollStartNanos = now
        enrollDeadlineNanos = now + ENROLL_TIMEOUT_NANOS
        phase = AssistantPhase.ENROLLING
        rec.reset()
        queue.clear()
        HushaiLog.info("enroll: started")
        publish { it.copy(phase = AssistantPhase.ENROLLING, enrollProgress = 0, note = "keep talking for a few seconds…") }
    }

    private fun collectEnrollment(spk: FloatArray?) {
        if (spk != null && spk.isNotEmpty()) {
            enrollVectors.add(spk)
            HushaiLog.info("enroll: captured voiceprint #${enrollVectors.size} (dim=${spk.size})")
            val progress = (enrollVectors.size * 100 / ENROLL_TARGET).coerceAtMost(99)
            publish { it.copy(enrollProgress = progress) }
        } else {
            HushaiLog.info("enroll: utterance had no voiceprint (spk null) — keep talking")
        }
        // Done when we have enough samples, OR at least one after a few seconds of
        // audio (handles a single continuous utterance with no pauses).
        val elapsed = System.nanoTime() - enrollStartNanos
        if (enrollVectors.size >= ENROLL_TARGET ||
            (enrollVectors.isNotEmpty() && elapsed >= MIN_ENROLL_NANOS)
        ) {
            finishEnroll()
        }
    }

    private fun finishEnroll() {
        val centroid = SpeakerMath.centroid(enrollVectors)
        enrolling = false
        enrollDeadlineNanos = 0
        phase = AssistantPhase.LISTENING
        recognizer?.reset()
        if (centroid != null) {
            ownerEmbedding = centroid
            onEnrollComplete(centroid)
            HushaiLog.info("enroll: complete (${enrollVectors.size} sample(s))")
            publish { it.copy(phase = AssistantPhase.LISTENING, enrolled = true, enrollProgress = 100, note = "enrolled ✓") }
        } else {
            publish { it.copy(phase = AssistantPhase.LISTENING, note = "enrollment failed — try again") }
        }
    }

    fun stop() {
        running = false
        worker?.join(2_000)
        worker = null
        queue.clear()
        runCatching { recognizer?.close() }
        recognizer = null
        runCatching { audioPlayer.stop() }
        runCatching { speakExecutor.shutdownNow() }
        AssistantBus.update { it.copy(phase = AssistantPhase.OFF, enabled = false) }
    }

    private fun extractText(json: String): String =
        runCatching { JSONObject(json).optString("text") }.getOrDefault("")

    private fun extractPartial(json: String): String =
        runCatching { JSONObject(json).optString("partial") }.getOrDefault("")

    private fun extractSpk(json: String): FloatArray? = runCatching {
        val arr = JSONObject(json).optJSONArray("spk") ?: return null
        FloatArray(arr.length()) { arr.getDouble(it).toFloat() }
    }.getOrNull()

    private fun publish(transform: (com.hushai.android.util.AssistantStatus) -> com.hushai.android.util.AssistantStatus) =
        AssistantBus.update(transform)

    companion object {
        private const val SAMPLE_RATE = 16_000
        private const val QUEUE_CAPACITY = 64
        private const val SPEAKER_THRESHOLD = 0.5f
        private const val ENROLL_TARGET = 2 // separate utterances; 1 + enough audio also completes
        private const val MIN_ENROLL_NANOS = 5_000_000_000L // 1 voiceprint after ~5s of audio is enough
        private const val MIN_QUESTION_WORDS = 2
        private const val QUESTION_TIMEOUT_NANOS = 8_000_000_000L
        // Watchdog backstop covering backend synthesis + network + playback.
        private const val SPEAK_TIMEOUT_NANOS = 60_000_000_000L
        private const val ENROLL_TIMEOUT_NANOS = 20_000_000_000L // finish with what we have after 20s
    }
}
