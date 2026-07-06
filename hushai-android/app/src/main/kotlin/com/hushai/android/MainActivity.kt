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
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.Surface as ComposeSurface
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.core.content.ContextCompat
import com.hushai.android.capture.CaptureService
import com.hushai.android.config.Settings
import com.hushai.android.net.EventsClient
import com.hushai.android.net.Http
import com.hushai.android.net.Reachability
import com.hushai.android.net.PersonsClient
import com.hushai.android.net.PlatesClient
import com.hushai.android.net.SpeakersClient
import com.hushai.android.ui.CaptureScreen
import com.hushai.android.ui.EventsScreen
import com.hushai.android.ui.PeopleScreen
import com.hushai.android.ui.PlatesScreen
import com.hushai.android.ui.VoicesScreen
import com.hushai.android.ui.theme.HushaiTheme
import com.hushai.android.util.StatusBus
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

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
    private var pendingStart: PendingStart? = null

    private data class PendingStart(val url: String, val token: String, val audioOnly: Boolean)

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
            pendingStart?.let { p ->
                pendingStart = null
                if (hasCapturePermissions(p.audioOnly)) launchService(p.url, p.token, p.audioOnly)
            }
        }

    // Re-checks admin status when the user returns from the "activate device admin"
    // system screen; if they granted it, lock immediately (their original intent).
    private val deviceAdminLauncher =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) {
            if (dpm.isAdminActive(adminComponent)) dpm.lockNow()
        }

    // SAF multi-file picker for manual import: returns content:// URIs we can take a
    // persistable read grant on (so a queued import survives process death).
    private val importPicker =
        registerForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
            onFilesPicked(uris)
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val initialUrl = settings.urlBlocking()
        val initialToken = settings.tokenBlocking()
        val deviceId = settings.deviceIdBlocking()
        val initialWakeWord = settings.wakeWordBlocking()
        val initialRagUrl = settings.ragUrlBlocking()
        val initialAssistantEnabled = settings.assistantEnabledBlocking()
        val initialAudioOnly = settings.audioOnlyBlocking()
        val initialDiskCapGb = settings.diskCapBytesBlocking() / (1024f * 1024f * 1024f)

        setContent {
            HushaiTheme {
                // Two screens, no nav framework: a simple toggle + system-back handling.
                var screen by remember { mutableStateOf(Screen.Capture) }
                ComposeSurface(modifier = Modifier.fillMaxSize()) {
                    when (screen) {
                    Screen.Voices -> {
                        BackHandler { screen = Screen.Capture }
                        // This branch is freshly composed each time Voices opens, so read the
                        // current backend target once on entry; key the client on it so a
                        // changed URL/token rebuilds it rather than reusing a stale one.
                        val voicesUrl = remember { settings.urlBlocking() }
                        val voicesToken = remember { settings.tokenBlocking() }
                        VoicesScreen(
                            client = remember(voicesUrl, voicesToken) {
                                SpeakersClient(Http.upload, voicesUrl, voicesToken)
                            },
                            onBack = { screen = Screen.Capture },
                        )
                    }
                    Screen.People -> {
                        BackHandler { screen = Screen.Capture }
                        // Freshly composed on entry; read the current backend target once and key
                        // the client on it so a changed URL/token rebuilds it (same as Voices).
                        val peopleUrl = remember { settings.urlBlocking() }
                        val peopleToken = remember { settings.tokenBlocking() }
                        PeopleScreen(
                            client = remember(peopleUrl, peopleToken) {
                                PersonsClient(Http.upload, peopleUrl, peopleToken)
                            },
                            onBack = { screen = Screen.Capture },
                        )
                    }
                    Screen.Plates -> {
                        BackHandler { screen = Screen.Capture }
                        // Freshly composed on entry; read the current backend target once and key
                        // the client on it so a changed URL/token rebuilds it (same as People).
                        val platesUrl = remember { settings.urlBlocking() }
                        val platesToken = remember { settings.tokenBlocking() }
                        PlatesScreen(
                            client = remember(platesUrl, platesToken) {
                                PlatesClient(Http.upload, platesUrl, platesToken)
                            },
                            onBack = { screen = Screen.Capture },
                        )
                    }
                    Screen.Events -> {
                        BackHandler { screen = Screen.Capture }
                        val eventsUrl = remember { settings.urlBlocking() }
                        val eventsToken = remember { settings.tokenBlocking() }
                        EventsScreen(
                            client = remember(eventsUrl, eventsToken) {
                                EventsClient(Http.upload, eventsUrl, eventsToken)
                            },
                            onBack = { screen = Screen.Capture },
                        )
                    }
                    Screen.Capture ->
                    CaptureScreen(
                        initialUrl = initialUrl,
                        initialToken = initialToken,
                        deviceId = deviceId,
                        initialWakeWord = initialWakeWord,
                        initialRagUrl = initialRagUrl,
                        initialAssistantEnabled = initialAssistantEnabled,
                        initialAudioOnly = initialAudioOnly,
                        initialDiskCapGb = initialDiskCapGb,
                        onOpenVoices = { screen = Screen.Voices },
                        onOpenPeople = { screen = Screen.People },
                        onOpenPlates = { screen = Screen.Plates },
                        onOpenEvents = { screen = Screen.Events },
                        onStart = { url, token, audioOnly -> requestStart(url, token, audioOnly) },
                        onAudioOnlyChange = { ao -> Thread { settings.setAudioOnlyBlocking(ao) }.start() },
                        onDiskCapChange = { gb ->
                            val bytes = (gb.coerceAtLeast(0.5f).toDouble() * 1024 * 1024 * 1024).toLong()
                            Thread {
                                settings.setDiskCapBytesBlocking(bytes)
                                // Apply to the running capture immediately (no-op if not bound/started);
                                // persistence above still covers the next cold start.
                                captureBinder?.updateDiskCap(bytes)
                            }.start()
                        },
                        onPickImport = { runCatching { importPicker.launch(arrayOf("audio/*", "video/*")) } },
                        onCancelImport = { captureBinder?.cancelImport() },
                        onStop = { stopService() },
                        // Pre-start health probe (GET /healthz + /readyz) off the main
                        // thread; seeds StatusBus so the Debug "Backend" line reflects it.
                        onCheckConnection = { url ->
                            val health = withContext(Dispatchers.IO) {
                                Reachability(Http.probe, url.trim()).check()
                            }
                            StatusBus.update {
                                it.copy(reachable = health.reachable, live = health.live, ready = health.ready)
                            }
                            health
                        },
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
        val ragUrl = intent.getStringExtra(CaptureService.EXTRA_RAG_URL)
        val ragToken = intent.getStringExtra(CaptureService.EXTRA_RAG_TOKEN)
        url?.let { settings.setUrlBlocking(it) }
        token?.let { settings.setTokenBlocking(it) }
        // Voice-assistant RAG/TTS host. Lets the headless harness force localhost
        // (USB `adb reverse` tunnel) and override any stale LAN-IP value a prior
        // wireless session persisted to DataStore (which survives `install -r`).
        ragUrl?.let { settings.setRagUrlBlocking(it) }
        // Bearer for the voice assistant's rag calls (matches the server's RAG_TOKEN).
        ragToken?.let { settings.setRagTokenBlocking(it) }
        // Audio-only is sticky: persist an explicit extra so it survives restarts
        // and the UI reflects it; otherwise fall back to the persisted setting.
        val audioOnly = if (intent.hasExtra(CaptureService.EXTRA_AUDIO_ONLY)) {
            intent.getBooleanExtra(CaptureService.EXTRA_AUDIO_ONLY, false)
                .also { settings.setAudioOnlyBlocking(it) }
        } else {
            settings.audioOnlyBlocking()
        }
        // Opt-in upright-BAKE (experimental): persist the extra so the next startCapture reads it
        // from Settings. Forwarded here because CaptureService.startIntent doesn't carry it, and a
        // direct start-foreground-service is blocked on some OEMs — the Activity autostart is the
        // supported headless path. (See CaptureService.EXTRA_UPRIGHT_BAKE.)
        if (intent.hasExtra(CaptureService.EXTRA_UPRIGHT_BAKE)) {
            settings.setUprightBakeBlocking(intent.getBooleanExtra(CaptureService.EXTRA_UPRIGHT_BAKE, false))
        }
        // Headless assistant enable (`--ez assistant true`): persist BEFORE the autostart
        // branch — CaptureService builds the assistant at capture start when the setting is
        // on, so persist-then-autostart is sufficient (no binder round-trip needed). Also
        // pushed live when already bound (assistant toggled without a restart).
        if (intent.hasExtra(EXTRA_ASSISTANT)) {
            val on = intent.getBooleanExtra(EXTRA_ASSISTANT, false)
            settings.setAssistantEnabledBlocking(on)
            captureBinder?.setAssistantEnabled(on)
        }
        if (intent.getBooleanExtra(EXTRA_AUTOSTART, false)) {
            requestStart(url ?: settings.urlBlocking(), token ?: settings.tokenBlocking(), audioOnly)
        }
        // Headless enrollment trigger (`--ez enroll true`), for the voice-assistant test
        // harness (local_dev/voice_assistant_loop.py). The harness sends this only AFTER
        // logcat shows "voice assistant ready", so the binder is up and the call lands; via
        // onNewIntent the activity is already bound. `enrollOwner()` is a volatile-flag
        // request the assistant worker picks up — safe no-op if the assistant is absent.
        if (intent.getBooleanExtra(EXTRA_ENROLL, false)) {
            captureBinder?.enrollOwner()
        }
    }

    private fun requestStart(url: String, token: String, audioOnly: Boolean) {
        if (hasCapturePermissions(audioOnly)) {
            launchService(url, token, audioOnly)
        } else {
            pendingStart = PendingStart(url, token, audioOnly)
            permissionLauncher.launch(requiredPermissions(audioOnly))
        }
    }

    private fun launchService(url: String, token: String, audioOnly: Boolean) {
        ContextCompat.startForegroundService(this, CaptureService.startIntent(this, url, token, audioOnly))
    }

    private fun stopService() {
        ContextCompat.startForegroundService(this, CaptureService.stopIntent(this))
    }

    /** Take a persistable read grant on each picked file, then hand them to the
     *  service to convert into segments and upload (works while capturing or not). */
    private fun onFilesPicked(uris: List<android.net.Uri>) {
        if (uris.isEmpty()) return
        for (u in uris) runCatching {
            contentResolver.takePersistableUriPermission(u, Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        ContextCompat.startForegroundService(this, CaptureService.importIntent(this, uris))
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

    // Audio-only needs no CAMERA permission (the camera is never opened), so we
    // neither request nor gate on it — a user who only wants audio isn't forced
    // to grant camera access.
    private fun requiredPermissions(audioOnly: Boolean): Array<String> = buildList {
        if (!audioOnly) add(Manifest.permission.CAMERA)
        add(Manifest.permission.RECORD_AUDIO)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            add(Manifest.permission.POST_NOTIFICATIONS)
        }
    }.toTypedArray()

    private fun hasCapturePermissions(audioOnly: Boolean): Boolean {
        val audioGranted = ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) ==
            PackageManager.PERMISSION_GRANTED
        val cameraGranted = audioOnly || ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA) ==
            PackageManager.PERMISSION_GRANTED
        return audioGranted && cameraGranted
    }

    companion object {
        const val EXTRA_AUTOSTART = "autostart"
        const val EXTRA_STOP = "stop"
        // Headless voice-assistant control (adb harness): enable/disable + trigger enrollment.
        const val EXTRA_ASSISTANT = "assistant"
        const val EXTRA_ENROLL = "enroll"
    }
}

/** The top-level screens (no nav framework — a simple state toggle in [MainActivity]). */
private enum class Screen { Capture, Voices, People, Plates, Events }
