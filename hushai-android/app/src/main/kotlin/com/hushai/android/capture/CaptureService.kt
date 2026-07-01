package com.hushai.android.capture

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Binder
import android.os.IBinder
import android.net.Uri
import android.os.PowerManager
import android.os.SystemClock
import android.provider.OpenableColumns
import android.view.Surface
import androidx.core.app.ServiceCompat
import com.hushai.android.assistant.SpeakerMath
import com.hushai.android.assistant.VoiceAssistant
import com.hushai.android.capture.imports.ImportManager
import com.hushai.android.capture.imports.ImportRequest
import com.hushai.android.config.DeviceIdentity
import com.hushai.android.config.Settings
import com.hushai.android.net.ConnectivityState
import com.hushai.android.net.Http
import com.hushai.android.net.NetworkMonitor
import com.hushai.android.net.RagClient
import com.hushai.android.net.TtsClient
import com.hushai.android.net.Reachability
import com.hushai.android.net.UploadOutcome
import com.hushai.android.net.Uploader
import com.hushai.android.util.AssistantBus
import com.hushai.android.util.CaptureStatus
import com.hushai.android.util.HushaiLog
import com.hushai.android.util.StatusBus
import com.hushai.android.util.formatBytes
import okio.ByteString
import java.io.File
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong
import kotlin.concurrent.thread

/**
 * Always-on foreground capture service (typed camera|microphone). Owns the camera
 * session, both encoders, the uploader drain loop, and the retry buffer — so
 * screen-off, backgrounding, and Activity death don't stop capture (only an
 * explicit Stop does). START_STICKY asks the OS to restart it if killed.
 *
 * The Activity is a thin controller; all capture lifecycle lives here.
 */
class CaptureService : Service() {

    private lateinit var settings: Settings
    // The "delivery context" (durable buffer + uploader + drain + connectivity) is set
    // up by ensureDeliveryRunning and is shared by live capture AND manual imports; it
    // outlives a capture stop if an import is still running.
    @Volatile private var buffer: DurableSegmentBuffer? = null
    @Volatile private var networkMonitor: NetworkMonitor? = null
    @Volatile private var connectivity: ConnectivityState? = null
    @Volatile private var delivering = false
    @Volatile private var importManager: ImportManager? = null
    @Volatile private var importActive = false
    @Volatile private var importWatermarkBytes = Long.MAX_VALUE
    private val importStreamCounter = AtomicLong(0)

    // These are written by the hushai-start capture thread and read by the main
    // thread (binder preview calls; stopCapture/releaseWakeLock from ACTION_STOP &
    // onDestroy), so they must be @Volatile for cross-thread visibility — otherwise
    // attachPreview can see a stale-null camera and silently drop the preview, and
    // stopCapture can miss cleanup. (settings/buffer are set in onCreate, which
    // happens-before onStartCommand per the Android lifecycle, so they need none.)
    @Volatile private var uploader: Uploader? = null
    @Volatile private var camera: CameraController? = null
    @Volatile private var video: VideoEncoder? = null
    @Volatile private var orientationTracker: OrientationTracker? = null
    @Volatile private var audio: AudioEncoder? = null
    @Volatile private var micSource: MicSource? = null
    @Volatile private var assistant: VoiceAssistant? = null
    @Volatile private var alertNotifier: AlertNotifier? = null
    @Volatile private var wakeLock: PowerManager.WakeLock? = null
    @Volatile private var uploadThread: Thread? = null

    // Every capture start/stop transition runs on this ONE thread, so they serialize
    // and can never overlap — the heavy teardown (thread joins, buffer flush) stays
    // off the main thread (no UI freeze) while still being race-free without locks.
    private val lifecycle = Executors.newSingleThreadExecutor { r -> Thread(r, "hushai-lifecycle") }
    // The user's last-requested state, set on the main thread by onStartCommand; the
    // lifecycle thread reconciles actual capture toward it. Captured start params let
    // a reconcile (re)start after a stop without re-reading the intent.
    @Volatile private var desiredRunning = false
    @Volatile private var pendingUrl: String? = null
    @Volatile private var pendingToken: String? = null
    @Volatile private var pendingAudioOnly = false

    private val videoSeq = AtomicLong(0)
    private val audioSeq = AtomicLong(0)
    @Volatile private var running = false
    // The mode the LIVE session is actually capturing in. Read on the main thread
    // (onStartCommand) to keep a redundant start from re-declaring a foreground type
    // that contradicts the running pipeline; written on the capture thread before
    // `running` flips true, so a reader that sees running==true also sees this.
    @Volatile private var activeAudioOnly = false

