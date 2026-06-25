package com.hushai.android.capture

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat

/** Persistent "Hushai is capturing" notification with a Stop action. */
object CaptureNotification {
    const val CHANNEL_ID = "hushai_capture"
    const val NOTIFICATION_ID = 1

    fun ensureChannel(context: Context) {
        val manager = context.getSystemService(NotificationManager::class.java)
        if (manager.getNotificationChannel(CHANNEL_ID) == null) {
            manager.createNotificationChannel(
                NotificationChannel(
                    CHANNEL_ID,
                    "Hushai capture",
                    NotificationManager.IMPORTANCE_LOW,
                ).apply { description = "Always-on capture is running" },
            )
        }
    }

    fun build(context: Context, text: String): Notification {
        val stopIntent = Intent(context, CaptureService::class.java).apply {
            action = CaptureService.ACTION_STOP
        }
        val stopPending = PendingIntent.getService(
            context, 0, stopIntent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return NotificationCompat.Builder(context, CHANNEL_ID)
            .setContentTitle("Hushai is capturing")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.presence_video_online)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .addAction(android.R.drawable.ic_menu_close_clear_cancel, "Stop", stopPending)
            .build()
    }

    fun update(service: Service, text: String) {
        val manager = service.getSystemService(NotificationManager::class.java)
        manager.notify(NOTIFICATION_ID, build(service, text))
    }
}
