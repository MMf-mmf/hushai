package com.hushai.android.ui

import android.graphics.BitmapFactory
import androidx.compose.foundation.Image
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
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
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
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import com.hushai.android.net.PlatesClient
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * The "Plates" screen: lists the server's discovered license plates, shows a cropped sample plate
 * so the user can identify a vehicle by sight, lets them name a plate (PATCH) and merge duplicate
 * ids (POST). The vehicle sibling of [PeopleScreen] — same card chrome + reload pattern; all
 * network calls go through [PlatesClient] (backend url + token from Settings) off the main thread.
 * No image-loading library: crops are fetched with OkHttp (bearer) + decoded with BitmapFactory.
 * Adds a search field at the top (server-side text + fuzzy match); blank query reverts to the full
 * catalog.
 */
@Composable
fun PlatesScreen(client: PlatesClient, onBack: () -> Unit) {
    var plates by remember { mutableStateOf<List<PlatesClient.Plate>>(emptyList()) }
    var query by remember { mutableStateOf("") }
    var loading by remember { mutableStateOf(true) }
    var error by remember { mutableStateOf(false) }
    // Named plates collapse behind a closed disclosure so the unidentified plates lead.
    var knownExpanded by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    suspend fun reload() {
        loading = true
        error = false
        withContext(Dispatchers.IO) {
            // null = the call failed (vs. an empty list = "no plates yet"); surface a retryable
            // error instead of a misleading "no plates" / stale list. A non-blank query searches.
            val q = query.trim()
            val ps = if (q.isEmpty()) client.listPlates() else client.search(q)
            withContext(Dispatchers.Main) {
                if (ps == null) error = true else plates = ps
            }
        }
        loading = false
    }

    LaunchedEffect(Unit) { reload() }

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
            Text("Plates", style = MaterialTheme.typography.headlineMedium)
            Spacer(Modifier.weight(1f))
            TextButton(onClick = { scope.launch { reload() } }, enabled = !loading) {
                Text(if (loading) "Refreshing…" else "Refresh")
            }
        }
        Text(
            "License plates seen on camera. Look at a plate to recognize a vehicle, give it a name, " +
                "or merge duplicates of the same plate.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )

        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedTextField(
                value = query,
                onValueChange = { query = it },
                label = { Text("Search plate text") },
                singleLine = true,
                modifier = Modifier.weight(1f),
            )
            Button(onClick = { scope.launch { reload() } }, enabled = !loading) { Text("Search") }
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
            plates.isEmpty() -> Text(
                if (query.isBlank()) "No plates discovered yet." else "No plates match \"${query.trim()}\".",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            else -> {
                val card: @Composable (PlatesClient.Plate) -> Unit = { p ->
                    PlateCard(
                        plate = p,
                        others = plates.filter { it.plateId != p.plateId },
                        client = client,
                        onSave = { name ->
                            scope.launch {
                                val ok = withContext(Dispatchers.IO) { client.setName(p.plateId, name) }
                                if (ok) reload()
                            }
                        },
                        onMerge = { intoId ->
                            scope.launch {
                                val ok = withContext(Dispatchers.IO) { client.merge(p.plateId, intoId) }
                                if (ok) reload()
                            }
                        },
                    )
                }
                // Named plates first (the known vehicles), then the ones still waiting to be named.
                val known = plates.filter { !it.displayName.isNullOrBlank() }
                val unknown = plates.filter { it.displayName.isNullOrBlank() }

                if (known.isEmpty()) {
                    Text("Named plates (0)", style = MaterialTheme.typography.titleMedium)
                    Text(
                        "No plates named yet — name one below to build your named-plate list.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                } else {
                    Text(
                        "${if (knownExpanded) "▾" else "▸"} Named plates (${known.size})",
                        style = MaterialTheme.typography.titleMedium,
                        modifier = Modifier.clickable { knownExpanded = !knownExpanded },
                    )
                    if (knownExpanded) known.forEach { card(it) }
                }
                if (unknown.isNotEmpty()) {
                    Text("Unidentified plates (${unknown.size})", style = MaterialTheme.typography.titleMedium)
                    unknown.forEach { card(it) }
                }
            }
        }
    }
}

