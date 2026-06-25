package com.hushai.android

import com.hushai.android.net.UploadOutcome
import com.hushai.android.net.Uploader
import okhttp3.OkHttpClient
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

class UploaderTest {

    private fun tmpBody(): File =
        File.createTempFile("body", ".mp4").apply { writeBytes(byteArrayOf(1, 2, 3, 4)) }

    @Test
    fun maps_status_codes_to_outcomes() {
        val server = MockWebServer()
        server.start()
        val uploader = Uploader(OkHttpClient(), server.url("/").toString(), "tok")
        val body = tmpBody()
        val manifest = byteArrayOf(9, 9, 9)

        val cases = listOf(
            200 to UploadOutcome.Accepted::class,
            401 to UploadOutcome.Unauthorized::class,
            422 to UploadOutcome.Resend::class,
            400 to UploadOutcome.PermanentClientError::class,
            409 to UploadOutcome.PermanentClientError::class,
            413 to UploadOutcome.PermanentClientError::class,
            429 to UploadOutcome.RetryLater::class,
            507 to UploadOutcome.RetryLater::class,
            503 to UploadOutcome.RetryLater::class,
            408 to UploadOutcome.RetryLater::class,
        )
        cases.forEach { (code, _) -> server.enqueue(MockResponse().setResponseCode(code)) }

        cases.forEach { (code, expected) ->
            val outcome = uploader.upload(manifest, body)
            assertTrue("code $code -> ${outcome::class.simpleName}", expected.isInstance(outcome))
        }
        server.shutdown()
    }

    @Test
    fun sends_bearer_and_named_parts() {
        val server = MockWebServer()
        server.start()
        server.enqueue(MockResponse().setResponseCode(200))
        val uploader = Uploader(OkHttpClient(), server.url("/").toString(), "secret-token")

        uploader.upload(byteArrayOf(1, 2, 3), tmpBody())

        val recorded = server.takeRequest()
        assertEquals("POST", recorded.method)
        assertEquals("/v1/segments", recorded.path)
        assertEquals("Bearer secret-token", recorded.getHeader("Authorization"))
        assertTrue(recorded.getHeader("Content-Type")!!.startsWith("multipart/form-data"))
        val rendered = recorded.body.readUtf8()
        assertTrue("manifest part", rendered.contains("name=\"manifest\""))
        assertTrue("body part", rendered.contains("name=\"body\""))
        server.shutdown()
    }

    @Test
    fun network_failure_is_retry_later() {
        // No server listening on this port -> connect failure -> RetryLater (never drop).
        val client = OkHttpClient.Builder()
            .connectTimeout(java.time.Duration.ofMillis(300))
            .callTimeout(java.time.Duration.ofMillis(800))
            .build()
        val uploader = Uploader(client, "http://127.0.0.1:1/", "tok")
        val outcome = uploader.upload(byteArrayOf(1), tmpBody())
        assertTrue(outcome is UploadOutcome.RetryLater)
    }
}
