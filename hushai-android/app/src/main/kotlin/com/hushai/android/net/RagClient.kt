package com.hushai.android.net

import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONObject

/**
 * Calls hushai-rag `POST /v1/rag/query` to answer a spoken question, grounded in
 * the user's own recorded history (scoped by `device_id`). Returns the `answer`
 * string from `{answer, sources[...]}`. Optional bearer if the server sets RAG_TOKEN.
 */
class RagClient(
    private val client: OkHttpClient,
    ragBaseUrl: String,
    private val token: String,
) {
    private val endpoint = ragBaseUrl.trimEnd('/') + "/v1/rag/query"

    sealed interface Result {
        data class Answer(val text: String) : Result
        data class Error(val reason: String) : Result
    }

    fun ask(question: String, deviceId: String): Result {
        val payload = JSONObject().apply {
            put("query", question)
            put("filters", JSONObject().put("device_id", deviceId))
        }.toString()
        val builder = Request.Builder()
            .url(endpoint)
            .post(payload.toRequestBody(JSON))
        if (token.isNotBlank()) builder.header("Authorization", "Bearer $token")
        return try {
            client.newCall(builder.build()).execute().use { resp ->
                val body = resp.body?.string().orEmpty()
                if (!resp.isSuccessful) return Result.Error("HTTP ${resp.code}")
                val answer = JSONObject(body).optString("answer").trim()
                if (answer.isEmpty()) Result.Error("empty answer") else Result.Answer(answer)
            }
        } catch (e: Exception) {
            Result.Error(e.message ?: "network error")
        }
    }

    companion object {
        private val JSON = "application/json".toMediaType()
    }
}
