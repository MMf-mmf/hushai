package com.hushai.android.net

import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONObject

/**
 * Calls hushai-advisor `POST /v1/advisor/chat` (SSE) — one turn of a book-grounded consultation.
 * Structural sibling of [RagChatClient]: blocking OkHttp on the caller's worker thread, full-body
 * SSE accumulation (the voice client has no streaming UI, so blocking is correct). [Http.advisor]'s
 * wide call timeout (300 s) covers a cold-model turn that chains gathering→…→memorizing.
 *
 * A turn ends in one of two shapes: a [Result.Questions] gate round (the advisor needs more detail)
 * or a streamed [Result.Answer]. The SSE parser mirrors the server-verified one in
 * hushai-eval/src/query_advisor.rs (blank-line-separated blocks; `event:`/`data:` lines; `:`
 * keep-alives skipped; \r tolerated). The `memory` event (retrieval telemetry) and `phase`
 * heartbeats are read and ignored — only the terminal shape matters to the voice loop.
 */
class AdvisorClient(
    private val client: OkHttpClient,
    advisorBaseUrl: String,
    private val token: String,
) {
    private val endpoint = advisorBaseUrl.trimEnd('/') + "/v1/advisor/chat"

    data class Chapter(val no: Int, val title: String?)

    sealed interface Result {
        /** The advisor asked a gate round; the turn ended without an answer. */
        data class Questions(val round: Int, val questions: List<String>, val sessionId: String) : Result
        /** A complete answer + its final chapter grounding. */
        data class Answer(val text: String, val sessionId: String, val chapters: List<Chapter>) : Result
        /** 404: the server doesn't know this session (pruned / DB wiped) — retry once fresh. */
        data object SessionNotFound : Result
        /** 409: a turn is already in flight for this session (the in-flight guard). */
        data object Busy : Result
        data class Error(val reason: String) : Result
    }

    /**
     * Run one consultation turn. [sessionId] continues an existing consult (null starts a new one;
     * its id arrives on the first `session` SSE event). Body is `{"session_id"?, "message"}` only —
     * the advisor has no filters/playback/exhaustive.
     */
    fun chat(message: String, sessionId: String?): Result {
        val payload = JSONObject().apply {
            put("message", message)
            if (sessionId != null) put("session_id", sessionId)
        }.toString()

        val builder = Request.Builder()
            .url(endpoint)
            .header("Accept", "text/event-stream")
            .post(payload.toRequestBody(JSON))
        if (token.isNotBlank()) builder.header("Authorization", "Bearer $token")

        return try {
            client.newCall(builder.build()).execute().use { resp ->
                if (!resp.isSuccessful) {
                    val body = resp.body?.string().orEmpty()
                    return when {
                        resp.code == 409 -> Result.Busy
                        resp.code == 404 -> Result.SessionNotFound
                        else -> Result.Error("HTTP ${resp.code}: ${body.take(200)}")
                    }
                }
                parseSse(resp.body?.string().orEmpty())
            }
        } catch (e: Exception) {
            Result.Error(e.message ?: "network error")
        }
    }

    /** Accumulate the SSE stream into one terminal result. Port of query_advisor.rs `handle_block`. */
    private fun parseSse(rawInput: String): Result {
        var sessionId = ""
        var sawQuestions = false
        var round = 0
        var questions = emptyList<String>()
        var chapters = emptyList<Chapter>()
        val answer = StringBuilder()
        var errored = false
        var errorReason = "assistant error"

        val raw = rawInput.replace("\r\n", "\n")
        for (block in raw.split("\n\n")) {
            if (block.isBlank()) continue
            var event = ""
            val dataLines = ArrayList<String>()
            for (rawLine in block.split("\n")) {
                val line = rawLine.trimEnd('\r')
                if (line.isEmpty() || line.startsWith(":")) continue
                when {
                    line.startsWith("event:") -> event = line.removePrefix("event:").trim()
                    line.startsWith("data:") -> dataLines.add(line.removePrefix("data:").removePrefix(" "))
                }
            }
            val data = dataLines.joinToString("\n")
            when (event) {
                "session" -> runCatching { sessionId = JSONObject(data).optString("session_id") }
                "questions" -> runCatching {
                    val o = JSONObject(data)
                    sawQuestions = true
                    round = o.optInt("round", round)
                    val arr = o.optJSONArray("questions")
                    questions = if (arr == null) emptyList()
                    else (0 until arr.length()).mapNotNull { arr.optString(it).ifBlank { null } }
                }
                // Each refine iteration re-emits the grounding; the LAST one wins, so overwrite.
                "chapters" -> runCatching {
                    val arr = JSONObject(data).optJSONArray("chapters")
                    if (arr != null) {
                        chapters = (0 until arr.length()).map {
                            val c = arr.getJSONObject(it)
                            Chapter(c.optInt("no"), if (c.isNull("title")) null else c.optString("title").ifBlank { null })
                        }
                    }
                }
                "token" -> runCatching { answer.append(JSONObject(data).optString("delta")) }
                "error" -> runCatching {
                    errored = true
                    errorReason = JSONObject(data).optString("message").ifBlank { "assistant error" }
                }
                // "phase" heartbeats and the "memory" telemetry event are intentionally ignored.
            }
        }

        // Classify. An error event wins; otherwise a seen `questions` round outranks empty text.
        if (errored) return Result.Error(errorReason)
        if (sawQuestions) return Result.Questions(round, questions, sessionId)
        val text = answer.toString().trim()
        return if (text.isEmpty()) Result.Error("empty answer")
        else Result.Answer(text, sessionId, chapters)
    }

    companion object {
        private val JSON = "application/json".toMediaType()
    }
}
