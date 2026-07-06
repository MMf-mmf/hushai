package com.hushai.android.assistant

import java.util.Locale
import kotlin.math.sqrt

/** x-vector helpers for owner-voice enrollment + verification (pure, unit-tested). */
object SpeakerMath {

    /** Cosine similarity in [-1, 1]; -1 for empty / mismatched / zero vectors. */
    fun cosine(a: FloatArray, b: FloatArray): Float {
        if (a.isEmpty() || a.size != b.size) return -1f
        var dot = 0.0
        var na = 0.0
        var nb = 0.0
        for (i in a.indices) {
            dot += a[i].toDouble() * b[i]
            na += a[i].toDouble() * a[i]
            nb += b[i].toDouble() * b[i]
        }
        if (na == 0.0 || nb == 0.0) return -1f
        return (dot / (sqrt(na) * sqrt(nb))).toFloat()
    }

    /** Mean of L2-normalized vectors, re-normalized — a robust enrollment centroid. */
    fun centroid(vectors: List<FloatArray>): FloatArray? {
        val nonEmpty = vectors.filter { it.isNotEmpty() }
        if (nonEmpty.isEmpty()) return null
        val dim = nonEmpty.first().size
        val acc = DoubleArray(dim)
        var used = 0
        for (v in nonEmpty) {
            if (v.size != dim) continue
            var norm = 0.0
            for (x in v) norm += x.toDouble() * x
            norm = sqrt(norm)
            if (norm == 0.0) continue
            for (i in 0 until dim) acc[i] += v[i] / norm
            used++
        }
        if (used == 0) return null
        val out = FloatArray(dim) { acc[it].toFloat() }
        var n = 0.0
        for (x in out) n += x.toDouble() * x
        n = sqrt(n)
        if (n == 0.0) return null
        for (i in out.indices) out[i] = (out[i] / n).toFloat()
        return out
    }

    fun format(v: FloatArray): String = v.joinToString(",")

    fun parse(s: String): FloatArray? {
        if (s.isBlank()) return null
        return runCatching { s.split(",").map { it.trim().toFloat() }.toFloatArray() }
            .getOrNull()
            ?.takeIf { it.isNotEmpty() }
    }

    // --- Multi-vector owner profile -------------------------------------------------------
    //
    // Enrollment stores the individual sample vectors (6 guided "core" samples + up to
    // [MAX_ADAPTED] high-confidence "adapted" slots appended during normal use), not just one
    // centroid: verification scores against the top-2 mean of per-vector cosines, which lifts
    // cross-environment owner scores WITHOUT touching the accept threshold. Serialized as
    // `kind|f,f,…;kind|f,f,…` (kind: c=core, a=adapted); a legacy single-centroid string
    // (plain comma floats) parses as a 1-core-vector profile — no migration, no data loss.

    const val MAX_ADAPTED = 4

    class OwnerProfile(val core: List<FloatArray>, val adapted: List<FloatArray>) {
        fun all(): List<FloatArray> = core + adapted
        fun isEmpty(): Boolean = core.isEmpty() && adapted.isEmpty()

        /** Append a high-confidence verification vector; oldest ADAPTED slot evicts at the
         *  cap — the guided core samples are never evicted. */
        fun withAdapted(v: FloatArray): OwnerProfile {
            val next = (adapted + listOf(v)).takeLast(MAX_ADAPTED)
            return OwnerProfile(core, next)
        }
    }

    fun formatProfile(p: OwnerProfile): String =
        (p.core.map { "c|" + format(it) } + p.adapted.map { "a|" + format(it) })
            .joinToString(";")

    fun parseProfile(s: String): OwnerProfile? {
        if (s.isBlank()) return null
        if (!s.contains(';') && !s.contains('|')) {
            // Legacy single-centroid format.
            val v = parse(s) ?: return null
            return OwnerProfile(core = listOf(v), adapted = emptyList())
        }
        val core = ArrayList<FloatArray>()
        val adapted = ArrayList<FloatArray>()
        for (entry in s.split(";")) {
            if (entry.isBlank()) continue
            val sep = entry.indexOf('|')
            if (sep <= 0) return null
            val v = parse(entry.substring(sep + 1)) ?: return null
            when (entry.substring(0, sep)) {
                "c" -> core.add(v)
                "a" -> adapted.add(v)
                else -> return null
            }
        }
        val p = OwnerProfile(core, adapted)
        return if (p.isEmpty()) null else p
    }

    /** Highest cosine of [v] against any profile vector. */
    fun maxSim(v: FloatArray, profile: List<FloatArray>): Float =
        profile.maxOfOrNull { cosine(v, it) } ?: -1f

    /**
     * Verification score: mean of the top-[k] per-vector cosines. Damps one noisy enrolled
     * vector (unlike pure max) while still crediting the closest environments (unlike a
     * single-centroid cosine, which averages away far-field/noisy enrollment samples).
     */
    fun topKMeanSim(v: FloatArray, profile: List<FloatArray>, k: Int = 2): Float {
        if (profile.isEmpty()) return -1f
        val sims = profile.map { cosine(v, it) }.sortedDescending()
        val take = sims.take(k.coerceAtLeast(1))
        return (take.sum() / take.size)
    }

    /** One enrollment sample's verdict from [enrollGate]. */
    sealed class GateResult {
        object Accept : GateResult()
        data class Reject(val reason: String) : GateResult()
    }

    /**
     * Per-sample enrollment gate (pure, unit-tested): a sample is accepted only when it
     * carries a real voiceprint, enough speech for a stable x-vector, and — from the second
     * sample on — is consistent with the samples already accepted (cosine vs their centroid
     * >= [consistencyFloor], deliberately BELOW the verify threshold because legitimate
     * cross-environment self-similarity is lower). Rejects a second person or a TV grabbing
     * a slot mid-enrollment.
     */
    fun enrollGate(
        spk: FloatArray?,
        wordCount: Int,
        accepted: List<FloatArray>,
        minWords: Int = 4,
        consistencyFloor: Float = 0.35f,
    ): GateResult {
        if (spk == null || spk.isEmpty()) {
            return GateResult.Reject("didn't catch a voiceprint — speak a little longer")
        }
        if (accepted.isNotEmpty() && spk.size != accepted.first().size) {
            return GateResult.Reject("voiceprint mismatch — try that phrase again")
        }
        if (wordCount < minWords) {
            return GateResult.Reject("too short — say the whole phrase")
        }
        if (accepted.isNotEmpty()) {
            val c = centroid(accepted) ?: return GateResult.Accept
            if (cosine(spk, c) < consistencyFloor) {
                return GateResult.Reject("that didn't sound like the same voice — just you, please")
            }
        }
        return GateResult.Accept
    }

    /** First index in [haystack] where [needle] appears as a contiguous run, else -1. */
    fun subsequenceIndex(haystack: List<String>, needle: List<String>): Int {
        if (needle.isEmpty() || needle.size > haystack.size) return -1
        outer@ for (i in 0..haystack.size - needle.size) {
            for (j in needle.indices) if (haystack[i + j] != needle[j]) continue@outer
            return i
        }
        return -1
    }

    fun tokens(text: String): List<String> =
        text.trim().split(Regex("\\s+")).filter { it.isNotBlank() }

    fun normalizeWord(w: String): String = w.trim().lowercase(Locale.US)
}
