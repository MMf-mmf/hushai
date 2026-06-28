package com.hushai.android.ui

import android.graphics.BitmapFactory
import androidx.compose.foundation.Image
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
import com.hushai.android.net.PersonsClient
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * The "People" screen: lists the server's discovered faces, shows a cropped sample face so the
 * user can identify a person by sight, lets them name a face (PATCH) and merge duplicate ids
 * (POST). The visual sibling of [VoicesScreen] — same card chrome + reload pattern; all network
 * calls go through [PersonsClient] (backend url + token from Settings) off the main thread. No
 * image-loading library: faces are fetched with OkHttp (bearer) + decoded with BitmapFactory.
 */
@Composable
fun PeopleScreen(client: PersonsClient, onBack: () -> Unit) {
    var persons by remember { mutableStateOf<List<PersonsClient.Person>>(emptyList()) }
    var loading by remember { mutableStateOf(true) }
    var error by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    suspend fun reload() {
        loading = true
        error = false
        withContext(Dispatchers.IO) {
            // null = the call failed (vs. an empty list = "no faces yet"); surface a retryable
            // error instead of a misleading "no faces" / stale list.
            val ps = client.listPersons()
            withContext(Dispatchers.Main) {
                if (ps == null) error = true else persons = ps
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
            Text("People", style = MaterialTheme.typography.headlineMedium)
            Spacer(Modifier.weight(1f))
            TextButton(onClick = { scope.launch { reload() } }, enabled = !loading) {
                Text(if (loading) "Refreshing…" else "Refresh")
            }
        }
        Text(
            "Faces seen on camera. Look at a face to recognize someone, give it a name, or merge " +
                "duplicates of the same person.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )

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
            persons.isEmpty() -> Text(
                "No faces discovered yet.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            else -> {
                val card: @Composable (PersonsClient.Person) -> Unit = { p ->
                    PersonCard(
                        person = p,
                        others = persons.filter { it.id != p.id },
                        client = client,
                        onSave = { name ->
                            scope.launch {
                                val ok = withContext(Dispatchers.IO) { client.setName(p.id, name) }
                                if (ok) reload()
                            }
                        },
                        onMerge = { intoId ->
                            scope.launch {
                                val ok = withContext(Dispatchers.IO) { client.merge(p.id, intoId) }
                                if (ok) reload()
                            }
                        },
                    )
                }
                // Named faces first (the known people), then the ones still waiting to be named.
                val known = persons.filter { !it.name.isNullOrBlank() }
                val unknown = persons.filter { it.name.isNullOrBlank() }

                Text("Known people (${known.size})", style = MaterialTheme.typography.titleMedium)
                if (known.isEmpty()) {
                    Text(
                        "No faces identified yet — name one below to build your known-people list.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                } else {
                    known.forEach { card(it) }
                }
                if (unknown.isNotEmpty()) {
                    Text("Unidentified faces (${unknown.size})", style = MaterialTheme.typography.titleMedium)
                    unknown.forEach { card(it) }
                }
            }
        }
    }
}

@Composable
private fun PersonCard(
    person: PersonsClient.Person,
    others: List<PersonsClient.Person>,
    client: PersonsClient,
    onSave: (String) -> Unit,
    onMerge: (String) -> Unit,
) {
    var name by remember(person.id) { mutableStateOf(person.name ?: "") }
    var mergeOpen by remember(person.id) { mutableStateOf(false) }

    SectionCard {
        Row(verticalAlignment = Alignment.Top, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            FaceThumbnail(client = client, personId = person.id)
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    person.name ?: "Unknown face (${person.id.take(8)})",
                    style = MaterialTheme.typography.titleMedium,
                )
                Text(
                    "${person.nSamples} sighting(s)",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                person.sampleSightingsNanos.firstOrNull()?.let { nanos ->
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

/**
 * Fetches the bearer-protected sample-face crop to cache, decodes it off the main thread, and
 * renders a ~96.dp thumbnail (with a placeholder while loading / on failure). One-shot per id.
 */
@Composable
private fun FaceThumbnail(client: PersonsClient, personId: String) {
    val ctx = LocalContext.current
    var bitmap by remember(personId) { mutableStateOf<ImageBitmap?>(null) }
    var failed by remember(personId) { mutableStateOf(false) }

    LaunchedEffect(personId) {
        withContext(Dispatchers.IO) {
            val file = client.downloadSampleFace(personId, ctx.cacheDir)
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
                contentDescription = "Sample face",
                contentScale = ContentScale.Crop,
                modifier = Modifier.size(96.dp),
            )
        } else {
            Text(
                if (failed) "no\nface" else "…",
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

/** Shared card chrome (mirrors VoicesScreen/CaptureScreen's private SectionCard). */
@Composable
private fun SectionCard(content: @Composable ColumnScope.() -> Unit) {
    OutlinedCard(modifier = Modifier.fillMaxWidth(), shape = MaterialTheme.shapes.large) {
        Column(modifier = Modifier.padding(16.dp), content = content)
    }
}