    // On-screen preview surface handed in by the foreground Activity (may be null
    // when the app is backgrounded / in battery-saver). Remembered here so it can
    // be (re)applied whenever the camera is (re)created. Touched from the main
    // thread (binder calls) and the capture thread (startCapture) -> @Volatile.
    @Volatile private var previewSurface: Surface? = null

    // The "online" notification text (capture summary) so refreshNotification can
    // restore it after an offline/draining stretch.
    @Volatile private var captureSummary: String = ""

    private val binder = LocalBinder()

    /** Lets the bound Activity feed the preview surface + drive the voice assistant. */
    inner class LocalBinder : Binder() {
        fun attachPreview(surface: Surface) = this@CaptureService.attachPreview(surface)
        fun detachPreview() = this@CaptureService.detachPreview()
        val isCapturing: Boolean get() = running
        fun setWakeWord(word: String) = this@CaptureService.setWakeWord(word)
        fun setAssistantEnabled(enabled: Boolean) = this@CaptureService.setAssistantEnabled(enabled)
        fun enrollOwner() { assistant?.startEnrollment() }
        fun cancelImport() { importManager?.cancelCurrent() }
    }

    override fun onBind(intent: Intent?): IBinder = binder

    private fun attachPreview(surface: Surface) {
        previewSurface = surface
        camera?.setPreviewSurface(surface)
    }

    private fun detachPreview() {
        previewSurface = null
        camera?.setPreviewSurface(null)
    }

    private fun setWakeWord(word: String) {
        // Persistence is handled by the caller (MainActivity, off the main thread);
        // here we only push the live value to a running assistant (no I/O).
        assistant?.wakeWord = word
    }

    /** Toggle the assistant live: attach/detach it as a second mic sink while capturing.
     *  Build/teardown run off the main thread (DataStore reads, model load, thread join). */
    private fun setAssistantEnabled(enabled: Boolean) {
        // Run on the lifecycle executor so build/start/stop serialize with
        // reconcile()/stopCapture() (which also mutate `assistant` there). Done off the
        // main thread anyway (DataStore reads, model load, thread join). Without this
        // serialization a stopCapture() that reads `assistant` just before this thread
        // assigned it would tear nothing down, leaking a live VoiceAssistant (worker
        // thread + loaded Vosk/speaker models + speak executor) for the process lifetime.
        lifecycle.submit {
            if (enabled) {
                val mic = micSource
                if (assistant == null && mic != null && running) {
                    val a = buildAssistant()
                    // A stop landed while we were building — don't start an orphan.
                    if (!running || micSource == null) { runCatching { a.stop() }; return@submit }
                    assistant = a
                    a.start()        // start (running=true) BEFORE the mic can call onPcm on it
                    mic.addSink(a)
                    AssistantBus.update { it.copy(enabled = true) }
                }
            } else {
                val a = assistant
                assistant = null
                if (a != null) { runCatching { micSource?.removeSink(a) }; runCatching { a.stop() } }
                AssistantBus.update { it.copy(enabled = false) }
            }
        }
    }

    private fun buildAssistant(): VoiceAssistant {
        val ragUrl = settings.ragUrlBlocking()
        val ragToken = settings.ragTokenBlocking()
        val ragClient = RagClient(Http.rag, ragUrl, ragToken)
        val ttsClient = TtsClient(Http.rag, ragUrl, ragToken)
        val owner = SpeakerMath.parse(settings.ownerEmbeddingBlocking())
        return VoiceAssistant(
            context = applicationContext,
            deviceId = settings.deviceIdBlocking(),
            initialWakeWord = settings.wakeWordBlocking(),
            ragClient = ragClient,
            ttsClient = ttsClient,
            initialOwnerEmbedding = owner,
            onEnrollComplete = { emb -> settings.setOwnerEmbeddingBlocking(SpeakerMath.format(emb)) },
        )
    }

