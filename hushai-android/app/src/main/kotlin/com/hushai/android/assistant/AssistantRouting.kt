package com.hushai.android.assistant

import java.util.Locale

/**
 * Decides where an utterance goes AFTER the wake word is stripped. Pure and side-effect-free so
 * the routing contract is unit-testable without the Vosk loop.
 *
 * The advisor is invoked by NAME — token[0] of the tail must be the keyword "advisor" (the ASR
 * "adviser" spelling is normalized here, NOT aliased). "advisor" anywhere else in the utterance
 * ("is my advisor lying") is a plain RAG question — the keyword only counts as the first token.
 */
object AssistantRouting {

    sealed interface Route {
        /** A recordings/reflection question for the RAG assistant (the existing path). */
        data class Rag(val question: String) : Route
        /** "⟨wake⟩ advisor" with no real question — prompt for one, then await it. */
        data object AdvisorBare : Route
        /** "⟨wake⟩ advisor ⟨question⟩" — consult the advisor with [question]. */
        data class Advisor(val question: String) : Route
    }

    /** ASR orthographies of the trigger word (normalization, not an alias — no "Ahithophel"). */
    private val ADVISOR_KEYWORDS = setOf("advisor", "adviser")

    /** A tail must carry at least this many words to count as a real question (mirrors the
     *  VoiceAssistant wake-path threshold), else it's treated as a bare invocation. */
    const val MIN_QUESTION_WORDS = 2

    /**
     * @param tailTokens the tokens that follow the wake word, in original order/case.
     */
    fun route(tailTokens: List<String>): Route {
        val head = tailTokens.firstOrNull()?.lowercase(Locale.US)
        if (head != null && head in ADVISOR_KEYWORDS) {
            val remainder = tailTokens.drop(1)
            return if (remainder.size >= MIN_QUESTION_WORDS) {
                Route.Advisor(remainder.joinToString(" "))
            } else {
                Route.AdvisorBare
            }
        }
        return Route.Rag(tailTokens.joinToString(" "))
    }

    /** In the follow-up window, these abort the current round (server session kept). */
    private val ABORT_PHRASES = setOf("cancel", "never mind", "nevermind", "stop")

    fun isAbort(text: String): Boolean {
        val norm = text
            .lowercase(Locale.US)
            .replace(Regex("[^a-z ]"), " ")
            .split(" ")
            .filter { it.isNotBlank() }
            .joinToString(" ")
        return norm in ABORT_PHRASES
    }
}
