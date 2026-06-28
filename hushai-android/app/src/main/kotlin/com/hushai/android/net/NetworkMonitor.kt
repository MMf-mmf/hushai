package com.hushai.android.net

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import com.hushai.android.util.HushaiLog

/**
 * Reports OS link up/down only (NOT backend reachability — a validated network can
 * still have a dead server). Requires INTERNET + VALIDATED so a captive-portal /
 * limited Wi-Fi isn't called "online". Callbacks arrive on a binder thread; we keep
 * [online] @Volatile and do no blocking work in them — just flip state and fire the
 * lambdas, which feed [ConnectivityState].
 */
class NetworkMonitor(
    context: Context,
    private val onAvailable: () -> Unit,
    private val onLost: () -> Unit,
) {
    private val cm = context.getSystemService(ConnectivityManager::class.java)
    private val available = HashSet<Network>()
    private var registered = false

    @Volatile
    var online: Boolean = false
        private set

    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) = onValidated(network, validated(network))
        override fun onLost(network: Network) = onGone(network)
        override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) =
            onValidated(network, caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED))
    }

    fun start() {
        if (registered) return
        val request = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED)
            .build()
        runCatching { cm?.registerNetworkCallback(request, callback) }
            .onSuccess { registered = true }
            .onFailure { HushaiLog.error("NetworkMonitor register failed", it) }
        // Seed from the current active network so we aren't wrongly "offline" before
        // the first callback fires.
        val active = cm?.activeNetwork
        if (active != null && validated(active)) onValidated(active, true)
    }

    fun stop() {
        if (!registered) return
        registered = false
        runCatching { cm?.unregisterNetworkCallback(callback) }
        synchronized(available) { available.clear() }
        online = false
    }

    private fun validated(network: Network): Boolean =
        cm?.getNetworkCapabilities(network)?.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED) == true

    private fun onValidated(network: Network, isValidated: Boolean) {
        val transitioned = synchronized(available) {
            val wasEmpty = available.isEmpty()
            val changed = if (isValidated) available.add(network) else available.remove(network)
            online = available.isNotEmpty()
            changed && wasEmpty && available.isNotEmpty()
        }
        if (transitioned) onAvailable()
    }

    private fun onGone(network: Network) {
        val nowEmpty = synchronized(available) {
            available.remove(network)
            online = available.isNotEmpty()
            available.isEmpty()
        }
        if (nowEmpty) onLost()
    }
}
