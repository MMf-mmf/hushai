package com.hushai.android.util

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.update

/** Live capture status shared from the service to the UI. */
data class CaptureStatus(
    val running: Boolean = false,
    val audioOnly: Boolean = false,
    val reachable: Boolean = false,
    val live: Boolean = false,
    val ready: Boolean = false,
    val videoSeq: Long = -1,
    val audioSeq: Long = -1,
    val accepted: Long = 0,
    val pending: Int = 0,
    val lastError: String? = null,

    // --- Offline store-and-forward (delivery state, from ConnectivityState) ---
    /** We currently cannot deliver to the backend; footage is being kept locally. */
    val offline: Boolean = false,
    /** Reconnected and replaying a backlog that isn't empty yet. */
    val draining: Boolean = false,
    /** Buffered segment count latched at reconnect — the "X of Y" denominator. */
    val backlogTotal: Int = 0,

    // --- Buffer + disk accounting (from the durable buffer / CaptureService) ---
    /** Sum of on-disk buffered segment body sizes. */
    val bufferedBytes: Long = 0,
    /** Free space on the volume the buffer lives on (noBackupFilesDir). */
    val diskFreeBytes: Long = 0,
    /** Effective local buffer cap (from Settings); 0 = unset. */
    val diskCapBytes: Long = 0,
    /** captureStartUnixNanos of the oldest buffered segment; 0 = none. */
    val oldestBufferedUnixNanos: Long = 0,
    /** Cumulative count of segments dropped to overflow while full. */
    val droppedToOverflow: Long = 0,
    /** True if the most recent offer had to evict to stay under the cap. */
    val overflowing: Boolean = false,

    // --- Manual file import ---
    val importing: Boolean = false,
    val importName: String? = null,
    val importDone: Int = 0,
    val importTotal: Int = 0,
    val importQueued: Int = 0,
    val importError: String? = null,
)

/** Process-wide status bus; the service publishes, the UI observes. */
object StatusBus {
    val state = MutableStateFlow(CaptureStatus())

    // Atomic CAS update — capture threads + the uploader thread publish concurrently.
    fun update(transform: (CaptureStatus) -> CaptureStatus) {
        state.update(transform)
    }

    /** Reset capture/delivery fields when capture stops, but PRESERVE import fields:
     *  an import can outlive a capture stop (it owns its own codec instances and only
     *  shares the buffer + uploader). */
    fun reset() {
        state.update { prev ->
            CaptureStatus(
                importing = prev.importing,
                importName = prev.importName,
                importDone = prev.importDone,
                importTotal = prev.importTotal,
                importQueued = prev.importQueued,
                importError = prev.importError,
            )
        }
    }
}
