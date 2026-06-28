package com.hushai.android.ui

import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import com.hushai.android.net.Reachability
import com.hushai.android.util.AssistantBus
import com.hushai.android.util.AssistantPhase
import com.hushai.android.util.AssistantStatus
import com.hushai.android.util.CaptureStatus
import com.hushai.android.util.StatusBus
import com.hushai.android.util.formatBytes
import com.hushai.android.util.formatDurationMicros
import kotlinx.coroutines.launch

// Must match CaptureService.SEGMENT_DURATION_US — used to show buffered duration.
private const val SEGMENT_DURATION_US = 2_000_000L
private const val LOW_DISK_WARN_BYTES = 500L * 1024 * 1024

/**
 * The app's single Compose screen. Layout is a vertical scroll of grouped cards
 * (Uber-style: white surfaces, hairline borders, bold headings) drawing all color
 * from [com.hushai.android.ui.theme.HushaiTheme]. The hero is the master on/off
 * control at the top, which simply reuses [onStart]/[onStop].
 */
@Composable
fun CaptureScreen(
    initialUrl: String,
    initialToken: String,
    deviceId: String,
    initialWakeWord: String,
    initialRagUrl: String,
    initialAssistantEnabled: Boolean,
    initialAudioOnly: Boolean,
    initialDiskCapGb: Float,
    onOpenVoices: () -> Unit,
    onOpenPeople: () -> Unit,
    onStart: (url: String, token: String, audioOnly: Boolean) -> Unit,
    onAudioOnlyChange: (Boolean) -> Unit,
    onDiskCapChange: (gb: Float) -> Unit,
    onPickImport: () -> Unit,
    onCancelImport: () -> Unit,
    onStop: () -> Unit,
    onCheckConnection: suspend (url: String) -> Reachability.Health,
    onBatterySaver: () -> Unit,
    onAssistantEnabledChange: (Boolean) -> Unit,
    onWakeWordChange: (String) -> Unit,
    onRagUrlChange: (String) -> Unit,
    onEnroll: () -> Unit,
    onPreviewSurfaceAvailable: (Surface) -> Unit,
    onPreviewSurfaceLost: (Surface) -> Unit,
) {
    var url by remember { mutableStateOf(initialUrl) }
    var token by remember { mutableStateOf(initialToken) }
    var ragUrl by remember { mutableStateOf(initialRagUrl) }
    var audioOnly by remember { mutableStateOf(initialAudioOnly) }
    var debugExpanded by remember { mutableStateOf(false) }
    val status by StatusBus.state.collectAsState()
    val assistant by AssistantBus.state.collectAsState()

    // Connectivity gate: probing the backend before a start, and the "not connected"
    // dialog when it fails. dialogHealth == null with the dialog shown means "no URL set".
    val scope = rememberCoroutineScope()
    var checkingConnection by remember { mutableStateOf(false) }
    var showDisconnectedDialog by remember { mutableStateOf(false) }
    var dialogHealth by remember { mutableStateOf<Reachability.Health?>(null) }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Column {
            Text("Hushai", style = MaterialTheme.typography.headlineMedium)
            Text(
                "Always-on capture",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }

        // Master on/off — the single, unmistakable control. Driven by the real
        // capture state (StatusBus), so a failed start / denied permission leaves
        // the switch off. Turning ON first probes the backend: capture only starts
        // when it's reachable + healthy, otherwise the "not connected" dialog explains why.
        MasterControlCard(
            running = status.running,
            checking = checkingConnection,
            onToggle = onToggle@{ on ->
                if (!on) {
                    onStop()
                    return@onToggle
                }
                if (checkingConnection) return@onToggle // ignore re-taps mid-probe
                val target = url.trim()
                if (target.isEmpty()) {
                    dialogHealth = null
                    showDisconnectedDialog = true
                    return@onToggle
                }
                checkingConnection = true
                scope.launch {
                    val health = runCatching { onCheckConnection(target) }
                        .getOrDefault(Reachability.Health(false, false, false, "probe failed"))
                    checkingConnection = false
                    if (health.live) {
                        onStart(target, token.trim(), audioOnly)
                    } else {
                        dialogHealth = health
                        showDisconnectedDialog = true
                    }
                }
            },
        )

        // Delivery banner: the first thing under the on/off control — online vs.
        // "storing locally (offline)" vs. "uploading backlog", with disk + buffer facts.
        if (status.running) {
            DeliveryBanner(status)
        }

        // Live group: only meaningful while capturing.
        if (status.running) {
            SectionCard {
                if (status.audioOnly) {
                    Text("🎙  Audio only", style = MaterialTheme.typography.titleMedium)
                    Text(
                        "Video disabled — recording audio only.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                } else {
                    Text("Live preview", style = MaterialTheme.typography.titleMedium)
                    Spacer(Modifier.height(12.dp))
                    CameraPreview(
                        modifier = Modifier.fillMaxWidth().aspectRatio(16f / 9f),
                        onPreviewSurfaceAvailable = onPreviewSurfaceAvailable,
                        onPreviewSurfaceLost = onPreviewSurfaceLost,
                    )
                }
                Spacer(Modifier.height(16.dp))
                OutlinedButton(onClick = onBatterySaver, modifier = Modifier.fillMaxWidth()) {
                    Text("Battery saver (lock screen, keep capturing)")
                }
            }
        }

        // Capture mode is chosen before Start (disabled while capturing): audio-only
        // skips the camera + video stream to save storage, bandwidth, and battery.
        SectionCard {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Column(modifier = Modifier.weight(1f)) {
                    Text("Audio only", style = MaterialTheme.typography.titleMedium)
                    Text(
                        "Skip video — record only audio to save storage & battery.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                Switch(
                    checked = audioOnly,
                    enabled = !status.running,
                    onCheckedChange = { audioOnly = it; onAudioOnlyChange(it) },
                )
            }
            if (status.running) {
                Spacer(Modifier.height(4.dp))
                Text(
                    "Stop capture to change this.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }

        AssistantCard(
            status = assistant,
            initialWakeWord = initialWakeWord,
            initialEnabled = initialAssistantEnabled,
            capturing = status.running,
            onEnabledChange = onAssistantEnabledChange,
            onWakeWordChange = onWakeWordChange,
            onEnroll = onEnroll,
        )

        // Voices: review discovered speakers, name them, and merge duplicates.
        SectionCard {
            Text("Voices", style = MaterialTheme.typography.titleMedium)
            Text(
                "Review who's been heard, name a voice, and merge duplicates.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(12.dp))
            OutlinedButton(onClick = onOpenVoices, modifier = Modifier.fillMaxWidth()) {
                Text("Open Voices")
            }
        }

        // People: review faces seen on camera, name them, and merge duplicates.
        SectionCard {
            Text("People", style = MaterialTheme.typography.titleMedium)
            Text(
                "Review faces seen, name a person, and merge duplicates.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(12.dp))
            OutlinedButton(onClick = onOpenPeople, modifier = Modifier.fillMaxWidth()) {
                Text("Open People")
            }
        }

        // Manual import: pick existing audio/video files; they're converted to the
        // same segments and uploaded via the same store-and-forward path.
        ImportCard(status = status, onPickImport = onPickImport, onCancelImport = onCancelImport)

        DebugSection(
            expanded = debugExpanded,
            onToggle = { debugExpanded = !debugExpanded },
            url = url,
            onUrlChange = { url = it },
            token = token,
            onTokenChange = { token = it },
            ragUrl = ragUrl,
            onRagUrlChange = { ragUrl = it },
            onRagUrlCommit = { onRagUrlChange(ragUrl.trim()) },
            initialDiskCapGb = initialDiskCapGb,
            onDiskCapChange = onDiskCapChange,
            deviceId = deviceId,
            status = status,
        )
    }

    if (showDisconnectedDialog) {
        DisconnectedDialog(
            url = url.trim(),
            health = dialogHealth,
            onDismiss = { showDisconnectedDialog = false },
        )
    }
}

/**
 * Manual file import: pick existing audio/video, with live progress while importing.
 * Imports run even while capture is off (a background data-sync job), sharing the same
 * durable buffer + uploader, so they survive disconnection just like live footage.
 */
@Composable
private fun ImportCard(status: CaptureStatus, onPickImport: () -> Unit, onCancelImport: () -> Unit) {
    SectionCard {
        Text("Import audio / video", style = MaterialTheme.typography.titleMedium)
        Text(
            "Upload existing recordings — they're converted and queued like live capture.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(12.dp))
        if (status.importing) {
            Text(status.importName ?: "Importing…", style = MaterialTheme.typography.bodyMedium)
            Spacer(Modifier.height(6.dp))
            val total = status.importTotal.coerceAtLeast(1)
            val done = status.importDone.coerceIn(0, total)
            if (status.importTotal > 0) {
                LinearProgressIndicator(progress = { done.toFloat() / total.toFloat() }, modifier = Modifier.fillMaxWidth())
            } else {
                LinearProgressIndicator(modifier = Modifier.fillMaxWidth())
            }
            Spacer(Modifier.height(6.dp))
            line("Progress", if (status.importTotal > 0) "$done / ${status.importTotal} segments" else "starting…")
            if (status.importQueued > 0) line("Queued", "${status.importQueued} more file(s)")
            Spacer(Modifier.height(8.dp))
            OutlinedButton(onClick = onCancelImport, modifier = Modifier.fillMaxWidth()) { Text("Cancel import") }
        } else {
            OutlinedButton(onClick = onPickImport, modifier = Modifier.fillMaxWidth()) { Text("Choose files…") }
        }
        status.importError?.let {
            Spacer(Modifier.height(8.dp))
            Text("⚠ $it", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
        }
    }
}

/**
 * Always-visible delivery state while capturing: Online / OFFLINE-storing-locally /
 * Reconnected-uploading-backlog, plus buffered amount, free disk, and the local cap.
 * This is the user's at-a-glance answer to "is my footage getting off the device?"
 */
@Composable
private fun DeliveryBanner(status: CaptureStatus) {
    val accent = if (status.offline) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.tertiary
    val icon: String
    val title: String
    val subtitle: String
    when {
        status.draining -> {
            icon = "↑"
            title = "Reconnected — uploading backlog"
            subtitle = "${(status.backlogTotal - status.pending).coerceAtLeast(0)} of ${status.backlogTotal} sent"
        }
        status.offline -> {
            icon = "▲"
            title = "Offline — storing locally"
            subtitle = "Recording continues; footage is saved on this device and uploads when reconnected."
        }
        else -> {
            icon = "●"
            title = "Online — uploading live"
            subtitle = "Footage is streaming to the server."
        }
    }
    SectionCard {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Box(modifier = Modifier.size(12.dp).background(accent, CircleShape))
            Spacer(Modifier.size(12.dp))
            Column(modifier = Modifier.weight(1f)) {
                Text("$icon  $title", style = MaterialTheme.typography.titleMedium, color = accent)
                Text(
                    subtitle,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        if (status.draining && status.backlogTotal > 0) {
            Spacer(Modifier.height(10.dp))
            val total = status.backlogTotal.coerceAtLeast(1)
            val done = (status.backlogTotal - status.pending).coerceIn(0, total)
            LinearProgressIndicator(
                progress = { done.toFloat() / total.toFloat() },
                modifier = Modifier.fillMaxWidth(),
            )
        }
        Spacer(Modifier.height(10.dp))
        line(
            "Buffered",
            "${status.pending} segments · " +
                "${formatDurationMicros(status.pending * SEGMENT_DURATION_US)} · " +
                formatBytes(status.bufferedBytes),
        )
        line("Disk free", formatBytes(status.diskFreeBytes))
        if (status.diskCapBytes > 0) line("Local cap", formatBytes(status.diskCapBytes))
        LowStorageWarning(status)
    }
}

@Composable
private fun LowStorageWarning(status: CaptureStatus) {
    val capNear = status.diskCapBytes > 0 &&
        status.bufferedBytes >= (status.diskCapBytes.toDouble() * 0.9).toLong()
    val diskLow = status.diskFreeBytes in 1 until LOW_DISK_WARN_BYTES
    if (capNear || diskLow) {
        Spacer(Modifier.height(8.dp))
        Text(
            "⚠ Local storage almost full — oldest footage will be overwritten.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.error,
        )
    }
}

/**
 * Prominent capture on/off control: a status dot + label and a large switch. Green
 * when capturing, neutral when stopped. The switch state mirrors [running] (the
 * source of truth), not local state, so it never drifts from the actual service.
 */
@Composable
private fun MasterControlCard(running: Boolean, checking: Boolean, onToggle: (Boolean) -> Unit) {
    val dotColor =
        if (running) MaterialTheme.colorScheme.tertiary else MaterialTheme.colorScheme.onSurfaceVariant
    SectionCard {
        Row(verticalAlignment = Alignment.CenterVertically) {
            if (checking) {
                CircularProgressIndicator(
                    modifier = Modifier.size(16.dp),
                    strokeWidth = 2.dp,
                    color = MaterialTheme.colorScheme.primary,
                )
            } else {
                Box(modifier = Modifier.size(12.dp).background(dotColor, CircleShape))
            }
            Spacer(Modifier.size(12.dp))
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    when {
                        checking -> "Checking connection…"
                        running -> "Capturing"
                        else -> "Stopped"
                    },
                    style = MaterialTheme.typography.titleLarge,
                )
                Text(
                    when {
                        checking -> "Verifying the backend is reachable"
                        running -> "Tap to turn off"
                        else -> "Tap to turn on"
                    },
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Switch(
                checked = running,
                onCheckedChange = onToggle,
                enabled = !checking,
                colors = SwitchDefaults.colors(
                    checkedThumbColor = MaterialTheme.colorScheme.onTertiary,
                    checkedTrackColor = MaterialTheme.colorScheme.tertiary,
                    checkedBorderColor = MaterialTheme.colorScheme.tertiary,
                ),
            )
        }
    }
}

/**
 * Hard-block "not connected" dialog shown when the user tries to start capture while
 * the backend is unreachable/unhealthy (or no URL is set). Dismiss-only — capture
 * can't start without a connection, since it would otherwise only buffer locally.
 */
@Composable
private fun DisconnectedDialog(
    url: String,
    health: Reachability.Health?,
    onDismiss: () -> Unit,
) {
    val displayUrl = if (url.isEmpty()) "(no backend URL set)" else url
    val body = when {
        url.isEmpty() ->
            "No backend URL is set. Add one under Debug / Advanced before starting capture."
        health == null || !health.reachable ->
            "Can't reach the backend at $displayUrl. Capture needs a live connection — " +
                "without one nothing uploads and recording would only buffer locally."
        !health.live ->
            "The backend at $displayUrl responded but isn't healthy. Capture needs a " +
                "healthy backend before it can start."
        else ->
            "The backend at $displayUrl isn't ready right now."
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        shape = MaterialTheme.shapes.large,
        containerColor = MaterialTheme.colorScheme.surface,
        titleContentColor = MaterialTheme.colorScheme.error,
        textContentColor = MaterialTheme.colorScheme.onSurface,
        title = { Text("Not connected") },
        text = { Text(body, style = MaterialTheme.typography.bodyMedium) },
        confirmButton = {
            TextButton(onClick = onDismiss) {
                Text("OK", color = MaterialTheme.colorScheme.primary)
            }
        },
    )
}

@Composable
private fun AssistantCard(
    status: AssistantStatus,
    initialWakeWord: String,
    initialEnabled: Boolean,
    capturing: Boolean,
    onEnabledChange: (Boolean) -> Unit,
    onWakeWordChange: (String) -> Unit,
    onEnroll: () -> Unit,
) {
    var enabled by remember { mutableStateOf(initialEnabled) }
    var wakeWord by remember { mutableStateOf(initialWakeWord) }

    SectionCard {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                "Voice assistant",
                modifier = Modifier.weight(1f),
                style = MaterialTheme.typography.titleMedium,
            )
            Switch(checked = enabled, onCheckedChange = { enabled = it; onEnabledChange(it) })
        }

        if (enabled) {
            Spacer(Modifier.height(12.dp))
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(
                    value = wakeWord,
                    onValueChange = { wakeWord = it },
                    label = { Text("Wake word") },
                    singleLine = true,
                    modifier = Modifier.weight(1f),
                )
                Button(onClick = { onWakeWordChange(wakeWord.trim()) }) { Text("Set") }
            }
            Spacer(Modifier.height(8.dp))
            Button(
                onClick = onEnroll,
                enabled = capturing && status.ready && status.phase != AssistantPhase.ENROLLING,
                modifier = Modifier.fillMaxWidth(),
            ) {
                Text(if (status.enrolled) "Re-enroll my voice" else "Enroll my voice")
            }

            Spacer(Modifier.height(12.dp))
            line("State", phaseLabel(status))
            line("Models", if (status.ready) "ready" else "loading…")
            line("Owner", if (status.enrolled) "enrolled ✓" else "not enrolled")
            if (status.phase == AssistantPhase.ENROLLING) {
                line("Enrolling", "${status.enrollProgress}% — keep talking")
            }
            status.lastHeard?.let { line("Heard", it) }
            status.lastQuestion?.let { line("Question", it) }
            status.lastAnswer?.let { line("Answer", it) }
            status.note?.let { line("Note", it) }
            if (!capturing) {
                Spacer(Modifier.height(8.dp))
                Text(
                    "Turn on capture to begin listening.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}

private fun phaseLabel(status: AssistantStatus): String = when (status.phase) {
    AssistantPhase.OFF -> "off"
    AssistantPhase.LISTENING -> "listening for wake word"
    AssistantPhase.AWAIT_QUESTION -> "listening for your question"
    AssistantPhase.THINKING -> "thinking…"
    AssistantPhase.SPEAKING -> "speaking"
    AssistantPhase.ENROLLING -> "enrolling your voice"
}

/**
 * Renders the camera preview into a [SurfaceView] and hands its [Surface] up to
 * the caller (which forwards it to the capture service's camera).
 *
 * Loss is reported with the *specific* surface that died (not just "null") so the
 * caller can ignore a stale teardown: on a stop→start cycle a fresh SurfaceView is
 * created while the old one's surfaceDestroyed may still be pending, and a blind
 * null would detach the new, valid preview. Comparing identity prevents that.
 */
@Composable
private fun CameraPreview(
    modifier: Modifier,
    onPreviewSurfaceAvailable: (Surface) -> Unit,
    onPreviewSurfaceLost: (Surface) -> Unit,
) {
    AndroidView(
        modifier = modifier,
        factory = { ctx ->
            SurfaceView(ctx).apply {
                holder.addCallback(object : SurfaceHolder.Callback {
                    private var reported: Surface? = null

                    override fun surfaceCreated(holder: SurfaceHolder) {
                        reported = holder.surface
                        onPreviewSurfaceAvailable(holder.surface)
                    }

                    override fun surfaceChanged(holder: SurfaceHolder, f: Int, w: Int, h: Int) {
                        reported = holder.surface
                        onPreviewSurfaceAvailable(holder.surface)
                    }

                    override fun surfaceDestroyed(holder: SurfaceHolder) {
                        reported?.let { onPreviewSurfaceLost(it) }
                        reported = null
                    }
                })
            }
        },
    )
}

@Composable
private fun DebugSection(
    expanded: Boolean,
    onToggle: () -> Unit,
    url: String,
    onUrlChange: (String) -> Unit,
    token: String,
    onTokenChange: (String) -> Unit,
    ragUrl: String,
    onRagUrlChange: (String) -> Unit,
    onRagUrlCommit: () -> Unit,
    initialDiskCapGb: Float,
    onDiskCapChange: (gb: Float) -> Unit,
    deviceId: String,
    status: CaptureStatus,
) {
    var diskCapGb by remember {
        mutableStateOf(if (initialDiskCapGb > 0) initialDiskCapGb.toString() else "")
    }
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text(
            text = (if (expanded) "▾ " else "▸ ") + "Debug / Advanced",
            style = MaterialTheme.typography.titleSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier
                .fillMaxWidth()
                .clickable { onToggle() }
                .padding(vertical = 8.dp),
        )
        if (expanded) {
            OutlinedTextField(
                value = url,
                onValueChange = onUrlChange,
                label = { Text("Backend URL") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = token,
                onValueChange = onTokenChange,
                label = { Text("Device token") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(
                    value = ragUrl,
                    onValueChange = onRagUrlChange,
                    label = { Text("RAG URL") },
                    singleLine = true,
                    modifier = Modifier.weight(1f),
                )
                Button(onClick = onRagUrlCommit) { Text("Set") }
            }
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(
                    value = diskCapGb,
                    onValueChange = { diskCapGb = it },
                    label = { Text("Max local storage (GB)") },
                    singleLine = true,
                    modifier = Modifier.weight(1f),
                )
                Button(onClick = { diskCapGb.toFloatOrNull()?.let { onDiskCapChange(it) } }) { Text("Set") }
            }
            Text(
                "Offline footage is kept on this device up to this size, then the oldest is overwritten.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Text(
                "device_id: $deviceId",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            StatusCard(status)
        }
    }
}

@Composable
private fun StatusCard(status: CaptureStatus) {
    SectionCard {
        line("State", if (status.running) "capturing" else "stopped")
        line("Mode", if (status.audioOnly) "audio only" else "audio + video")
        line("Backend", reachability(status))
        line(
            "cam0-video seq",
            when {
                status.audioOnly -> "off"
                status.videoSeq >= 0 -> status.videoSeq.toString()
                else -> "—"
            },
        )
        line("cam0-audio seq", if (status.audioSeq >= 0) status.audioSeq.toString() else "—")
        line("Accepted (200)", status.accepted.toString())
        line("Buffered", "${status.pending} · ${formatBytes(status.bufferedBytes)}")
        line(
            "Delivery",
            when {
                status.draining -> "draining ${status.pending}/${status.backlogTotal}"
                status.offline -> "offline"
                else -> "online"
            },
        )
        line("Disk free", formatBytes(status.diskFreeBytes))
        if (status.droppedToOverflow > 0) line("Dropped (full)", status.droppedToOverflow.toString())
        status.lastError?.let { line("Note", it) }
    }
}

/** Shared card chrome: white surface, hairline border, rounded corners, inset padding. */
@Composable
private fun SectionCard(
    modifier: Modifier = Modifier,
    content: @Composable ColumnScope.() -> Unit,
) {
    OutlinedCard(modifier = modifier.fillMaxWidth(), shape = MaterialTheme.shapes.large) {
        Column(modifier = Modifier.padding(16.dp), content = content)
    }
}

@Composable
private fun line(label: String, value: String) {
    Row(modifier = Modifier.fillMaxWidth().padding(vertical = 2.dp)) {
        Text(
            label,
            modifier = Modifier.weight(1f),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Text(value, style = MaterialTheme.typography.bodyMedium)
    }
}

private fun reachability(status: CaptureStatus): String = when {
    !status.reachable -> "unreachable"
    status.live && status.ready -> "reachable + ready"
    status.live -> "reachable (not ready)"
    else -> "reachable"
}
