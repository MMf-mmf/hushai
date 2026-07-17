package com.hushai.android.net

import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONObject

/**
 * Calls hushai-rag `POST /v1/rag/chat` with `agent_id="gotham"` (SSE) — one turn of the Gotham
 * "Detective" agentic investigation over the entity graph. Unlike the advisor, the Detective is NOT
 * a separate service: it rides the SAME `/v1/rag/chat` endpoint as [RagChatClient], only the agent
 * binding differs — so this shares [RagChatClient]'s host/token ([Http.rag]) and never needs a
 * second port. Blocking OkHttp on the caller's worker thread, full-body SSE accumulation (the voice
 * client has no streaming UI, so blocking is correct), structural sibling of [RagChatClient] and
 * [AdvisorClient].
 *
 * The Detective streams a SUPERSET of the plain-chat SSE: on top of `session`/`token`/`sources`/
 * `done`/`error` it emits `phase` heartbeats and `tool_call`/`tool_result` trace frames (for the
 * viewer's Detective pane) and — for a Wave-3 mutation — a `confirm` event. **A voice caller ignores
 * `phase`/`tool_*`/`sources` entirely** (the `when` below simply doesn't match them, so unknown /
 * forward-compat events are inert) and speaks only the answer. `confirm` is a two-phase mutation
 * gate: the turn ends awaiting a spoken yes/no, which the caller rides via `AWAIT_FOLLOWUP` (§2.7).
 * It is DORMANT in Phase 1 (mutations off, `GOTHAM_MUTATIONS_ENABLED=false`) — present here so the
 * path exists and is exercised, exactly like the viewer's confirm bubble.
 *
 * The SSE parser mirrors the server-verified one in hushai-eval/src/query_agent.rs (blocks split on
 * a blank line; `event:`/`data:` lines; `:` keep-alives skipped; \r tolerated; only `done`/`error`
 * terminate — every other event is non-terminal so the answer + confirm state keep accumulating).
 */
class DetectiveClient(
    private val client: OkHttpClient,
    ragBaseUrl: String,
    private val token: String,
) {
    private val endpoint = ragBaseUrl.trimEnd('/') + "/v1/rag/chat"

    sealed interface Result {
        /** A complete investigation answer. [sessionId] continues the conversation. */
        data class Answer(val text: String, val sessionId: String) : Result
        /** A two-phase confirmation gate (Wave-3 mutation): [summary] is the deterministic action
         *  summary to speak; the next owner utterance ("yes"/"no") continues the same [sessionId].
         *  Dormant in Phase 1. */
        data class Confirm(val summary: String, val sessionId: String) : Result
        /** The server 404'd a KNOWN session id (pruned / DB wiped) — caller clears it and retries fresh. */
        data object SessionNotFound : Result
        /** The server has no `/v1/rag/chat` route (older build) — the Detective is unavailable. */
        data object EndpointMissing : Result
        data class Error(val reason: String) : Result
    }

    /**
     * Run one Detective turn. [sessionId] continues an existing investigation (null starts a new one,
     * and only then is `agent_id:"gotham"` sent — the session's agent binding is immutable
     * server-side, so a continuation omits it exactly like [RagChatClient]). [ownerVerified] is the
     * on-device voice-ID result for this utterance (the server needs it for identity-aware answers
     * and, for a Wave-3 confirm, execution requires it). No `filters` are sent — investigations span
     * all devices.
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
                put("agent_id", "gotham")
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

    /** Accumulate the superset SSE stream into one terminal result. Port of query_agent.rs
     *  `handle_block`: unknown / trace events are inert; `confirm` outranks the streamed answer. */
    private fun parseSse(rawInput: String): Result {
        val answer = StringBuilder()
        var sessionId = ""
        var sawConfirm = false
        var confirmSummary = ""
        var errored = false

        // axum emits LF-only SSE; normalize CRLF → LF up front so a CRLF-emitting proxy can't hide
        // the "\n\n" event separator.
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
                    line.startsWith("data:") ->
                        dataLines.add(line.removePrefix("data:").removePrefix(" "))
                }
            }
            val data = dataLines.joinToString("\n")
            when (event) {
                "session" -> runCatching { sessionId = JSONObject(data).optString("session_id") }
                "token" -> runCatching { answer.append(JSONObject(data).optString("delta")) }
                "confirm" -> runCatching {
                    sawConfirm = true
                    confirmSummary = JSONObject(data).optString("summary")
                }
                "error" -> errored = true
                // "phase"/"tool_call"/"tool_result"/"sources" (and any future event) are inert here —
                // the voice client speaks only the answer. This IS the unknown-event tolerance.
            }
        }

        if (errored) return Result.Error("assistant error")
        // A confirm gate outranks the streamed natural-language line: the turn ended awaiting yes/no.
        if (sawConfirm) {
            val summary = confirmSummary.ifBlank { answer.toString().trim() }
                .ifBlank { "Confirm this action?" }
            return Result.Confirm(summary, sessionId)
        }
        val text = answer.toString().trim()
        return if (text.isEmpty()) Result.Error("empty answer")
        else Result.Answer(text, sessionId)
    }

    companion object {
        private val JSON = "application/json".toMediaType()
    }
}
