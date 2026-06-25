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
