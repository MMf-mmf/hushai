package com.hushai.android

import com.hushai.android.net.EventsClient
import okhttp3.OkHttpClient
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

class EventsClientTest {
    private lateinit var server: MockWebServer

    @Before fun setUp() { server = MockWebServer(); server.start() }
    @After fun tearDown() { server.shutdown() }

    private fun client(token: String = "") =
        EventsClient(OkHttpClient(), server.url("/").toString(), token)

    @Test fun parsesFeedAndNulls() {
        server.enqueue(
            MockResponse().setBody(
                """[
                  {"delivery_id":"d1","event_type":"unknown_person","severity":"warning",
                   "device_id":"cam1","subject_label":"Bob","created_unix_nanos":123,"acknowledged":false},
                  {"delivery_id":"d2","event_type":"object_seen","severity":null,
                   "device_id":null,"subject_label":null,"created_unix_nanos":0,"acknowledged":true}
                ]""",
            ),
        )
        val feed = client().listFeed(status = "pending")!!
        assertEquals(2, feed.size)
        assertEquals("d1", feed[0].deliveryId)
        assertEquals("unknown_person", feed[0].eventType)
        assertEquals("warning", feed[0].severity)
        assertEquals("Bob", feed[0].subjectLabel)
        assertEquals(123L, feed[0].createdUnixNanos)
        assertFalse(feed[0].acknowledged)
        // explicit JSON null must decode to kotlin null (not the string "null")
        assertNull(feed[1].severity)
        assertNull(feed[1].deviceId)
        assertNull(feed[1].subjectLabel)
        assertTrue(feed[1].acknowledged)
        // the status filter rode along on the query string
        assertTrue(server.takeRequest().path!!.contains("status=pending"))
    }

    @Test fun bearerHeaderSetWhenTokenPresent() {
        server.enqueue(MockResponse().setBody("[]"))
        client("tok").listFeed()
        assertEquals("Bearer tok", server.takeRequest().getHeader("Authorization"))
    }

    @Test fun ackPostsToAckPath() {
        server.enqueue(MockResponse().setResponseCode(200).setBody("""{"acknowledged":"d1"}"""))
        assertTrue(client().ack("d1"))
        val rec = server.takeRequest()
        assertEquals("POST", rec.method)
        assertTrue(rec.path!!.endsWith("/v1/events/feed/d1/ack"))
    }

    @Test fun httpErrorReturnsNull() {
        server.enqueue(MockResponse().setResponseCode(500))
        assertNull(client().listFeed())
    }
}
