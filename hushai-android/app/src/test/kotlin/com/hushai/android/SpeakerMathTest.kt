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

    // --- Multi-vector profile ---------------------------------------------------------

    @Test fun profileFormatParseRoundTrip() {
        val p = SpeakerMath.OwnerProfile(
            core = listOf(floatArrayOf(1f, 0f), floatArrayOf(0.9f, 0.1f)),
            adapted = listOf(floatArrayOf(0.8f, 0.2f)),
        )
        val parsed = SpeakerMath.parseProfile(SpeakerMath.formatProfile(p))!!
        assertEquals(2, parsed.core.size)
        assertEquals(1, parsed.adapted.size)
        assertEquals(0.9f, parsed.core[1][0], 1e-6f)
        assertEquals(0.2f, parsed.adapted[0][1], 1e-6f)
    }

    @Test fun legacySingleCentroidParsesAsOneCoreVector() {
        val parsed = SpeakerMath.parseProfile("0.5,-1.25,3.0")!!
        assertEquals(1, parsed.core.size)
        assertEquals(0, parsed.adapted.size)
        assertEquals(-1.25f, parsed.core[0][1], 1e-6f)
        assertNull(SpeakerMath.parseProfile(""))
        assertNull(SpeakerMath.parseProfile("x|1,2"))
    }

    @Test fun adaptedSlotsRingEvictButCoreNever() {
        var p = SpeakerMath.OwnerProfile(core = listOf(floatArrayOf(1f, 0f)), adapted = emptyList())
        for (i in 0 until SpeakerMath.MAX_ADAPTED + 2) {
            p = p.withAdapted(floatArrayOf(i.toFloat(), 1f))
        }
        assertEquals(SpeakerMath.MAX_ADAPTED, p.adapted.size)
        assertEquals(1, p.core.size)
        // Oldest adapted evicted: the first remaining is i=2.
        assertEquals(2f, p.adapted[0][0], 1e-6f)
    }

    @Test fun topKMeanLiftsOwnerWithoutLiftingStranger() {
        // A realistic 6-sample profile: four near-field samples (e0-ish) and two far-field
        // samples (e1-ish). When the owner speaks far-field, the single centroid (dominated
        // by the four near-field samples) scores LOW — the "enrolled but not recognized in
        // another session/room" failure — while the top-2 mean credits the two matching
        // enrollment conditions.
        val near = floatArrayOf(1f, 0f, 0f)
        val far = floatArrayOf(0f, 1f, 0f)
        val profile = listOf(near, near, near, near, far, far)
        val ownerFarNow = floatArrayOf(0.1f, 0.99f, 0f)
        val centroid = SpeakerMath.centroid(profile)!!
        val centroidScore = SpeakerMath.cosine(ownerFarNow, centroid)
        val topK = SpeakerMath.topKMeanSim(ownerFarNow, profile)
        assertTrue("top-2 ($topK) must beat centroid ($centroidScore)", topK > centroidScore + 0.2f)
        assertTrue("owner passes the 0.5 gate: $topK", topK >= 0.5f)
        // A stranger orthogonal to every sample stays low under every scoring.
        val stranger = floatArrayOf(0f, 0f, 1f)
        assertTrue(SpeakerMath.topKMeanSim(stranger, profile) < 0.1f)
        assertEquals(-1f, SpeakerMath.topKMeanSim(stranger, emptyList()), 0f)
        assertEquals(1f, SpeakerMath.maxSim(near, profile), 1e-6f)
    }

    // --- Enrollment gate ---------------------------------------------------------------

    @Test fun enrollGateRejectsMissingShortAndInconsistentSamples() {
        val accepted = listOf(floatArrayOf(1f, 0f, 0f), floatArrayOf(0.95f, 0.05f, 0f))
        // No voiceprint.
        assertTrue(SpeakerMath.enrollGate(null, 8, accepted) is SpeakerMath.GateResult.Reject)
        assertTrue(SpeakerMath.enrollGate(floatArrayOf(), 8, accepted) is SpeakerMath.GateResult.Reject)
        // Dim mismatch.
        assertTrue(SpeakerMath.enrollGate(floatArrayOf(1f), 8, accepted) is SpeakerMath.GateResult.Reject)
        // Too little speech.
        assertTrue(SpeakerMath.enrollGate(floatArrayOf(1f, 0f, 0f), 2, accepted) is SpeakerMath.GateResult.Reject)
        // A second person / TV: orthogonal to the accepted centroid.
        assertTrue(
            SpeakerMath.enrollGate(floatArrayOf(0f, 0f, 1f), 8, accepted) is SpeakerMath.GateResult.Reject,
        )
    }

    @Test fun enrollGateAcceptsFirstAndConsistentSamples() {
        // First sample: no consistency reference yet.
        assertTrue(SpeakerMath.enrollGate(floatArrayOf(1f, 0f, 0f), 5, emptyList()) is SpeakerMath.GateResult.Accept)
        // Later sample consistent with the accepted set — including a legitimately lower
        // cross-environment similarity (the floor is deliberately below the verify gate).
        val accepted = listOf(floatArrayOf(1f, 0f, 0f))
        assertTrue(
            SpeakerMath.enrollGate(floatArrayOf(0.5f, 0.86f, 0f), 5, accepted) is SpeakerMath.GateResult.Accept,
        )
    }
}
