package com.hushai.android.net

import okhttp3.MediaType.Companion.toMediaType
import okhttp3.MultipartBody
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.asRequestBody
import okhttp3.RequestBody.Companion.toRequestBody
import java.io.File
import java.io.IOException

/**
 * Uploads one segment as `multipart/form-data` to `POST {baseUrl}/v1/segments`
 * with `Authorization: Bearer <token>`. Parts are named exactly `manifest` and
 * `body` — the backend keys on part NAME only and ignores Content-Type — but we
 * tag them like the verified reference client (feed_segments.py) for parity.
 *
 * Re-sending is just calling [upload] again with the same manifest+file: the
 * segment_id inside the manifest is the idempotency key.
 */
class Uploader(
    private val client: OkHttpClient,
    baseUrl: String,
    private val token: String,
) {
    private val endpoint = baseUrl.trimEnd('/') + "/v1/segments"

    fun upload(manifestBytes: ByteArray, body: File): UploadOutcome {
        val multipart = MultipartBody.Builder()
            .setType(MultipartBody.FORM)
            .addFormDataPart(
                "manifest", "manifest",
                manifestBytes.toRequestBody(PROTOBUF),
            )
            .addFormDataPart(
                "body", "body",
                body.asRequestBody(OCTET_STREAM),
            )
            .build()

        val request = Request.Builder()
            .url(endpoint)
            .header("Authorization", "Bearer $token")
            .post(multipart)
            .build()

        return try {
            client.newCall(request).execute().use { classify(it.code) }
        } catch (e: IOException) {
            UploadOutcome.RetryLater("io: ${e.message}")
        }
    }

    private fun classify(code: Int): UploadOutcome = when (code) {
        200 -> UploadOutcome.Accepted
        401 -> UploadOutcome.Unauthorized("401 unauthorized")
        422 -> UploadOutcome.Resend("422 integrity")
        400, 409, 413 -> UploadOutcome.PermanentClientError(code, "permanent client error $code")
        429, 507 -> UploadOutcome.RetryLater("$code backpressure")
        else -> if (code in 200..299) UploadOutcome.Accepted
        else UploadOutcome.RetryLater("transient $code")
    }

    companion object {
        private val PROTOBUF = "application/x-protobuf".toMediaType()
        private val OCTET_STREAM = "application/octet-stream".toMediaType()
    }
}
