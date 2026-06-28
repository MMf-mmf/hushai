package com.hushai.android.capture

import com.hushai.android.config.DeviceIdentity
import com.hushai.android.util.HushaiLog
import hushai.v1.SegmentManifest
import okio.ByteString
import java.io.File
import java.io.FileOutputStream

/**
 * Crash-durable, disk-byte-bounded store-and-forward queue (replaces the old
 * in-memory RetryBuffer). Each buffered segment is two files in [segmentDir]:
 *
 *   <segmentIdHex>.mp4       — the opaque body the encoder finalized
 *   <segmentIdHex>.manifest  — the serialized hushai.v1.SegmentManifest (the exact
 *                              wire bytes we POST), written atomically (tmp+fsync+rename)
 *
 * The manifest IS the complete re-upload payload, so a segment survives process death
 * and replays verbatim after a crash/reboot. [recover] rebuilds the in-memory index by
 * scanning the directory on startup. The buffer is bounded by BYTES (a user cap +
 * a device-free safety floor), not a segment count: on overflow we drop the OLDEST
 * (deleting both files) and remember its stream so the next surviving segment on that
 * stream declares `gap_before` — an honest gap instead of a silent loss.
 *
 * Thread-safety: a single lock. The capture/import threads
 * [offer]; the uploader thread [peek]/[remove]/[quarantine]. [recover] runs once on the
 * lifecycle thread BEFORE any encoder or the upload thread starts, so it never races a
 * live [offer].
 */
