package com.hushai.android.net

import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.net.URLEncoder

/**
 * Calls the backend's authenticated license-plate (ALPR) surface (`/v1/plates`) used by the Plates
 * screen: list discovered plates, search by text, name one, merge duplicates, and fetch a cropped
 * sample-plate JPEG so a human can identify a vehicle by sight. Hits the BACKEND url + device token
 * (the same ones the uploader uses), not the RAG service. Blocking OkHttp — call off the main
 * thread. The vehicle sibling of [PersonsClient]; same org.json / bearer / `.use {}` idioms. KEY
 * DIVERGENCE: a plate's identity is its text, so every plate carries a `plate_text` (the OCR string)
 * in addition to the optional human `display_name`, and there's a `search` route.
 */
class PlatesClient(
    private val client: OkHttpClient,
    baseUrl: String,
    private val token: String,
) {
    private val base = baseUrl.trimEnd('/')

    data class Plate(
        val plateId: String,
        /** Canonical OCR plate string (always present); shown when there's no human display name. */
        val plateText: String,
        val displayName: String?,
        /** Raw per-read OCR template count (over-counts a single pass); internal weight, not UI. */
        val nSamples: Long,
        /** Distinct appearances, detections clustered by time gap — what the UI shows as "sightings". */
        val nSightings: Long,
        /** Up to 3 recent sighting times (unix nanos), most-recent first. */
        val sampleSightingsNanos: List<Long>,
    )

    /**
     * `GET /v1/plates`. Returns `null` on a transport/HTTP error so the screen can show a
     * retryable error state; a non-null (possibly empty) list means the server answered.
     */
    fun listPlates(): List<Plate>? {
        val req = bearer(Request.Builder().url("$base/v1/plates").get())
        return execList(req)
    }

    /**
     * `GET /v1/plates/search?q=...`. Same shape as [listPlates]; `null` on error. A blank `q`
     * yields an empty list from the server.
     */
    fun search(q: String): List<Plate>? {
        val encoded = URLEncoder.encode(q, "UTF-8")
        val req = bearer(Request.Builder().url("$base/v1/plates/search?q=$encoded").get())
        return execList(req)
    }

    /** Shared GET-list executor: parse the catalog JSON array, or null on transport/HTTP error. */
    private fun execList(req: Request): List<Plate>? {
        return try {
            client.newCall(req).execute().use { resp ->
                if (!resp.isSuccessful) return null
                val arr = JSONArray(resp.body?.string().orEmpty())
                (0 until arr.length()).map { i ->
                    val o = arr.getJSONObject(i)
                    val t = o.optJSONArray("sample_sighting_unix_nanos")
                    Plate(
                        plateId = o.getString("plate_id"),
                        plateText = o.optString("plate_text"),
                        displayName = if (o.isNull("display_name")) null else o.optString("display_name"),
                        nSamples = o.optLong("n_samples"),
                        // Fall back to n_samples if an older backend doesn't send n_sightings.
                        nSightings = if (o.has("n_sightings")) o.optLong("n_sightings") else o.optLong("n_samples"),
                        sampleSightingsNanos = if (t == null) emptyList()
                        else (0 until t.length()).map { t.getLong(it) },
                    )
                }
            }
        } catch (e: Exception) {
            null
        }
    }

    /** `PATCH /v1/plates/{id}` — name a plate. */
    fun setName(id: String, name: String): Boolean {
        val body = JSONObject().put("display_name", name).toString().toRequestBody(JSON)
        val req = bearer(Request.Builder().url("$base/v1/plates/$id").patch(body))
        return try {
            client.newCall(req).execute().use { it.isSuccessful }
        } catch (e: Exception) {
            false
        }
    }

    /** `POST /v1/plates/{loser}/merge` — fold `loser` into `into`. */
    fun merge(loser: String, into: String): Boolean {
        val body = JSONObject().put("into", into).toString().toRequestBody(JSON)
        val req = bearer(Request.Builder().url("$base/v1/plates/$loser/merge").post(body))
        return try {
            client.newCall(req).execute().use { it.isSuccessful }
        } catch (e: Exception) {
            false
        }
    }

    /**
     * Download the plate's sample crop (a JPEG) to a temp file under [cacheDir] so a bearer
     * header can be applied via OkHttp (more reliable than BitmapFactory fetching a URL). Returns
     * the file, or null on failure. Caller decodes + deletes after rendering.
     */
    fun downloadSampleCrop(id: String, cacheDir: File): File? {
        val req = bearer(Request.Builder().url("$base/v1/plates/$id/sample-crop").get())
        return try {
            client.newCall(req).execute().use { resp ->
                if (!resp.isSuccessful) return null
                val bytes = resp.body?.bytes() ?: return null
                File.createTempFile("plate-", ".jpg", cacheDir).apply { writeBytes(bytes) }
            }
        } catch (e: Exception) {
            null
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
