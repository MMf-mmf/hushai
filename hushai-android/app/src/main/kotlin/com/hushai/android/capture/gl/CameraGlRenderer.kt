package com.hushai.android.capture.gl

import android.graphics.SurfaceTexture
import android.opengl.EGLSurface
import android.opengl.GLES20
import android.opengl.Matrix
import android.os.Handler
import android.os.HandlerThread
import android.view.Surface
import com.hushai.android.util.HushaiLog
import java.util.concurrent.CountDownLatch

/**
 * Renders camera frames UPRIGHT into a MediaCodec encoder input [Surface].
 *
 * Pipeline: the camera draws into our [SurfaceTexture] (an external-OES texture); on each
 * `onFrameAvailable` we sample that texture and draw a full-screen quad — with the frame
 * rotated by [bakedRotation] degrees — into an EGL window surface backed by [encoderSurface].
 * The encoded H.264 pixels are therefore physically upright, so downstream stream-copy
 * consumers (the viewer's `-c copy` remux → hls.js) play upright with NO rotation matrix.
 *
 * The output dimensions ([outWidth] x [outHeight]) are the encoder's; for a 90°/270° mount
 * they are the sensor's swapped (portrait). Rotation is applied to the TEXCOORDS so the quad
 * fills the viewport regardless. A 16:9 source rotated 90° is exactly 9:16 = the swapped
 * output, so there is no letterbox/distortion.
 *
 * Live self-correction: [rotationProvider] is read each frame; a change WITHIN the same
 * dimension bucket (0↔180) is applied immediately. A cross-bucket change (portrait↔landscape)
 * can't change the running encoder's dimensions, so it is ignored — the mount orientation is
 * fixed at capture start; a physical re-mount needs a capture restart.
 */
