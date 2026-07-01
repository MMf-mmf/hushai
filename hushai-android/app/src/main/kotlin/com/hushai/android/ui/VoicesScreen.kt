package com.hushai.android.ui

import android.content.Context
import android.media.MediaPlayer
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import com.hushai.android.net.SpeakersClient
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * The "Voices" screen: lists the server's discovered speakers, plays a sample-audio
 * snippet so the user can identify a voice by ear, lets them name a voice (PATCH) and
 * merge duplicate ids (POST). All network calls go through [SpeakersClient] (backend url +
 * token from Settings) off the main thread. Mirrors CaptureScreen's card chrome.
 */
@Composable
fun VoicesScreen(client: SpeakersClient, onBack: () -> Unit) {
    var speakers by remember { mutableStateOf<List<SpeakersClient.Speaker>>(emptyList()) }
    var dups by remember { mutableStateOf<List<SpeakersClient.DupGroup>>(emptyList()) }
    var unattr by remember { mutableStateOf<List<SpeakersClient.UnattributedCluster>>(emptyList()) }
    var loading by remember { mutableStateOf(true) }
    var error by remember { mutableStateOf(false) }
    // Known (named) voices collapse behind a closed disclosure so the unidentified ones lead.
    var knownExpanded by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val ctx = LocalContext.current

    suspend fun reload() {
        loading = true
        error = false
        withContext(Dispatchers.IO) {
            // null = the call failed (vs. an empty list = "no speakers yet"); surface a
            // retryable error instead of a misleading "no speakers" / stale list.
            val sp = client.listSpeakers()
            val dp = client.listDuplicates()
            val ua = client.listUnattributed()
            withContext(Dispatchers.Main) {
                if (sp == null) {
                    error = true
                } else {
                    speakers = sp
                    dups = dp ?: emptyList()
                    unattr = ua ?: emptyList()
                }
            }
        }
        loading = false
    }

    androidx.compose.runtime.LaunchedEffect(Unit) { reload() }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            TextButton(onClick = onBack) { Text("‹ Back") }
            Spacer(Modifier.width(8.dp))
            Text("Voices", style = MaterialTheme.typography.headlineMedium)
            Spacer(Modifier.weight(1f))
            TextButton(onClick = { scope.launch { reload() } }, enabled = !loading) {
                Text(if (loading) "Refreshing…" else "Refresh")
            }
        }
        Text(
            "Discovered speakers. Play a sample to recognize a voice, give it a name, or merge " +
                "duplicates of the same person.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )

        if (dups.isNotEmpty()) {
            CleanUpSection(
                dups = dups,
                onMergeGroup = { g ->
                    scope.launch {
                        val ok = withContext(Dispatchers.IO) {
                            client.mergeGroup(g.suggestedInto, g.members.map { it.id })
                        }
                        if (ok) reload()
                    }
                },
                onMergeAll = {
                    scope.launch {
                        val mergeable = dups.filter { !it.nameConflict }
                        withContext(Dispatchers.IO) {
                            mergeable.forEach { client.mergeGroup(it.suggestedInto, it.members.map { m -> m.id }) }
                        }
                        reload()
                    }
                },
                onPlay = { id -> playSample(ctx, client, id, scope) },
            )
        }

        when {
            loading -> Text("Loading…", style = MaterialTheme.typography.bodyMedium)
            error -> Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    "Couldn't reach the backend. Check the URL and token in Capture ▸ Debug, then retry.",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.error,
                )
                OutlinedButton(onClick = { scope.launch { reload() } }) { Text("Retry") }
            }
            speakers.isEmpty() -> Text(
                "No speakers discovered yet.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            else -> {
                val card: @Composable (SpeakersClient.Speaker) -> Unit = { sp ->
                    SpeakerCard(
                        speaker = sp,
                        others = speakers.filter { it.id != sp.id },
                        onSave = { name ->
                            scope.launch {
                                val ok = withContext(Dispatchers.IO) { client.setName(sp.id, name) }
                                if (ok) reload()
                            }
                        },
                        onPlay = { playSample(ctx, client, sp.id, scope) },
                        onMerge = { intoId ->
                            scope.launch {
                                val ok = withContext(Dispatchers.IO) { client.merge(sp.id, intoId) }
                                if (ok) reload()
                            }
                        },
                    )
                }
                // Identified (named) voices first so the known voices are visible at a glance,
                // then the ones still waiting to be named.
                val known = speakers.filter { !it.name.isNullOrBlank() }
                val unknown = speakers.filter { it.name.isNullOrBlank() }

                if (known.isEmpty()) {
                    Text("Known voices (0)", style = MaterialTheme.typography.titleMedium)
                    Text(
                        "No voices identified yet — name one below to build your known-voices list.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                } else {
                    Text(
                        "${if (knownExpanded) "▾" else "▸"} Known voices (${known.size})",
                        style = MaterialTheme.typography.titleMedium,
                        modifier = Modifier.clickable { knownExpanded = !knownExpanded },
                    )
                    if (knownExpanded) known.forEach { card(it) }
                }
                if (unknown.isNotEmpty()) {
                    Text("Unidentified voices (${unknown.size})", style = MaterialTheme.typography.titleMedium)
                    unknown.forEach { card(it) }
                }
            }
        }

        // Voices clustered from audio the matcher never linked to any speaker (chronically
        // marginal audio it refused to auto-create). Naming one mints it into the catalog.
        if (unattr.isNotEmpty()) {
            IdentifyNewVoicesSection(
                clusters = unattr,
                onName = { cluster, name ->
                    scope.launch {
                        val ok = withContext(Dispatchers.IO) {
                            client.nameUnattributed(name, cluster.segmentIds)
                        }
                        if (ok) reload()
                    }
                },
            )
        }
    }
}

