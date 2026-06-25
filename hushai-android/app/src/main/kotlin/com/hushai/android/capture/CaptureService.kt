package com.hushai.android.capture

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Binder
import android.os.IBinder
import android.os.PowerManager
import android.os.SystemClock
import android.view.Surface
import androidx.core.app.ServiceCompat
import com.hushai.android.assistant.SpeakerMath
import com.hushai.android.assistant.VoiceAssistant
import com.hushai.android.config.DeviceIdentity
import com.hushai.android.config.Settings
import com.hushai.android.net.Http
import com.hushai.android.net.RagClient
import com.hushai.android.net.Reachability
import com.hushai.android.net.UploadOutcome
import com.hushai.android.net.Uploader
import com.hushai.android.util.AssistantBus
import com.hushai.android.util.CaptureStatus
import com.hushai.android.util.HushaiLog
import com.hushai.android.util.StatusBus
import okio.ByteString
import java.io.File
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
    private lateinit var buffer: RetryBuffer

    // These are written by the hushai-start capture thread and read by the main
    // thread (binder preview calls; stopCapture/releaseWakeLock from ACTION_STOP &
    // onDestroy), so they must be @Volatile for cross-thread visibility — otherwise
    // attachPreview can see a stale-null camera and silently drop the preview, and
    // stopCapture can miss cleanup. (settings/buffer are set in onCreate, which
    // happens-before onStartCommand per the Android lifecycle, so they need none.)
    @Volatile private var uploader: Uploader? = null
    @Volatile private var camera: CameraController? = null
    @Volatile private var video: VideoEncoder? = null
    @Volatile private var audio: AudioEncoder? = null
    @Volatile private var micSource: MicSource? = null
    @Volatile private var assistant: VoiceAssistant? = null
    @Volatile private var wakeLock: PowerManager.WakeLock? = null
    @Volatile private var uploadThread: Thread? = null

    private val videoSeq = AtomicLong(0)
    private val audioSeq = AtomicLong(0)
    @Volatile private var running = false

    // On-screen preview surface handed in by the foreground Activity (may be null
    // when the app is backgrounded / in battery-saver). Remembered here so it can
    // be (re)applied whenever the camera is (re)created. Touched from the main
    // thread (binder calls) and the capture thread (startCapture) -> @Volatile.
    @Volatile private var previewSurface: Surface? = null

    private val binder = LocalBinder()

    /** Lets the bound Activity feed the preview surface + drive the voice assistant. */
    inner class LocalBinder : Binder() {
        fun attachPreview(surface: Surface) = this@CaptureService.attachPreview(surface)
        fun detachPreview() = this@CaptureService.detachPreview()
        val isCapturing: Boolean get() = running
        fun setWakeWord(word: String) = this@CaptureService.setWakeWord(word)
        fun setAssistantEnabled(enabled: Boolean) = this@CaptureService.setAssistantEnabled(enabled)
        fun enrollOwner() { assistant?.startEnrollment() }
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
        if (enabled) {
            val mic = micSource
            if (assistant == null && mic != null && running) {
                thread(name = "hushai-va-enable") {
                    val a = buildAssistant()
                    assistant = a
                    a.start()        // start (running=true) BEFORE the mic can call onPcm on it
                    mic.addSink(a)
                }
            }
        } else {
            val a = assistant
            assistant = null
            if (a != null) thread(name = "hushai-va-disable") { micSource?.removeSink(a); a.stop() }
        }
    }

    private fun buildAssistant(): VoiceAssistant {
        val ragClient = RagClient(Http.rag, settings.ragUrlBlocking(), RAG_TOKEN)
        val owner = SpeakerMath.parse(settings.ownerEmbeddingBlocking())
        return VoiceAssistant(
            context = applicationContext,
            deviceId = settings.deviceIdBlocking(),
            initialWakeWord = settings.wakeWordBlocking(),
            ragClient = ragClient,
            initialOwnerEmbedding = owner,
            onEnrollComplete = { emb -> settings.setOwnerEmbeddingBlocking(SpeakerMath.format(emb)) },
        )
    }

    override fun onCreate() {
        super.onCreate()
        settings = Settings(applicationContext)
        buffer = RetryBuffer(MAX_BUFFER_SEGMENTS, File(cacheDir, "quarantine"))
        CaptureNotification.ensureChannel(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopCapture()
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            return START_NOT_STICKY
        }

        // Enter the foreground promptly (FGS start window), then configure + start
        // capture off the main thread (DataStore reads, camera open).
        startForegroundTyped("starting…")
        if (running) return START_STICKY

        val url = intent?.getStringExtra(EXTRA_URL)
        val token = intent?.getStringExtra(EXTRA_TOKEN)
        thread(name = "hushai-start") { startCapture(url, token) }
        return START_STICKY
    }

    private fun startForegroundTyped(text: String) {
        ServiceCompat.startForeground(
            this,
            CaptureNotification.NOTIFICATION_ID,
            CaptureNotification.build(this, text),
            ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA or
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE,
        )
    }

    private fun startCapture(urlOverride: String?, tokenOverride: String?) {
        val url = (urlOverride ?: settings.urlBlocking()).also {
            if (urlOverride != null) settings.setUrlBlocking(urlOverride)
        }
        val token = (tokenOverride ?: settings.tokenBlocking()).also {
            if (tokenOverride != null) settings.setTokenBlocking(tokenOverride)
        }
        val deviceId = settings.deviceIdBlocking()
        val identity = DeviceIdentity(deviceId = deviceId, sessionId = com.hushai.android.util.Uuid7.bytes())

        acquireWakeLock()
        uploader = Uploader(Http.upload, url, token)
        running = true
        StatusBus.update { CaptureStatus(running = true) }

        // Preflight reachability (non-fatal — capture still buffers if unreachable).
        thread(name = "hushai-preflight") {
            val health = Reachability(Http.probe, url).check()
            StatusBus.update {
                it.copy(reachable = health.reachable, live = health.live, ready = health.ready)
            }
            HushaiLog.info("preflight ${health.detail} url=$url")
        }

        uploadThread = thread(name = "hushai-uploader") { drainLoop(identity) }

        val selection = CameraController.select(this)
        if (selection == null) {
            HushaiLog.error("no camera available")
            StatusBus.update { it.copy(lastError = "no camera") }
            return
        }
        val segmentDir = File(cacheDir, "segments").apply { mkdirs() }

        video = VideoEncoder(
            segmentDir, selection.size, VIDEO_BITRATE, FRAME_RATE, SEGMENT_DURATION_US, videoSeq, ::onSegment,
        ).also { it.start() }

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

        camera = CameraController(this, selection.cameraId, video!!.inputSurface).also {
            it.start()
            // If the Activity is already in the foreground and handed us a preview
            // surface before capture began, wire it in now (idempotent).
            previewSurface?.let { s -> it.setPreviewSurface(s) }
        }

        CaptureNotification.update(this, "capturing ${selection.size.width}x${selection.size.height}")
        HushaiLog.info("capture started device=$deviceId stream sizes ${selection.size}")
    }

    /** Called from encoder threads when a segment is finalized. */
    private fun onSegment(segment: Segment) {
        val withGap = if (buffer.consumeGap(segment.streamId)) segment.copy(gapBefore = true) else segment
        buffer.offer(withGap)
        StatusBus.update {
            val isVideo = withGap.streamId == VideoEncoder.STREAM_ID
            it.copy(
                videoSeq = if (isVideo) withGap.sequence else it.videoSeq,
                audioSeq = if (!isVideo) withGap.sequence else it.audioSeq,
                pending = buffer.size(),
            )
        }
    }

    private fun drainLoop(identity: DeviceIdentity) {
        var backoffMs = INITIAL_BACKOFF_MS
        val resendAttempts = HashMap<ByteString, Int>()

        while (running) {
            val segment = buffer.peek()
            if (segment == null) {
                Thread.sleep(IDLE_POLL_MS)
                continue
            }
            val manifest = SegmentManifestBuilder.build(segment, identity)
            val outcome = uploader?.upload(manifest, segment.file) ?: UploadOutcome.RetryLater("no uploader")
            val sha = segment.contentSha256.hex()

            when (outcome) {
                is UploadOutcome.Accepted -> {
                    buffer.remove(segment)
                    resendAttempts.remove(segment.segmentId)
                    HushaiLog.tx(segment.streamId, segment.sequence, segment.byteLen, sha, "200")
                    StatusBus.update { it.copy(accepted = it.accepted + 1, pending = buffer.size(), lastError = null) }
                    backoffMs = INITIAL_BACKOFF_MS
                }
                is UploadOutcome.PermanentClientError -> {
                    HushaiLog.warn("PERMANENT ${outcome.code} stream=${segment.streamId} seq=${segment.sequence} — quarantining (${outcome.reason})")
                    HushaiLog.tx(segment.streamId, segment.sequence, segment.byteLen, sha, outcome.code.toString())
                    buffer.quarantine(segment)
                    StatusBus.update { it.copy(pending = buffer.size(), lastError = "client error ${outcome.code}") }
                }
                is UploadOutcome.Resend -> {
                    val n = (resendAttempts[segment.segmentId] ?: 0) + 1
                    resendAttempts[segment.segmentId] = n
                    HushaiLog.tx(segment.streamId, segment.sequence, segment.byteLen, sha, "422")
                    if (n > MAX_RESEND_ATTEMPTS) {
                        HushaiLog.warn("422 persisted ${segment.streamId} seq=${segment.sequence} after $n tries — quarantining")
                        buffer.quarantine(segment)
                        StatusBus.update { it.copy(pending = buffer.size(), lastError = "integrity 422") }
                    } else {
                        StatusBus.update { it.copy(lastError = "integrity 422 (retry $n)") }
                        Thread.sleep(RESEND_BACKOFF_MS)
                    }
                }
                is UploadOutcome.Unauthorized -> {
                    HushaiLog.warn("401 unauthorized — retaining, awaiting valid token")
                    StatusBus.update { it.copy(lastError = "401 — check token") }
                    Thread.sleep(backoffMs)
                    backoffMs = (backoffMs * 2).coerceAtMost(MAX_BACKOFF_MS)
                }
                is UploadOutcome.RetryLater -> {
                    StatusBus.update { it.copy(lastError = "retry: ${outcome.reason}", pending = buffer.size()) }
                    Thread.sleep(backoffMs)
                    backoffMs = (backoffMs * 2).coerceAtMost(MAX_BACKOFF_MS)
                }
            }
        }
    }

    private fun stopCapture() {
        if (!running && camera == null) return
        HushaiLog.info("stopping capture — finalizing in-flight segments")
        runCatching { camera?.stop() }; camera = null
        // Stop the mic FIRST so no more PCM is pushed, then it's safe to tear down
        // the consumers (encoder + assistant) without racing onPcm.
        runCatching { micSource?.stop() }; micSource = null
        runCatching { video?.stop() }; video = null
        runCatching { audio?.stop() }; audio = null
        runCatching { assistant?.stop() }; assistant = null

        // Flush whatever is buffered within a deadline so the last segments deliver.
        val deadline = SystemClock.elapsedRealtime() + FLUSH_TIMEOUT_MS
        while (buffer.size() > 0 && SystemClock.elapsedRealtime() < deadline) {
            Thread.sleep(100)
        }
        running = false
        runCatching { uploadThread?.join(2_000) }
        uploadThread = null
        releaseWakeLock()
        StatusBus.reset()
        HushaiLog.info("capture stopped; ${buffer.size()} segment(s) still buffered")
    }

    override fun onDestroy() {
        stopCapture()
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
        const val EXTRA_URL = "url"
        const val EXTRA_TOKEN = "token"

        private const val SEGMENT_DURATION_US = 2_000_000L
        private const val VIDEO_BITRATE = 4_000_000
        private const val FRAME_RATE = 30
        // 16 kHz mono: what Vosk + whisper both want, fed to the AAC encoder and the
        // voice assistant from one shared mic. (ffmpeg in the worker resamples anyway.)
        private const val AUDIO_SAMPLE_RATE = 16_000
        private const val AUDIO_CHANNELS = 1
        private const val AUDIO_BITRATE = 96_000
        // The local hushai-rag dev server runs with no RAG_TOKEN, so no bearer is sent.
        private const val RAG_TOKEN = ""

        private const val MAX_BUFFER_SEGMENTS = 300 // ~5 min outage; overflow -> gap_before
        private const val IDLE_POLL_MS = 200L
        private const val INITIAL_BACKOFF_MS = 1_000L
        private const val MAX_BACKOFF_MS = 30_000L
        private const val RESEND_BACKOFF_MS = 500L
        private const val MAX_RESEND_ATTEMPTS = 5
        private const val FLUSH_TIMEOUT_MS = 10_000L

        fun startIntent(context: Context, url: String?, token: String?): Intent =
            Intent(context, CaptureService::class.java).apply {
                url?.let { putExtra(EXTRA_URL, it) }
                token?.let { putExtra(EXTRA_TOKEN, it) }
            }

        fun stopIntent(context: Context): Intent =
            Intent(context, CaptureService::class.java).apply { action = ACTION_STOP }
    }
}