    override fun onCreate() {
        super.onCreate()
        settings = Settings(applicationContext)
        CaptureNotification.ensureChannel(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            desiredRunning = false
            // A live capture is already foreground (and holds the capture perms), so
            // re-assert it with the SAME type the session declared to honour the
            // startForegroundService() ~5s contract; teardown removes it on the
            // lifecycle thread. When not running there's no foreground/perms to promote
            // (a typed startForeground without the runtime permission would throw on 14+).
            if (running) startForegroundTyped("stopping…", activeAudioOnly)
            // Flip the UI OFF instantly on the main thread; the (possibly slow) teardown
            // then runs in the background so the toggle never freezes.
            StatusBus.update { it.copy(running = false) }
            // Abort any hung upload so the buffer-flush loop can't wait out its timeout.
            uploader?.cancelInFlight()
            lifecycle.submit { reconcile(startId) }
            return START_NOT_STICKY
        }

        if (intent?.action == ACTION_IMPORT) {
            val uris = intent.getStringArrayListExtra(EXTRA_IMPORT_URIS) ?: arrayListOf()
            // Enter the foreground promptly (startForegroundService contract). A live
            // capture already owns the foreground (camera|microphone); otherwise promote
            // with DATA_SYNC for the background import. Redeliver the URIs if killed.
            if (!running) {
                ServiceCompat.startForeground(
                    this,
                    CaptureNotification.NOTIFICATION_ID,
                    CaptureNotification.build(this, "Importing…"),
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
                )
            }
            lifecycle.submit { handleImport(uris) }
            return START_REDELIVER_INTENT
        }

        // Audio-only mode decides which foreground-service type we declare (camera
        // requires the type + permission), so resolve it BEFORE startForeground.
        // The caller passes it as an extra; on a START_STICKY OS-restart the intent
        // is null, so fall back to the persisted setting (a single cached boolean).
        val requestedAudioOnly = if (intent?.hasExtra(EXTRA_AUDIO_ONLY) == true) {
            intent.getBooleanExtra(EXTRA_AUDIO_ONLY, false)
        } else {
            settings.audioOnlyBlocking()
        }

        desiredRunning = true
        pendingUrl = intent?.getStringExtra(EXTRA_URL)
        pendingToken = intent?.getStringExtra(EXTRA_TOKEN)
        pendingAudioOnly = requestedAudioOnly

        // Enter the foreground promptly (the startForegroundService() contract), then
        // configure + start capture off the main thread (DataStore reads, camera open).
        // A *redundant* start while already capturing must NOT re-declare a type that
        // contradicts the live session — e.g. narrowing to microphone-only while the
        // camera is still open (the headless autostart path can fire a second start
        // with a flipped flag). When already running, the live mode is authoritative
        // and the pipeline is untouched below; changing mode requires Stop → Start.
        val declaredAudioOnly = if (running) activeAudioOnly else requestedAudioOnly
        startForegroundTyped("starting…", declaredAudioOnly)
        lifecycle.submit { reconcile(startId) }
        return START_STICKY
    }

    /**
     * Drive the actual capture pipeline toward [desiredRunning]. Runs only on the
     * single-thread lifecycle executor, so transitions are serialized: a quick OFF→ON
     * leaves capture running uninterrupted (we never tore it down), ON→OFF never starts
     * then stops cleanly, and the foreground state is never stripped from a live session.
     */
    private fun reconcile(startId: Int) {
        if (desiredRunning) {
            if (!running) {
                startCapture(pendingUrl, pendingToken, pendingAudioOnly)
            } else {
                // Already capturing (e.g. a quick OFF→ON we never tore down): re-assert
                // running so the UI we flipped OFF in the STOP branch is correct again.
                StatusBus.update { it.copy(running = true, audioOnly = activeAudioOnly) }
            }
        } else {
            if (running || camera != null) runCatching { stopCapture() }
            // Only stop the service if no newer START countermanded this stop AND no
            // import still needs the (now data-sync) foreground; stopSelf(startId) is a
            // no-op once a higher startId has arrived.
            if (!desiredRunning && !importActive) {
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf(startId)
            }
        }
    }

    private fun startForegroundTyped(text: String, audioOnly: Boolean) {
        ServiceCompat.startForeground(
            this,
            CaptureNotification.NOTIFICATION_ID,
            CaptureNotification.build(this, text),
            foregroundServiceType(audioOnly),
        )
    }