/**
 * "Identify new voices" — candidate voices clustered from audio the matcher left
 * unattributed (no speaker row at all). These never show in the speakers list until named,
 * so this is the only way to give a chronically-marginal speaker a name. Naming one mints a
 * speaker and claims that cluster's segments.
 */
@Composable
private fun IdentifyNewVoicesSection(
    clusters: List<SpeakersClient.UnattributedCluster>,
    onName: (SpeakersClient.UnattributedCluster, String) -> Unit,
) {
    SectionCard {
        Text("Identify new voices", style = MaterialTheme.typography.titleMedium)
        Text(
            "${clusters.size} voice(s) heard in recordings but not yet linked to anyone. " +
                "Name one to add it to your voices.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
    clusters.forEach { c -> UnattributedClusterCard(cluster = c, onName = { name -> onName(c, name) }) }
}

@Composable
private fun UnattributedClusterCard(
    cluster: SpeakersClient.UnattributedCluster,
    onName: (String) -> Unit,
) {
    var name by remember(cluster.handle) { mutableStateOf("") }
    val confidence = if (cluster.maxDistance <= 0.2) "Very likely one person" else "Likely one person"

    SectionCard {
        Text("Unrecognized voice", style = MaterialTheme.typography.titleMedium)
        Text(
            "$confidence · ${cluster.nSegments} clip(s)",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        cluster.samples.forEach { Text("“$it”", style = MaterialTheme.typography.bodySmall) }

        Spacer(Modifier.height(12.dp))
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedTextField(
                value = name,
                onValueChange = { name = it },
                label = { Text("Name") },
                singleLine = true,
                modifier = Modifier.weight(1f),
            )
            Button(onClick = { onName(name.trim()) }, enabled = name.isNotBlank()) { Text("Save") }
        }
    }
}

@Composable
private fun SpeakerCard(
    speaker: SpeakersClient.Speaker,
    others: List<SpeakersClient.Speaker>,
    onSave: (String) -> Unit,
    onPlay: () -> Unit,
    onMerge: (String) -> Unit,
) {
    var name by remember(speaker.id) { mutableStateOf(speaker.name ?: "") }
    var mergeOpen by remember(speaker.id) { mutableStateOf(false) }

    SectionCard {
        Text(
            speaker.name ?: "Unknown speaker (${speaker.id.take(8)})",
            style = MaterialTheme.typography.titleMedium,
        )
        Text(
            "${speaker.nSamples} samples",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        speaker.samples.forEach {
            Text("“$it”", style = MaterialTheme.typography.bodySmall)
        }

        Spacer(Modifier.height(12.dp))
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedTextField(
                value = name,
                onValueChange = { name = it },
                label = { Text("Name") },
                singleLine = true,
                modifier = Modifier.weight(1f),
            )
            Button(onClick = { onSave(name.trim()) }, enabled = name.isNotBlank()) { Text("Save") }
        }

        Spacer(Modifier.height(8.dp))
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedButton(onClick = onPlay) { Text("Play sample") }
            if (others.isNotEmpty()) {
                Box {
                    OutlinedButton(onClick = { mergeOpen = true }) { Text("Merge into…") }
                    DropdownMenu(expanded = mergeOpen, onDismissRequest = { mergeOpen = false }) {
                        others.forEach { other ->
                            DropdownMenuItem(
                                text = { Text(other.name ?: "Unknown (${other.id.take(8)})") },
                                onClick = {
                                    mergeOpen = false
                                    onMerge(other.id)
                                },
                            )
                        }
                    }
                }
            }
        }
    }
}

/**
 * "Clean up voices" — surfaces the backend's suggested duplicate groups (the static-induced
 * over-splits) so the user merges them in one tap instead of one-by-one. "Merge all" folds
 * every non-conflicting group. Name-conflict groups are shown but not one-tap mergeable.
 */
@Composable
private fun CleanUpSection(
    dups: List<SpeakersClient.DupGroup>,
    onMergeGroup: (SpeakersClient.DupGroup) -> Unit,
    onMergeAll: () -> Unit,
    onPlay: (String) -> Unit,
) {
    val mergeableCount = dups.count { !it.nameConflict }
    SectionCard {
        Text("Clean up voices", style = MaterialTheme.typography.titleMedium)
        Text(
            "${dups.size} possible duplicate group(s) detected — these look like the same person " +
                "split across several voices. Review and merge.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        if (mergeableCount > 1) {
            Spacer(Modifier.height(8.dp))
            Button(onClick = onMergeAll) { Text("Merge all ($mergeableCount)") }
        }
    }
    dups.forEach { g -> DuplicateGroupCard(group = g, onMergeGroup = { onMergeGroup(g) }, onPlay = onPlay) }
}

@Composable
private fun DuplicateGroupCard(
    group: SpeakersClient.DupGroup,
    onMergeGroup: () -> Unit,
    onPlay: (String) -> Unit,
) {
    val survivor = group.members.firstOrNull { it.id == group.suggestedInto }
    val survivorLabel = survivor?.name ?: "Unknown (${group.suggestedInto.take(8)})"
    val confidence = if (group.maxDistance <= 0.2) "Very likely" else "Likely"

    SectionCard {
        Text("Possible duplicate of $survivorLabel", style = MaterialTheme.typography.titleMedium)
        Text(
            "$confidence the same person · ${group.members.size} voices",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        group.members.forEach { m ->
            Spacer(Modifier.height(8.dp))
            Text(m.name ?: "Unknown (${m.id.take(8)})", style = MaterialTheme.typography.bodyMedium)
            Text(
                "${m.nSamples} samples",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            m.samples.firstOrNull()?.let { Text("“$it”", style = MaterialTheme.typography.bodySmall) }
            OutlinedButton(onClick = { onPlay(m.id) }) { Text("Play sample") }
        }

        Spacer(Modifier.height(12.dp))
        if (group.nameConflict) {
            Text(
                "These voices carry different names — merge them manually below so a name isn't lost.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.error,
            )
        } else {
            Button(onClick = onMergeGroup) { Text("Merge group") }
        }
    }
}

/** Download the bearer-protected sample to cache, then play it with MediaPlayer. */
private fun playSample(ctx: Context, client: SpeakersClient, id: String, scope: CoroutineScope) {
    scope.launch(Dispatchers.IO) {
        val file = client.downloadSample(id, ctx.cacheDir) ?: return@launch
        var mp: MediaPlayer? = null
        try {
            mp = MediaPlayer().apply {
                setDataSource(file.path)
                setOnCompletionListener {
                    it.release()
                    file.delete()
                }
                // prepareAsync() reports decode/codec/IO failures asynchronously via this
                // callback, NOT the catch below — without it onCompletion never fires and both
                // the native MediaPlayer (globally hard-capped) and the cached sample leak.
                setOnErrorListener { player, _, _ ->
                    player.release()
                    file.delete()
                    true
                }
                setOnPreparedListener { it.start() }
                prepareAsync()
            }
        } catch (e: Exception) {
            // Synchronous throw (e.g. setDataSource): release the player we may have built.
            mp?.release()
            file.delete()
        }
    }
}

/** Shared card chrome (mirrors CaptureScreen's private SectionCard). */
@Composable
private fun SectionCard(content: @Composable ColumnScope.() -> Unit) {
    OutlinedCard(modifier = Modifier.fillMaxWidth(), shape = MaterialTheme.shapes.large) {
        Column(modifier = Modifier.padding(16.dp), content = content)
    }
}
