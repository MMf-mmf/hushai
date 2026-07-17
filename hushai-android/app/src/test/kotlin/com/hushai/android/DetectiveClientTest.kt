package com.hushai.android

import com.hushai.android.net.DetectiveClient
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
 * SSE consumer + request shape for the Gotham Detective voice path (`/v1/rag/chat` with
 * `agent_id="gotham"`). MockWebServer, same style as [RagChatClientTest] / [AdvisorClientTest]. The
 * bodies interleave the SUPERSET events a Detective turn streams — `phase`, `tool_call`,
 * `tool_result`, `sources` — plus keep-alive `:` lines and `\r\n` endings, to prove the parser
 * IGNORES every non-answer event (the §2.7 "voice ignores phase/tool_*" contract = SSE unknown-event
 * tolerance) and speaks only the accumulated answer.
 */
class DetectiveClientTest {
    private lateinit var server: MockWebServer

    @Before fun setUp() { server = MockWebServer(); server.start() }
    @After fun tearDown() { server.shutdown() }

    private fun client(token: String = "") =
        DetectiveClient(OkHttpClient(), server.url("/").toString(), token)

    /** A realistic Detective SSE turn: session, phase heartbeats, a tool call+result, sources, the
     *  answer tokens, done — with keep-alives and CRLF endings. */
    private fun sseTurn(sessionId: String, vararg deltas: String): String = buildString {
        append(": keep-alive\r\n\r\n")
        append("event: session\r\ndata: {\"session_id\":\"$sessionId\",\"agent_id\":\"gotham\",\"routed_agent_id\":\"gotham\"}\r\n\r\n")
        append("event: phase\r\ndata: {\"phase\":\"planning\"}\r\n\r\n")
        append("event: tool_call\r\ndata: {\"seq\":1,\"tool\":\"graph_entity\",\"label\":\"Look up entity\",\"args_summary\":\"Alice\"}\r\n\r\n")
        append("event: tool_result\r\ndata: {\"seq\":1,\"tool\":\"graph_entity\",\"ok\":true,\"summary\":\"found\",\"sources_added\":2,\"elapsed_ms\":40}\r\n\r\n")
        append("event: phase\r\ndata: {\"phase\":\"answering\"}\r\n\r\n")
        append("event: sources\r\ndata: []\r\n\r\n")
        for (d in deltas) append("event: token\r\ndata: {\"delta\":\"$d\"}\r\n\r\n")
        append("event: done\r\ndata: {\"message_id\":\"m1\"}\r\n\r\n")
    }

    @Test fun ignoresTraceEventsAndAccumulatesAnswer() {
        server.enqueue(
            MockResponse().setHeader("Content-Type", "text/event-stream")
                .setBody(sseTurn("g1", "Alice arrived ", "with Bob.")),
        )
        val r = client().chat("who did Alice arrive with", null, ownerVerified = true, deviceId = "android-1", tzOffsetSecs = -14400)
        assertTrue(r is DetectiveClient.Result.Answer)
        r as DetectiveClient.Result.Answer
        assertEquals("Alice arrived with Bob.", r.text)
        assertEquals("g1", r.sessionId)
    }

    @Test fun newSessionSendsGothamAgentAndVoiceCallerNoFilters() {
        server.enqueue(MockResponse().setBody(sseTurn("g", "ok")))
        client().chat("investigate", null, ownerVerified = true, deviceId = "android-9", tzOffsetSecs = 0)
        val req = server.takeRequest()
        assertEquals("/v1/rag/chat", req.path)
        val body = req.body.readUtf8()
        assertTrue("agent_id gotham only on new session", body.contains("\"agent_id\":\"gotham\""))
        assertTrue(body.contains("\"kind\":\"voice\""))
        assertTrue(body.contains("\"owner_verified\":true"))
        assertTrue(body.contains("\"tz_offset_secs\""))
        assertTrue("voice must not scope to one device", !body.contains("\"filters\""))
        assertTrue("device_id rides in caller", body.contains("android-9"))
    }