    private fun startCapture(urlOverride: String?, tokenOverride: String?, audioOnly: Boolean) {
        val url = (urlOverride ?: settings.urlBlocking()).also {
            if (urlOverride != null) settings.setUrlBlocking(urlOverride)
        }
        val token = (tokenOverride ?: settings.tokenBlocking()).also {
            if (tokenOverride != null) settings.setTokenBlocking(tokenOverride)
        }
        val deviceId = settings.deviceIdBlocking()
        val identity = DeviceIdentity(deviceId = deviceId, sessionId = com.hushai.android.util.Uuid7.bytes())

        // Bring up the shared delivery context (durable buffer + recovery + uploader +
        // connectivity + drain thread). Idempotent: a prior import may have started it.
        ensureDeliveryRunning(url, token, identity)

        // Alert "push" (roadmap A7): poll the backend's alert feed + raise notifications while the
        // always-on capture service runs. Self-contained; restarting capture re-targets it cleanly.
        alertNotifier = AlertNotifier(this).also { it.start(url, token) }

        // Publish the live mode BEFORE flipping `running` so a concurrent redundant
        // start (which reads `running` then `activeAudioOnly`) sees a consistent pair.
        activeAudioOnly = audioOnly
        running = true
        StatusBus.update { it.copy(running = true, audioOnly = audioOnly) }

        // Preflight reachability (non-fatal — capture still buffers if unreachable).
        thread(name = "hushai-preflight") {
            val health = Reachability(Http.probe, url).check()
            StatusBus.update {
                it.copy(reachable = health.reachable, live = health.live, ready = health.ready)
            }
            HushaiLog.info("preflight ${health.detail} url=$url")
        }

        // Build the live pipeline (encoders, mic, camera). Any of these can throw on
        // device-specific MediaCodec/AudioRecord/Camera2 init; the lifecycle submit() that
        // called us never .get()s its Future, so a throw here would be silently swallowed
        // — leaving running=true with a half-built pipeline that advertises "capturing"
        // while producing nothing. On failure, tear down what we built and clear running.
        try {
            // Encoders write scratch bodies here; offer() renames them to durable names.
            val segmentDir = File(File(noBackupFilesDir, "segments"), "incoming").apply { mkdirs() }

            // Audio-only skips the entire video pipeline: no camera is opened and no
            // H.264 encoder runs, so only the cam0-audio stream is produced. This is
            // why we never select a camera here — saving storage, bandwidth, battery.
            val selection = if (audioOnly) null else CameraController.select(this)
            if (!audioOnly && selection == null) {
                throw IllegalStateException("no camera available")
            }

            if (selection != null) {
                // Make every recorded segment UPRIGHT regardless of how the phone is held/mounted:
                // the tracker reads the physical orientation (accelerometer, works screen-off) and the
                // encoder stamps each segment's MP4 rotation matrix (worker ffmpeg + browser autorotate).
                val tracker = OrientationTracker(this, selection.sensorOrientation, selection.facingFront)
                    .also { it.enable() }
                orientationTracker = tracker
                video = VideoEncoder(
                    segmentDir, selection.size, VIDEO_BITRATE, FRAME_RATE, SEGMENT_DURATION_US, videoSeq, ::onSegment,
                    rotationProvider = { tracker.orientationHint() },
                ).also { it.start() }
            }

            // One mic, fanned out: the AAC segment encoder always, plus the voice
            // assistant when enabled. Both consume the same 16 kHz mono PCM.
            val audioEnc = AudioEncoder(
                segmentDir, AUDIO_SAMPLE_RATE, AUDIO_CHANNELS, AUDIO_BITRATE, SEGMENT_DURATION_US, audioSeq, ::onSegment,
            ).also { it.start() }
            audio = audioEnc
            val sinks = mutableListOf<PcmSink>(audioEnc)
            if (settings.assistantEnabledBlocking()) {
                val a = buildAssistant()
                assistant = a
                sinks.add(a)
                a.start()
                AssistantBus.update { it.copy(enabled = true) }
            }
            micSource = MicSource(AUDIO_SAMPLE_RATE, AUDIO_CHANNELS, sinks).also { it.start() }

            if (selection != null) {
                camera = CameraController(this, selection.cameraId, video!!.inputSurface).also {
                    it.start()
                    // If the Activity is already in the foreground and handed us a preview
                    // surface before capture began, wire it in now (idempotent).
                    previewSurface?.let { s -> it.setPreviewSurface(s) }
                }
            }

            val summary = if (audioOnly) "audio only" else "capturing ${selection!!.size.width}x${selection.size.height}"
            captureSummary = summary
            CaptureNotification.update(this, summary)
            HushaiLog.info(
                "capture started device=$deviceId audioOnly=$audioOnly " +
                    (selection?.let { "video ${it.size}" } ?: "(no video)"),
            )
        } catch (e: Exception) {
            HushaiLog.error("startCapture failed — tearing down partial pipeline", e)
            runCatching { camera?.stop() }; camera = null
            runCatching { micSource?.stop() }; micSource = null
            runCatching { video?.stop() }; video = null
            runCatching { orientationTracker?.disable() }; orientationTracker = null
            runCatching { audio?.stop() }; audio = null
            runCatching { assistant?.stop() }; assistant = null
            running = false
            AssistantBus.update { it.copy(enabled = false) }
            StatusBus.update { it.copy(running = false, lastError = e.message ?: "capture failed to start") }
            maybeStopDelivery()
        }
    }

