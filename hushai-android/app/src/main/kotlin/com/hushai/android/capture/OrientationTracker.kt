package com.hushai.android.capture

import android.content.Context
import android.view.OrientationEventListener

/**
 * Tracks the device's PHYSICAL orientation via the accelerometer and computes the MediaMuxer
 * orientation hint that makes the recorded video UPRIGHT no matter how the phone is held or mounted.
 *
 * Why the accelerometer (OrientationEventListener) rather than the display rotation: this is an
 * always-on capture device that usually runs with the screen OFF and/or rotation locked, so
 * `Display.rotation` is unreliable/frozen. The accelerometer reports the true physical orientation
 * even screen-off, so "whichever direction the phone is in, the frame is upright" holds for a phone
 * propped or mounted in ANY of the four orientations.
 *
 * The hint (0/90/180/270 clockwise) is stamped into each segment's MP4 rotation matrix by
 * [SegmentMuxer]; the worker's ffmpeg autorotates on it (no `-noautorotate`) and browsers honor it,
 * so both the detection frames and NVR playback come out upright. Re-read per segment, so rotating
 * the phone mid-capture self-corrects on the next ~2s segment.
 */
class OrientationTracker(
    context: Context,
    private val sensorOrientation: Int,
    private val facingFront: Boolean,
) {
    // Device rotation from its natural orientation, snapped to 0/90/180/270. @Volatile: written on the
    // sensor callback thread, read on the encoder drain thread at each segment boundary.
    @Volatile private var deviceRotation = 0

    private val listener = object : OrientationEventListener(context.applicationContext) {
        override fun onOrientationChanged(orientation: Int) {
            // ORIENTATION_UNKNOWN (device flat / indeterminate) — keep the last good rotation.
            if (orientation == ORIENTATION_UNKNOWN) return
            deviceRotation = when {
                orientation <= 45 || orientation > 315 -> 0
                orientation <= 135 -> 90
                orientation <= 225 -> 180
                else -> 270
            }
        }
    }

    fun enable() {
        if (listener.canDetectOrientation()) listener.enable()
    }

    fun disable() = listener.disable()

    /**
     * Clockwise degrees to rotate the recorded frames so they display upright — the MediaMuxer hint.
     * Standard Camera2 recording formula: for the back camera the device rotation adds to the sensor
     * orientation; the front camera is mirrored. (Verified against the physical rig; adjust the sign
     * here if a captured frame comes out flipped.)
     */
    fun orientationHint(): Int {
        val sign = if (facingFront) 1 else -1
        return (sensorOrientation - deviceRotation * sign + 360) % 360
    }
}
