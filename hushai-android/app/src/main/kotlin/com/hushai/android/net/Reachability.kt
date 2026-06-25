package com.hushai.android.net

import okhttp3.OkHttpClient
import okhttp3.Request
import java.io.IOException

/**
 * Preflight reachability/health for a base URL. `/healthz` is unauthenticated and
 * returns 200 whenever the process is up (verified); `/readyz` returns 503 when
 * Postgres is down or disk is low. Only a connect/timeout failure is "unreachable".
 */
class Reachability(
    private val client: OkHttpClient,
    baseUrl: String,
) {
    private val base = baseUrl.trimEnd('/')

    data class Health(
        val reachable: Boolean, // got any HTTP response (vs connect/timeout failure)
        val live: Boolean,      // /healthz == 200
        val ready: Boolean,     // /readyz == 200
        val detail: String,
    )

    fun check(): Health {
        val healthz = get("$base/healthz")
            ?: return Health(reachable = false, live = false, ready = false, detail = "unreachable")
        val ready = get("$base/readyz") == 200
        return Health(
            reachable = true,
            live = healthz == 200,
            ready = ready,
            detail = "healthz=$healthz readyz=${if (ready) 200 else "not-ready"}",
        )
    }

    private fun get(url: String): Int? = try {
        client.newCall(Request.Builder().url(url).get().build()).execute().use { it.code }
    } catch (e: IOException) {
        null
    }
}