    /**
     * Idempotently bring up the delivery context shared by live capture AND imports:
     * the wake lock, uploader, durable buffer (with crash recovery), connectivity
     * state, and the drain thread. Safe to call repeatedly; a no-op if already up.
     * Runs on the lifecycle thread, so the recovery scan never races a live offer().
     */
    private fun ensureDeliveryRunning(url: String, token: String, identity: DeviceIdentity) {
        if (delivering) return
        acquireWakeLock()
        uploader = Uploader(Http.upload, url, token)

        // Durable store under noBackupFilesDir — NOT cacheDir, which the OS can purge
        // mid-outage. Encoders/import write scratch bodies into segments/incoming;
        // offer() renames each to <segmentId>.mp4 + a sidecar in segments/.
        val segmentRoot = File(noBackupFilesDir, "segments").apply { mkdirs() }
        File(segmentRoot, "incoming").mkdirs()
        val quarantineDir = File(noBackupFilesDir, "quarantine")
        val diskCap = settings.diskCapBytesBlocking()
        importWatermarkBytes = (diskCap.toDouble() * IMPORT_WATERMARK_FRACTION).toLong()
        val buf = DurableSegmentBuffer(
            segmentDir = segmentRoot,
            quarantineDir = quarantineDir,
            maxBytes = diskCap,
            minFreeBytesFloor = MIN_FREE_BYTES_FLOOR,
            identity = identity,
        )
        buffer = buf
        StatusBus.update { it.copy(diskCapBytes = diskCap) }

        // Rebuild the queue from any segments left by a prior process (crash, reboot,
        // or a clean stop with undelivered footage) BEFORE the drain thread starts.
        val recovered = buf.recover()
        if (recovered > 0) HushaiLog.info("recovered $recovered buffered segment(s) for replay")
        StatusBus.update {
            it.copy(
                pending = buf.size(),
                bufferedBytes = buf.byteSize(),
                diskFreeBytes = buf.freeBytes(),
                oldestBufferedUnixNanos = buf.oldestUnixNanos(),
            )
        }

        // Connectivity awareness: a NetworkCallback for instant link up/down, fused
        // with real upload outcomes + a probe into a single offline/draining state.
        val conn = ConnectivityState(
            reachability = Reachability(Http.probe, url),
            onWake = { uploadThread?.interrupt() },
            onStateChange = { refreshNotification() },
        )
        connectivity = conn
        val monitor = NetworkMonitor(
            applicationContext,
            onAvailable = { conn.onLinkUp() },
            onLost = { conn.onLinkDown() },
        )
        networkMonitor = monitor
        monitor.start()
        conn.start(initialOnline = monitor.online)
        conn.onBufferSizeChanged(buf.size())

        delivering = true
        uploadThread = thread(name = "hushai-uploader") { drainLoop() }
    }

    /** Tear down the delivery context once neither capture nor an import needs it. */
    private fun maybeStopDelivery() {
        if (running || importActive) return
        if (!delivering) {
            releaseWakeLock()
            return
        }
        delivering = false
        runCatching { connectivity?.stop() }; connectivity = null
        runCatching { networkMonitor?.stop() }; networkMonitor = null
        runCatching { uploader?.cancelInFlight() }

        // Best-effort flush; undelivered segments are durable + re-sent next start.
        val buf = buffer
        val deadline = SystemClock.elapsedRealtime() + FLUSH_TIMEOUT_MS
        while ((buf?.size() ?: 0) > 0 && SystemClock.elapsedRealtime() < deadline) {
            try {
                Thread.sleep(100)
            } catch (e: InterruptedException) {
                Thread.currentThread().interrupt()
                break
            }
        }
        runCatching { uploadThread?.interrupt() }
        runCatching { uploadThread?.join(2_000) }
        uploadThread = null
        uploader = null
        releaseWakeLock()
        StatusBus.reset()
        HushaiLog.info("delivery stopped; ${buf?.size() ?: 0} segment(s) buffered (durable, resume next start)")
    }

    // --- Manual file import (runs on the lifecycle thread unless noted) ---