class CameraGlRenderer(
    private val encoderSurface: Surface,
    private val outWidth: Int,
    private val outHeight: Int,
    private val sensorWidth: Int,
    private val sensorHeight: Int,
    bakedRotation: Int,
    private val rotationProvider: () -> Int = { bakedRotation },
) {
    private val thread = HandlerThread("hushai-gl")
    private var handler: Handler? = null
    private var egl: EglCore? = null
    private var eglSurface: EGLSurface? = null
    private var program: OesTextureProgram? = null
    private var surfaceTexture: SurfaceTexture? = null

    /** The Surface the camera must target (backed by our SurfaceTexture). Valid after [start]. */
    var inputSurface: Surface? = null
        private set

    // Whether the encoder frame is portrait (dims swapped). Only same-bucket rotations may
    // change live; a cross-bucket request is dropped (can't resize a running encoder).
    private val bakedBucket = Math.floorMod(bakedRotation, 180)
    @Volatile private var rotationDegrees = Math.floorMod(bakedRotation, 360)
    private var loggedCrossBucket = false

    private val stMatrix = FloatArray(16)
    private val rotMatrix = FloatArray(16)
    private val texMatrix = FloatArray(16)

    /**
     * Bring up the GL thread + EGL + SurfaceTexture and publish [inputSurface]. Blocks until
     * ready. THROWS on any EGL/GL init failure — the caller must catch it and fall back to the
     * direct-surface + rotation-matrix path so capture never depends on GL being available.
     */
    fun start() {
        thread.start()
        val h = Handler(thread.looper)
        handler = h
        val latch = CountDownLatch(1)
        var initError: Throwable? = null
        h.post {
            try {
                val core = EglCore()
                egl = core
                val surf = core.createWindowSurface(encoderSurface)
                eglSurface = surf
                core.makeCurrent(surf)
                val prog = OesTextureProgram()
                program = prog
                val st = SurfaceTexture(prog.textureId)
                st.setDefaultBufferSize(sensorWidth, sensorHeight)
                st.setOnFrameAvailableListener({ h.post { drawFrame() } }, h)
                surfaceTexture = st
                inputSurface = Surface(st)
            } catch (t: Throwable) {
                initError = t
            } finally {
                latch.countDown()
            }
        }
        latch.await()
        initError?.let { throw it }
    }

    private fun drawFrame() {
        val st = surfaceTexture ?: return
        val core = egl ?: return
        val surf = eglSurface ?: return
        val prog = program ?: return
        try {
            st.updateTexImage()
        } catch (e: Exception) {
            HushaiLog.warn("gl updateTexImage failed: ${e.message}")
            return
        }
        st.getTransformMatrix(stMatrix)

        // Live within-bucket self-correction (e.g. 0↔180). Cross-bucket is impossible without
        // an encoder resize, so keep the mount orientation and note it once.
        val want = Math.floorMod(runCatching { rotationProvider() }.getOrDefault(rotationDegrees), 360)
        if (Math.floorMod(want, 180) == bakedBucket) {
            rotationDegrees = want
        } else if (!loggedCrossBucket) {
            loggedCrossBucket = true
            HushaiLog.warn(
                "upright-bake: orientation changed buckets (want=$want, baked bucket=$bakedBucket); " +
                    "keeping mount orientation — restart capture after re-mounting.",
            )
        }

        buildTexMatrix(rotationDegrees)
        GLES20.glViewport(0, 0, outWidth, outHeight)
        GLES20.glClearColor(0f, 0f, 0f, 1f)
        GLES20.glClear(GLES20.GL_COLOR_BUFFER_BIT)
        prog.draw(texMatrix)
        // PTS first, then swap (see EglCore.setPresentationTime).
        core.setPresentationTime(surf, st.timestamp)
        core.swapBuffers(surf)
    }

    /**
     * Compose the texcoord matrix: rotate about the texcoord centre (0.5, 0.5), THEN apply the
     * SurfaceTexture transform. `vTexCoord = stMatrix * rotate * aTexCoord`.
     *
     * SIGN NOTE (empirical — verify on the rig): [rotationDegrees] is the clockwise angle from
     * [com.hushai.android.capture.OrientationTracker.orientationHint] that makes the image
     * upright. Positive `rotateM` about +Z spins texcoords counter-clockwise, which spins the
     * sampled IMAGE clockwise — so this sign should be correct. If a captured frame comes out
     * rotated the wrong way, negate the angle here (mirrors OrientationTracker's own sign note).
     */
    private fun buildTexMatrix(rotationDeg: Int) {
        Matrix.setIdentityM(rotMatrix, 0)
        Matrix.translateM(rotMatrix, 0, 0.5f, 0.5f, 0f)
        // Texcoord angle = rotationDeg - 90. Measured on the rig (Galaxy S8 back cam,
        // SENSOR_ORIENTATION=90): the SurfaceTexture transform already applies the sensor rotation,
        // so the displayed image orientation equals the texcoord angle (image_offset = tc). tc = 0
        // is upright at this mount (orientationHint=90); rotationDeg-90 generalizes to the others.
        Matrix.rotateM(rotMatrix, 0, (rotationDeg - 90).toFloat(), 0f, 0f, 1f)
        Matrix.translateM(rotMatrix, 0, -0.5f, -0.5f, 0f)
        Matrix.multiplyMM(texMatrix, 0, stMatrix, 0, rotMatrix, 0)
    }

    /** Tear down GL on its own thread, then quit it. Idempotent-ish; safe after a failed start. */
    fun release() {
        val h = handler
        if (h != null) {
            val latch = CountDownLatch(1)
            h.post {
                runCatching { inputSurface?.release() }
                runCatching { surfaceTexture?.release() }
                runCatching { program?.release() }
                runCatching { eglSurface?.let { egl?.releaseSurface(it) } }
                runCatching { egl?.release() }
                inputSurface = null
                surfaceTexture = null
                program = null
                eglSurface = null
                egl = null
                latch.countDown()
            }
            runCatching { latch.await() }
        }
        thread.quitSafely()
    }
}
