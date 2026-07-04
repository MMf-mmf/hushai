package com.hushai.android.net

import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONObject

/**
 * Calls hushai-rag `POST /v1/rag/chat` (SSE) — the RICH assistant pipeline the voice loop uses:
 * auto-router, DB-backed sessions + history, query condensation, deterministic roster/recency/
 * identity answers, and all six capability agents. Replaces [RagClient]'s single-shot `/v1/rag/query`
 * for the primary path; [RagClient] is retained only as the older-server fallback.
 *
 * The response is Server-Sent Events; TTS needs the FULL answer text, so we accumulate `token`
 * deltas and return one string once `done` arrives. Blocking OkHttp on the caller's worker thread
 * (like [RagClient.ask]); [Http.rag]'s keep-alive-tolerant timeouts cover slow local-LLM generation.
 *
 * The SSE parser mirrors the server-verified one in hushai-eval/src/query_rag.rs (blocks split on a
 * blank line; `event:`/`data:` lines; `:` keep-alive comments skipped; \r tolerated).
 */
class RagChatClient(
    private val client: OkHttpClient,
    ragBaseUrl: String,
    private val token: String,
) {
    private val endpoint = ragBaseUrl.trimEnd('/') + "/v1/rag/chat"

    sealed interface Result {
        /** A complete answer. [sessionId] continues the conversation; [routedAgentId] is where it landed. */
        data class Answer(val text: String, val sessionId: String, val routedAgentId: String?) : Result
        /** The server 404'd a KNOWN session id (pruned / DB wiped) — caller clears it and retries fresh. */
        data object SessionNotFound : Result
        /** The server has no `/v1/rag/chat` route (older build) — caller falls back to `/v1/rag/query`. */
        data object EndpointMissing : Result
        data class Error(val reason: String) : Result
    }

    /**
     * Ask one question. [sessionId] continues an existing conversation (null starts a new one, and
     * only then is `agent_id:"auto"` sent — the session's agent binding is immutable server-side).
     * [ownerVerified] is the on-device voice-ID result for this utterance; [tzOffsetSecs] is the
     * phone's real UTC offset (better than the server env default). No `filters` are sent — voice
     * questions search all devices.
     */
    fun chat(
        message: String,
        sessionId: String?,
        ownerVerified: Boolean,
        deviceId: String,
        tzOffsetSecs: Long,
    ): Result {
        val payload = JSONObject().apply {
            put("message", message)
            put("tz_offset_secs", tzOffsetSecs)
            if (sessionId != null) {
                put("session_id", sessionId)
            } else {
                put("agent_id", "auto")
            }
            put(
                "caller",
                JSONObject()
                    .put("kind", "voice")
                    .put("owner_verified", ownerVerified)
                    .put("device_id", deviceId),
            )
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
                        // A 404 naming the session → stale id; anything else 404/405 → no such route.
                        resp.code == 404 && body.contains("chat session not found") -> Result.SessionNotFound
                        resp.code == 404 || resp.code == 405 -> Result.EndpointMissing
                        else -> Result.Error("HTTP ${resp.code}")
                    }
                }
                parseSse(resp.body?.string().orEmpty())
            }
        } catch (e: Exception) {
            Result.Error(e.message ?: "network error")
        }
    }

    /** Accumulate the SSE stream into a single answer. Port of query_rag.rs `handle_block`. */
    private fun parseSse(rawInput: String): Result {
        val answer = StringBuilder()
        var sessionId = ""
        var routedAgentId: String? = null
        var errored = false

        // axum emits LF-only SSE, but normalize CRLF → LF up front so a CRLF-emitting proxy can't
        // hide the "\n\n" event separator (more robust than the eval parser's raw split).
        val raw = rawInput.replace("\r\n", "\n")
        // Events are separated by a blank line; a keep-alive comment line starts with ':'.
        for (block in raw.split("\n\n")) {
            if (block.isBlank()) continue
            var event = ""
            val dataLines = ArrayList<String>()
            for (rawLine in block.split("\n")) {
                val line = rawLine.trimEnd('\r')
                if (line.isEmpty() || line.startsWith(":")) continue
                when {
                    line.startsWith("event:") -> event = line.removePrefix("event:").trim()
                    line.startsWith("data:") ->
                        dataLines.add(line.removePrefix("data:").removePrefix(" "))
                }
            }
            val data = dataLines.joinToString("\n")
            when (event) {
                "session" -> runCatching {
                    val o = JSONObject(data)
                    sessionId = o.optString("session_id")
                    routedAgentId = if (o.isNull("routed_agent_id")) null
                    else o.optString("routed_agent_id").ifBlank { null }
                }
                "token" -> runCatching {
                    answer.append(JSONObject(data).optString("delta"))
                }
                "error" -> errored = true
                // "sources" is ignored here — the voice client only speaks the answer text.
            }
        }

        if (errored) return Result.Error("assistant error")
        val text = answer.toString().trim()
        return if (text.isEmpty()) Result.Error("empty answer")
        else Result.Answer(text, sessionId, routedAgentId)
    }

    companion object {
        private val JSON = "application/json".toMediaType()
    }
}
