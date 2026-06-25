package com.hushai.android.util

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.update

/** Live capture status shared from the service to the UI. */
data class CaptureStatus(
    val running: Boolean = false,
    val reachable: Boolean = false,
    val live: Boolean = false,
    val ready: Boolean = false,
    val videoSeq: Long = -1,
    val audioSeq: Long = -1,
    val accepted: Long = 0,
    val pending: Int = 0,
    val lastError: String? = null,
)

/** Process-wide status bus; the service publishes, the UI observes. */
object StatusBus {
    val state = MutableStateFlow(CaptureStatus())

    // Atomic CAS update — capture threads + the uploader thread publish concurrently.
    fun update(transform: (CaptureStatus) -> CaptureStatus) {
        state.update(transform)
    }

    fun reset() {
        state.value = CaptureStatus()
    }
}
