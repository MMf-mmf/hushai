package com.hushai.android.assistant

import java.util.Locale

/**
 * Decides where an utterance goes AFTER the wake word is stripped. Pure and side-effect-free so
 * the routing contract is unit-testable without the Vosk loop.
 *
 * Two personas are invoked by NAME — token[0] of the tail must be the keyword. The advisor
 * (book-grounded consult) triggers on "advisor" (the ASR "adviser" spelling is normalized here,
 * NOT aliased); the Gotham Detective (investigation over the entity graph, `agent_id="gotham"`)
 * triggers on "detective" (with "inspector"/"sherlock" accepted as Vosk-orthography fallbacks — the
 * §2.7 risk note: whichever the small acoustic model recognizes reliably wins, mirroring how
 * "adviser" hedges "advisor"). A keyword anywhere else in the utterance ("is my advisor lying",
 * "the detective show") is a plain RAG question — a keyword only counts as the first token.
 */
object AssistantRouting {

    sealed interface Route {
        /** A recordings/reflection question for the RAG assistant (the existing path). */
        data class Rag(val question: String) : Route
        /** "⟨wake⟩ advisor" with no real question — prompt for one, then await it. */
        data object AdvisorBare : Route
        /** "⟨wake⟩ advisor ⟨question⟩" — consult the advisor with [question]. */
        data class Advisor(val question: String) : Route
        /** "⟨wake⟩ detective" with no real question — prompt for one, then await it. */
        data object DetectiveBare : Route
        /** "⟨wake⟩ detective ⟨question⟩" — investigate with the Gotham Detective (`agent_id="gotham"`). */
        data class Detective(val question: String) : Route
    }

    /** ASR orthographies of the trigger word (normalization, not an alias — no "Ahithophel"). */
    private val ADVISOR_KEYWORDS = setOf("advisor", "adviser")

    /** Detective trigger + its Vosk-orthography fallbacks (§2.7). "detective" is primary; the
     *  fallbacks are accepted so a small-model that mishears the primary still routes — the same
     *  hedge "adviser" gives "advisor". All disjoint from [ADVISOR_KEYWORDS]. */
    private val DETECTIVE_KEYWORDS = setOf("detective", "inspector", "sherlock")

    /** A tail must carry at least this many words to count as a real question (mirrors the
     *  VoiceAssistant wake-path threshold), else it's treated as a bare invocation. */
    const val MIN_QUESTION_WORDS = 2

    /**
     * @param tailTokens the tokens that follow the wake word, in original order/case.
     */
    fun route(tailTokens: List<String>): Route {
        val head = tailTokens.firstOrNull()?.lowercase(Locale.US)
        val remainder = tailTokens.drop(1)
        if (head != null && head in ADVISOR_KEYWORDS) {
            return if (remainder.size >= MIN_QUESTION_WORDS) {
                Route.Advisor(remainder.joinToString(" "))
            } else {
                Route.AdvisorBare
            }
        }
        if (head != null && head in DETECTIVE_KEYWORDS) {
            return if (remainder.size >= MIN_QUESTION_WORDS) {
                Route.Detective(remainder.joinToString(" "))
            } else {
                Route.DetectiveBare
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
