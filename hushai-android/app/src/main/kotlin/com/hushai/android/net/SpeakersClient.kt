package com.hushai.android.net

import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

/**
 * Calls the backend's authenticated speaker surface (`/v1/speakers`) used by the Voices
 * screen: list discovered speakers, name one, merge duplicates, and fetch a sample-audio
 * snippet so a human can identify a voice by ear. Hits the BACKEND url + device token (the
 * same ones the uploader uses), not the RAG service. Blocking OkHttp — call off the main
 * thread. Mirrors [RagClient]'s shape (org.json, bearer, `.use {}`).
 */
class SpeakersClient(
    private val client: OkHttpClient,
    baseUrl: String,
    private val token: String,
) {
    private val base = baseUrl.trimEnd('/')

    data class Speaker(
        val id: String,
        val name: String?,
        val nSamples: Long,
        val samples: List<String>,
        /** Disregarded by the operator — shown under a collapsed "Archived" section. */
        val archived: Boolean = false,
    )

    /** One member of a suggested duplicate group. */
    data class DupMember(
        val id: String,
        val name: String?,
        val nSamples: Long,
        val samples: List<String>,
    )

    /** A suggested group of duplicate voices (same person, over-split). */
    data class DupGroup(
        /** Loosest internal link distance — smaller is more confident. */
        val maxDistance: Double,
        /** True when the group spans >=2 distinct names: never one-tap merge (would lose a label). */
        val nameConflict: Boolean,
        /** Suggested survivor id (named member if any, else most-sampled). */
        val suggestedInto: String,
        val members: List<DupMember>,
    )

    /**
     * `GET /v1/speakers`. Returns `null` on a transport/HTTP error so the screen can show a
     * retryable error state; a non-null (possibly empty) list means the server answered.
     */
    fun listSpeakers(): List<Speaker>? {
        val req = bearer(Request.Builder().url("$base/v1/speakers").get())
        return try {
            client.newCall(req).execute().use { resp ->
                if (!resp.isSuccessful) return null
                val arr = JSONArray(resp.body?.string().orEmpty())
                (0 until arr.length()).map { i ->
                    val o = arr.getJSONObject(i)
                    val s = o.optJSONArray("sample_utterances")
                    Speaker(
                        id = o.getString("speaker_id"),
                        name = if (o.isNull("display_name")) null else o.optString("display_name"),
                        nSamples = o.optLong("n_samples"),
                        samples = if (s == null) emptyList()
                        else (0 until s.length()).map { s.getString(it) },
                        archived = o.optBoolean("archived", false),
                    )
                }
            }
        } catch (e: Exception) {
            null
        }
    }

    /** `PATCH /v1/speakers/{id}` — name a voice. */
    fun setName(id: String, name: String): Boolean {
        val body = JSONObject().put("display_name", name).toString().toRequestBody(JSON)
        val req = bearer(Request.Builder().url("$base/v1/speakers/$id").patch(body))
        return try {
            client.newCall(req).execute().use { it.isSuccessful }
        } catch (e: Exception) {
            false
        }
    }

    /**
     * `POST /v1/speakers/{id}/archive|unarchive` — disregard (or restore) a voice. Display-level
     * only: the matcher still attributes new audio to it; it just moves to the Archived section.
     */
    fun setArchived(id: String, archived: Boolean): Boolean {
        val verb = if (archived) "archive" else "unarchive"
        val req = bearer(
            Request.Builder().url("$base/v1/speakers/$id/$verb")
                .post(ByteArray(0).toRequestBody(null)),
        )
        return try {
            client.newCall(req).execute().use { it.isSuccessful }
        } catch (e: Exception) {
            false
        }
    }

    /** `POST /v1/speakers/{loser}/merge` — fold `loser` into `into`. */
    fun merge(loser: String, into: String): Boolean {
        val body = JSONObject().put("into", into).toString().toRequestBody(JSON)
        val req = bearer(Request.Builder().url("$base/v1/speakers/$loser/merge").post(body))
        return try {
            client.newCall(req).execute().use { it.isSuccessful }
        } catch (e: Exception) {
            false
        }
    }

    /** `GET /v1/speakers/duplicates` — suggested groups of duplicate voices. `null` on error. */
    fun listDuplicates(): List<DupGroup>? {
        val req = bearer(Request.Builder().url("$base/v1/speakers/duplicates").get())
        return try {
            client.newCall(req).execute().use { resp ->
                if (!resp.isSuccessful) return null
                val arr = JSONArray(resp.body?.string().orEmpty())
                (0 until arr.length()).map { i ->
                    val o = arr.getJSONObject(i)
                    val mem = o.optJSONArray("members")
                    DupGroup(
                        maxDistance = o.optDouble("max_distance", 1.0),
                        nameConflict = o.optBoolean("name_conflict", false),
                        suggestedInto = o.getString("suggested_into"),
                        members = if (mem == null) emptyList() else (0 until mem.length()).map { j ->
                            val m = mem.getJSONObject(j)
                            val s = m.optJSONArray("sample_utterances")
                            DupMember(
                                id = m.getString("speaker_id"),
                                name = if (m.isNull("display_name")) null else m.optString("display_name"),
                                nSamples = m.optLong("n_samples"),
                                samples = if (s == null) emptyList()
                                else (0 until s.length()).map { s.getString(it) },
                            )
                        },
                    )
                }
            }
        } catch (e: Exception) {
            null
        }
    }

    /** `POST /v1/speakers/merge-group` — fold a whole duplicate group into `into` atomically. */
    fun mergeGroup(into: String, members: List<String>): Boolean {
        val body = JSONObject()
            .put("into", into)
            .put("members", JSONArray(members))
            .toString()
            .toRequestBody(JSON)
        val req = bearer(Request.Builder().url("$base/v1/speakers/merge-group").post(body))
        return try {
            client.newCall(req).execute().use { it.isSuccessful }
        } catch (e: Exception) {
            false
        }
    }

    /**
     * A candidate voice clustered from audio the matcher left unattributed (speaker_id NULL).
     * Naming it mints a speaker and claims [segmentIds]. The matcher never auto-mints these
     * (they're chronically-marginal audio), so this is the only way to give them a name.
     */
    data class UnattributedCluster(
        /** Smallest member segment id — a stable-ish display handle. */
        val handle: String,
        val nSegments: Long,
        /** Loosest internal link distance — smaller is more confident. */
        val maxDistance: Double,
        /** The segments this voice will claim — sent back verbatim to [nameUnattributed]. */
        val segmentIds: List<String>,
        val samples: List<String>,
    )

    /** `GET /v1/speakers/unattributed` — candidate voices among unattributed audio. `null` on error. */
    fun listUnattributed(): List<UnattributedCluster>? {
        val req = bearer(Request.Builder().url("$base/v1/speakers/unattributed").get())
        return try {
            client.newCall(req).execute().use { resp ->
                if (!resp.isSuccessful) return null
                val arr = JSONArray(resp.body?.string().orEmpty())
                (0 until arr.length()).map { i ->
                    val o = arr.getJSONObject(i)
                    val segs = o.optJSONArray("segment_ids")
                    val s = o.optJSONArray("sample_utterances")
                    UnattributedCluster(
                        handle = o.getString("cluster_handle"),
                        nSegments = o.optLong("n_segments"),
                        maxDistance = o.optDouble("max_distance", 1.0),
                        segmentIds = if (segs == null) emptyList()
                        else (0 until segs.length()).map { segs.getString(it) },
                        samples = if (s == null) emptyList()
                        else (0 until s.length()).map { s.getString(it) },
                    )
                }
            }
        } catch (e: Exception) {
            null
        }
    }

    /** `POST /v1/speakers/unattributed/name` — mint a named voice from the given segments. */
    fun nameUnattributed(name: String, segmentIds: List<String>): Boolean {
        val body = JSONObject()
            .put("display_name", name)
            .put("segment_ids", JSONArray(segmentIds))
            .toString()
            .toRequestBody(JSON)
        val req = bearer(Request.Builder().url("$base/v1/speakers/unattributed/name").post(body))
        return try {
            client.newCall(req).execute().use { it.isSuccessful }
        } catch (e: Exception) {
            false
        }
    }

    /**
     * Download the speaker's sample-audio snippet to a temp file under [cacheDir] (so a
     * bearer header can be applied via OkHttp — more reliable than MediaPlayer's flaky
     * header overload). Returns the file, or null on failure. Caller deletes after playback.
     */
    fun downloadSample(id: String, cacheDir: File): File? {
        val req = bearer(Request.Builder().url("$base/v1/speakers/$id/sample-audio").get())
        return try {
            client.newCall(req).execute().use { resp ->
                if (!resp.isSuccessful) return null
                val bytes = resp.body?.bytes() ?: return null
                File.createTempFile("sample-", ".mp4", cacheDir).apply { writeBytes(bytes) }
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
