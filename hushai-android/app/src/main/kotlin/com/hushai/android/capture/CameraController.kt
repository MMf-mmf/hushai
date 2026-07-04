package com.hushai.android.capture

import android.annotation.SuppressLint
import android.content.Context
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.params.StreamConfigurationMap
import android.media.MediaCodec
import android.os.Handler
import android.os.HandlerThread
import android.util.Size
import android.view.Surface
import com.hushai.android.util.HushaiLog

/**
 * Opens a camera and streams frames into the video encoder's input [Surface] via
 * a repeating TEMPLATE_RECORD request. No per-frame code — the encoder pulls
 * frames off the surface. Capture choices (which camera, what resolution) are
 * the client's to make per contract §1/§4.
 *
 * Optionally an on-screen [previewSurface] (a SurfaceView from the foreground UI)
 * can be attached as a *second* output so the user sees exactly the frames being
 * encoded. The [encoderSurface] is the source of truth and is always a target;
 * the preview is best-effort and may come and go as the Activity binds/unbinds.
 * Adding or removing the preview recreates the capture session (Camera2 sessions
 * have a fixed surface set); the brief frame gap that causes is contractually
 * fine (the encoder marks gap_before on the next segment).
 */
class CameraController(
    private val context: Context,
    private val cameraId: String,
    private val encoderSurface: Surface,
    // Optional low-res analysis output (MotionHintAnalyzer). BEST-EFFORT: if the device
    // rejects the wider stream combination (LIMITED hardware level), the session retries
    // WITHOUT it — capture must never fail because of a hint stream.
    private val analysisSurface: Surface? = null,
) {
    private val manager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
    // @Volatile: written on [handler] (open/(re)configure) but ALSO read+closed by stop() on the
    // caller's (lifecycle) thread; without it stop() can read a stale device==null and skip
    // device.close(), leaking the held CameraDevice.
    @Volatile private var device: CameraDevice? = null
    @Volatile private var session: CameraCaptureSession? = null
    @Volatile private var previewSurface: Surface? = null
    // Bumped on every (re)configure. A session's async onConfigured only "wins" if
    // its generation is still current; a stale callback (e.g. the encoder-only
    // session whose config raced with an encoder+preview reconfigure) closes itself
    // and bows out. Without this the active session could end up without the preview
    // target (black preview) or hit "session has been closed" on setRepeatingRequest.
    // Touched only on [handler] (unlike device/session, which stop() also touches).
    private var configGeneration = 0
    // Set true by [stop]; gates the handler-side (re)configure paths so a teardown
    // that races a queued setPreviewSurface/onOpened post can't drive a (re)configure
    // onto a camera we've already closed. @Volatile because stop() flips it from the
    // caller's thread while the posts read it on [handler].
    @Volatile private var closed = false
    // Dropped to false (permanently, for this controller) the first time a session
    // configure fails while the analysis surface was a target — the retry-without-it
    // fallback. Touched only on [handler].
    private var analysisEnabled = analysisSurface != null
    private val thread = HandlerThread("hushai-camera").apply { start() }
    private val handler = Handler(thread.looper)

    @SuppressLint("MissingPermission") // CAMERA granted before the service starts
    fun start() {
        manager.openCamera(cameraId, object : CameraDevice.StateCallback() {
            override fun onOpened(camera: CameraDevice) {
                // A stop() that landed while the open was in flight already tore us
                // down — don't adopt (or leak) this now-orphaned camera.
                if (closed) { runCatching { camera.close() }; return }
                device = camera
                createSession(camera)
            }

            override fun onDisconnected(camera: CameraDevice) {
                HushaiLog.warn("camera disconnected")
                camera.close()
                device = null
            }

            override fun onError(camera: CameraDevice, error: Int) {
                HushaiLog.error("camera error $error")
                camera.close()
                device = null
            }
        }, handler)
    }

    /**
     * Attach (non-null) or detach (null) an on-screen preview surface. Recreates
     * the capture session on the camera thread so the new target set takes effect.
     * Safe to call before the camera has opened — the surface is remembered and
     * applied once [onOpened] runs.
     */
    fun setPreviewSurface(surface: Surface?) {
        handler.post {
            if (closed) return@post
            if (previewSurface === surface) return@post
            previewSurface = surface
            device?.let { createSession(it) }
        }
    }

    @Suppress("DEPRECATION") // SessionConfiguration is API 28+; this overload supports minSdk 26.
    private fun createSession(camera: CameraDevice) {
        // Close any prior session before reconfiguring; the encoder surface is
        // untouched (owned by the encoder, not the session) so the stream resumes.
        runCatching { session?.close() }
        session = null
        // Torn down (stop() ran) — never touch the now-closed camera.
        if (closed) return

        // Capture the exact target set this session is built with, and reuse it
        // verbatim when building the request — never re-read previewSurface, which
        // could change underneath a config callback that is already in flight.
        val targets = buildList {
            add(encoderSurface)
            previewSurface?.let { add(it) }
            if (analysisEnabled) analysisSurface?.let { add(it) }
        }
        val generation = ++configGeneration

        // A teardown can still slip in between the `closed` check above and this
        // call (stop() runs on the caller thread), closing the device mid-flight and
        // making createCaptureSession throw "CameraDevice was already closed". Swallow
        // it — a closed camera needs no session — so the camera thread never crashes
        // the whole app (was: uncaught IllegalStateException → process death on Stop).
        runCatching {
            camera.createCaptureSession(
                targets,
                object : CameraCaptureSession.StateCallback() {
                    override fun onConfigured(configured: CameraCaptureSession) {
                        if (closed || generation != configGeneration) {
                            // Torn down, or a newer reconfigure superseded this one — discard it.
                            runCatching { configured.close() }
                            return
                        }
                        session = configured
                        // createCaptureRequest/setRepeatingRequest can also throw if the
                        // camera closed after onConfigured — keep them inside runCatching.
                        runCatching {
                            val request = camera.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                                targets.forEach { addTarget(it) }
                                set(CaptureRequest.CONTROL_MODE, CaptureRequest.CONTROL_MODE_AUTO)
                            }
                            configured.setRepeatingRequest(request.build(), null, handler)
                        }.onFailure { HushaiLog.error("setRepeatingRequest failed", it) }
                    }

                    override fun onConfigureFailed(failed: CameraCaptureSession) {
                        if (generation != configGeneration) return
                        if (analysisEnabled && analysisSurface != null) {
                            // The extra analysis output likely exceeded the device's supported
                            // stream combination — retry once without it. Capture > hints.
                            HushaiLog.warn(
                                "camera session configure failed with analysis stream " +
                                    "(targets=${targets.size}); retrying without motion hints",
                            )
                            analysisEnabled = false
                            device?.let { createSession(it) }
                            return
                        }
                        HushaiLog.error("camera session configure failed (targets=${targets.size})")
                    }
                },
                handler,
            )
        }.onFailure { HushaiLog.warn("createCaptureSession skipped (camera closing): ${it.message}") }
    }

    fun stop() {
        // Flip the gate BEFORE closing so any handler-queued (re)configure that
        // hasn't started yet sees `closed` and bails instead of configuring a
        // camera we're about to close.
        closed = true
        runCatching { session?.close() }
        runCatching { device?.close() }
        session = null
        device = null
        previewSurface = null
        thread.quitSafely()
    }

    data class Selection(
        val cameraId: String,
        val size: Size,
        // Clockwise mount angle of the sensor vs the device's natural orientation, and whether the
        // camera is front-facing — the two inputs (with the device's physical orientation) that
        // OrientationTracker needs to make the recording upright.
        val sensorOrientation: Int,
        val facingFront: Boolean,
    )

    companion object {
        private val TARGET = Size(1280, 720)

        /**
         * Pick a back-facing camera (fall back to any) and an output size for the
         * encoder surface close to 720p — robust across devices since we never
         * hard-code a size the camera may not support.
         */
        fun select(context: Context): Selection? {
            val manager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
            val ids = runCatching { manager.cameraIdList }.getOrDefault(emptyArray())
            if (ids.isEmpty()) return null

            val back = ids.firstOrNull {
                manager.getCameraCharacteristics(it)
                    .get(CameraCharacteristics.LENS_FACING) == CameraCharacteristics.LENS_FACING_BACK
            } ?: ids.first()

            val chars = manager.getCameraCharacteristics(back)
            val map = chars.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
            val size = chooseSize(map) ?: TARGET
            val sensorOrientation = chars.get(CameraCharacteristics.SENSOR_ORIENTATION) ?: 0
            val facingFront =
                chars.get(CameraCharacteristics.LENS_FACING) == CameraCharacteristics.LENS_FACING_FRONT
            return Selection(back, size, sensorOrientation, facingFront)
        }

        private fun chooseSize(map: StreamConfigurationMap?): Size? {
            val sizes = map?.getOutputSizes(MediaCodec::class.java) ?: return null
            if (sizes.isEmpty()) return null
            // Largest size not exceeding the target; else the smallest available.
            return sizes
                .filter { it.width <= TARGET.width && it.height <= TARGET.height }
                .maxByOrNull { it.width.toLong() * it.height }
                ?: sizes.minByOrNull { it.width.toLong() * it.height }
        }
    }
}
