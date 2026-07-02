package com.hushai.android

import com.hushai.android.net.PlatesClient
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

class PlatesClientTest {
    private lateinit var server: MockWebServer

    @Before fun setUp() { server = MockWebServer(); server.start() }
    @After fun tearDown() { server.shutdown() }

    private fun client(token: String = "") =
        PlatesClient(OkHttpClient(), server.url("/").toString(), token)

    @Test fun parsesPlateList() {
        server.enqueue(
            MockResponse().setBody(
                """[{"plate_id":"abc-123","plate_text":"ABC123","display_name":"Mom's car",
                   "n_samples":7,"n_sightings":2,"sample_sighting_unix_nanos":[3000,2000,1000],
                   "archived":true},
                   {"plate_id":"def-456","plate_text":"XYZ789","display_name":null,"n_samples":2,
                   "sample_sighting_unix_nanos":[]}]""",
            ),
        )
        val list = client().listPlates()
        assertNotNull(list)
        assertEquals(2, list!!.size)
        assertEquals("abc-123", list[0].plateId)
        assertEquals("ABC123", list[0].plateText)
        assertEquals("Mom's car", list[0].displayName)
        assertEquals(7L, list[0].nSamples)
        assertEquals(2L, list[0].nSightings) // distinct appearances, not the 7 raw reads
        assertEquals(listOf(3000L, 2000L, 1000L), list[0].sampleSightingsNanos)
        assertTrue(list[0].archived)
        assertEquals("XYZ789", list[1].plateText)
        assertNull(list[1].displayName) // null display_name -> null, not "null"
        assertEquals(2L, list[1].nSightings) // no n_sightings -> falls back to n_samples
        assertTrue(list[1].sampleSightingsNanos.isEmpty())
        assertEquals(false, list[1].archived) // absent (older backend) -> false

        assertEquals("/v1/plates", server.takeRequest().path)
    }

    @Test fun setArchivedIssuesPostToVerbRoute() {
        server.enqueue(MockResponse().setBody("""{"plate_id":"abc","plate_text":"ABC123","display_name":null,"n_samples":1,"archived":true}"""))
        assertTrue(client().setArchived("abc", true))
        val req = server.takeRequest()
        assertEquals("POST", req.method)
        assertEquals("/v1/plates/abc/archive", req.path)

        server.enqueue(MockResponse().setBody("""{"plate_id":"abc","plate_text":"ABC123","display_name":null,"n_samples":1,"archived":false}"""))
        assertTrue(client().setArchived("abc", false))
        assertEquals("/v1/plates/abc/unarchive", server.takeRequest().path)
    }

    @Test fun listReturnsNullOnHttpError() {
        server.enqueue(MockResponse().setResponseCode(500))
        assertNull(client().listPlates())
    }

    @Test fun searchHitsSearchRouteWithEncodedQuery() {
        server.enqueue(
            MockResponse().setBody(
                """[{"plate_id":"abc-123","plate_text":"ABC123","display_name":null,"n_samples":1,
                   "n_sightings":1,"sample_sighting_unix_nanos":[]}]""",
            ),
        )
        val list = client().search("abc 123")
        assertNotNull(list)
        assertEquals(1, list!!.size)
        assertEquals("ABC123", list[0].plateText)

        val req = server.takeRequest()
        assertEquals("GET", req.method)
        // URLEncoder encodes a space as '+'; the path carries the q param.
        assertEquals("/v1/plates/search?q=abc+123", req.path)
    }

    @Test fun setNameIssuesPatchWithBody() {
        server.enqueue(MockResponse().setBody("""{"plate_id":"abc","plate_text":"ABC123","display_name":"Bob","n_samples":1}"""))
        assertTrue(client().setName("abc", "Bob"))

        val req = server.takeRequest()
        assertEquals("PATCH", req.method)
        assertEquals("/v1/plates/abc", req.path)
        val body = req.body.readUtf8()
        assertTrue(body.contains("\"display_name\""))
        assertTrue(body.contains("Bob"))
    }

    @Test fun mergeIssuesPostWithInto() {
        server.enqueue(MockResponse().setResponseCode(200))
        assertTrue(client().merge("loser-1", "winner-2"))

        val req = server.takeRequest()
        assertEquals("POST", req.method)
        assertEquals("/v1/plates/loser-1/merge", req.path)
        val body = req.body.readUtf8()
        assertTrue(body.contains("\"into\""))
        assertTrue(body.contains("winner-2"))
    }

    @Test fun downloadSampleCropWritesBytes() {
        server.enqueue(MockResponse().setBody("JPEGBYTES"))
        val dir = File(System.getProperty("java.io.tmpdir"), "hushai-plate-test-${System.nanoTime()}").apply { mkdirs() }
        try {
            val file = client().downloadSampleCrop("abc", dir)
            assertNotNull(file)
            assertArrayEquals("JPEGBYTES".toByteArray(), file!!.readBytes())
            assertEquals("/v1/plates/abc/sample-crop", server.takeRequest().path)
            file.delete()
        } finally {
            dir.deleteRecursively()
        }
    }

    @Test fun sendsBearerWhenTokenSet() {
        server.enqueue(MockResponse().setBody("[]"))
        client("secret").listPlates()
        assertEquals("Bearer secret", server.takeRequest().getHeader("Authorization"))
    }

    @Test fun noBearerWhenTokenBlank() {
        server.enqueue(MockResponse().setBody("[]"))
        client("").listPlates()
        assertNull(server.takeRequest().getHeader("Authorization"))
    }
}
