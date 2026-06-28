package com.hushai.android.net

import com.hushai.android.util.HushaiLog
import com.hushai.android.util.StatusBus
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledFuture
import java.util.concurrent.TimeUnit

/**
 * The single authoritative "can we deliver to the backend?" state, fusing three
 * signals: OS link state ([NetworkMonitor]), real drain-loop upload outcomes (the
 * authoritative proof), and a debounced periodic [Reachability] probe used only
 * while we believe we're offline (so a recovered server wakes the drain loop without
 * waiting out a 30 s backoff).
 *
 * All inputs funnel through a single-thread reducer so transitions serialize. The
 * periodic probe runs on its own thread and only READS volatile snapshots + calls
 * [onWake]; it never mutates state — the resulting upload outcome does.
 *
 * Edges: an actual upload 200 is hard proof and flips us out of offline immediately;
 * a link-up alone is a soft signal that only triggers a probe + wake (the upload
 * outcome then drives the real transition), which avoids banner flicker on Wi-Fi flap.
 */
class ConnectivityState(
    private val reachability: Reachability,
    private val onWake: () -> Unit,
    private val onStateChange: (DeliveryState) -> Unit,
    private val probeIntervalMs: Long = 15_000,
) {
    enum class DeliveryState { ONLINE, OFFLINE, DRAINING }

    private val reducer = Executors.newSingleThreadExecutor { r -> Thread(r, "hushai-connstate") }
    private val prober = Executors.newSingleThreadScheduledExecutor { r -> Thread(r, "hushai-probe") }
    private var probeTask: ScheduledFuture<*>? = null

    // Written only by the reducer thread; read by the prober thread -> @Volatile.
    @Volatile private var state = DeliveryState.ONLINE
    @Volatile private var linkUp = true
    @Volatile private var pending = 0
    private var backlogTotal = 0 // reducer-only

    fun start(initialOnline: Boolean) {
        post {
            linkUp = initialOnline
            publish()
        }
        probeTask = prober.scheduleWithFixedDelay(
            { runCatching { maybeProbe() }.onFailure { HushaiLog.error("probe failed", it) } },
            probeIntervalMs, probeIntervalMs, TimeUnit.MILLISECONDS,
        )
    }

    fun stop() {
        probeTask?.cancel(false)
        runCatching { prober.shutdownNow() }
        runCatching { reducer.shutdownNow() }
    }

    fun onLinkUp() = post {
        linkUp = true
        // Soft signal: don't claim ONLINE, but if we're holding a backlog, kick the
        // drain loop now (the upload outcome decides the real state).
        if (state == DeliveryState.OFFLINE && pending > 0) onWake()
    }

    fun onLinkDown() = post {
        linkUp = false
        transition(DeliveryState.OFFLINE)
    }

    fun onUploadSuccess() = post {
        if (state == DeliveryState.OFFLINE) backlogTotal = pending.coerceAtLeast(1) // latch denominator
        transition(if (pending > 0) DeliveryState.DRAINING else DeliveryState.ONLINE)
    }

    /** Transient delivery failure (network/timeout/5xx/429/507/401). Content errors
     *  (4xx quarantine paths) must NOT call this — they aren't connectivity. */
    fun onUploadFailure() = post {
        transition(DeliveryState.OFFLINE)
    }

    fun onBufferSizeChanged(p: Int) = post {
        pending = p
        if (state == DeliveryState.DRAINING && p == 0) {
            transition(DeliveryState.ONLINE)
        } else {
            publish()
        }
    }

    private fun transition(next: DeliveryState) {
        if (next == state) {
            publish()
            return
        }
        state = next
        if (next == DeliveryState.ONLINE) backlogTotal = 0
        publish()
        runCatching { onStateChange(next) }
    }

    private fun publish() {
        val s = state
        val total = backlogTotal
        StatusBus.update {
            it.copy(
                offline = s == DeliveryState.OFFLINE,
                draining = s == DeliveryState.DRAINING,
                backlogTotal = total,
            )
        }
    }

    private fun maybeProbe() {
        if (state == DeliveryState.OFFLINE && linkUp && pending > 0) {
            if (reachability.check().live) onWake()
        }
    }

    private fun post(block: () -> Unit) {
        runCatching { reducer.execute(block) }
    }
}
