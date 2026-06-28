package com.hushai.android.util

import java.util.Locale

/**
 * Human-readable formatting shared by the capture UI and the foreground
 * notification (so "128 MB buffered" reads the same in both places).
 */

/** Bytes -> "0 B" / "512 KB" / "1.4 GB" (binary 1024 steps). */
fun formatBytes(bytes: Long): String {
    if (bytes <= 0) return "0 B"
    val units = arrayOf("B", "KB", "MB", "GB", "TB")
    var v = bytes.toDouble()
    var i = 0
    while (v >= 1024 && i < units.lastIndex) {
        v /= 1024
        i++
    }
    return if (i == 0) "$bytes B" else String.format(Locale.US, "%.1f %s", v, units[i])
}

/** Microseconds -> "12s" / "4m 12s" / "1h 04m". */
fun formatDurationMicros(micros: Long): String {
    val totalSec = (micros / 1_000_000).coerceAtLeast(0)
    val h = totalSec / 3600
    val m = (totalSec % 3600) / 60
    val s = totalSec % 60
    return when {
        h > 0 -> String.format(Locale.US, "%dh %02dm", h, m)
        m > 0 -> "${m}m ${s}s"
        else -> "${s}s"
    }
}
