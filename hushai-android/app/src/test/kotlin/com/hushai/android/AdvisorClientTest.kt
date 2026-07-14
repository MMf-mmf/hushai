package com.hushai.android

import com.hushai.android.net.AdvisorClient
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
 * SSE consumer + request shape for the advisor consult (`/v1/advisor/chat`). MockWebServer, same
 * style as [RagChatClientTest]; bodies include keep-alive `:` lines, `\r\n` endings, and the
 * ignored `phase`/`memory` events to exercise the parser's tolerance.
 */
class AdvisorClientTest {
    private lateinit var server: MockWebServer

    @Before fun setUp() { server = MockWebServer(); server.start() }
    @After fun tearDown() { server.shutdown() }

    private fun client(token: String = "") =
        AdvisorClient(OkHttpClient(), server.url("/").toString(), token)

    @Test fun questionsOutcome() {
        val body = buildString {
            append(": keep-alive\r\n\r\n")
            append("event: session\r\ndata: {\"session_id\":\"c1\",\"phase\":\"gathering\"}\r\n\r\n")
            append("event: phase\r\ndata: {\"phase\":\"gathering\"}\r\n\r\n")
            append("event: questions\r\ndata: {\"round\":1,\"questions\":[\"Who is involved?\",\"What is your goal?\"]}\r\n\r\n")
            append("event: done\r\ndata: {\"message_id\":\"m1\"}\r\n\r\n")
        }
        server.enqueue(MockResponse().setHeader("Content-Type", "text/event-stream").setBody(body))
        val r = client().chat("I have a problem", null)
        assertTrue(r is AdvisorClient.Result.Questions)
        r as AdvisorClient.Result.Questions
        assertEquals(1, r.round)
        assertEquals(listOf("Who is involved?", "What is your goal?"), r.questions)
        assertEquals("c1", r.sessionId)
    }

    @Test fun answerOutcomeWithLastChaptersSetWins() {
        val body = buildString {
            append("event: session\r\ndata: {\"session_id\":\"c2\"}\r\n\r\n")
            append("event: memory\r\ndata: {\"recalled\":1,\"nearest_distance\":0.42}\r\n\r\n")
            // Two chapters events — the LAST converged set must win.
            append("event: chapters\r\ndata: {\"iteration\":0,\"chapters\":[{\"no\":7,\"title\":\"Anchoring\"}]}\r\n\r\n")
            append("event: chapters\r\ndata: {\"iteration\":1,\"chapters\":[{\"no\":12,\"title\":\"Reciprocity\"},{\"no\":19,\"title\":null}]}\r\n\r\n")
            append("event: token\r\ndata: {\"delta\":\"Offer \"}\r\n\r\n")
            append("event: token\r\ndata: {\"delta\":\"a concession.\"}\r\n\r\n")
            append("event: done\r\ndata: {\"message_id\":\"m2\"}\r\n\r\n")
        }
        server.enqueue(MockResponse().setBody(body))
        val r = client().chat("full detail", "c2")
        assertTrue(r is AdvisorClient.Result.Answer)
        r as AdvisorClient.Result.Answer
        assertEquals("Offer a concession.", r.text)
        assertEquals("c2", r.sessionId)
        assertEquals(listOf(12, 19), r.chapters.map { it.no })
        assertEquals(listOf("Reciprocity", null), r.chapters.map { it.title })
    }

    @Test fun questionsOutrankEmptyText() {
        // A questions turn streams no token text; it must classify as Questions, not empty-Error.
        val body = "event: session\r\ndata: {\"session_id\":\"c3\"}\r\n\r\n" +
            "event: questions\r\ndata: {\"round\":2,\"questions\":[\"One more thing?\"]}\r\n\r\n" +
            "event: done\r\ndata: {}\r\n\r\n"
        server.enqueue(MockResponse().setBody(body))
        assertTrue(client().chat("q", "c3") is AdvisorClient.Result.Questions)
    }

    @Test fun conflict409IsBusy() {
        server.enqueue(MockResponse().setResponseCode(409).setBody("a turn is already in progress"))
        assertTrue(client().chat("q", "c4") is AdvisorClient.Result.Busy)
    }

    @Test fun notFound404IsSessionNotFound() {
        server.enqueue(MockResponse().setResponseCode(404).setBody("advisor session not found"))
        assertTrue(client().chat("q", "gone") is AdvisorClient.Result.SessionNotFound)
    }

    @Test fun errorEventIsError() {
        val body = "event: session\r\ndata: {\"session_id\":\"c5\"}\r\n\r\n" +
            "event: error\r\ndata: {\"message\":\"boom\"}\r\n\r\n"
        server.enqueue(MockResponse().setBody(body))
        val r = client().chat("q", null)
        assertTrue(r is AdvisorClient.Result.Error)
        assertEquals("boom", (r as AdvisorClient.Result.Error).reason)
    }

    @Test fun emptyAnswerIsError() {
        val body = "event: session\r\ndata: {\"session_id\":\"c6\"}\r\n\r\n" +
            "event: done\r\ndata: {}\r\n\r\n"
        server.enqueue(MockResponse().setBody(body))
        assertTrue(client().chat("q", "c6") is AdvisorClient.Result.Error)
    }

    @Test fun requestShapeIsSlimBodyWithSessionAndBearerAndPath() {
        server.enqueue(MockResponse().setBody("event: session\r\ndata: {\"session_id\":\"c\"}\r\n\r\nevent: token\r\ndata: {\"delta\":\"ok\"}\r\n\r\nevent: done\r\ndata: {}\r\n\r\n"))
        client("secret").chat("hello there", "sess-x")
        val req = server.takeRequest()
        assertEquals("/v1/advisor/chat", req.path)
        val body = req.body.readUtf8()
        assertTrue(body.contains("\"message\":\"hello there\""))
        assertTrue(body.contains("\"session_id\":\"sess-x\""))
        // Slim body — none of the rag-only fields.
        assertTrue("no filters", !body.contains("filters"))
        assertTrue("no playback", !body.contains("playback"))
        assertTrue("no exhaustive", !body.contains("exhaustive"))
        assertEquals("Bearer secret", req.getHeader("Authorization"))
    }

    @Test fun newSessionOmitsSessionId() {
        server.enqueue(MockResponse().setBody("event: session\r\ndata: {\"session_id\":\"c\"}\r\n\r\nevent: token\r\ndata: {\"delta\":\"ok\"}\r\n\r\nevent: done\r\ndata: {}\r\n\r\n"))
        client().chat("q", null)
        val body = server.takeRequest().body.readUtf8()
        assertTrue("new consult sends no session_id", !body.contains("session_id"))
    }

    @Test fun noBearerWhenTokenBlank() {
        server.enqueue(MockResponse().setBody("event: session\r\ndata: {\"session_id\":\"c\"}\r\n\r\nevent: token\r\ndata: {\"delta\":\"ok\"}\r\n\r\nevent: done\r\ndata: {}\r\n\r\n"))
        client("").chat("q", null)
        assertNull(server.takeRequest().getHeader("Authorization"))
    }
}
