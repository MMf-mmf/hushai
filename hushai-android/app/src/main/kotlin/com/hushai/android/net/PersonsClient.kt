package com.hushai.android.net

import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

/**
 * Calls the backend's authenticated person (face) surface (`/v1/persons`) used by the People
 * screen: list discovered faces, name one, merge duplicates, and fetch a cropped sample-face
 * JPEG so a human can identify a person by sight. Hits the BACKEND url + device token (the same
 * ones the uploader uses), not the RAG service. Blocking OkHttp — call off the main thread. The
 * visual sibling of [SpeakersClient]; same org.json / bearer / `.use {}` idioms. Only the four
 * routes the persons sub-router exposes (no duplicates/unattributed — faces ship single-pair merge).
 */
class PersonsClient(
    private val client: OkHttpClient,
    baseUrl: String,
    private val token: String,
) {
    private val base = baseUrl.trimEnd('/')

    data class Person(
        val id: String,
        val name: String?,
        /** Raw per-frame face-template count (over-counts a single clip); internal weight, not UI. */
        val nSamples: Long,
        /** Distinct appearances, detections clustered by time gap — what the UI shows as "sightings". */
        val nSightings: Long,
        /** Up to 3 recent sighting times (unix nanos), most-recent first. */
        val sampleSightingsNanos: List<Long>,
    )

    /**
     * `GET /v1/persons`. Returns `null` on a transport/HTTP error so the screen can show a
     * retryable error state; a non-null (possibly empty) list means the server answered.
     */
    fun listPersons(): List<Person>? {
        val req = bearer(Request.Builder().url("$base/v1/persons").get())
        return try {
            client.newCall(req).execute().use { resp ->
                if (!resp.isSuccessful) return null
                val arr = JSONArray(resp.body?.string().orEmpty())
                (0 until arr.length()).map { i ->
                    val o = arr.getJSONObject(i)
                    val t = o.optJSONArray("sample_sighting_unix_nanos")
                    Person(
                        id = o.getString("person_id"),
                        name = if (o.isNull("display_name")) null else o.optString("display_name"),
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

    /** `PATCH /v1/persons/{id}` — name a face. */
    fun setName(id: String, name: String): Boolean {
        val body = JSONObject().put("display_name", name).toString().toRequestBody(JSON)
        val req = bearer(Request.Builder().url("$base/v1/persons/$id").patch(body))
        return try {
            client.newCall(req).execute().use { it.isSuccessful }
        } catch (e: Exception) {
            false
        }
    }

    /** `POST /v1/persons/{loser}/merge` — fold `loser` into `into`. */
    fun merge(loser: String, into: String): Boolean {
        val body = JSONObject().put("into", into).toString().toRequestBody(JSON)
        val req = bearer(Request.Builder().url("$base/v1/persons/$loser/merge").post(body))
        return try {
            client.newCall(req).execute().use { it.isSuccessful }
        } catch (e: Exception) {
            false
        }
    }

    /**
     * Download the person's sample-face crop (a JPEG) to a temp file under [cacheDir] so a bearer
     * header can be applied via OkHttp (more reliable than BitmapFactory fetching a URL). Returns
     * the file, or null on failure. Caller decodes + deletes after rendering.
     */
    fun downloadSampleFace(id: String, cacheDir: File): File? {
        val req = bearer(Request.Builder().url("$base/v1/persons/$id/sample-face").get())
        return try {
            client.newCall(req).execute().use { resp ->
                if (!resp.isSuccessful) return null
                val bytes = resp.body?.bytes() ?: return null
                File.createTempFile("face-", ".jpg", cacheDir).apply { writeBytes(bytes) }
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
