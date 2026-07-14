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

    /** Advisor consults chain several LLM stages per turn (gathering→…→memorizing). Phase
     *  heartbeats double as SSE keep-alives, so the read timeout gates only true stalls; the
     *  call timeout is wider than rag's 190 s because a cold-model turn can exceed it. */
    val advisor: OkHttpClient by lazy {
        upload.newBuilder()
            .readTimeout(60, TimeUnit.SECONDS)
            .callTimeout(300, TimeUnit.SECONDS)
            .build()
    }
}
