package com.hushai.android.net

import okhttp3.HttpUrl.Companion.toHttpUrl
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray

/**
 * Calls the backend's in-app alert feed (`/v1/events/feed`) — the notification surface for the
 * proactive events/alerts engine (roadmap A7, the mobile twin of the web "Events" page). Lists feed
 * deliveries (an alert per matched rule) and acknowledges them. Hits the BACKEND url + device token
 * (the same ones the uploader/Voices use), not RAG. Blocking OkHttp — call off the main thread.
 * Mirrors [SpeakersClient]'s shape (org.json, bearer, `.use {}`, null-on-error).
 */
class EventsClient(
    private val client: OkHttpClient,
    baseUrl: String,
    private val token: String,
) {
    private val base = baseUrl.trimEnd('/')

    data class FeedItem(
        val deliveryId: String,
        val eventType: String?,
        val severity: String?,
        val deviceId: String?,
        val subjectLabel: String?,
        val createdUnixNanos: Long,
        val acknowledged: Boolean,
    )

    /**
     * `GET /v1/events/feed` (optionally `?status=pending`). Returns `null` on a transport/HTTP error
     * (retryable), or a (possibly empty) list when the server answered.
     */
    fun listFeed(status: String? = null, limit: Int = 100): List<FeedItem>? {
        val url = "$base/v1/events/feed".toHttpUrl().newBuilder()
            .addQueryParameter("limit", limit.toString())
        if (!status.isNullOrBlank()) url.addQueryParameter("status", status)
        val req = bearer(Request.Builder().url(url.build()).get())
        return try {
            client.newCall(req).execute().use { resp ->
                if (!resp.isSuccessful) return null
                val arr = JSONArray(resp.body?.string().orEmpty())
                (0 until arr.length()).map { i ->
                    val o = arr.getJSONObject(i)
                    FeedItem(
                        deliveryId = o.getString("delivery_id"),
                        eventType = o.optStringOrNull("event_type"),
                        severity = o.optStringOrNull("severity"),
                        deviceId = o.optStringOrNull("device_id"),
                        subjectLabel = o.optStringOrNull("subject_label"),
                        createdUnixNanos = o.optLong("created_unix_nanos"),
                        acknowledged = o.optBoolean("acknowledged", false),
                    )
                }
            }
        } catch (e: Exception) {
            null
        }
    }

    /** `POST /v1/events/feed/{id}/ack` — mark a feed alert acknowledged (read/dismissed). */
    fun ack(deliveryId: String): Boolean {
        val body = "".toRequestBody(JSON)
        val req = bearer(Request.Builder().url("$base/v1/events/feed/$deliveryId/ack").post(body))
        return try {
            client.newCall(req).execute().use { it.isSuccessful }
        } catch (e: Exception) {
            false
        }
    }

    private fun bearer(b: Request.Builder): Request {
        if (token.isNotBlank()) b.header("Authorization", "Bearer $token")
        return b.build()
    }

    companion object {
        private val JSON = "application/json".toMediaType()
    }
}

/** org.json returns the string "null" for an explicit JSON null via optString — guard it. */
private fun org.json.JSONObject.optStringOrNull(key: String): String? =
    if (isNull(key)) null else optString(key).ifBlank { null }
