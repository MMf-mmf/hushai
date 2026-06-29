package com.hushai.android.ui

import android.text.format.DateUtils
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.core.app.NotificationManagerCompat
import com.hushai.android.net.EventsClient
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * The "Alerts" screen (roadmap A7): the mobile twin of the web Events feed. Lists the backend's
 * in-app alert deliveries (a watchlist hit / matched rule), newest first, and lets the user
 * acknowledge them. Auto-refreshes while open; the background [com.hushai.android.capture.AlertNotifier]
 * raises a system notification for new alerts even when this screen is closed. All calls go through
 * [EventsClient] (backend url + token from Settings) off the main thread.
 */
@Composable
fun EventsScreen(client: EventsClient, onBack: () -> Unit) {
    var feed by remember { mutableStateOf<List<EventsClient.FeedItem>>(emptyList()) }
    var loading by remember { mutableStateOf(true) }
    var error by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    val notifsBlocked = remember { !NotificationManagerCompat.from(context).areNotificationsEnabled() }

    suspend fun reload() {
        error = false
        withContext(Dispatchers.IO) {
            val f = client.listFeed(limit = 100) // null = call failed (retryable) vs [] = no alerts
            withContext(Dispatchers.Main) {
                if (f == null) error = true else feed = f
            }
        }
        loading = false
    }

    // Load on open, then poll while the screen is visible so new alerts appear without a tap.
    LaunchedEffect(Unit) {
        reload()
        while (true) {
            delay(10_000)
            reload()
        }
    }

    Column(
        modifier = Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            TextButton(onClick = onBack) { Text("‹ Back") }
            Spacer(Modifier.width(8.dp))
            Text("Alerts", style = MaterialTheme.typography.headlineMedium)
            Spacer(Modifier.weight(1f))
            TextButton(onClick = { scope.launch { reload() } }, enabled = !loading) { Text("Refresh") }
        }

        if (notifsBlocked) {
            Text(
                "Notifications are turned off for Hushai — alerts still appear here, but you won't get a " +
                    "phone notification. Enable them in system Settings.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.error,
            )
        }
        // A transient refresh failure must not look like "no alerts": flag the stale view.
        if (error && feed.isNotEmpty()) {
            Text(
                "Couldn't refresh just now — showing the last known alerts.",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.error,
            )
        }

        when {
            loading && feed.isEmpty() -> Text("Loading alerts…", color = MaterialTheme.colorScheme.onSurfaceVariant)
            error && feed.isEmpty() -> Text(
                "Couldn't reach the backend. Check it's running and the URL/token are set, then Refresh.",
                color = MaterialTheme.colorScheme.error,
            )
            feed.isEmpty() -> Text(
                "No alerts yet. Mark a person or plate “of interest”, or add an alert rule — matching " +
                    "sightings show up here and as a phone notification.",
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            else -> feed.forEach { item ->
                AlertCard(item) {
                    scope.launch {
                        val ok = withContext(Dispatchers.IO) { client.ack(item.deliveryId) }
                        if (ok) {
                            // Optimistically mark it read so the button clears immediately, even if a
                            // background poll that started before the ack is still in flight.
                            feed = feed.map {
                                if (it.deliveryId == item.deliveryId) it.copy(acknowledged = true) else it
                            }
                            reload()
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun AlertCard(item: EventsClient.FeedItem, onAck: () -> Unit) {
    OutlinedCard(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(14.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    (item.severity ?: "info").uppercase(),
                    color = severityColor(item.severity),
                    fontWeight = FontWeight.Bold,
                    style = MaterialTheme.typography.labelMedium,
                )
                Spacer(Modifier.width(10.dp))
                Text(item.eventType ?: "event", fontWeight = FontWeight.SemiBold)
                if (item.acknowledged) {
                    Spacer(Modifier.width(8.dp))
                    Text("✓ read", style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
            item.subjectLabel?.takeIf { it.isNotBlank() }?.let { Text(it) }
            Text(
                buildString {
                    item.deviceId?.takeIf { it.isNotBlank() }?.let { append(it).append(" · ") }
                    append(relativeTime(item.createdUnixNanos))
                },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            if (!item.acknowledged) {
                Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                    OutlinedButton(onClick = onAck) { Text("Acknowledge") }
                }
            }
        }
    }
}

private fun severityColor(severity: String?): Color = when (severity) {
    "critical" -> Color(0xFFFF5252)
    "warning" -> Color(0xFFF0B400)
    else -> Color(0xFF8AA0B0)
}

private fun relativeTime(unixNanos: Long): String {
    if (unixNanos <= 0) return "—"
    val ms = unixNanos / 1_000_000
    return DateUtils.getRelativeTimeSpanString(
        ms, System.currentTimeMillis(), DateUtils.MINUTE_IN_MILLIS,
    ).toString()
}