    @Test fun continuingSessionOmitsAgentId() {
        server.enqueue(MockResponse().setBody(sseTurn("g2", "ok")))
        client().chat("and then?", "sess-existing", ownerVerified = false, deviceId = "d", tzOffsetSecs = 0)
        val body = server.takeRequest().body.readUtf8()
        assertTrue(body.contains("\"session_id\":\"sess-existing\""))
        assertTrue("agent binding is immutable — no agent_id on a continuation", !body.contains("agent_id"))
        assertTrue(body.contains("\"owner_verified\":false"))
    }

    @Test fun confirmEventOutranksStreamedLine() {
        // A Wave-3 mutation: a natural-language line streams as tokens AND a confirm event carries
        // the deterministic summary. The client must classify Confirm (→ AWAIT_FOLLOWUP) and speak
        // the summary, not the line. (Dormant in Phase 1; the parser path must still exist.)
        val body = buildString {
            append("event: session\r\ndata: {\"session_id\":\"g3\"}\r\n\r\n")
            append("event: token\r\ndata: {\"delta\":\"I'm ready to add Casey to the watchlist.\"}\r\n\r\n")
            append("event: confirm\r\ndata: {\"action_id\":\"a1\",\"tool\":\"watchlist_add\",\"summary\":\"Add Casey to the watchlist\",\"expires_at\":123}\r\n\r\n")
            append("event: done\r\ndata: {\"message_id\":\"m3\"}\r\n\r\n")
        }
        server.enqueue(MockResponse().setBody(body))
        val r = client().chat("add casey to the watchlist", null, ownerVerified = true, deviceId = "d", tzOffsetSecs = 0)
        assertTrue(r is DetectiveClient.Result.Confirm)
        assertEquals("Add Casey to the watchlist", (r as DetectiveClient.Result.Confirm).summary)
        assertEquals("g3", r.sessionId)
    }

    @Test fun staleSessionIs404WithBody() {
        server.enqueue(MockResponse().setResponseCode(404).setBody("chat session not found"))
        val r = client().chat("q", "gone", ownerVerified = true, deviceId = "d", tzOffsetSecs = 0)
        assertTrue(r is DetectiveClient.Result.SessionNotFound)
    }

    @Test fun bare404IsEndpointMissing() {
        server.enqueue(MockResponse().setResponseCode(404).setBody("Not Found"))
        val r = client().chat("q", null, ownerVerified = true, deviceId = "d", tzOffsetSecs = 0)
        assertTrue(r is DetectiveClient.Result.EndpointMissing)
    }

    @Test fun errorEventIsError() {
        val body = "event: session\r\ndata: {\"session_id\":\"s\"}\r\n\r\n" +
            "event: error\r\ndata: {\"message\":\"boom\"}\r\n\r\n"
        server.enqueue(MockResponse().setBody(body))
        val r = client().chat("q", null, ownerVerified = true, deviceId = "d", tzOffsetSecs = 0)
        assertTrue(r is DetectiveClient.Result.Error)
    }

    @Test fun emptyAnswerIsError() {
        val body = "event: session\r\ndata: {\"session_id\":\"s\"}\r\n\r\n" +
            "event: done\r\ndata: {\"message_id\":\"m\"}\r\n\r\n"
        server.enqueue(MockResponse().setBody(body))
        val r = client().chat("q", null, ownerVerified = true, deviceId = "d", tzOffsetSecs = 0)
        assertTrue(r is DetectiveClient.Result.Error)
    }

    @Test fun bearerSentWhenTokenSet() {
        server.enqueue(MockResponse().setBody(sseTurn("g", "ok")))
        client("secret").chat("q", null, ownerVerified = true, deviceId = "d", tzOffsetSecs = 0)
        assertEquals("Bearer secret", server.takeRequest().getHeader("Authorization"))
    }

    @Test fun noBearerWhenTokenBlank() {
        server.enqueue(MockResponse().setBody(sseTurn("g", "ok")))
        client("").chat("q", null, ownerVerified = true, deviceId = "d", tzOffsetSecs = 0)
        assertNull(server.takeRequest().getHeader("Authorization"))
    }
}
