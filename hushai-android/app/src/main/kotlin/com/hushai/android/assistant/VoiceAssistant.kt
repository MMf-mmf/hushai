package com.hushai.android.assistant

import android.content.Context
import com.hushai.android.capture.PcmSink
import com.hushai.android.net.AdvisorClient
import com.hushai.android.net.DetectiveClient
import com.hushai.android.net.RagChatClient
import com.hushai.android.net.RagClient
import com.hushai.android.net.TtsClient
import com.hushai.android.util.AssistantBus
import com.hushai.android.util.AssistantPhase
import com.hushai.android.util.HushaiLog
import org.json.JSONObject
import org.vosk.Recognizer
import java.util.Locale
import java.util.TimeZone
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
 *
 * Q&A goes to the RICH `/v1/rag/chat` pipeline via [chatClient] (auto-router, sessions +
 * history, condensation, deterministic identity/recency/roster answers, and the system
 * context briefing). [voiceSession] carries the session id across spoken turns so follow-ups
 * condense correctly. The legacy single-shot [ragClient] (`/v1/rag/query`) + on-device
 * [ReflectionIntent] routing are retained ONLY as the fallback for an older server that 404s
 * the chat endpoint.
 */
class VoiceAssistant(
    private val context: Context,
    private val deviceId: String,
    initialWakeWord: String,
    private val chatClient: RagChatClient,
    private val ragClient: RagClient,
    private val ttsClient: TtsClient,
    private val voiceSession: VoiceSession,
    /** The advisor consult client + its own (30-min) session — invoked by name ("advisor …"). */
    private val advisorClient: AdvisorClient,
    private val advisorSession: VoiceSession,
    /** The Gotham "Detective" client + its own (30-min) session — invoked by name ("detective …").
     *  Rides the SAME `/v1/rag/chat` pipeline as the RAG path, only `agent_id="gotham"` differs. */
    private val detectiveClient: DetectiveClient,
    private val detectiveSession: VoiceSession,
    initialOwnerProfile: SpeakerMath.OwnerProfile?,
    /** Persist the SERIALIZED multi-vector profile (SpeakerMath.formatProfile). Called on a
     *  verified enrollment commit and on each guarded adaptation append. */
    private val onEnrollComplete: (String) -> Unit,
) : PcmSink {

    @Volatile var wakeWord: String = SpeakerMath.normalizeWord(initialWakeWord)
        set(value) { field = SpeakerMath.normalizeWord(value) }

    @Volatile private var ownerProfile: SpeakerMath.OwnerProfile? = initialOwnerProfile

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
    // The phase to resume into once the current TTS playback finishes (set by the speak thread,
    // consumed by the worker) — answer-TTS resumes LISTENING, questions-TTS resumes AWAIT_FOLLOWUP,
    // the bare-advisor prompt resumes AWAIT_QUESTION. Null = nothing pending.
    @Volatile private var pendingResumeTarget: AssistantPhase? = null
    // A bare "⟨wake⟩ advisor" is awaiting its question: the next verified AWAIT_QUESTION utterance
    // routes to the advisor rather than the rag assistant. Worker-thread only.
    private var advisorPending = false
    // The follow-up round the advisor is currently on (for the followup-captured marker). Worker only.
    private var advisorRound = 0
    // A bare "⟨wake⟩ detective" is awaiting its question (mirrors advisorPending). Worker-thread only.
    private var detectivePending = false
    // Which persona owns the CURRENT AWAIT_FOLLOWUP window, so the shared follow-up code (owner
    // utterance dispatch + the window-open/timeout markers) routes to the right consult. Worker only.
    private var followupAgent = FollowupAgent.NONE

    private enum class FollowupAgent { NONE, ADVISOR, DETECTIVE }

    // Guided-enrollment state (worker thread only): accepted core samples, the current prompt
    // index, reject strikes, whether we're at the final self-verification step, and whether
    // that step already burned its one retry.
    private val enrollVectors = ArrayList<FloatArray>()
    private var enrolling = false
    private var enrollVerifying = false
    private var enrollVerifyRetried = false
    private var enrollStrikes = 0
    private var awaitDeadlineNanos = 0L
    private var speakDeadlineNanos = 0L
    private var enrollStepDeadlineNanos = 0L

    fun start() {
        if (running) return
        running = true
        // Model load is slow (unpack + mmap) — do it off the caller's thread.
        Thread({ init() }, "hushai-va-init").start()
    }

    private fun init() {
        publish { it.copy(enabled = true, phase = AssistantPhase.LISTENING, enrolled = ownerProfile != null) }
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
        HushaiLog.info("voice assistant ready (wake='$wakeWord' enrolled=${ownerProfile != null})")
    }

    override fun onPcm(data: ByteArray, length: Int) {
        if (!running) return
        // Only capture while listening — never during THINKING/SPEAKING (avoids
        // hearing our own spoken answer and wasting CPU).
        val p = phase
        if (p != AssistantPhase.LISTENING && p != AssistantPhase.AWAIT_QUESTION &&
            p != AssistantPhase.AWAIT_FOLLOWUP && p != AssistantPhase.ENROLLING) return
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
            val target = pendingResumeTarget
            if (target != null) {
                pendingResumeTarget = null
                // Only resume FROM speaking — if the watchdog already bailed us out, drop the stale
                // signal so a completed-late TTS can't reopen a window we already left.
                if (phase == AssistantPhase.SPEAKING) resumeInto(rec, target)
            }
            val now = System.nanoTime()
            if (phase == AssistantPhase.AWAIT_QUESTION && awaitDeadlineNanos > 0L && now >= awaitDeadlineNanos) {
                awaitDeadlineNanos = 0
                resumeListening(rec, note = "no question heard")
            }
            // Follow-up window: the owner has a wake-word-free window to answer the advisor's
            // questions; on timeout resume listening — the consult survives in advisorSession, so
            // a wake-prefixed "advisor …" continues the same server session.
            if (phase == AssistantPhase.AWAIT_FOLLOWUP && awaitDeadlineNanos > 0L && now >= awaitDeadlineNanos) {
                awaitDeadlineNanos = 0
                // Marker before resumeListening (which clears followupAgent). The consult survives in
                // its session, so a wake-prefixed "advisor …"/"detective …" continues it.
                if (followupAgent == FollowupAgent.DETECTIVE) {
                    HushaiLog.info("detective followup timeout — resuming listen")
                } else {
                    HushaiLog.info("advisor followup timeout — resuming listen")
                }
                resumeListening(rec)
            }
            // Watchdog: never get stuck SPEAKING if synth/playback hangs past the deadline.
            if (phase == AssistantPhase.SPEAKING && speakDeadlineNanos > 0L && now >= speakDeadlineNanos) {
                HushaiLog.warn("speak did not finish in time — resuming")
                resumeListening(rec)
            }
            // Per-step enrollment deadline: no usable utterance for this prompt in time
            // counts as a strike (three strikes aborts, old profile untouched).
            if (enrolling && enrollStepDeadlineNanos > 0L && now >= enrollStepDeadlineNanos) {
                enrollStrike(rec, "didn't hear that — let's try the phrase again")
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
        if (enrolling) {
            if (enrollVerifying) collectVerify(rec, spk) else collectEnrollment(rec, text, spk)
            return
        }
        when (phase) {
            AssistantPhase.LISTENING -> {
                val wake = SpeakerMath.tokens(wakeWord)
                if (wake.isEmpty()) return
                val orig = SpeakerMath.tokens(text)
                val lower = orig.map { it.lowercase(Locale.US) }
                val at = SpeakerMath.subsequenceIndex(lower, wake)
                if (at < 0) return
                publish { it.copy(lastHeard = text, note = null) }
                val tailTokens =
                    if (at + wake.size < orig.size) orig.subList(at + wake.size, orig.size) else emptyList()
                when (val route = AssistantRouting.route(tailTokens)) {
                    is AssistantRouting.Route.Advisor -> {
                        // "⟨wake⟩ advisor ⟨question⟩" in one utterance — verify on it, then consult.
                        if (verifyOwner(spk)) advisorConsult(rec, route.question, continueSession = true)
                        else reject(rec)
                    }
                    AssistantRouting.Route.AdvisorBare -> {
                        // Bare "⟨wake⟩ advisor": prompt for the question; the next VERIFIED utterance
                        // routes to the advisor (verify happens on that longer, better voice sample).
                        HushaiLog.info("advisor route: awaiting question")
                        advisorPending = true
                        speakThen("What would you like advice on?", AssistantPhase.AWAIT_QUESTION)
                    }
                    is AssistantRouting.Route.Detective -> {
                        // "⟨wake⟩ detective ⟨question⟩" in one utterance — verify on it, then investigate.
                        if (verifyOwner(spk)) {
                            HushaiLog.info("detective owner verified")
                            detectiveConsult(rec, route.question, ownerVerified(spk), continueSession = true)
                        } else reject(rec)
                    }
                    AssistantRouting.Route.DetectiveBare -> {
                        // Bare "⟨wake⟩ detective": prompt for the question (mirrors AdvisorBare).
                        HushaiLog.info("detective route: awaiting question")
                        detectivePending = true
                        speakThen("What would you like me to investigate?", AssistantPhase.AWAIT_QUESTION)
                    }
                    is AssistantRouting.Route.Rag -> {
                        if (tailTokens.size >= MIN_QUESTION_WORDS) {
                            // Single utterance: wake + question together — verify on it.
                            if (verifyOwner(spk)) answer(rec, route.question, ownerVerified(spk)) else reject(rec)
                        } else {
                            // Bare wake word: wait for the question (a longer, better voice sample).
                            // Clear BOTH consult-pending flags — a fresh bare wake is a plain-RAG
                            // question until proven otherwise, so a stale advisor/detective-pending
                            // from an abandoned bare consult must not capture it (would misroute).
                            advisorPending = false
                            detectivePending = false
                            rec.reset()
                            phase = AssistantPhase.AWAIT_QUESTION
                            awaitDeadlineNanos = System.nanoTime() + QUESTION_TIMEOUT_NANOS
                            publish { it.copy(phase = AssistantPhase.AWAIT_QUESTION, note = null) }
                        }
                    }
                }
            }
            AssistantPhase.AWAIT_QUESTION -> {
                if (text.isBlank()) return
                publish { it.copy(lastHeard = text) }
                if (!verifyOwner(spk)) { reject(rec); return }
                // A pending bare-advisor invocation ("⟨wake⟩ advisor" → prompt) routes this whole
                // utterance to the advisor as the question.
                if (advisorPending) {
                    advisorPending = false
                    advisorConsult(rec, text, continueSession = true)
                    return
                }
                // A pending bare-detective invocation routes this whole utterance as the question.
                if (detectivePending) {
                    detectivePending = false
                    HushaiLog.info("detective owner verified")
                    detectiveConsult(rec, text, ownerVerified(spk), continueSession = true)
                    return
                }
                // Otherwise re-route the utterance: this also covers the two-clip rig path, where the
                // wake ("computer") and "advisor/detective ⟨question⟩" arrive as SEPARATE utterances —
                // the wake lands here as a bare AWAIT_QUESTION, and the "advisor …"/"detective …"
                // utterance must still route by name rather than becoming a rag question.
                when (val route = AssistantRouting.route(SpeakerMath.tokens(text))) {
                    is AssistantRouting.Route.Advisor -> advisorConsult(rec, route.question, continueSession = true)
                    AssistantRouting.Route.AdvisorBare -> {
                        HushaiLog.info("advisor route: awaiting question")
                        advisorPending = true
                        speakThen("What would you like advice on?", AssistantPhase.AWAIT_QUESTION)
                    }
                    is AssistantRouting.Route.Detective -> {
                        HushaiLog.info("detective owner verified")
                        detectiveConsult(rec, route.question, ownerVerified(spk), continueSession = true)
                    }
                    AssistantRouting.Route.DetectiveBare -> {
                        HushaiLog.info("detective route: awaiting question")
                        detectivePending = true
                        speakThen("What would you like me to investigate?", AssistantPhase.AWAIT_QUESTION)
                    }
                    is AssistantRouting.Route.Rag -> answer(rec, text, ownerVerified(spk))
                }
            }
            AssistantPhase.AWAIT_FOLLOWUP -> {
                if (text.isBlank()) return
                val forDetective = followupAgent == FollowupAgent.DETECTIVE
                // Per-turn owner verify: a rejected speaker is IGNORED and the phase stays
                // AWAIT_FOLLOWUP until the deadline — a stranger must not consume the owner's window.
                if (!verifyOwner(spk)) {
                    HushaiLog.info(
                        if (forDetective) "speaker rejected (detective follow-up) — ignoring, still awaiting"
                        else "speaker rejected (advisor follow-up) — ignoring, still awaiting",
                    )
                    return
                }
                // "cancel"/"never mind" aborts the round (server session kept), returning to LISTENING.
                if (AssistantRouting.isAbort(text)) {
                    HushaiLog.info(if (forDetective) "detective follow-up aborted by user" else "advisor follow-up aborted by user")
                    speakAnswer("Okay.")
                    return
                }
                publish { it.copy(lastHeard = text) }
                if (forDetective) {
                    // The detective follow-up is the spoken yes/no to a confirm gate; the server
                    // intercepts it before routing (execution additionally needs owner_verified).
                    HushaiLog.info("detective followup captured (words=${SpeakerMath.tokens(text).size})")
                    detectiveConsult(rec, text, ownerVerified(spk), continueSession = true)
                } else {
                    HushaiLog.info("advisor followup captured (round=$advisorRound, words=${SpeakerMath.tokens(text).size})")
                    advisorConsult(rec, text, continueSession = true)
                }
            }
            else -> {}
        }
    }

    /**
     * Blocks the worker on the RAG-chat call, then speaks the answer (resume on speak-done).
     * `ownerVerified` is the on-device voice-ID result for this utterance — sent to the server so a
     * verified owner gets identity-aware answers ("what's my name") and first-person resolution.
     */
    private fun answer(rec: Recognizer, question: String, ownerVerified: Boolean) {
        // "New chat" / "start over" is handled on-device: drop the session so the next spoken
        // question opens a fresh conversation (no stale history/condensation), acknowledge out
        // loud, and never send the phrase to the server.
        if (VoiceSession.isResetCommand(question)) {
            voiceSession.reset()
            HushaiLog.info("voice session reset by spoken command")
            publish { it.copy(lastQuestion = question) }
            speakAnswer("Okay, starting fresh.")
            return
        }
        phase = AssistantPhase.THINKING
        publish { it.copy(phase = AssistantPhase.THINKING, lastQuestion = question, note = null) }
        val now = System.currentTimeMillis()
        val tzOffsetSecs = TimeZone.getDefault().getOffset(now) / 1000L
        when (val r = chatClient.chat(question, voiceSession.currentOrNull(now), ownerVerified, deviceId, tzOffsetSecs)) {
            is RagChatClient.Result.Answer -> {
                voiceSession.record(r.sessionId, now)
                HushaiLog.info("rag chat ok (session=${r.sessionId} routed=${r.routedAgentId})")
                speakAnswer(r.text)
            }
            RagChatClient.Result.SessionNotFound -> {
                // The stored session was pruned / the DB was wiped — forget it and retry once fresh.
                HushaiLog.info("rag chat session gone — retrying sessionless")
                voiceSession.reset()
                when (val retry = chatClient.chat(question, null, ownerVerified, deviceId, tzOffsetSecs)) {
                    is RagChatClient.Result.Answer -> {
                        voiceSession.record(retry.sessionId, now)
                        speakAnswer(retry.text)
                    }
                    else -> {
                        publish { it.copy(note = "RAG error") }
                        resumeListening(rec)
                    }
                }
            }
            RagChatClient.Result.EndpointMissing -> {
                // Older server without /v1/rag/chat — fall back to the single-shot path.
                HushaiLog.info("rag chat endpoint missing — falling back to /v1/rag/query")
                fallbackAnswer(rec, question)
            }
            is RagChatClient.Result.Error -> {
                publish { it.copy(note = "RAG error: ${r.reason}") }
                resumeListening(rec)
            }
        }
    }

    /** Legacy single-shot answer via `/v1/rag/query` + on-device intent routing (older-server fallback). */
    private fun fallbackAnswer(rec: Recognizer, question: String) {
        val agentId = ReflectionIntent.agentFor(question)
        when (val r = ragClient.ask(question, deviceId, agentId)) {
            is RagClient.Result.Answer -> speakAnswer(r.text)
            is RagClient.Result.Error -> {
                publish { it.copy(note = "RAG error: ${r.reason}") }
                resumeListening(rec)
            }
        }
    }

    /** Transition to SPEAKING and play `text`, then resume into `resumeTo` once playback finishes. */
    private fun speakThen(text: String, resumeTo: AssistantPhase) {
        phase = AssistantPhase.SPEAKING
        speakDeadlineNanos = System.nanoTime() + SPEAK_TIMEOUT_NANOS
        publish { it.copy(phase = AssistantPhase.SPEAKING, lastAnswer = text) }
        speak(text, resumeTo)
    }

    /** Speak an answer, then return to LISTENING (the default terminal spoken reply). */
    private fun speakAnswer(text: String) = speakThen(text, AssistantPhase.LISTENING)

    /** Speak the advisor's follow-up questions, then open the wake-word-free answer window. */
    private fun speakQuestions(text: String) = speakThen(text, AssistantPhase.AWAIT_FOLLOWUP)

    private fun verifyOwner(spk: FloatArray?): Boolean {
        val owner = ownerProfile ?: run {
            publish { it.copy(note = "not enrolled — responding to anyone") }
            return true
        }
        if (spk == null || spk.isEmpty()) {
            HushaiLog.info("speaker verify: no voiceprint captured")
            publish { it.copy(note = "couldn't capture a voiceprint") }
            return false
        }
        val sim = SpeakerMath.topKMeanSim(spk, owner.all())
        HushaiLog.info("speaker cosine=$sim (threshold=$SPEAKER_THRESHOLD)")
        if (sim >= SPEAKER_THRESHOLD) {
            maybeAdapt(spk, sim)
            return true
        }
        return false
    }

    /**
     * Whether we can CLAIM this utterance is the verified owner: only when a voiceprint IS enrolled
     * AND it matches. An unenrolled assistant answers anyone ([verifyOwner] returns true) but must
     * NOT assert owner identity to the server, so this returns false there.
     */
    private fun ownerVerified(spk: FloatArray?): Boolean {
        val owner = ownerProfile ?: return false
        if (spk == null || spk.isEmpty()) return false
        return SpeakerMath.topKMeanSim(spk, owner.all()) >= SPEAKER_THRESHOLD
    }

    /**
     * Guarded adaptation: append this utterance's vector as an ADAPTED slot only on a
     * high-confidence match (>= [ADAPT_FLOOR], far above the accept gate — never adapt in the
     * [SPEAKER_THRESHOLD, ADAPT_FLOOR) band, so a borderline impostor can't poison the
     * profile). Capped ring of adapted slots; the guided core samples are never evicted.
     * Handles slow acoustic drift (new room, new distance) across sessions.
     */
    private fun maybeAdapt(spk: FloatArray, sim: Float) {
        if (sim < ADAPT_FLOOR) return
        val owner = ownerProfile ?: return
        val next = owner.withAdapted(spk)
        ownerProfile = next
        onEnrollComplete(SpeakerMath.formatProfile(next))
        HushaiLog.info("owner profile adapted (cosine=$sim, adapted=${next.adapted.size}/${SpeakerMath.MAX_ADAPTED})")
    }

    private fun reject(rec: Recognizer) {
        rec.reset()
        phase = AssistantPhase.LISTENING
        awaitDeadlineNanos = 0
        advisorPending = false
        detectivePending = false
        followupAgent = FollowupAgent.NONE
        HushaiLog.info("speaker rejected (cosine below threshold)")
        publish { it.copy(phase = AssistantPhase.LISTENING, note = "speaker not recognized — ignoring") }
    }

    /**
     * Fetch the spoken answer from the backend and play it, off the worker thread so
     * the loop/watchdog keep running. Always sets [pendingResume] (success or not) so
     * the assistant returns to LISTENING; if audio is unavailable the answer text is
     * still shown.
     */
    private fun speak(text: String, resumeTo: AssistantPhase) {
        speakExecutor.execute {
            val played = runCatching {
                val wav = ttsClient.synthesize(text)
                wav != null && audioPlayer.play(wav)
            }.getOrElse { e -> HushaiLog.error("speak failed", e); false }
            if (!played) HushaiLog.warn("spoken answer unavailable — showing text only")
            // Always signal (success or not) so we never wedge in SPEAKING — the worker resumes
            // into the requested phase; the follow-up window opens even if TTS playback failed.
            pendingResumeTarget = resumeTo
        }
    }

    /** Post-TTS transition on the worker thread: LISTENING (answer), AWAIT_FOLLOWUP (questions),
     *  or AWAIT_QUESTION (the bare-advisor prompt). */
    private fun resumeInto(rec: Recognizer, target: AssistantPhase) {
        when (target) {
            AssistantPhase.AWAIT_FOLLOWUP -> beginFollowupWindow(rec)
            AssistantPhase.AWAIT_QUESTION -> {
                rec.reset()
                queue.clear()
                speakDeadlineNanos = 0
                phase = AssistantPhase.AWAIT_QUESTION
                awaitDeadlineNanos = System.nanoTime() + QUESTION_TIMEOUT_NANOS
                publish { it.copy(phase = AssistantPhase.AWAIT_QUESTION, note = null) }
            }
            else -> resumeListening(rec)
        }
    }

    /** Open the wake-word-free follow-up window. The marker fires HERE (after TTS playback), the
     *  moment the window actually opens — never at synthesis start (the rig's sequencing anchor). */
    private fun beginFollowupWindow(rec: Recognizer) {
        rec.reset()
        queue.clear()
        speakDeadlineNanos = 0
        phase = AssistantPhase.AWAIT_FOLLOWUP
        awaitDeadlineNanos = System.nanoTime() + FOLLOWUP_TIMEOUT_NANOS
        // The window-open anchor (post-TTS): rig sequencing depends on it firing only after playback.
        if (followupAgent == FollowupAgent.DETECTIVE) {
            HushaiLog.info("detective confirm — awaiting yes/no")
        } else {
            HushaiLog.info("advisor questions spoken — awaiting answer")
        }
        publish { it.copy(phase = AssistantPhase.AWAIT_FOLLOWUP, note = null) }
    }

    private fun resumeListening(rec: Recognizer, note: String? = null) {
        rec.reset()
        queue.clear()
        awaitDeadlineNanos = 0
        speakDeadlineNanos = 0
        advisorPending = false
        detectivePending = false
        followupAgent = FollowupAgent.NONE
        phase = AssistantPhase.LISTENING
        publish { it.copy(phase = AssistantPhase.LISTENING, note = note) }
    }

    // --- Advisor consult (the "advisor …" route; §2.3 loop) -----------------------
    //
    // LISTENING --"⟨wake⟩ advisor ⟨q⟩"--> [verify] --> THINKING (advisor chat)
    //   THINKING --Questions--> SPEAKING (intro + numbered questions) --done--> AWAIT_FOLLOWUP (30 s)
    //   AWAIT_FOLLOWUP --owner utterance--> THINKING (same session_id)   [server-capped rounds]
    //   THINKING --Answer--> SPEAKING (advice) --> LISTENING
    // A completed consult returns to LISTENING (the wake word starts the next turn); only follow-up
    // ROUNDS are wake-free. The worker BLOCKS on the advisor call — PCM is dropped during THINKING,
    // so the phone never transcribes its own reply (accepted: the thread is deaf 30–300 s per turn).

    private fun advisorConsult(rec: Recognizer, question: String, continueSession: Boolean) {
        // "new chat"/"start over" resets the consult on-device (never sent to the server).
        if (VoiceSession.isResetCommand(question)) {
            advisorSession.reset()
            HushaiLog.info("advisor session reset by spoken command")
            publish { it.copy(lastQuestion = question) }
            speakAnswer("Okay, starting fresh.")
            return
        }
        phase = AssistantPhase.THINKING
        val now = System.currentTimeMillis()
        val sid = if (continueSession) advisorSession.currentOrNull(now) else null
        HushaiLog.info("advisor route (session=${sid ?: "new"})")
        publish { it.copy(phase = AssistantPhase.THINKING, lastQuestion = question, note = null) }
        deliverAdvisorResult(rec, advisorClient.chat(question, sid), now, question, allowRetry = true)
    }

    /** Dispatch one advisor turn's result: speak questions (→ follow-up window) or the answer,
     *  retry once on a lost session, and speak the graceful fallbacks for Busy/Error. */
    private fun deliverAdvisorResult(
        rec: Recognizer,
        result: AdvisorClient.Result,
        now: Long,
        question: String,
        allowRetry: Boolean,
    ) {
        when (result) {
            is AdvisorClient.Result.Questions -> {
                advisorSession.record(result.sessionId, now)
                advisorRound = result.round
                followupAgent = FollowupAgent.ADVISOR
                HushaiLog.info("advisor questions round=${result.round} count=${result.questions.size} (session=${result.sessionId})")
                speakQuestions(buildQuestionsTts(result.questions))
            }
            is AdvisorClient.Result.Answer -> {
                advisorSession.record(result.sessionId, now)
                val chapters = result.chapters.joinToString(",") { it.no.toString() }
                HushaiLog.info("advisor answer ok (session=${result.sessionId} chapters=[$chapters])")
                speakAnswer(result.text)
            }
            AdvisorClient.Result.SessionNotFound -> {
                if (allowRetry) {
                    // The stored session was pruned / the DB wiped — forget it and retry once fresh.
                    HushaiLog.info("advisor session gone — retrying sessionless")
                    advisorSession.reset()
                    deliverAdvisorResult(rec, advisorClient.chat(question, null), now, question, allowRetry = false)
                } else {
                    HushaiLog.info("advisor error: session lost")
                    speakAnswer("Sorry — the advisor isn't available right now.")
                }
            }
            AdvisorClient.Result.Busy -> {
                HushaiLog.info("advisor busy (409)")
                speakAnswer("The advisor is still thinking about your last question — give it a moment.")
            }
            is AdvisorClient.Result.Error -> {
                // Empty answers map to Error upstream — treated identically (spoken fallback → LISTENING).
                HushaiLog.info("advisor error: ${result.reason}")
                speakAnswer("Sorry — the advisor isn't available right now.")
            }
        }
    }

    /** One TTS synthesis call for a whole gate round: intro + numbered, period-joined questions
     *  (natural pauses; fits the speak watchdog). */
    private fun buildQuestionsTts(questions: List<String>): String {
        if (questions.isEmpty()) return "I need a bit more detail. Could you tell me more about your situation?"
        val sb = StringBuilder("I need a bit more detail.")
        questions.forEachIndexed { i, q ->
            val ordinal = ADVISOR_ORDINALS.getOrElse(i) { (i + 1).toString() }
            sb.append(" Question ").append(ordinal).append(": ").append(q.trim().trimEnd('.')).append(" .")
        }
        return sb.toString()
    }

    // --- Detective consult (the "detective …" route; Gotham §2.7 voice) ------------
    //
    // LISTENING --"⟨wake⟩ detective ⟨q⟩"--> [verify] --> THINKING (gotham chat, agent_id="gotham")
    //   THINKING --Answer--> SPEAKING (investigation result) --> LISTENING
    //   THINKING --Confirm(summary)--> SPEAKING (summary + "say yes to confirm")
    //        --speech done--> AWAIT_FOLLOWUP (30 s) --owner "yes"/"no"--> THINKING (same session;
    //        the server intercepts the yes/no before routing) --> Answer --> SPEAKING --> LISTENING
    // The confirm path is DORMANT in Phase 1 (mutations off) but present + exercised, mirroring the
    // viewer's confirm bubble. The Detective streams a SUPERSET SSE; the voice client ignores every
    // phase/tool trace event and speaks only the answer (unknown-event tolerance in DetectiveClient).
    // The worker BLOCKS on the chat call — PCM is dropped during THINKING, so the phone never
    // transcribes its own reply.

    private fun detectiveConsult(
        rec: Recognizer,
        question: String,
        ownerVerified: Boolean,
        continueSession: Boolean,
    ) {
        // "new chat"/"start over" resets the investigation on-device (never sent to the server).
        if (VoiceSession.isResetCommand(question)) {
            detectiveSession.reset()
            HushaiLog.info("detective session reset by spoken command")
            publish { it.copy(lastQuestion = question) }
            speakAnswer("Okay, starting fresh.")
            return
        }
        phase = AssistantPhase.THINKING
        val now = System.currentTimeMillis()
        val tzOffsetSecs = TimeZone.getDefault().getOffset(now) / 1000L
        val sid = if (continueSession) detectiveSession.currentOrNull(now) else null
        HushaiLog.info("detective route (session=${sid ?: "new"})")
        publish { it.copy(phase = AssistantPhase.THINKING, lastQuestion = question, note = null) }
        deliverDetectiveResult(
            rec,
            detectiveClient.chat(question, sid, ownerVerified, deviceId, tzOffsetSecs),
            now, question, ownerVerified, allowRetry = true,
        )
    }

    /** Dispatch one Detective turn's result: speak the answer, or the confirm summary (→ follow-up
     *  window for the spoken yes/no), retry once on a lost session, else speak a graceful fallback. */
    private fun deliverDetectiveResult(
        rec: Recognizer,
        result: DetectiveClient.Result,
        now: Long,
        question: String,
        ownerVerified: Boolean,
        allowRetry: Boolean,
    ) {
        when (result) {
            is DetectiveClient.Result.Answer -> {
                detectiveSession.record(result.sessionId, now)
                HushaiLog.info("detective answer ok (session=${result.sessionId})")
                speakAnswer(result.text)
            }
            is DetectiveClient.Result.Confirm -> {
                detectiveSession.record(result.sessionId, now)
                followupAgent = FollowupAgent.DETECTIVE
                HushaiLog.info("detective confirm spoken (session=${result.sessionId})")
                // speakQuestions resumes into AWAIT_FOLLOWUP (the yes/no window); the deterministic
                // summary is server-composed (§2.5 — never the model). 30 s window, wake-free.
                speakQuestions("${result.summary} Say yes to confirm.")
            }
            DetectiveClient.Result.SessionNotFound -> {
                if (allowRetry) {
                    // The stored session was pruned / the DB wiped — forget it and retry once fresh.
                    HushaiLog.info("detective session gone — retrying sessionless")
                    detectiveSession.reset()
                    val tz = TimeZone.getDefault().getOffset(now) / 1000L
                    deliverDetectiveResult(
                        rec,
                        detectiveClient.chat(question, null, ownerVerified, deviceId, tz),
                        now, question, ownerVerified, allowRetry = false,
                    )
                } else {
                    HushaiLog.info("detective error: session lost")
                    speakAnswer("Sorry — the detective isn't available right now.")
                }
            }
            DetectiveClient.Result.EndpointMissing -> {
                // Older rag server without the gotham agent — degrade gracefully.
                HushaiLog.info("detective error: endpoint missing")
                speakAnswer("Sorry — the detective isn't available on this server.")
            }
            is DetectiveClient.Result.Error -> {
                // Empty answers map to Error upstream — treated identically (spoken fallback → LISTENING).
                HushaiLog.info("detective error: ${result.reason}")
                speakAnswer("Sorry — the detective isn't available right now.")
            }
        }
    }

    // --- Guided enrollment --------------------------------------------------------
    //
    // Six prompted samples (varied phrases; the last two ask for a different distance /
    // background), each gated by SpeakerMath.enrollGate (real voiceprint, enough speech,
    // consistent with the samples already accepted), then a MANDATORY held-out
    // self-verification that must clear VERIFY_FLOOR before anything is stored. The previous
    // profile stays active until the replacement passes — an interrupted or failed enrollment
    // can no longer wipe a working profile (the old flow committed a possibly-single-sample
    // centroid unconditionally, which is exactly the "works sometimes, forgets my voice"
    // failure the owner reported).

    /** Request enrollment; the worker picks it up (keeps recognizer single-threaded). */
    fun startEnrollment() { pendingEnroll = true }

    private fun beginEnroll(rec: Recognizer) {
        enrollVectors.clear()
        enrolling = true
        enrollVerifying = false
        enrollVerifyRetried = false
        enrollStrikes = 0
        // Enrollment supersedes any in-flight consult: clear the pending/follow-up flags so a bare
        // advisor/detective invocation interrupted by Enroll can't leave a stale flag that misroutes
        // the next question after enrollment returns to LISTENING.
        advisorPending = false
        detectivePending = false
        followupAgent = FollowupAgent.NONE
        phase = AssistantPhase.ENROLLING
        rec.reset()
        queue.clear()
        HushaiLog.info("enroll: started")
        promptEnrollStep(null)
    }

    /** Publish the current prompt (sample N of TOTAL, or the verify phrase) + step deadline. */
    private fun promptEnrollStep(note: String?) {
        enrollStepDeadlineNanos = System.nanoTime() + ENROLL_STEP_TIMEOUT_NANOS
        val prompt = if (enrollVerifying) VERIFY_PROMPT else ENROLL_PROMPTS[enrollVectors.size]
        val step = enrollVectors.size
        publish {
            it.copy(
                phase = AssistantPhase.ENROLLING,
                enrollStep = step,
                enrollTotal = ENROLL_TARGET,
                enrollPrompt = prompt,
                enrollProgress = (step * 100 / (ENROLL_TARGET + 1)).coerceAtMost(99),
                note = note,
            )
        }
    }

    private fun collectEnrollment(rec: Recognizer, text: String, spk: FloatArray?) {
        when (val g = SpeakerMath.enrollGate(spk, SpeakerMath.tokens(text).size, enrollVectors)) {
            is SpeakerMath.GateResult.Reject -> enrollStrike(rec, g.reason)
            SpeakerMath.GateResult.Accept -> {
                enrollVectors.add(spk!!)
                enrollStrikes = 0
                HushaiLog.info("enroll: captured voiceprint #${enrollVectors.size}/$ENROLL_TARGET (dim=${spk.size})")
                if (enrollVectors.size >= ENROLL_TARGET) {
                    enrollVerifying = true
                    HushaiLog.info("enroll: verification step")
                }
                rec.reset()
                promptEnrollStep(if (enrollVerifying) "great — one last check" else "got it ✓")
            }
        }
    }

    /** The held-out self-verification: the new profile must recognize its own owner NOW. */
    private fun collectVerify(rec: Recognizer, spk: FloatArray?) {
        val score = if (spk == null || spk.isEmpty()) -1f
        else SpeakerMath.topKMeanSim(spk, enrollVectors)
        HushaiLog.info("enroll: verify cosine=$score (floor=$VERIFY_FLOOR)")
        if (score >= VERIFY_FLOOR) {
            val profile = SpeakerMath.OwnerProfile(core = enrollVectors.toList(), adapted = emptyList())
            ownerProfile = profile
            onEnrollComplete(SpeakerMath.formatProfile(profile))
            endEnroll(rec)
            HushaiLog.info("enroll: complete (${profile.core.size} sample(s), verified)")
            publish { it.copy(phase = AssistantPhase.LISTENING, enrolled = true, enrollProgress = 100, enrollPrompt = null, note = "enrolled ✓ (voice check passed)") }
        } else if (!enrollVerifyRetried) {
            enrollVerifyRetried = true
            rec.reset()
            promptEnrollStep("hmm, that didn't match — one more try")
        } else {
            endEnroll(rec)
            HushaiLog.info("enroll: failed verification — previous profile kept")
            publish { it.copy(phase = AssistantPhase.LISTENING, enrolled = ownerProfile != null, enrollPrompt = null, note = "enrollment didn't pass the voice check — your previous profile is unchanged; try again somewhere quieter") }
        }
    }

    /** A rejected/missed sample: strike, re-prompt, or abort after three strikes. */
    private fun enrollStrike(rec: Recognizer, reason: String) {
        enrollStrikes++
        if (enrollStrikes >= ENROLL_MAX_STRIKES) {
            endEnroll(rec)
            HushaiLog.info("enroll: aborted after $ENROLL_MAX_STRIKES strikes — previous profile kept")
            publish { it.copy(phase = AssistantPhase.LISTENING, enrolled = ownerProfile != null, enrollPrompt = null, note = "enrollment cancelled — try again in a quieter spot") }
        } else {
            rec.reset()
            promptEnrollStep(reason)
        }
    }

    private fun endEnroll(rec: Recognizer) {
        enrolling = false
        enrollVerifying = false
        enrollStepDeadlineNanos = 0
        phase = AssistantPhase.LISTENING
        rec.reset()
        queue.clear()
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
        // The accept gate. UNCHANGED on purpose (cardinal rule: never loosen a gate) — the
        // multi-vector top-2-mean scoring raises the OWNER's score across environments while
        // measured stranger scores (~0.41-0.42 per vector) stay under it.
        private const val SPEAKER_THRESHOLD = 0.5f
        private const val MIN_QUESTION_WORDS = 2
        private const val QUESTION_TIMEOUT_NANOS = 8_000_000_000L
        // The owner composes an answer to up to 3 follow-up questions — 8 s is too tight, 30 s fits.
        private const val FOLLOWUP_TIMEOUT_NANOS = 30_000_000_000L
        // Watchdog backstop covering backend synthesis + network + playback.
        private const val SPEAK_TIMEOUT_NANOS = 60_000_000_000L
        // Spoken ordinals for the numbered follow-up questions ("Question one: …").
        private val ADVISOR_ORDINALS = listOf("one", "two", "three", "four", "five")

        // Guided enrollment: six prompted samples + a held-out verification.
        private const val ENROLL_TARGET = 6
        private const val ENROLL_STEP_TIMEOUT_NANOS = 15_000_000_000L
        private const val ENROLL_MAX_STRIKES = 3
        // The new profile must pass its own voice check with margin above the accept gate
        // before it replaces anything.
        private const val VERIFY_FLOOR = 0.55f
        // Adaptation only far above the gate — a borderline match must never write the profile.
        private const val ADAPT_FLOOR = 0.70f
        private val ENROLL_PROMPTS = listOf(
            "Say: the quick brown fox jumps over the lazy dog",
            "Say: my voice is my passport, please verify me",
            "Say: I am teaching this assistant to know my voice",
            "Say: seven green apples fell from the old oak tree",
            "Step a few feet back, then say: I am speaking from across the room",
            "In your normal voice, say: this is how I usually talk every day",
        )
        private const val VERIFY_PROMPT =
            "Last check — say: it's really me, open up"
    }
}