@Composable
private fun PlateCard(
    plate: PlatesClient.Plate,
    others: List<PlatesClient.Plate>,
    client: PlatesClient,
    onSave: (String) -> Unit,
    onMerge: (String) -> Unit,
) {
    var name by remember(plate.plateId) { mutableStateOf(plate.displayName ?: "") }
    var mergeOpen by remember(plate.plateId) { mutableStateOf(false) }

    SectionCard {
        Row(verticalAlignment = Alignment.Top, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            PlateThumbnail(client = client, plateId = plate.plateId)
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    plate.displayName ?: plate.plateText.ifBlank { "Unknown plate (${plate.plateId.take(8)})" },
                    style = MaterialTheme.typography.titleMedium,
                )
                // When a plate has a human name, still surface its raw OCR text underneath.
                if (!plate.displayName.isNullOrBlank() && plate.plateText.isNotBlank()) {
                    Text(
                        plate.plateText,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                Text(
                    "${plate.nSightings} sighting(s)",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                plate.sampleSightingsNanos.firstOrNull()?.let { nanos ->
                    Text(
                        "Last seen ${relativeSighting(nanos)}",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
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

        if (others.isNotEmpty()) {
            Spacer(Modifier.height(8.dp))
            Box {
                OutlinedButton(onClick = { mergeOpen = true }) { Text("Merge into…") }
                DropdownMenu(expanded = mergeOpen, onDismissRequest = { mergeOpen = false }) {
                    others.forEach { other ->
                        DropdownMenuItem(
                            text = {
                                Text(
                                    other.displayName
                                        ?: other.plateText.ifBlank { "Unknown (${other.plateId.take(8)})" },
                                )
                            },
                            onClick = {
                                mergeOpen = false
                                onMerge(other.plateId)
                            },
                        )
                    }
                }
            }
        }
    }
}

/**
 * Fetches the bearer-protected sample-plate crop to cache, decodes it off the main thread, and
 * renders a ~96.dp thumbnail (with a placeholder while loading / on failure). One-shot per id.
 */
@Composable
private fun PlateThumbnail(client: PlatesClient, plateId: String) {
    val ctx = LocalContext.current
    var bitmap by remember(plateId) { mutableStateOf<ImageBitmap?>(null) }
    var failed by remember(plateId) { mutableStateOf(false) }

    LaunchedEffect(plateId) {
        withContext(Dispatchers.IO) {
            val file = client.downloadSampleCrop(plateId, ctx.cacheDir)
            val decoded = file?.let { BitmapFactory.decodeFile(it.path) }
            file?.delete()
            withContext(Dispatchers.Main) {
                if (decoded != null) bitmap = decoded.asImageBitmap() else failed = true
            }
        }
    }

    Box(
        modifier = Modifier
            .size(96.dp)
            .clip(RoundedCornerShape(8.dp)),
        contentAlignment = Alignment.Center,
    ) {
        val b = bitmap
        if (b != null) {
            Image(
                bitmap = b,
                contentDescription = "Sample plate",
                contentScale = ContentScale.Crop,
                modifier = Modifier.size(96.dp),
            )
        } else {
            Text(
                if (failed) "no\nplate" else "…",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                textAlign = TextAlign.Center,
            )
        }
    }
}

/** Minimal on-device relative time for a sighting (no humanize lib on Android). */
private fun relativeSighting(unixNanos: Long): String {
    val nowMs = System.currentTimeMillis()
    val thenMs = unixNanos / 1_000_000
    val deltaSec = ((nowMs - thenMs) / 1000).coerceAtLeast(0)
    return when {
        deltaSec < 60 -> "just now"
        deltaSec < 3600 -> "${deltaSec / 60}m ago"
        deltaSec < 86_400 -> "${deltaSec / 3600}h ago"
        deltaSec < 604_800 -> "${deltaSec / 86_400}d ago"
        else -> "${deltaSec / 604_800}w ago"
    }
}

/** Shared card chrome (mirrors PeopleScreen/VoicesScreen's private SectionCard). */
@Composable
private fun SectionCard(content: @Composable ColumnScope.() -> Unit) {
    OutlinedCard(modifier = Modifier.fillMaxWidth(), shape = MaterialTheme.shapes.large) {
        Column(modifier = Modifier.padding(16.dp), content = content)
    }
}
