package com.hushai.android.config

import okio.ByteString

/**
 * Identity carried on every segment (contract §4): a stable per-install
 * [deviceId] and a per-capture-run [sessionId] (16 raw bytes, fresh each time
 * the service starts). `sequence` restarts at 0 per session, tracked elsewhere.
 */
data class DeviceIdentity(
    val deviceId: String,
    val sessionId: ByteString,
)