class DurableSegmentBuffer(
    private val segmentDir: File,
    private val quarantineDir: File,
    private val maxBytes: Long,
    private val minFreeBytesFloor: Long,
    private val identity: DeviceIdentity,
    /** SHA re-verify on recovery is expensive and the body is immutable once
     *  finalized; the cheap byte-length check catches truncation. Off by default. */
    private val verifyShaOnRecover: Boolean = false,
) {
    private val lock = Any()
    // Insertion-ordered (≈ capture order for live; explicitly sorted on recover) and
    // keyed by segment_id for O(1) removal. ByteString has value equality/hashCode.
    private val queue = LinkedHashMap<ByteString, Entry>()
    private val gapPending = HashSet<String>()
    private var totalBytes = 0L

    /** A buffered segment, ready to upload its persisted [manifestBytes] verbatim. */
    class Entry(
        val segmentId: ByteString,
        val streamId: String,
        val sequence: Long,
        val captureStartUnixNanos: Long,
        val byteLen: Long,
        val contentSha256: ByteString,
        val body: File,
        val sidecar: File,
        val manifestBytes: ByteArray,
    )

    sealed interface OfferResult {
        /** Stored durably; [evicted] = how many oldest entries we had to drop to fit. */
        data class Stored(val evicted: Int) : OfferResult
        /** The lone incoming segment couldn't fit even with an empty buffer — dropped. */
        data object DroppedSelf : OfferResult
    }

    init {
        segmentDir.mkdirs()
    }

    /**
     * Startup reconciliation. Reaps incomplete writes, pairs bodies with sidecars,
     * decodes + validates each, sorts by (capture time, sequence), and rebuilds the
     * in-memory index. Returns the number of segments recovered for replay.
     */
    fun recover(): Int = synchronized(lock) {
        queue.clear()
        gapPending.clear()
        totalBytes = 0L

        val files = segmentDir.listFiles()?.filter { it.isFile } ?: emptyList()
        // 1. Reap incomplete sidecar writes.
        var reaped = 0
        for (f in files) if (f.name.endsWith(MANIFEST_TMP_SUFFIX)) { if (f.delete()) reaped++ }

        val bodies = HashMap<String, File>()      // stem -> body
        val sidecars = HashMap<String, File>()     // stem -> sidecar
        for (f in files) {
            when {
                f.name.endsWith(BODY_SUFFIX) -> bodies[f.name.removeSuffix(BODY_SUFFIX)] = f
                f.name.endsWith(MANIFEST_SUFFIX) -> sidecars[f.name.removeSuffix(MANIFEST_SUFFIX)] = f
            }
        }

        var orphanBodies = 0
        var danglingSidecars = 0
        var corrupt = 0
        val recovered = ArrayList<Entry>()

        // 2. Orphan bodies (no sidecar): un-committed or already-accepted — delete.
        for ((stem, body) in bodies) {
            if (stem !in sidecars) { if (body.delete()) orphanBodies++ }
        }
        // 3. Dangling sidecars (no body): delete.
        for ((stem, sidecar) in sidecars) {
            if (stem !in bodies) { if (sidecar.delete()) danglingSidecars++ }
        }
        // 4. Matched pairs: decode + validate.
        for ((stem, sidecar) in sidecars) {
            val body = bodies[stem] ?: continue
            val manifestBytes = runCatching { sidecar.readBytes() }.getOrNull()
            val manifest = manifestBytes?.let { runCatching { SegmentManifest.ADAPTER.decode(it) }.getOrNull() }
            if (manifest == null) {
                corrupt++; sidecar.delete(); body.delete(); continue
            }
            if (body.length() != manifest.byte_len) {
                // Truncated/corrupt body (e.g. crash mid-MP4 finalize). Drop + flag a gap.
                corrupt++; sidecar.delete(); body.delete(); gapPending.add(manifest.stream_id); continue
            }
            if (verifyShaOnRecover &&
                com.hushai.android.util.Sha.sha256(body) != manifest.content_sha256
            ) {
                corrupt++; sidecar.delete(); body.delete(); gapPending.add(manifest.stream_id); continue
            }
            recovered.add(
                Entry(
                    segmentId = manifest.segment_id,
                    streamId = manifest.stream_id,
                    sequence = manifest.sequence,
                    captureStartUnixNanos = manifest.capture_start_unix_nanos,
                    byteLen = manifest.byte_len,
                    contentSha256 = manifest.content_sha256,
                    body = body,
                    sidecar = sidecar,
                    manifestBytes = manifestBytes,
                ),
            )
        }

        // 5. Oldest-first by capture time, then sequence.
        recovered.sortWith(compareBy({ it.captureStartUnixNanos }, { it.sequence }))
        for (e in recovered) {
            queue[e.segmentId] = e
            totalBytes += e.byteLen
        }
        if (recovered.isNotEmpty() || orphanBodies + danglingSidecars + corrupt + reaped > 0) {
            HushaiLog.info(
                "buffer recover: ${recovered.size} segment(s), ${totalBytes} bytes; " +
                    "reaped=$reaped orphanBody=$orphanBodies dangling=$danglingSidecars corrupt=$corrupt",
            )
        }
        recovered.size
    }

    /**
     * Live capture / import path. Renames the finalized body to its durable name,
     * persists the manifest sidecar atomically, enforces the disk bound (drop-oldest),
     * and enqueues. A segment is "buffered" only after its sidecar is durably on disk.
     */
    fun offer(segment: Segment): OfferResult = synchronized(lock) {
        val incoming = segment.file.length().coerceAtLeast(0)

        // Enforce the bound by evicting oldest entries until the incoming fits.
        var evicted = 0
        while (queue.isNotEmpty() && !fits(incoming)) {
            val oldest = queue.entries.iterator().next().value
            deleteEntryFiles(oldest)
            queue.remove(oldest.segmentId)
            totalBytes -= oldest.byteLen
            gapPending.add(oldest.streamId)
            evicted++
        }
        // The lone incoming segment still won't fit on an empty buffer — drop it.
        if (queue.isEmpty() && !fits(incoming)) {
            segment.file.delete()
            gapPending.add(segment.streamId)
            HushaiLog.warn("buffer full: dropping incoming ${segment.streamId} seq=${segment.sequence} (${incoming} B won't fit)")
            return OfferResult.DroppedSelf
        }

        val hex = segment.segmentId.hex()
        val body = File(segmentDir, "$hex$BODY_SUFFIX")
        // Move the encoder's scratch body to its durable, globally-unique name.
        if (segment.file.absolutePath != body.absolutePath && !segment.file.renameTo(body)) {
            segment.file.delete()
            gapPending.add(segment.streamId)
            HushaiLog.warn("buffer: body rename failed for ${segment.streamId} seq=${segment.sequence}")
            return OfferResult.DroppedSelf
        }

        // Build the manifest once (verbatim-replayed forever after) and persist it
        // atomically. Resolve the session: imported segments carry their own.
        val effectiveIdentity =
            if (segment.sessionId != null) identity.copy(sessionId = segment.sessionId) else identity
        val manifestBytes = SegmentManifestBuilder.build(segment.copy(file = body), effectiveIdentity)
        val sidecar = File(segmentDir, "$hex$MANIFEST_SUFFIX")
        if (!writeSidecarAtomically(hex, manifestBytes, sidecar)) {
            // ENOSPC or I/O failure: never enqueue a body without a committed sidecar.
            body.delete()
            gapPending.add(segment.streamId)
            HushaiLog.warn("buffer: sidecar write failed for ${segment.streamId} seq=${segment.sequence}")
            return OfferResult.DroppedSelf
        }

        queue[segment.segmentId] = Entry(
            segmentId = segment.segmentId,
            streamId = segment.streamId,
            sequence = segment.sequence,
            captureStartUnixNanos = segment.captureStartUnixNanos,
            byteLen = body.length(),
            contentSha256 = segment.contentSha256,
            body = body,
            sidecar = sidecar,
            manifestBytes = manifestBytes,
        )
        totalBytes += body.length()
        OfferResult.Stored(evicted)
    }

    /** Head of the queue (oldest by capture time) to attempt next, or null. */
    fun peek(): Entry? = synchronized(lock) { queue.entries.firstOrNull()?.value }

    /** Accepted (200): remove from queue and delete the sidecar THEN the body. */
    fun remove(entry: Entry) = synchronized(lock) {
        if (queue.remove(entry.segmentId) != null) {
            totalBytes -= entry.byteLen
            entry.sidecar.delete()
            entry.body.delete()
        }
    }

    /** Permanent client error (400/409/413): move both files aside and drop it. */
    fun quarantine(entry: Entry) = synchronized(lock) {
        if (queue.remove(entry.segmentId) != null) totalBytes -= entry.byteLen
        runCatching {
            quarantineDir.mkdirs()
            val dest = File(quarantineDir, "${entry.streamId}-${entry.sequence}-${entry.body.name}")
            if (!entry.body.renameTo(dest)) entry.body.delete()
            entry.sidecar.delete()
        }
    }

    /** Whether a freshly captured segment on [streamId] must declare `gap_before`. */
    fun consumeGap(streamId: String): Boolean = synchronized(lock) { gapPending.remove(streamId) }

    fun size(): Int = synchronized(lock) { queue.size }
    fun byteSize(): Long = synchronized(lock) { totalBytes }
    fun oldestUnixNanos(): Long = synchronized(lock) {
        queue.entries.firstOrNull()?.value?.captureStartUnixNanos ?: 0L
    }

    /** Free bytes on the volume the buffer lives on. */
    fun freeBytes(): Long = segmentDir.usableSpace

    // --- internals (call only under lock) ---

    private fun fits(incoming: Long): Boolean {
        if (totalBytes + incoming > maxBytes) return false
        // Keep minFreeBytesFloor free on the volume AFTER writing this segment.
        if (segmentDir.usableSpace - incoming < minFreeBytesFloor) return false
        return true
    }

    private fun deleteEntryFiles(e: Entry) {
        runCatching { e.sidecar.delete() }
        runCatching { e.body.delete() }
    }

    private fun writeSidecarAtomically(hex: String, bytes: ByteArray, dest: File): Boolean = runCatching {
        val tmp = File(segmentDir, "$hex$MANIFEST_TMP_SUFFIX")
        FileOutputStream(tmp).use { fos ->
            fos.write(bytes)
            fos.flush()
            fos.fd.sync()
        }
        if (!tmp.renameTo(dest)) {
            tmp.delete()
            return false
        }
        true
    }.getOrElse { false }

    companion object {
        private const val BODY_SUFFIX = ".mp4"
        private const val MANIFEST_SUFFIX = ".manifest"
        private const val MANIFEST_TMP_SUFFIX = ".manifest.tmp"
    }
}
