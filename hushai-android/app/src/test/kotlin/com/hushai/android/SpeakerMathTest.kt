package com.hushai.android

import com.hushai.android.assistant.SpeakerMath
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class SpeakerMathTest {

    @Test fun cosineIdenticalIsOne() {
        val v = floatArrayOf(1f, 2f, 3f)
        assertEquals(1f, SpeakerMath.cosine(v, v), 1e-5f)
    }

    @Test fun cosineOppositeIsMinusOne() {
        assertEquals(-1f, SpeakerMath.cosine(floatArrayOf(1f, 0f), floatArrayOf(-1f, 0f)), 1e-5f)
    }

    @Test fun cosineOrthogonalIsZero() {
        assertEquals(0f, SpeakerMath.cosine(floatArrayOf(1f, 0f), floatArrayOf(0f, 1f)), 1e-5f)
    }

    @Test fun cosineMismatchedOrEmptyIsMinusOne() {
        assertEquals(-1f, SpeakerMath.cosine(floatArrayOf(1f, 2f), floatArrayOf(1f)), 0f)
        assertEquals(-1f, SpeakerMath.cosine(floatArrayOf(), floatArrayOf()), 0f)
        assertEquals(-1f, SpeakerMath.cosine(floatArrayOf(0f, 0f), floatArrayOf(1f, 1f)), 0f)
    }

    @Test fun centroidIsUnitLengthAndNearMembers() {
        val a = floatArrayOf(1f, 0f, 0f)
        val b = floatArrayOf(0.9f, 0.1f, 0f)
        val c = SpeakerMath.centroid(listOf(a, b))!!
        var norm = 0.0
        for (x in c) norm += x.toDouble() * x
        assertEquals(1.0, norm, 1e-4) // unit length
        // The centroid should look like its members (same speaker → high cosine).
        assertTrue(SpeakerMath.cosine(c, a) > 0.9f)
    }

    @Test fun centroidEmptyIsNull() {
        assertNull(SpeakerMath.centroid(emptyList()))
        assertNull(SpeakerMath.centroid(listOf(floatArrayOf())))
    }

    @Test fun formatParseRoundTrip() {
        val v = floatArrayOf(0.5f, -1.25f, 3f)
        val parsed = SpeakerMath.parse(SpeakerMath.format(v))!!
        assertEquals(v.size, parsed.size)
        for (i in v.indices) assertEquals(v[i], parsed[i], 1e-6f)
    }

    @Test fun parseBlankIsNull() {
        assertNull(SpeakerMath.parse(""))
        assertNull(SpeakerMath.parse("   "))
    }

    @Test fun subsequenceIndexFindsWakePhrase() {
        val hay = SpeakerMath.tokens("hey computer what did i say about lunch")
        assertEquals(1, SpeakerMath.subsequenceIndex(hay, listOf("computer")))
        assertEquals(0, SpeakerMath.subsequenceIndex(hay, listOf("hey", "computer")))
        assertEquals(-1, SpeakerMath.subsequenceIndex(hay, listOf("alexa")))
        assertEquals(-1, SpeakerMath.subsequenceIndex(hay, emptyList()))
    }

    @Test fun normalizeWordLowercasesAndTrims() {
        assertEquals("computer", SpeakerMath.normalizeWord("  Computer "))
    }
}
