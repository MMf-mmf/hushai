package com.hushai.android

import com.hushai.android.net.PersonsClient
import okhttp3.OkHttpClient
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import java.io.File

class PersonsClientTest {
    private lateinit var server: MockWebServer

    @Before fun setUp() { server = MockWebServer(); server.start() }
    @After fun tearDown() { server.shutdown() }

    private fun client(token: String = "") =
        PersonsClient(OkHttpClient(), server.url("/").toString(), token)

    @Test fun parsesPersonList() {
        server.enqueue(
            MockResponse().setBody(
                """[{"person_id":"abc-123","display_name":"Alice","n_samples":7,
                   "sample_sighting_unix_nanos":[3000,2000,1000]},
                   {"person_id":"def-456","display_name":null,"n_samples":2,
                   "sample_sighting_unix_nanos":[]}]""",
            ),
        )
        val list = client().listPersons()
        assertNotNull(list)
        assertEquals(2, list!!.size)
        assertEquals("abc-123", list[0].id)
        assertEquals("Alice", list[0].name)
        assertEquals(7L, list[0].nSamples)
        assertEquals(listOf(3000L, 2000L, 1000L), list[0].sampleSightingsNanos)
        assertNull(list[1].name) // null display_name -> null, not "null"
        assertTrue(list[1].sampleSightingsNanos.isEmpty())

        assertEquals("/v1/persons", server.takeRequest().path)
    }

    @Test fun listReturnsNullOnHttpError() {
        server.enqueue(MockResponse().setResponseCode(500))
        assertNull(client().listPersons())
    }

    @Test fun setNameIssuesPatchWithBody() {
        server.enqueue(MockResponse().setBody("""{"person_id":"abc","display_name":"Bob","n_samples":1}"""))
        assertTrue(client().setName("abc", "Bob"))

        val req = server.takeRequest()
        assertEquals("PATCH", req.method)
        assertEquals("/v1/persons/abc", req.path)
        val body = req.body.readUtf8()
        assertTrue(body.contains("\"display_name\""))
        assertTrue(body.contains("Bob"))
    }

    @Test fun mergeIssuesPostWithInto() {
        server.enqueue(MockResponse().setResponseCode(200))
        assertTrue(client().merge("loser-1", "winner-2"))

        val req = server.takeRequest()
        assertEquals("POST", req.method)
        assertEquals("/v1/persons/loser-1/merge", req.path)
        val body = req.body.readUtf8()
        assertTrue(body.contains("\"into\""))
        assertTrue(body.contains("winner-2"))
    }

    @Test fun downloadSampleFaceWritesBytes() {
        server.enqueue(MockResponse().setBody("JPEGBYTES"))
        val dir = File(System.getProperty("java.io.tmpdir"), "hushai-face-test-${System.nanoTime()}").apply { mkdirs() }
        try {
            val file = client().downloadSampleFace("abc", dir)
            assertNotNull(file)
            assertArrayEquals("JPEGBYTES".toByteArray(), file!!.readBytes())
            assertEquals("/v1/persons/abc/sample-face", server.takeRequest().path)
            file.delete()
        } finally {
            dir.deleteRecursively()
        }
    }

    @Test fun sendsBearerWhenTokenSet() {
        server.enqueue(MockResponse().setBody("[]"))
        client("secret").listPersons()
        assertEquals("Bearer secret", server.takeRequest().getHeader("Authorization"))
    }

    @Test fun noBearerWhenTokenBlank() {
        server.enqueue(MockResponse().setBody("[]"))
        client("").listPersons()
        assertNull(server.takeRequest().getHeader("Authorization"))
    }
}