    private fun handleImport(uriStrings: List<String>) {
        val uris = uriStrings.mapNotNull { runCatching { Uri.parse(it) }.getOrNull() }
        if (uris.isEmpty()) {
            if (!running && !importActive) { stopForeground(STOP_FOREGROUND_REMOVE); stopSelf() }
            return
        }
        // Re-take the persistable read grant defensively (survives OS redelivery).
        for (u in uris) runCatching {
            contentResolver.takePersistableUriPermission(u, Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        val url = settings.urlBlocking()
        val token = settings.tokenBlocking()
        val deviceId = settings.deviceIdBlocking()
        ensureDeliveryRunning(url, token, DeviceIdentity(deviceId, com.hushai.android.util.Uuid7.bytes()))
        val mgr = ensureImportManager()
        val requests = uris.map { u ->
            ImportRequest(u, displayName(u), importStreamCounter.getAndIncrement().toInt())
        }
        mgr.enqueue(requests)
    }

    private fun ensureImportManager(): ImportManager {
        importManager?.let { return it }
        val incoming = File(File(noBackupFilesDir, "segments"), "incoming").apply { mkdirs() }
        val mgr = ImportManager(
            context = applicationContext,
            segmentDir = incoming,
            segmentDurationUs = SEGMENT_DURATION_US,
            onSegment = ::onSegment,
            backpressure = ::importBackpressure,
            publish = { transform -> StatusBus.update(transform) },
            onActiveChange = { active -> lifecycle.submit { onImportActiveChange(active) } },
        )
        importManager = mgr
        return mgr
    }

    /** Block the import worker while the buffer is near the cap, so a large import
     *  never outruns the uploader and evicts live footage (drop-oldest). */
    private fun importBackpressure() {
        val buf = buffer ?: return
        while (buf.byteSize() > importWatermarkBytes && delivering) {
            try {
                Thread.sleep(IMPORT_BACKPRESSURE_POLL_MS)
            } catch (e: InterruptedException) {
                Thread.currentThread().interrupt()
                return
            }
        }
    }

    private fun onImportActiveChange(active: Boolean) {
        importActive = active
        if (active) {
            if (!running) {
                ServiceCompat.startForeground(
                    this,
                    CaptureNotification.NOTIFICATION_ID,
                    CaptureNotification.build(this, "Importing…"),
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
                )
            }
            refreshNotification()
        } else if (!running) {
            // Import finished and capture isn't running: tear everything down.
            maybeStopDelivery()
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
        } else {
            refreshNotification()
        }
    }

    private fun displayName(uri: Uri): String = runCatching {
        contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst() && !c.isNull(0)) c.getString(0) else null
        }
    }.getOrNull() ?: (uri.lastPathSegment ?: "file")

    /** Called from encoder (and import) threads when a segment is finalized. */
    private fun onSegment(segment: Segment) {
        val buffer = this.buffer ?: return
        val withGap = if (buffer.consumeGap(segment.streamId)) segment.copy(gapBefore = true) else segment
        val result = buffer.offer(withGap)
        val isVideo = withGap.streamId == VideoEncoder.STREAM_ID
        val isImport = withGap.streamId.startsWith(IMPORT_STREAM_PREFIX)
        val droppedNow = result is DurableSegmentBuffer.OfferResult.DroppedSelf
        val evicted = droppedNow ||
            (result is DurableSegmentBuffer.OfferResult.Stored && result.evicted > 0)
        StatusBus.update {
            it.copy(
                videoSeq = if (isVideo) withGap.sequence else it.videoSeq,
                audioSeq = if (!isVideo && !isImport) withGap.sequence else it.audioSeq,
                droppedToOverflow = if (droppedNow) it.droppedToOverflow + 1 else it.droppedToOverflow,
                overflowing = evicted,
            ).withBufferGauges(buffer)
        }
        connectivity?.onBufferSizeChanged(buffer.size())
    }

    /** Apply the live buffer/disk gauges onto a status snapshot. */
    private fun CaptureStatus.withBufferGauges(buffer: DurableSegmentBuffer): CaptureStatus =
        copy(
            pending = buffer.size(),
            bufferedBytes = buffer.byteSize(),
            diskFreeBytes = buffer.freeBytes(),
            oldestBufferedUnixNanos = buffer.oldestUnixNanos(),
        )

    /** Sleep that returns false if interrupted (a reconnect wake or teardown). On a
     *  real teardown the outer `while (running)` gate still exits the loop. */
    private fun sleepInterruptible(ms: Long): Boolean = try {
        Thread.sleep(ms)
        true
    } catch (e: InterruptedException) {
        Thread.interrupted() // clear the flag so the next sleep isn't pre-interrupted
        false
    }

