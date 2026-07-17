package com.hushai.android

import com.hushai.android.assistant.AssistantRouting
import com.hushai.android.assistant.AssistantRouting.Route
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The pure post-wake-word routing decision: advisor-by-name vs the RAG assistant. "advisor" only
 * counts as the trigger when it is token[0] of the tail; the ASR "adviser" spelling normalizes to it.
 */
class AssistantRoutingTest {

    private fun tail(s: String) = s.split(" ").filter { it.isNotBlank() }

    @Test fun advisorPlusQuestionRoutesToAdvisor() {
        val r = AssistantRouting.route(tail("advisor should I confront my business partner"))
        assertTrue(r is Route.Advisor)
        assertEquals("should I confront my business partner", (r as Route.Advisor).question)
    }

    @Test fun bareAdvisorRoutesToAdvisorBare() {
        assertEquals(Route.AdvisorBare, AssistantRouting.route(tail("advisor")))
    }

    @Test fun adviserSpellingNormalizesToAdvisor() {
        val r = AssistantRouting.route(tail("adviser what should I do about my lease"))
        assertTrue(r is Route.Advisor)
        assertEquals("what should I do about my lease", (r as Route.Advisor).question)
    }

    @Test fun noKeywordRoutesToRag() {
        val r = AssistantRouting.route(tail("who did I see today"))
        assertTrue(r is Route.Rag)
        assertEquals("who did I see today", (r as Route.Rag).question)
    }

    @Test fun advisorMidQuestionRoutesToRag() {
        // The keyword only counts as token 0 — "advisor" inside a question is a plain rag query.
        val r = AssistantRouting.route(tail("is my advisor lying"))
        assertTrue(r is Route.Rag)
        assertEquals("is my advisor lying", (r as Route.Rag).question)
    }

    @Test fun emptyTailIsRag() {
        val r = AssistantRouting.route(emptyList())
        assertTrue(r is Route.Rag)
        assertEquals("", (r as Route.Rag).question)
    }

    @Test fun advisorWithOneWordRemainderIsBare() {
        // "advisor hi" — a 1-word remainder isn't a real question, so prompt for one.
        assertEquals(Route.AdvisorBare, AssistantRouting.route(tail("advisor hi")))
    }

    @Test fun abortPhrasesRecognizedInFollowup() {
        for (p in listOf("cancel", "never mind", "nevermind", "Stop.")) assertTrue(p, AssistantRouting.isAbort(p))
        for (q in listOf("cancel the meeting tomorrow", "I never mind the noise", "")) assertFalse(q, AssistantRouting.isAbort(q))
    }

    // --- Gotham Detective route (agent_id="gotham"; keyword "detective" + Vosk fallbacks) --------

    @Test fun detectivePlusQuestionRoutesToDetective() {
        val r = AssistantRouting.route(tail("detective who did Alice arrive with yesterday"))
        assertTrue(r is Route.Detective)
        assertEquals("who did Alice arrive with yesterday", (r as Route.Detective).question)
    }

    @Test fun bareDetectiveRoutesToDetectiveBare() {
        assertEquals(Route.DetectiveBare, AssistantRouting.route(tail("detective")))
    }

    @Test fun detectiveWithOneWordRemainderIsBare() {
        // "detective hey" — a 1-word remainder isn't a real question, so prompt for one.
        assertEquals(Route.DetectiveBare, AssistantRouting.route(tail("detective hey")))
    }

    @Test fun inspectorFallbackRoutesToDetective() {
        val r = AssistantRouting.route(tail("inspector who visits the front door"))
        assertTrue(r is Route.Detective)
        assertEquals("who visits the front door", (r as Route.Detective).question)
    }

    @Test fun sherlockFallbackRoutesToDetective() {
        assertTrue(AssistantRouting.route(tail("sherlock")) === Route.DetectiveBare)
        val r = AssistantRouting.route(tail("sherlock connect Bob and the blue truck"))
        assertTrue(r is Route.Detective)
    }

    @Test fun detectiveMidQuestionRoutesToRag() {
        // The keyword only counts as token 0 — "detective" inside a question is a plain rag query.
        val r = AssistantRouting.route(tail("did I record the detective show"))
        assertTrue(r is Route.Rag)
        assertEquals("did I record the detective show", (r as Route.Rag).question)
    }

    @Test fun advisorAndDetectiveKeywordsAreDisjoint() {
        // A detective utterance must not be captured by the advisor branch (and vice versa).
        assertTrue(AssistantRouting.route(tail("detective what happened")) is Route.Detective)
        assertTrue(AssistantRouting.route(tail("advisor what happened")) is Route.Advisor)
    }
}
