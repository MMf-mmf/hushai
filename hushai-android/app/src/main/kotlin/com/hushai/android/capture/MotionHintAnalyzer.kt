package com.hushai.android.capture

import android.graphics.ImageFormat
import android.media.Image
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import android.os.SystemClock
import android.util.Size
import android.view.Surface
import com.hushai.android.util.HushaiLog

/**
 * Device-side mirror of the worker's skip-static motion metric
 * (hushai-worker/src/vision/motion.rs): a low-res YUV analysis output on the existing
 * Camera2 session, sampled at ~2 fps, area-averaged into a 32×32 luma tile; consecutive
 * samples are compared with the same mean-subtracted MSE distance. [take] reports the MAX
 * distance observed since the previous call — the per-segment `hint.motion_score` manifest
 * attr (max = conservative: ANY motion inside the segment window blocks an ingest skip).
 *
 * RAW measurement only — the backend owns the threshold (`INGEST_MOTION_THRESHOLD`), which
 * is deliberately conservative because this pixel path (sensor YUV + area-average) is
 * correlated with, but not identical to, the worker's (H.264-decoded RGB + Lanczos).
 *
 * Cost: one extra 320×240 stream on the camera session + ~1 KB of math per 500 ms.
 * [CameraController] treats the surface as OPTIONAL: if the device rejects the three-output
 * stream combination, the session retries without it and segments simply carry no motion
 * hint (the backend fails open to full processing).
 */
class MotionHintAnalyzer(size: Size = Size(320, 240)) : AutoCloseable {
    private val thread = HandlerThread("hushai-motion").apply { start() }
    private val handler = Handler(thread.looper)
    private val reader: ImageReader =
        ImageReader.newInstance(size.width, size.height, ImageFormat.YUV_420_888, 2)

    private val lock = Any()
    private var prevTile: FloatArray? = null
    private var prevMean = 0f
    private var maxDistance: Float? = null
    private var lastSampleUptimeMs = 0L

    init {
        reader.setOnImageAvailableListener({ r ->
            // Always acquire+close, even when not sampling — a full reader stalls the camera.
            val img = r.acquireLatestImage() ?: return@setOnImageAvailableListener
            try {
                val now = SystemClock.uptimeMillis()
                if (now - lastSampleUptimeMs >= SAMPLE_INTERVAL_MS) {
                    lastSampleUptimeMs = now
                    sample(img)
                }
            } catch (e: Exception) {
                HushaiLog.warn("motion hint sample failed: ${e.message}")
            } finally {
                img.close()
            }
        }, handler)
    }

    val surface: Surface get() = reader.surface

    /**
     * Max fingerprint distance since the previous call, then reset the window. `null` until
     * two frames have been compared (camera warm-up, analysis stream rejected, first segment).
     */
    fun take(): Float? = synchronized(lock) {
        val v = maxDistance
        maxDistance = null
        v
    }

    private fun sample(img: Image) {
        val tile = lumaTile(img, FP_SIDE)
        var mean = 0f
        for (v in tile) mean += v
        mean /= tile.size
        synchronized(lock) {
            val pt = prevTile
            if (pt != null) {
                // Mean-subtracted MSE — identical formula to the worker's motion::distance,
                // so the score is robust to global exposure/IR-level drift.
                var sumSq = 0.0
                for (i in tile.indices) {
                    val d = (tile[i] - mean) - (pt[i] - prevMean)
                    sumSq += (d * d).toDouble()
                }
                val dist = (sumSq / tile.size).toFloat()
                val cur = maxDistance
                if (cur == null || dist > cur) maxDistance = dist
            }
            prevTile = tile
            prevMean = mean
        }
    }

    /** Area-average the Y plane into an N×N tile (row/pixel-stride aware, cell-subsampled). */
    private fun lumaTile(img: Image, side: Int): FloatArray {
        val plane = img.planes[0]
        val buf = plane.buffer
        val rowStride = plane.rowStride
        val pixStride = plane.pixelStride
        val w = img.width
        val h = img.height
        val out = FloatArray(side * side)
        for (ty in 0 until side) {
            val y0 = ty * h / side
            val y1 = ((ty + 1) * h / side).coerceAtLeast(y0 + 1)
            for (tx in 0 until side) {
                val x0 = tx * w / side
                val x1 = ((tx + 1) * w / side).coerceAtLeast(x0 + 1)
                var sum = 0L
                var n = 0
                var y = y0
                while (y < y1) {
                    val rowBase = y * rowStride
                    var x = x0
                    while (x < x1) {
                        sum += buf.get(rowBase + x * pixStride).toInt() and 0xFF
                        n++
                        x += 2 // subsample inside the cell — ample for a 32×32 mean
                    }
                    y += 2
                }
                out[ty * side + tx] = if (n > 0) sum.toFloat() / n else 0f
            }
        }
        return out
    }

    override fun close() {
        runCatching { reader.close() }
        thread.quitSafely()
    }

    companion object {
        /** Mirrors the worker's VISION_MOTION_FP_SIDE default; sent as `hint.motion_fp_side`. */
        const val FP_SIDE = 32
        private const val SAMPLE_INTERVAL_MS = 500L // ~2 fps
    }
}
