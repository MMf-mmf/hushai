package com.hushai.android

import com.hushai.android.net.RagClient
import okhttp3.OkHttpClient
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

class RagClientTest {
    private lateinit var server: MockWebServer

    @Before fun setUp() { server = MockWebServer(); server.start() }
    @After fun tearDown() { server.shutdown() }

    private fun client(token: String = "") =
        RagClient(OkHttpClient(), server.url("/").toString(), token)

    @Test fun parsesAnswerAndSendsQueryWithDeviceFilter() {
        server.enqueue(MockResponse().setBody("""{"answer":"you said lunch at noon","sources":[]}"""))
        val r = client().ask("what about lunch", "android-dev-1")
        assertTrue(r is RagClient.Result.Answer)
        assertEquals("you said lunch at noon", (r as RagClient.Result.Answer).text)

        val req = server.takeRequest()
        assertEquals("/v1/rag/query", req.path)
        val body = req.body.readUtf8()
        assertTrue(body.contains("\"query\""))
        assertTrue(body.contains("what about lunch"))
        assertTrue(body.contains("android-dev-1")) // device_id filter present
    }

    @Test fun emptyAnswerIsError() {
        server.enqueue(MockResponse().setBody("""{"answer":"   ","sources":[]}"""))
        assertTrue(client().ask("q", "d") is RagClient.Result.Error)
    }

    @Test fun httpErrorIsError() {
        server.enqueue(MockResponse().setResponseCode(500))
        assertTrue(client().ask("q", "d") is RagClient.Result.Error)
    }

    @Test fun sendsBearerWhenTokenSet() {
        server.enqueue(MockResponse().setBody("""{"answer":"ok"}"""))
        client("secret").ask("q", "d")
        assertEquals("Bearer secret", server.takeRequest().getHeader("Authorization"))
    }

    @Test fun noBearerWhenTokenBlank() {
        server.enqueue(MockResponse().setBody("""{"answer":"ok"}"""))
        client("").ask("q", "d")
        org.junit.Assert.assertNull(server.takeRequest().getHeader("Authorization"))
    }
}