    private fun drainLoop() {
        val buffer = this.buffer ?: return
        var backoffMs = INITIAL_BACKOFF_MS
        val resendAttempts = HashMap<ByteString, Int>()

        while (delivering) {
            val entry = buffer.peek()
            if (entry == null) {
                sleepInterruptible(IDLE_POLL_MS)
                continue
            }
            // Upload the manifest persisted at offer() time, VERBATIM — no rebuild.
            val outcome = uploader?.upload(entry.manifestBytes, entry.body)
                ?: UploadOutcome.RetryLater("no uploader")
            val sha = entry.contentSha256.hex()

            when (outcome) {
                is UploadOutcome.Accepted -> {
                    buffer.remove(entry)
                    resendAttempts.remove(entry.segmentId)
                    HushaiLog.tx(entry.streamId, entry.sequence, entry.byteLen, sha, "200")
                    StatusBus.update { it.copy(accepted = it.accepted + 1, lastError = null).withBufferGauges(buffer) }
                    backoffMs = INITIAL_BACKOFF_MS
                    connectivity?.onBufferSizeChanged(buffer.size())
                    connectivity?.onUploadSuccess()
                }
                is UploadOutcome.PermanentClientError -> {
                    HushaiLog.warn("PERMANENT ${outcome.code} stream=${entry.streamId} seq=${entry.sequence} — quarantining (${outcome.reason})")
                    HushaiLog.tx(entry.streamId, entry.sequence, entry.byteLen, sha, outcome.code.toString())
                    buffer.quarantine(entry)
                    StatusBus.update { it.copy(lastError = "client error ${outcome.code}").withBufferGauges(buffer) }
                    connectivity?.onBufferSizeChanged(buffer.size())
                }
                is UploadOutcome.Resend -> {
                    val n = (resendAttempts[entry.segmentId] ?: 0) + 1
                    resendAttempts[entry.segmentId] = n
                    HushaiLog.tx(entry.streamId, entry.sequence, entry.byteLen, sha, "422")
                    if (n > MAX_RESEND_ATTEMPTS) {
                        HushaiLog.warn("422 persisted ${entry.streamId} seq=${entry.sequence} after $n tries — quarantining")
                        buffer.quarantine(entry)
                        StatusBus.update { it.copy(lastError = "integrity 422").withBufferGauges(buffer) }
                        connectivity?.onBufferSizeChanged(buffer.size())
                    } else {
                        StatusBus.update { it.copy(lastError = "integrity 422 (retry $n)") }
                        sleepInterruptible(RESEND_BACKOFF_MS)
                    }
                }
                is UploadOutcome.Unauthorized -> {
                    HushaiLog.warn("401 unauthorized — retaining, awaiting valid token")
                    StatusBus.update { it.copy(lastError = "401 — check token") }
                    connectivity?.onUploadFailure()
                    backoffMs = if (sleepInterruptible(backoffMs)) (backoffMs * 2).coerceAtMost(MAX_BACKOFF_MS) else INITIAL_BACKOFF_MS
                }
                is UploadOutcome.RetryLater -> {
                    StatusBus.update { it.copy(lastError = "retry: ${outcome.reason}").withBufferGauges(buffer) }
                    connectivity?.onUploadFailure()
                    backoffMs = if (sleepInterruptible(backoffMs)) (backoffMs * 2).coerceAtMost(MAX_BACKOFF_MS) else INITIAL_BACKOFF_MS
                }
            }
        }
    }

    /** Recompose the persistent notification to reflect delivery/import state. Cheap;
     *  the channel is IMPORTANCE_LOW + alert-once so there's no sound/heads-up churn. */
    private fun refreshNotification() {
        if (!running) return
        val s = StatusBus.state.value
        val text = when {
            s.importing -> "Importing ${s.importName ?: "file"} — ${s.importDone}/${s.importTotal}"
            s.offline -> "Storing locally (offline) — ${formatBytes(s.bufferedBytes)} buffered"
            s.draining -> "Reconnected — uploading backlog (${(s.backlogTotal - s.pending).coerceAtLeast(0)} of ${s.backlogTotal})"
            else -> captureSummary.ifEmpty { if (s.audioOnly) "audio only" else "capturing" }
        }
        runCatching { CaptureNotification.update(this, text) }
    }

    private fun stopCapture() {
        if (!running && camera == null) return
        HushaiLog.info("stopping capture — finalizing in-flight segments")
        runCatching { alertNotifier?.stop() }; alertNotifier = null
        runCatching { camera?.stop() }; camera = null
        // Stop the mic FIRST so no more PCM is pushed, then it's safe to tear down
        // the consumers (encoder + assistant) without racing onPcm.
        runCatching { micSource?.stop() }; micSource = null
        runCatching { video?.stop() }; video = null
        runCatching { orientationTracker?.disable() }; orientationTracker = null
        runCatching { audio?.stop() }; audio = null
        runCatching { assistant?.stop() }; assistant = null
        running = false
        // If an import is still running, demote the foreground type from
        // camera|microphone to dataSync (we no longer hold the camera/mic).
        if (importActive) {
            runCatching {
                ServiceCompat.startForeground(
                    this,
                    CaptureNotification.NOTIFICATION_ID,
                    CaptureNotification.build(this, "Importing…"),
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
                )
            }
            refreshNotification()
        }
        // Tear down the shared delivery context unless an import still needs it.
        maybeStopDelivery()
    }

