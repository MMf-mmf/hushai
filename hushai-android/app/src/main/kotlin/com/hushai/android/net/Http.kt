package com.hushai.android.net

import okhttp3.OkHttpClient
import java.util.concurrent.TimeUnit

/** Shared OkHttp clients. Uploads tolerate slow links; preflight fails fast. */
object Http {
    val upload: OkHttpClient by lazy {
        OkHttpClient.Builder()
            .connectTimeout(5, TimeUnit.SECONDS)
            .writeTimeout(60, TimeUnit.SECONDS)
            .readTimeout(60, TimeUnit.SECONDS)
            .callTimeout(120, TimeUnit.SECONDS)
            .retryOnConnectionFailure(true)
            .build()
    }

    val probe: OkHttpClient by lazy {
        upload.newBuilder()
            .connectTimeout(3, TimeUnit.SECONDS)
            .callTimeout(4, TimeUnit.SECONDS)
            .build()
    }

    /** RAG queries: local LLM generation on CPU can take a while — be patient. */
    val rag: OkHttpClient by lazy {
        upload.newBuilder()
            .readTimeout(180, TimeUnit.SECONDS)
            .callTimeout(190, TimeUnit.SECONDS)
            .build()
    }
}
