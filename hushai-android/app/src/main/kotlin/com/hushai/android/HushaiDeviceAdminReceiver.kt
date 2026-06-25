package com.hushai.android

import android.app.admin.DeviceAdminReceiver

/**
 * Minimal device-admin component. We request only the `force-lock` policy (see
 * `res/xml/device_admin.xml`) so battery-saver can call
 * `DevicePolicyManager.lockNow()` to turn the screen off while the foreground
 * capture service keeps running. No other admin powers are used.
 */
class HushaiDeviceAdminReceiver : DeviceAdminReceiver()
