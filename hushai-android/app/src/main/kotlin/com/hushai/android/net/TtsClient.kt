package com.hushai.android.net

import com.hushai.android.util.HushaiLog
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONObject

/**
 * Calls hushai-rag `POST /v1/tts` to turn the assistant's answer text into speech.
 * Synthesis runs on the backend (a natural neural voice); this just fetches the
 * 16-bit PCM WAV bytes. Returns null on any failure (network, 503 when TTS is off,
 * non-2xx) so the caller can fall back to showing the answer text silently.
 *
 * Mirrors [RagClient]'s client/URL/auth conventions. Optional bearer when RAG_TOKEN.
 */
class TtsClient(
    private val client: OkHttpClient,
    ragBaseUrl: String,
    private val token: String,
) {
    private val endpoint = ragBaseUrl.trimEnd('/') + "/v1/tts"

    /** Synthesize [text] to a WAV byte array, or null if unavailable. */
    fun synthesize(text: String): ByteArray? {
        val payload = JSONObject().put("text", text).toString()
        val builder = Request.Builder()
            .url(endpoint)
            .post(payload.toRequestBody(JSON))
        if (token.isNotBlank()) builder.header("Authorization", "Bearer $token")
        return try {
            client.newCall(builder.build()).execute().use { resp ->
                if (!resp.isSuccessful) {
                    HushaiLog.warn("TTS HTTP ${resp.code}")
                    return null
                }
                resp.body?.bytes()
            }
        } catch (e: Exception) {
            HushaiLog.warn("TTS request failed: ${e.message}")
            null
        }
    }

    companion object {
        private val JSON = "application/json".toMediaType()
    }
}
