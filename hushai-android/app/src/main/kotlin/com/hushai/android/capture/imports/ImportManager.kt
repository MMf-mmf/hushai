package com.hushai.android.capture.imports

import android.content.Context
import com.hushai.android.capture.Segment
import com.hushai.android.util.CaptureStatus
import com.hushai.android.util.HushaiLog
import java.io.File
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicInteger

/**
 * Serializes manual file imports: one import at a time on a low-priority worker
 * thread, queued. Each runs an [ImportPipeline] that emits 2s segments via [onSegment]
 * into the shared durable buffer. Reports progress/queue/errors through [publish] and
 * signals [onActiveChange] so the service can bring the delivery context up/down and
 * promote/demote the foreground notification.
 */
class ImportManager(
    private val context: Context,
    private val segmentDir: File,
    private val segmentDurationUs: Long,
    private val onSegment: (Segment) -> Unit,
    private val backpressure: () -> Unit,
    private val publish: (transform: (CaptureStatus) -> CaptureStatus) -> Unit,
    private val onActiveChange: (active: Boolean) -> Unit,
) {
    private val exec = Executors.newSingleThreadExecutor { r ->
        Thread(r, "hushai-import").apply { priority = Thread.NORM_PRIORITY - 2 }
    }
    private val outstanding = AtomicInteger(0)

    @Volatile private var cancelCurrent = false
    @Volatile private var shuttingDown = false

    fun enqueue(requests: List<ImportRequest>) {
        if (requests.isEmpty() || shuttingDown) return
        for (req in requests) {
            if (outstanding.getAndIncrement() == 0) onActiveChange(true)
            publishQueued()
            exec.execute { runOne(req) }
        }
    }

    private fun runOne(req: ImportRequest) {
        if (shuttingDown) { afterOne(); return }
        cancelCurrent = false
        publish { it.copy(importing = true, importName = req.displayName, importDone = 0, importTotal = 0, importError = null) }
        val pipeline = ImportPipeline(
            context = context,
            request = req,
            segmentDir = segmentDir,
            segmentDurationUs = segmentDurationUs,
            onSegment = onSegment,
            backpressure = backpressure,
            isCancelled = { cancelCurrent || shuttingDown },
            onProgress = { done, total -> publish { it.copy(importDone = done, importTotal = total) } },
        )
        val result = runCatching { pipeline.run() }
            .getOrElse { ImportResult.Failed(it.message ?: "import error") }
        when (result) {
            is ImportResult.Failed -> {
                HushaiLog.warn("import ${req.displayName} failed: ${result.reason}")
                publish { it.copy(importError = "${req.displayName}: ${result.reason}") }
            }
            ImportResult.Cancelled -> HushaiLog.info("import ${req.displayName} cancelled")
            ImportResult.Completed -> HushaiLog.info("import ${req.displayName} complete")
        }
        afterOne()
    }

    private fun afterOne() {
        val left = outstanding.decrementAndGet()
        if (left <= 0) {
            publish { it.copy(importing = false, importName = null, importDone = 0, importTotal = 0, importQueued = 0) }
            onActiveChange(false)
        } else {
            publishQueued()
        }
    }

    private fun publishQueued() {
        publish { it.copy(importQueued = (outstanding.get() - 1).coerceAtLeast(0)) }
    }

    /** Cancel the in-flight import (already-emitted segments stay durable). */
    fun cancelCurrent() {
        cancelCurrent = true
    }

    fun shutdown() {
        shuttingDown = true
        cancelCurrent = true
        runCatching { exec.shutdownNow() }
    }
}
