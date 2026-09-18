package com.hushai.android

import com.hushai.android.net.RagChatClient
import okhttp3.OkHttpClient
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

/**
 * Verifies the SSE consumer and request shape for the rich `/v1/rag/chat` voice path. Uses
 * MockWebServer (same style as [RagClientTest]); the SSE bodies deliberately include keep-alive
 * `:` comment lines and `\r\n` line endings to exercise the parser's tolerance.
 */
class RagChatClientTest {
    private lateinit var server: MockWebServer

    @Before fun setUp() { server = MockWebServer(); server.start() }
    @After fun tearDown() { server.shutdown() }

    private fun client(token: String = "") =
        RagChatClient(OkHttpClient(), server.url("/").toString(), token)

    /** A well-formed SSE body: session, sources, a run of token deltas, done — with keep-alives. */
    private fun sseBody(sessionId: String, routed: String, vararg deltas: String): String {
        val sb = StringBuilder()
        sb.append(": keep-alive\r\n\r\n")
        sb.append("event: session\r\n")
        sb.append("data: {\"session_id\":\"$sessionId\",\"agent_id\":\"auto\",\"routed_agent_id\":\"$routed\"}\r\n\r\n")
        sb.append("event: sources\r\ndata: []\r\n\r\n")
        for (d in deltas) {
            sb.append("event: token\r\ndata: {\"delta\":\"$d\"}\r\n\r\n")
        }
        sb.append("event: done\r\ndata: {\"message_id\":\"m1\"}\r\n\r\n")
        return sb.toString()
    }

    @Test fun accumulatesTokensAndCapturesSession() {
        server.enqueue(
            MockResponse()
                .setHeader("Content-Type", "text/event-stream")
                .setBody(sseBody("sess-1", "recordings", "You're ", "Morgan."))
        )
        val r = client().chat("what's my name", null, ownerVerified = true, deviceId = "android-1", tzOffsetSecs = -14400)
        assertTrue(r is RagChatClient.Result.Answer)
        r as RagChatClient.Result.Answer
        assertEquals("You're Morgan.", r.text)
        assertEquals("sess-1", r.sessionId)
        assertEquals("recordings", r.routedAgentId)
    }

    @Test fun newSessionSendsAutoAgentAndCallerAndNoFilters() {
        server.enqueue(MockResponse().setBody(sseBody("s", "recordings", "ok")))
        client().chat("hi", null, ownerVerified = true, deviceId = "android-9", tzOffsetSecs = 0)
        val req = server.takeRequest()
        assertEquals("/v1/rag/chat", req.path)
        val body = req.body.readUtf8()
        assertTrue("agent_id auto only on new session", body.contains("\"agent_id\":\"auto\""))
        assertTrue(body.contains("\"kind\":\"voice\""))
        assertTrue(body.contains("\"owner_verified\":true"))
        assertTrue(body.contains("\"tz_offset_secs\""))
        assertTrue("voice must not scope to one device", !body.contains("\"filters\""))
        assertTrue("device_id rides in caller, not filters", body.contains("android-9"))
    }

    @Test fun continuingSessionOmitsAgentIdAndSendsSessionId() {
        server.enqueue(MockResponse().setBody(sseBody("s2", "recordings", "ok")))
        client().chat("and before that?", "sess-existing", ownerVerified = false, deviceId = "d", tzOffsetSecs = 0)
        val body = server.takeRequest().body.readUtf8()
        assertTrue(body.contains("\"session_id\":\"sess-existing\""))
        assertTrue("agent binding is immutable — no agent_id on a continuation", !body.contains("agent_id"))
        assertTrue(body.contains("\"owner_verified\":false"))
    }

    @Test fun staleSessionIs404WithBody() {
        server.enqueue(MockResponse().setResponseCode(404).setBody("chat session not found"))
        val r = client().chat("q", "gone", ownerVerified = true, deviceId = "d", tzOffsetSecs = 0)
        assertTrue(r is RagChatClient.Result.SessionNotFound)
    }

    @Test fun bare404IsEndpointMissing() {
        server.enqueue(MockResponse().setResponseCode(404).setBody("Not Found"))
        val r = client().chat("q", null, ownerVerified = true, deviceId = "d", tzOffsetSecs = 0)
        assertTrue(r is RagChatClient.Result.EndpointMissing)
    }

    @Test fun errorEventIsError() {
        val body = "event: session\r\ndata: {\"session_id\":\"s\"}\r\n\r\n" +
            "event: error\r\ndata: {\"message\":\"boom\"}\r\n\r\n"
        server.enqueue(MockResponse().setBody(body))
        val r = client().chat("q", null, ownerVerified = true, deviceId = "d", tzOffsetSecs = 0)
        assertTrue(r is RagChatClient.Result.Error)
    }

    @Test fun bearerSentWhenTokenSet() {
        server.enqueue(MockResponse().setBody(sseBody("s", "recordings", "ok")))
        client("secret").chat("q", null, ownerVerified = true, deviceId = "d", tzOffsetSecs = 0)
        assertEquals("Bearer secret", server.takeRequest().getHeader("Authorization"))
    }

    @Test fun noBearerWhenTokenBlank() {
        server.enqueue(MockResponse().setBody(sseBody("s", "recordings", "ok")))
        client("").chat("q", null, ownerVerified = true, deviceId = "d", tzOffsetSecs = 0)
        assertNull(server.takeRequest().getHeader("Authorization"))
    }
}
