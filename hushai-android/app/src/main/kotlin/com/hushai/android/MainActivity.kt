package com.hushai.android

import android.Manifest
import android.app.admin.DevicePolicyManager
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.view.Surface
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface as ComposeSurface
import androidx.compose.ui.Modifier
import androidx.core.content.ContextCompat
import com.hushai.android.capture.CaptureService
import com.hushai.android.config.Settings
import com.hushai.android.ui.CaptureScreen

/**
 * Thin controller: gathers runtime permissions, hosts the Compose settings UI,
 * and starts/stops [CaptureService]. While in the foreground it also *binds* to
 * the service to hand it the on-screen preview surface (so the user sees exactly
 * what's being streamed), and drives the battery-saver screen-lock via Device
 * Admin. Debug-friendly headless path: `url`/`token`/`autostart` Intent extras
 * let the automation script drive everything without any UI taps.
 */
class MainActivity : ComponentActivity() {

    private val settings by lazy { Settings(applicationContext) }
    private val dpm by lazy { getSystemService(DevicePolicyManager::class.java) }
    private val adminComponent by lazy { ComponentName(this, HushaiDeviceAdminReceiver::class.java) }

    // Start requested before permissions resolved; replayed once granted.
    private var pendingStart: Pair<String, String>? = null

    // Live preview wiring: the SurfaceView's surface and the bound service binder
    // arrive independently; whenever both exist we push the surface to the camera.
    private var captureBinder: CaptureService.LocalBinder? = null
    private var previewSurface: Surface? = null

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, service: IBinder) {
            captureBinder = service as CaptureService.LocalBinder
            syncPreview()
        }

        override fun onServiceDisconnected(name: ComponentName) {
            captureBinder = null
        }
    }

    private val permissionLauncher =
        registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
            pendingStart?.let { (url, token) ->
                pendingStart = null
                if (hasCapturePermissions()) launchService(url, token)
            }
        }

    // Re-checks admin status when the user returns from the "activate device admin"
    // system screen; if they granted it, lock immediately (their original intent).
    private val deviceAdminLauncher =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) {
            if (dpm.isAdminActive(adminComponent)) dpm.lockNow()
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val initialUrl = settings.urlBlocking()
        val initialToken = settings.tokenBlocking()
        val deviceId = settings.deviceIdBlocking()
        val initialWakeWord = settings.wakeWordBlocking()
        val initialRagUrl = settings.ragUrlBlocking()
        val initialAssistantEnabled = settings.assistantEnabledBlocking()

        setContent {
            MaterialTheme {
                ComposeSurface(modifier = Modifier.fillMaxSize()) {
                    CaptureScreen(
                        initialUrl = initialUrl,
                        initialToken = initialToken,
                        deviceId = deviceId,
                        initialWakeWord = initialWakeWord,
                        initialRagUrl = initialRagUrl,
                        initialAssistantEnabled = initialAssistantEnabled,
                        onStart = { url, token -> requestStart(url, token) },
                        onStop = { stopService() },
                        onBatterySaver = { enterBatterySaver() },
                        onAssistantEnabledChange = { enabled ->
                            Thread { settings.setAssistantEnabledBlocking(enabled) }.start()
                            captureBinder?.setAssistantEnabled(enabled)
                        },
                        onWakeWordChange = { word ->
                            Thread { settings.setWakeWordBlocking(word) }.start()
                            captureBinder?.setWakeWord(word)
                        },
                        onRagUrlChange = { Thread { settings.setRagUrlBlocking(it) }.start() },
                        onEnroll = { captureBinder?.enrollOwner() },
                        onPreviewSurfaceAvailable = { surface ->
                            previewSurface = surface
                            syncPreview()
                        },
                        // Ignore a stale teardown from a previous SurfaceView: only
                        // clear if the surface that died is the one we're still using.
                        onPreviewSurfaceLost = { surface ->
                            if (previewSurface === surface) {
                                previewSurface = null
                                syncPreview()
                            }
                        },
                    )
                }
            }
        }

        handleIntent(intent)
    }

    override fun onStart() {
        super.onStart()
        // Bind to feed the preview surface. BIND_AUTO_CREATE only stands up a
        // bound (no-capture) instance; capture begins solely via Start/onStartCommand.
        bindService(Intent(this, CaptureService::class.java), connection, Context.BIND_AUTO_CREATE)
    }

    override fun onStop() {
        super.onStop()
        // Backgrounding (incl. battery-saver screen-lock) drops the preview and
        // unbinds; a started/capturing service keeps running, capture uninterrupted.
        captureBinder?.detachPreview()
        runCatching { unbindService(connection) }
        captureBinder = null
    }

    /** Push the preview surface to the camera whenever both binder + surface exist. */
    private fun syncPreview() {
        val binder = captureBinder ?: return
        val surface = previewSurface
        if (surface != null) binder.attachPreview(surface) else binder.detachPreview()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleIntent(intent)
    }

    /** Headless config: persist provided url/token and autostart if requested. */
    private fun handleIntent(intent: Intent?) {
        intent ?: return
        if (intent.getBooleanExtra(EXTRA_STOP, false)) {
            stopService()
            return
        }
        val url = intent.getStringExtra(CaptureService.EXTRA_URL)
        val token = intent.getStringExtra(CaptureService.EXTRA_TOKEN)
        url?.let { settings.setUrlBlocking(it) }
        token?.let { settings.setTokenBlocking(it) }
        if (intent.getBooleanExtra(EXTRA_AUTOSTART, false)) {
            requestStart(url ?: settings.urlBlocking(), token ?: settings.tokenBlocking())
        }
    }

    private fun requestStart(url: String, token: String) {
        if (hasCapturePermissions()) {
            launchService(url, token)
        } else {
            pendingStart = url to token
            permissionLauncher.launch(requiredPermissions())
        }
    }

    private fun launchService(url: String, token: String) {
        ContextCompat.startForegroundService(this, CaptureService.startIntent(this, url, token))
    }

    private fun stopService() {
        ContextCompat.startForegroundService(this, CaptureService.stopIntent(this))
    }

    /**
     * Battery saver: lock the screen off while capture keeps running in the
     * foreground service. Needs Device Admin (force-lock) once; if not yet
     * granted, send the user to the system activation screen and lock on return.
     */
    private fun enterBatterySaver() {
        if (dpm.isAdminActive(adminComponent)) {
            dpm.lockNow()
        } else {
            val intent = Intent(DevicePolicyManager.ACTION_ADD_DEVICE_ADMIN).apply {
                putExtra(DevicePolicyManager.EXTRA_DEVICE_ADMIN, adminComponent)
                putExtra(
                    DevicePolicyManager.EXTRA_ADD_EXPLANATION,
                    "Hushai locks the screen so it can keep capturing with the display " +
                        "off (battery saver). Capture continues; nothing else is changed.",
                )
            }
            deviceAdminLauncher.launch(intent)
        }
    }

    private fun requiredPermissions(): Array<String> = buildList {
        add(Manifest.permission.CAMERA)
        add(Manifest.permission.RECORD_AUDIO)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            add(Manifest.permission.POST_NOTIFICATIONS)
        }
    }.toTypedArray()

    private fun hasCapturePermissions(): Boolean =
        ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA) ==
            PackageManager.PERMISSION_GRANTED &&
            ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) ==
            PackageManager.PERMISSION_GRANTED

    companion object {
        const val EXTRA_AUTOSTART = "autostart"
        const val EXTRA_STOP = "stop"
    }
}
