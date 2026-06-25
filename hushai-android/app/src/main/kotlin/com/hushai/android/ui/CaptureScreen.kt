package com.hushai.android.ui

import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import com.hushai.android.util.AssistantBus
import com.hushai.android.util.AssistantPhase
import com.hushai.android.util.AssistantStatus
import com.hushai.android.util.CaptureStatus
import com.hushai.android.util.StatusBus

@Composable
fun CaptureScreen(
    initialUrl: String,
    initialToken: String,
    deviceId: String,
    initialWakeWord: String,
    initialRagUrl: String,
    initialAssistantEnabled: Boolean,
    onStart: (url: String, token: String) -> Unit,
    onStop: () -> Unit,
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
    var debugExpanded by remember { mutableStateOf(false) }
    val status by StatusBus.state.collectAsState()
    val assistant by AssistantBus.state.collectAsState()

    Column(
        modifier = Modifier
            .padding(20.dp)
            .verticalScroll(rememberScrollState()),
    ) {
        Text("Hushai Capture", style = MaterialTheme.typography.headlineSmall)
        Spacer(Modifier.height(16.dp))

        Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            Button(onClick = { onStart(url.trim(), token.trim()) }, enabled = !status.running) {
                Text("Start")
            }
            OutlinedButton(onClick = onStop, enabled = status.running) {
                Text("Stop")
            }
        }

        if (status.running) {
            Spacer(Modifier.height(16.dp))
            CameraPreview(
                modifier = Modifier.fillMaxWidth().aspectRatio(16f / 9f),
                onPreviewSurfaceAvailable = onPreviewSurfaceAvailable,
                onPreviewSurfaceLost = onPreviewSurfaceLost,
            )
            Spacer(Modifier.height(12.dp))
            OutlinedButton(onClick = onBatterySaver, modifier = Modifier.fillMaxWidth()) {
                Text("Battery saver (lock screen, keep capturing)")
            }
        }

        Spacer(Modifier.height(20.dp))
        AssistantCard(
            status = assistant,
            initialWakeWord = initialWakeWord,
            initialEnabled = initialAssistantEnabled,
            capturing = status.running,
            onEnabledChange = onAssistantEnabledChange,
            onWakeWordChange = onWakeWordChange,
            onEnroll = onEnroll,
        )

        Spacer(Modifier.height(20.dp))
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
            deviceId = deviceId,
            status = status,
        )
    }
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

    Card(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(16.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text("Voice assistant", modifier = Modifier.weight(1f), style = MaterialTheme.typography.titleMedium)
                Switch(checked = enabled, onCheckedChange = { enabled = it; onEnabledChange(it) })
            }

            if (enabled) {
                Spacer(Modifier.height(8.dp))
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
                        "Press Start to begin listening.",
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
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
    deviceId: String,
    status: CaptureStatus,
) {
    Column {
        Text(
            text = (if (expanded) "▾ " else "▸ ") + "Debug / Advanced",
            style = MaterialTheme.typography.titleSmall,
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
            Spacer(Modifier.height(8.dp))
            OutlinedTextField(
                value = token,
                onValueChange = onTokenChange,
                label = { Text("Device token") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Spacer(Modifier.height(8.dp))
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
            Spacer(Modifier.height(8.dp))
            Text("device_id: $deviceId", style = MaterialTheme.typography.bodySmall)
            Spacer(Modifier.height(12.dp))
            StatusCard(status)
        }
    }
}

@Composable
private fun StatusCard(status: CaptureStatus) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(16.dp)) {
            line("State", if (status.running) "capturing" else "stopped")
            line("Backend", reachability(status))
            line("cam0-video seq", if (status.videoSeq >= 0) status.videoSeq.toString() else "—")
            line("cam0-audio seq", if (status.audioSeq >= 0) status.audioSeq.toString() else "—")
            line("Accepted (200)", status.accepted.toString())
            line("Buffered", status.pending.toString())
            status.lastError?.let { line("Note", it) }
        }
    }
}

@Composable
private fun line(label: String, value: String) {
    Row(modifier = Modifier.fillMaxWidth().padding(vertical = 2.dp)) {
        Text(label, modifier = Modifier.weight(1f), style = MaterialTheme.typography.bodyMedium)
        Text(value, style = MaterialTheme.typography.bodyMedium)
    }
}

private fun reachability(status: CaptureStatus): String = when {
    !status.reachable -> "unreachable"
    status.live && status.ready -> "reachable + ready"
    status.live -> "reachable (not ready)"
    else -> "reachable"
}
