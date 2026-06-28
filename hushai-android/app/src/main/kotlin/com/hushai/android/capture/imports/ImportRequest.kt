package com.hushai.android.capture.imports

import android.net.Uri

/** One file the user picked to import. [streamIndex] gives it a distinct stream lane. */
data class ImportRequest(
    val uri: Uri,
    val displayName: String,
    val streamIndex: Int,
)

/** Outcome of importing one file. */
sealed interface ImportResult {
    data object Completed : ImportResult
    data object Cancelled : ImportResult
    data class Failed(val reason: String) : ImportResult
}
