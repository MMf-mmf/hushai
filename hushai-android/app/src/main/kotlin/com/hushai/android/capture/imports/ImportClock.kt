package com.hushai.android.capture.imports

import android.os.SystemClock
import com.hushai.android.capture.SegmentMuxer

/**
 * Maps an imported file's per-segment PTS onto the file's ORIGINAL capture time, so
 * the backend (which orders globally by capture_start_unix_nanos) places imported
 * footage at the moment it was actually recorded — not at upload time.
 *
 * [baseUnixNanos] is the file's best-known creation time (see [MediaProbe]); each
 * segment's wall clock = base + its first-sample PTS. The monotonic clock has no real
 * cross-process meaning for historical media, so we return a single synthetic value.
 */
class ImportClock(private val baseUnixNanos: Long) : SegmentMuxer.SegmentClock {
    private val monoBase = SystemClock.elapsedRealtimeNanos()
    override fun captureWallNanos(firstPtsUs: Long): Long = baseUnixNanos + firstPtsUs * 1000L
    override fun monotonicNanos(): Long = monoBase
}
