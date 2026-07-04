package com.hushai.android.capture

import android.util.Size
import android.view.Surface
import com.hushai.android.capture.gl.CameraGlRenderer
import java.io.File
import java.util.concurrent.atomic.AtomicLong

/**
 * The upright-BAKE video path (opt-in; see [com.hushai.android.config.Settings.uprightBake]).
 *
 * Wires the camera → a GL rotation pass ([CameraGlRenderer]) → the H.264 [VideoEncoder], so the
 * encoded pixels are physically upright and each segment's MP4 rotation matrix is 0. That lets the
 * viewer's fast `-c copy` remux play Android footage upright with no re-encode. Contrast the
 * default path, which stamps the rotation MATRIX and relies on a re-decode (worker/viewer) to
 * autorotate.
 *
 * The camera targets [cameraTargetSurface] (the renderer's SurfaceTexture), NOT the encoder's
 * input surface — the renderer draws the rotated frame into the encoder. The encoder dimensions
 * are baked from the mount orientation at construction (portrait swaps w/h).
 */
class GlVideoPipeline(
    segmentDir: File,
    sensorSize: Size,
    bitRate: Int,
    frameRate: Int,
    segmentDurationUs: Long,
    sequencer: AtomicLong,
    onSegment: (Segment) -> Unit,
    // Clockwise upright degrees sampled from OrientationTracker at capture start (fixes the
    // encoder dimensions). The renderer re-reads [rotationProvider] per frame for within-bucket
    // self-correction (0↔180).
    bakedRotation: Int,
    rotationProvider: () -> Int,
    motionScoreProvider: () -> Float? = { null },
) {
    val outputSize: Size = outputSizeFor(sensorSize, bakedRotation)

    // The encoder output is the (possibly swapped) upright size; it must NOT stamp the matrix
    // (pixels are already baked upright by the renderer).
    private val encoder = VideoEncoder(
        segmentDir, outputSize, bitRate, frameRate, segmentDurationUs, sequencer, onSegment,
        rotationProvider = { 0 },
        motionScoreProvider = motionScoreProvider,
        stampMatrix = false,
    )

    private val renderer = CameraGlRenderer(
        encoderSurface = encoder.inputSurface,
        outWidth = outputSize.width,
        outHeight = outputSize.height,
        sensorWidth = sensorSize.width,
        sensorHeight = sensorSize.height,
        bakedRotation = bakedRotation,
        rotationProvider = rotationProvider,
    )

    /** The camera's video output target. Valid after [start]. */
    val cameraTargetSurface: Surface
        get() = renderer.inputSurface ?: error("GlVideoPipeline.start() not called or GL init failed")

    /**
     * Start the encoder and the GL renderer. THROWS if GL/EGL init fails — the caller must catch
     * it, release() this, and fall back to the plain [VideoEncoder] + rotation-matrix path.
     */
    fun start() {
        encoder.start()
        // start() throws on EGL failure; encoder is already running and will be torn down by the
        // caller's release() on the exception path.
        renderer.start()
    }

    fun stop() {
        // Renderer first (stops feeding the encoder input surface), then the encoder.
        runCatching { renderer.release() }
        runCatching { encoder.stop() }
    }

    companion object {
        /** Encoder output size: swap w/h for a 90°/270° (portrait) mount, else the sensor size. */
        fun outputSizeFor(sensor: Size, rotationDegrees: Int): Size =
            if (Math.floorMod(rotationDegrees, 180) != 0) Size(sensor.height, sensor.width) else sensor
    }
}
