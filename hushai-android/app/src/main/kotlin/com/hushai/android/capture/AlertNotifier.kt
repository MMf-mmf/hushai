package com.hushai.android.capture

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import com.hushai.android.MainActivity
import com.hushai.android.net.EventsClient
import com.hushai.android.net.Http
import com.hushai.android.util.HushaiLog

/**
 * Background alert "push" (roadmap A7): polls the backend's in-app alert feed (`/v1/events/feed`) and
 * raises a system notification for each NEW alert — a watchlist hit, an unknown-person-at-night rule,
 * etc. Local-first, NO FCM/Google services: a coarse poll over the same backend connection the
 * uploader uses, so it works on the LAN / over the USB `adb reverse` tunnel with zero egress.
 *
 * Hosted by [CaptureService] (the always-on FGS), started/stopped with capture.
 *
 * DEDUPE BY A PERSISTED HIGH-WATER MARK (not an in-memory "seen" set). Feed deliveries stay
 * `status=pending` until the owner acks them (nothing auto-advances them), so the pending set is the
 * whole un-acked backlog. We therefore key dedupe on `created_unix_nanos` (the SERVER's clock, so no
 * client/server skew) and persist the last-notified watermark in SharedPreferences. Consequences:
 *   - First run EVER on this install: prime the watermark to the current max so we don't blast the
 *     entire historical backlog. Notify nothing on the prime poll.
 *   - On any restart (app relaunch / OS START_STICKY / Stop→Start): the persisted watermark survives,
 *     so an alert that fired WHILE CAPTURE WAS STOPPED (created after the watermark) is notified on
 *     the next poll — no lost push, and no unbounded memory.
 */
class AlertNotifier(private val context: Context) {
    @Volatile private var thread: Thread? = null

    fun start(url: String, token: String) {
        if (url.isBlank()) return
        stop()
        ensureChannel()
        val t = Thread({ loop(url, token) }, "hushai-alerts").apply { isDaemon = true }
        thread = t
        t.start()
    }

    fun stop() {
        thread?.interrupt()
        thread = null
    }

    private fun loop(url: String, token: String) {
        // Short-timeout client (probe): a background poll must not block teardown for long after
        // stop() interrupts the thread (OkHttp's blocking call ignores interrupt, but completes ≤4s).
        val client = EventsClient(Http.probe, url, token)
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        // -1 sentinel = "never primed on this install" (distinct from a legitimate 0 watermark).
        var watermark = prefs.getLong(KEY_WATERMARK, -1L)

        while (!Thread.currentThread().isInterrupted) {
            val feed = client.listFeed(status = "pending", limit = 100)
            if (feed != null) {
                if (watermark < 0L) {
                    // First-ever prime: anchor to the current max created time; don't blast history.
                    watermark = feed.maxOfOrNull { it.createdUnixNanos } ?: 0L
                    prefs.edit().putLong(KEY_WATERMARK, watermark).apply()
                } else {
                    // Notify everything created after the watermark (incl. alerts that fired while we
                    // were down), oldest-first, advancing the persisted watermark monotonically.
                    val fresh = feed.filter { it.createdUnixNanos > watermark }
                        .sortedBy { it.createdUnixNanos }
                    for (item in fresh) {
                        notify(item)
                        if (item.createdUnixNanos > watermark) watermark = item.createdUnixNanos
                    }
                    if (fresh.isNotEmpty()) prefs.edit().putLong(KEY_WATERMARK, watermark).apply()
                }
            }
            try {
                Thread.sleep(POLL_MS)
            } catch (e: InterruptedException) {
                Thread.currentThread().interrupt()
            }
        }
    }

    private fun notify(item: EventsClient.FeedItem) {
        val title = when (item.severity) {
            "critical" -> "⚠ Critical alert"
            "warning" -> "Alert"
            else -> "Hushai alert"
        }
        val subject = item.subjectLabel?.takeIf { it.isNotBlank() }
        val where = item.deviceId?.takeIf { it.isNotBlank() }?.let { " · $it" } ?: ""
        val text = buildString {
            append(item.eventType ?: "event")
            if (subject != null) append(": ").append(subject)
            append(where)
        }
        val open = PendingIntent.getActivity(
            context,
            0,
            Intent(context, MainActivity::class.java).apply {
                flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP
            },
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val n = NotificationCompat.Builder(context, CHANNEL_ID)
            .setContentTitle(title)
            .setContentText(text)
            .setSmallIcon(android.R.drawable.stat_sys_warning)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setCategory(NotificationCompat.CATEGORY_EVENT)
            .setAutoCancel(true)
            .setContentIntent(open)
            .build()
        try {
            // TAG = the delivery_id (collision-free), with a constant id — so distinct alerts stack
            // and never overwrite each other (a 32-bit hashCode id could collide silently).
            NotificationManagerCompat.from(context).notify(item.deliveryId, ALERT_NOTIF_ID, n)
        } catch (e: SecurityException) {
            // POST_NOTIFICATIONS not granted (Android 13+). Log + continue; the in-app Alerts screen
            // still surfaces it (and flags that notifications are blocked).
            HushaiLog.warn("alert notify denied: ${e.message}")
        }
    }

    private fun ensureChannel() {
        val manager = context.getSystemService(NotificationManager::class.java)
        if (manager.getNotificationChannel(CHANNEL_ID) == null) {
            manager.createNotificationChannel(
                NotificationChannel(
                    CHANNEL_ID,
                    "Hushai alerts",
                    NotificationManager.IMPORTANCE_HIGH,
                ).apply { description = "People/plates of interest and other matched alert rules" },
            )
        }
    }

    companion object {
        const val CHANNEL_ID = "hushai_alerts"
        private const val ALERT_NOTIF_ID = 2 // distinct from CaptureNotification's id 1; tag carries identity
        private const val PREFS = "hushai_alerts"
        private const val KEY_WATERMARK = "last_notified_created_ns"
        private const val POLL_MS = 20_000L
    }
}
