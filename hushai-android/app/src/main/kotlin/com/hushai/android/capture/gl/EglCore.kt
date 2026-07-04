package com.hushai.android.capture.gl

import android.opengl.EGL14
import android.opengl.EGLConfig
import android.opengl.EGLContext
import android.opengl.EGLDisplay
import android.opengl.EGLExt
import android.opengl.EGLSurface
import android.view.Surface

/**
 * Minimal EGL14 + GLES2 setup for rendering into a MediaCodec encoder input [Surface].
 * Grafika's `EglCore`/`WindowSurface` pattern, trimmed to what [CameraGlRenderer] needs.
 *
 * The one non-obvious bit is `EGL_RECORDABLE_ANDROID` in the config: without it the chosen
 * config may not be a valid draw target for an encoder input surface on some drivers.
 *
 * All methods must be called on the single thread that constructed this (the GL thread).
 */
class EglCore {
    private var display: EGLDisplay = EGL14.EGL_NO_DISPLAY
    private var context: EGLContext = EGL14.EGL_NO_CONTEXT
    private var config: EGLConfig? = null

    init {
        display = EGL14.eglGetDisplay(EGL14.EGL_DEFAULT_DISPLAY)
        if (display === EGL14.EGL_NO_DISPLAY) throw RuntimeException("eglGetDisplay failed")
        val version = IntArray(2)
        if (!EGL14.eglInitialize(display, version, 0, version, 1)) {
            display = EGL14.EGL_NO_DISPLAY
            throw RuntimeException("eglInitialize failed")
        }
        val configAttribs = intArrayOf(
            EGL14.EGL_RED_SIZE, 8,
            EGL14.EGL_GREEN_SIZE, 8,
            EGL14.EGL_BLUE_SIZE, 8,
            EGL14.EGL_RENDERABLE_TYPE, EGL14.EGL_OPENGL_ES2_BIT,
            EGL_RECORDABLE_ANDROID, 1,
            EGL14.EGL_NONE,
        )
        val configs = arrayOfNulls<EGLConfig>(1)
        val numConfigs = IntArray(1)
        if (!EGL14.eglChooseConfig(display, configAttribs, 0, configs, 0, 1, numConfigs, 0) ||
            numConfigs[0] <= 0
        ) {
            throw RuntimeException("eglChooseConfig failed (no recordable ES2 config)")
        }
        config = configs[0]
        val contextAttribs = intArrayOf(EGL14.EGL_CONTEXT_CLIENT_VERSION, 2, EGL14.EGL_NONE)
        context = EGL14.eglCreateContext(display, config, EGL14.EGL_NO_CONTEXT, contextAttribs, 0)
        checkEglError("eglCreateContext")
        if (context === EGL14.EGL_NO_CONTEXT) throw RuntimeException("eglCreateContext returned null")
    }

    /** A window EGL surface backed by the encoder input [surface]. */
    fun createWindowSurface(surface: Surface): EGLSurface {
        val attribs = intArrayOf(EGL14.EGL_NONE)
        val eglSurface = EGL14.eglCreateWindowSurface(display, config, surface, attribs, 0)
        checkEglError("eglCreateWindowSurface")
        if (eglSurface == null || eglSurface === EGL14.EGL_NO_SURFACE) {
            throw RuntimeException("eglCreateWindowSurface returned null")
        }
        return eglSurface
    }

    fun makeCurrent(eglSurface: EGLSurface) {
        if (!EGL14.eglMakeCurrent(display, eglSurface, eglSurface, context)) {
            throw RuntimeException("eglMakeCurrent failed")
        }
    }

    fun swapBuffers(eglSurface: EGLSurface): Boolean = EGL14.eglSwapBuffers(display, eglSurface)

    /**
     * Stamp the frame's presentation time onto the encoder input surface. LOAD-BEARING: the
     * MediaCodec input surface takes the encoded sample's PTS from here, so without it the
     * segment durations / PDT alignment break. Pass the camera SurfaceTexture's timestamp.
     */
    fun setPresentationTime(eglSurface: EGLSurface, nanos: Long) {
        EGLExt.eglPresentationTimeANDROID(display, eglSurface, nanos)
    }

    fun releaseSurface(eglSurface: EGLSurface) {
        if (display !== EGL14.EGL_NO_DISPLAY) EGL14.eglDestroySurface(display, eglSurface)
    }

    fun release() {
        if (display !== EGL14.EGL_NO_DISPLAY) {
            EGL14.eglMakeCurrent(
                display, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_CONTEXT,
            )
            EGL14.eglDestroyContext(display, context)
            EGL14.eglReleaseThread()
            EGL14.eglTerminate(display)
        }
        display = EGL14.EGL_NO_DISPLAY
        context = EGL14.EGL_NO_CONTEXT
        config = null
    }

    private fun checkEglError(op: String) {
        val err = EGL14.eglGetError()
        if (err != EGL14.EGL_SUCCESS) {
            throw RuntimeException("$op: EGL error 0x${Integer.toHexString(err)}")
        }
    }

    private companion object {
        // EGL_RECORDABLE_ANDROID from eglext.h — not exposed as a constant in EGL14.
        const val EGL_RECORDABLE_ANDROID = 0x3142
    }
}
