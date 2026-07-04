package com.hushai.android.assistant

/**
 * Tracks the voice assistant's current `/v1/rag/chat` session id so spoken follow-ups continue the
 * same conversation (server-side query condensation + history need a stable session_id). A session
 * expires after [IDLE_WINDOW_MILLIS] of silence: long enough for a natural spoken back-and-forth,
 * short enough that tomorrow's first question starts fresh so condensation never drags in stale
 * context.
 *
 * Persistence is injected as callbacks (the same idiom as [VoiceAssistant]'s `onEnrollComplete`), so
 * this class stays pure and unit-testable while the real wiring stores the pair in DataStore — which
 * survives the assistant being torn down and rebuilt (capture stop/start, battery-saver kill, a quick
 * process restart) without orphaning the conversation. All methods run on the single VoiceAssistant
 * worker thread, so no internal synchronization is needed.
 */
class VoiceSession(
    private val load: () -> Pair<String, Long>?,
    private val save: (String, Long) -> Unit,
    private val clear: () -> Unit,
) {
    private var sessionId: String? = null
    private var lastTurnAtMillis: Long = 0L
    private var loaded = false

    private fun ensureLoaded() {
        if (loaded) return
        load()?.let { (id, at) ->
            sessionId = id
            lastTurnAtMillis = at
        }
        loaded = true
    }

    /**
     * The session id to continue, or null to start a new one. Returns the stored id only when the
     * last turn was within [IDLE_WINDOW_MILLIS]; past that (or never set) it returns null so the
     * next turn opens a fresh session.
     */
    fun currentOrNull(nowMillis: Long): String? {
        ensureLoaded()
        val id = sessionId ?: return null
        return if (nowMillis - lastTurnAtMillis < IDLE_WINDOW_MILLIS) id else null
    }

    /** Record the session id returned by the server and stamp the turn time (persisted). */
    fun record(sessionId: String, nowMillis: Long) {
        ensureLoaded()
        this.sessionId = sessionId
        this.lastTurnAtMillis = nowMillis
        save(sessionId, nowMillis)
    }

    /** Forget the session (e.g. the server reported it gone) so the next turn starts fresh. */
    fun reset() {
        ensureLoaded()
        sessionId = null
        lastTurnAtMillis = 0L
        clear()
    }

    companion object {
        const val IDLE_WINDOW_MILLIS = 15L * 60L * 1000L // 15 minutes
    }
}
