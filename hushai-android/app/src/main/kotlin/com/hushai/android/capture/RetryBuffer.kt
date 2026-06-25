package com.hushai.android.capture

import java.io.File

/**
 * Bounded store-and-forward queue (contract §5.7/§5.8) for v1: an in-memory FIFO
 * of finalized [Segment]s (their bodies already spilled to files on disk). A
 * segment leaves the queue only after a `200` ([remove]) or when it is
 * [quarantine]d as a permanent client error. On overflow during a long outage we
 * drop the OLDEST (delete its file) and remember its stream, so the next freshly
 * captured segment on that stream is marked `gap_before=true` — an honest gap
 * instead of a silent loss.
 *
 * NOT crash-durable across process death (named fast-follow, out of scope).
 * Thread-safe: the capture thread offers; the uploader thread drains.
 */
class RetryBuffer(
    private val maxEntries: Int,
    private val quarantineDir: File,
) {
    private val lock = Any()
    private val queue = ArrayDeque<Segment>()
    private val gapPending = HashSet<String>() // stream_ids that lost data to overflow

    fun offer(segment: Segment) = synchronized(lock) {
        while (queue.size >= maxEntries) {
            val dropped = queue.removeFirst()
            dropped.file.delete()
            gapPending.add(dropped.streamId)
        }
        queue.addLast(segment)
    }

    /** Head of the queue to attempt next, or null if empty. Does not remove. */
    fun peek(): Segment? = synchronized(lock) { queue.firstOrNull() }

    /** Accepted (200): remove from queue and delete the local body. */
    fun remove(segment: Segment) = synchronized(lock) {
        if (queue.remove(segment)) segment.file.delete()
    }

    /** Permanent client error (400/409/413): move the body aside and drop it. */
    fun quarantine(segment: Segment) = synchronized(lock) {
        queue.remove(segment)
        runCatching {
            quarantineDir.mkdirs()
            val dest = File(quarantineDir, "${segment.streamId}-${segment.sequence}-${segment.file.name}")
            if (!segment.file.renameTo(dest)) segment.file.delete()
        }
    }

    /**
     * Whether a freshly captured segment on [streamId] must declare `gap_before`
     * because the buffer previously had to drop data on that stream. Consumes the
     * flag so exactly one surviving segment carries the gap.
     */
    fun consumeGap(streamId: String): Boolean = synchronized(lock) { gapPending.remove(streamId) }

    fun size(): Int = synchronized(lock) { queue.size }
}