    override fun onDestroy() {
        // System-initiated destroy: we must finish teardown (camera/mic still held),
        // but bound the only main-thread block (Android offers no async onDestroy).
        desiredRunning = false
        runCatching { importManager?.shutdown() }
        importActive = false
        runCatching { connectivity?.stop() }
        runCatching { networkMonitor?.stop() }
        runCatching { uploader?.cancelInFlight() }
        val task = lifecycle.submit { runCatching { stopCapture(); maybeStopDelivery() } }
        runCatching { task.get(ONDESTROY_JOIN_MS, TimeUnit.MILLISECONDS) }
        lifecycle.shutdownNow()
        super.onDestroy()
    }

    private fun acquireWakeLock() {
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "hushai:capture").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    private fun releaseWakeLock() {
        runCatching { if (wakeLock?.isHeld == true) wakeLock?.release() }
        wakeLock = null
    }

    companion object {
        const val ACTION_STOP = "com.hushai.android.action.STOP"
        const val ACTION_IMPORT = "com.hushai.android.action.IMPORT"
        const val EXTRA_URL = "url"
        const val EXTRA_TOKEN = "token"
        const val EXTRA_RAG_URL = "rag_url"
        const val EXTRA_RAG_TOKEN = "rag_token"
        const val EXTRA_AUDIO_ONLY = "audio_only"
        const val EXTRA_IMPORT_URIS = "import_uris"

        /**
         * The foreground-service type to declare at startForeground(). Audio-only
         * narrows to MICROPHONE so the service neither needs nor claims the camera
         * type/permission; the normal path keeps CAMERA|MICROPHONE. Both are a
         * subset of the manifest's declared `camera|microphone` (required on 14+).
         */
        fun foregroundServiceType(audioOnly: Boolean): Int =
            if (audioOnly) {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
            } else {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA or
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
            }

        private const val SEGMENT_DURATION_US = 2_000_000L
        private const val VIDEO_BITRATE = 4_000_000
        private const val FRAME_RATE = 30
        // 16 kHz mono: what Vosk + whisper both want, fed to the AAC encoder and the
        // voice assistant from one shared mic. (ffmpeg in the worker resamples anyway.)
        private const val AUDIO_SAMPLE_RATE = 16_000
        private const val AUDIO_CHANNELS = 1
        private const val AUDIO_BITRATE = 96_000

        // Keep at least this much free on the device regardless of the user's cap, so
        // the offline buffer can never fill the phone. Enforced alongside the cap.
        private const val MIN_FREE_BYTES_FLOOR = 500L * 1024 * 1024 // 500 MB
        // Stream-id prefix for manually imported files (distinct lane from cam0-*).
        const val IMPORT_STREAM_PREFIX = "import-"
        // Throttle imports below the hard cap so they never evict live footage.
        private const val IMPORT_WATERMARK_FRACTION = 0.75
        private const val IMPORT_BACKPRESSURE_POLL_MS = 250L
        private const val IDLE_POLL_MS = 200L
        private const val INITIAL_BACKOFF_MS = 1_000L
        private const val MAX_BACKOFF_MS = 30_000L
        private const val RESEND_BACKOFF_MS = 500L
        private const val MAX_RESEND_ATTEMPTS = 5
        // Teardown runs off the main thread now, so this no longer freezes the UI; keep
        // it short so a normal Stop finalizes quickly. Undelivered segments stay buffered
        // on disk and re-send next session (segment_id is the idempotency key).
        private const val FLUSH_TIMEOUT_MS = 3_000L
        // Upper bound on the only place teardown can still block the main thread.
        private const val ONDESTROY_JOIN_MS = 8_000L

        fun startIntent(context: Context, url: String?, token: String?, audioOnly: Boolean): Intent =
            Intent(context, CaptureService::class.java).apply {
                url?.let { putExtra(EXTRA_URL, it) }
                token?.let { putExtra(EXTRA_TOKEN, it) }
                putExtra(EXTRA_AUDIO_ONLY, audioOnly)
            }

        fun stopIntent(context: Context): Intent =
            Intent(context, CaptureService::class.java).apply { action = ACTION_STOP }

        /** Queue one or more picked files for import. The caller must already hold a
         *  (persistable) read grant on each URI (SAF OpenDocument provides this). */
        fun importIntent(context: Context, uris: List<Uri>): Intent =
            Intent(context, CaptureService::class.java).apply {
                action = ACTION_IMPORT
                putStringArrayListExtra(EXTRA_IMPORT_URIS, ArrayList(uris.map { it.toString() }))
            }
    }
}
