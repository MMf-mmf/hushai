package com.hushai.android.assistant

/**
 * Pure, offline classifier that decides whether a spoken question is *introspective* —
 * "how have I been doing?", "what can I improve?", "how are my conversational skills?" —
 * and should be routed to the backend `reflection` agent instead of the default
 * `recordings` retrieval agent.
 *
 * It is deliberately conservative: it requires a self-reference combined with an
 * assessment/progress cue, so factual recall questions that merely mention the speaker
 * ("what did I say about lunch?") stay on the recordings agent. When nothing matches the
 * caller falls back to today's exact behaviour, so a misclassification only ever loses the
 * reflection routing — it never breaks normal Q&A.
 */
object ReflectionIntent {

    // Any match => route to the reflection agent. Authored against lowercased, whitespace-
    // collapsed text. Each pattern pairs a self-reference with an assessment/progress cue.
    private val PATTERNS: List<Regex> = listOf(
        // "how have I been", "how am I doing", "how's my", "how are my", "how was I"
        Regex("""\bhow('?s| is| are| have| has| am| was| were)\s+(i|me|my)\b"""),
        // "how can/do/should I improve / do better / get better"
        Regex("""\bhow (can|do|could|should|might) i (improve|do better|get better|be better|grow)\b"""),
        // "improve my ...", "work on my ...", "better at my ..."
        Regex("""\b(improve|improvement|work on|better at)\b.*\bmy\b"""),
        // self-assessment over a named dimension
        Regex("""\bmy (conversation|conversational|social|communication|people|listening|speaking|talking|mood|emotion|sentiment|habit|progress|skill|relationship|interaction|wellbeing|well-being)"""),
        // "how productive have I been" (the agent declines productivity, but it's a reflection ask)
        Regex("""\bhow productive\b"""),
        // "give me feedback/advice/tips", "what improvements/advice/feedback"
        Regex("""\b(give me|any) (feedback|advice|tips|pointers)\b"""),
        Regex("""\bwhat (improvements?|advice|feedback|tips)\b"""),
        // explicit self-reflection
        Regex("""\b(reflect on my|self[- ]?reflect|how did i do)\b"""),
    )

    fun isReflective(question: String): Boolean {
        val q = question.lowercase().trim().replace(Regex("""\s+"""), " ")
        if (q.isEmpty()) return false
        return PATTERNS.any { it.containsMatchIn(q) }
    }

    /** The backend agent id for an introspective question, or null to use the default. */
    fun agentFor(question: String): String? = if (isReflective(question)) "reflection" else null
}
