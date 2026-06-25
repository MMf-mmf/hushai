package com.hushai.android.util

import android.util.Log

/**
 * Structured logcat under a single tag so the automation script can observe
 * footage flowing: `adb logcat | grep HUSHAI_TX`. One line per upload attempt.
 */
object HushaiLog {
    const val TAG = "HUSHAI_TX"

    fun tx(streamId: String, seq: Long, bytes: Long, sha256Hex: String, status: String) {
        Log.i(TAG, "stream=$streamId seq=$seq bytes=$bytes sha256=${sha256Hex.take(16)} status=$status")
    }

    fun info(msg: String) = Log.i(TAG, msg)
    fun warn(msg: String) = Log.w(TAG, msg)
    fun error(msg: String, t: Throwable? = null) = Log.e(TAG, msg, t)
}
