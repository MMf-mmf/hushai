package com.hushai.android.config

import android.content.Context
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.longPreferencesKey
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import com.hushai.android.util.Uuid7
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.runBlocking

private val Context.dataStore by preferencesDataStore(name = "hushai_settings")

/**
 * Persisted backend URL + token + stable device_id (DataStore). Defaults match
 * the verified backend for the emulator. Intent-extra overrides for headless
 * runs are applied by the caller, not here.
 */
class Settings(private val context: Context) {

    suspend fun url(): String = context.dataStore.data.map { it[KEY_URL] ?: DEFAULT_URL }.first()
    suspend fun token(): String = context.dataStore.data.map { it[KEY_TOKEN] ?: DEFAULT_TOKEN }.first()

    suspend fun setUrl(value: String) = edit(KEY_URL, value.trim())
    suspend fun setToken(value: String) = edit(KEY_TOKEN, value.trim())

    /** Audio-only capture mode: skip the camera + H.264 video stream entirely
     *  (only the cam0-audio stream is produced) to save storage, upload bandwidth,
     *  and battery when the video isn't needed. Chosen before Start. */
    suspend fun audioOnly(): Boolean = context.dataStore.data.map { it[KEY_AUDIO_ONLY] ?: false }.first()

    /** Bake the upright rotation into the encoded PIXELS on-device (GL render pass) instead of
     *  only stamping the MP4 rotation matrix. Off by default: the matrix path already makes the
     *  worker/frames and the viewer's re-encode path upright. Enable this (and verify on the
     *  physical rig — the rotation sign is empirical) so the viewer's fast `-c copy` path plays
     *  Android footage upright with no re-encode. Falls back to the matrix path if GL init fails.
     *  Baked at capture START from the mount orientation; a re-mount needs a capture restart. */
    suspend fun uprightBake(): Boolean = context.dataStore.data.map { it[KEY_UPRIGHT_BAKE] ?: false }.first()

    /** Stable per-install id, minted once and persisted (contract §4 device_id). */
    suspend fun deviceId(): String {
        // Mint atomically inside ONE DataStore transaction. A separate read-then-write let two
        // concurrent callers on a fresh install (cold onCreate + a headless startForegroundService
        // autostart) both observe null and mint two different ids, fragmenting uploaded data across
        // device_ids (the RAG scoping key). The edit{} lambda runs in DataStore's serialized
        // transaction, so a second caller sees the first's committed value.
        val prefs = context.dataStore.edit { p ->
            if (p[KEY_DEVICE_ID] == null) p[KEY_DEVICE_ID] = "android-" + Uuid7.bytes().hex()
        }
        return prefs[KEY_DEVICE_ID]!!
    }

    /** Hard cap (bytes) on the local store-and-forward buffer's segment bodies.
     *  Bounds how much footage we keep on disk while offline; the buffer drops the
     *  OLDEST segment (marking a gap) once this is reached. User-adjustable — and the
     *  change applies LIVE to a running capture via `CaptureService.updateDiskCap`
     *  (not just at next start). The core layer additionally enforces a device-free
     *  safety reserve. */
    suspend fun diskCapBytes(): Long =
        context.dataStore.data.map { it[KEY_DISK_CAP_BYTES] ?: DEFAULT_DISK_CAP_BYTES }.first()

    suspend fun setDiskCapBytes(value: Long) {
        context.dataStore.edit { it[KEY_DISK_CAP_BYTES] = value }
    }

    // --- Voice assistant settings ---
    suspend fun wakeWord(): String = context.dataStore.data.map { it[KEY_WAKE_WORD] ?: DEFAULT_WAKE_WORD }.first()
    suspend fun ragUrl(): String = context.dataStore.data.map { it[KEY_RAG_URL] ?: DEFAULT_RAG_URL }.first()
    /** Bearer the voice assistant presents to hushai-rag (`/v1/rag/query`, `/v1/tts`).
     *  Empty ⇒ no bearer (a dev rag with no RAG_TOKEN). Set it to match the server's
     *  RAG_TOKEN once rag auth is on. */
    suspend fun ragToken(): String = context.dataStore.data.map { it[KEY_RAG_TOKEN] ?: DEFAULT_RAG_TOKEN }.first()
    /** Base URL of the hushai-advisor service the voice consult calls (`/v1/advisor/chat`). */
    suspend fun advisorUrl(): String = context.dataStore.data.map { it[KEY_ADVISOR_URL] ?: DEFAULT_ADVISOR_URL }.first()
    /** Bearer the voice consult presents to hushai-advisor (matches the server's ADVISOR_TOKEN;
     *  empty ⇒ no bearer). Without the extra a stale DataStore token 401s every consult. */
    suspend fun advisorToken(): String = context.dataStore.data.map { it[KEY_ADVISOR_TOKEN] ?: DEFAULT_ADVISOR_TOKEN }.first()
    suspend fun assistantEnabled(): Boolean = context.dataStore.data.map { it[KEY_ASSISTANT_ENABLED] ?: false }.first()
    suspend fun ownerEmbedding(): String = context.dataStore.data.map { it[KEY_OWNER_EMBEDDING] ?: "" }.first()

    fun urlBlocking(): String = runBlocking { url() }
    fun tokenBlocking(): String = runBlocking { token() }
    fun deviceIdBlocking(): String = runBlocking { deviceId() }
    fun setUrlBlocking(value: String) = runBlocking { setUrl(value) }
    fun setTokenBlocking(value: String) = runBlocking { setToken(value) }
    fun audioOnlyBlocking(): Boolean = runBlocking { audioOnly() }
    fun setAudioOnlyBlocking(value: Boolean) =
        runBlocking { context.dataStore.edit { it[KEY_AUDIO_ONLY] = value } }

    fun uprightBakeBlocking(): Boolean = runBlocking { uprightBake() }
    fun setUprightBakeBlocking(value: Boolean) =
        runBlocking { context.dataStore.edit { it[KEY_UPRIGHT_BAKE] = value } }

    fun diskCapBytesBlocking(): Long = runBlocking { diskCapBytes() }
    fun setDiskCapBytesBlocking(value: Long) = runBlocking { setDiskCapBytes(value) }

    fun wakeWordBlocking(): String = runBlocking { wakeWord() }
    fun ragUrlBlocking(): String = runBlocking { ragUrl() }
    fun ragTokenBlocking(): String = runBlocking { ragToken() }
    fun setRagTokenBlocking(value: String) = runBlocking { edit(KEY_RAG_TOKEN, value.trim()) }
    fun advisorUrlBlocking(): String = runBlocking { advisorUrl() }
    fun setAdvisorUrlBlocking(value: String) = runBlocking { edit(KEY_ADVISOR_URL, value.trim()) }
    fun advisorTokenBlocking(): String = runBlocking { advisorToken() }
    fun setAdvisorTokenBlocking(value: String) = runBlocking { edit(KEY_ADVISOR_TOKEN, value.trim()) }
    fun assistantEnabledBlocking(): Boolean = runBlocking { assistantEnabled() }
    fun ownerEmbeddingBlocking(): String = runBlocking { ownerEmbedding() }
    fun setWakeWordBlocking(value: String) = runBlocking { edit(KEY_WAKE_WORD, value.trim()) }
    fun setRagUrlBlocking(value: String) = runBlocking { edit(KEY_RAG_URL, value.trim()) }
    fun setAssistantEnabledBlocking(value: Boolean) =
        runBlocking { context.dataStore.edit { it[KEY_ASSISTANT_ENABLED] = value } }
    fun setOwnerEmbeddingBlocking(value: String) = runBlocking { edit(KEY_OWNER_EMBEDDING, value) }

    // --- Voice-assistant chat session (continuity across turns; see assistant/VoiceSession) ---
    /** The persisted `(sessionId, lastTurnAtMillis)`, or null when none has been stored yet.
     *  DataStore-backed (not memory) so a session survives the assistant being torn down and
     *  rebuilt (capture stop/start, battery-saver kill, quick process restart). */
    fun loadVoiceSessionBlocking(): Pair<String, Long>? = runBlocking {
        context.dataStore.data.map {
            val id = it[KEY_VOICE_SESSION_ID]
            val at = it[KEY_VOICE_SESSION_AT] ?: 0L
            if (id.isNullOrBlank()) null else id to at
        }.first()
    }

    fun saveVoiceSessionBlocking(sessionId: String, atMillis: Long) = runBlocking {
        context.dataStore.edit {
            it[KEY_VOICE_SESSION_ID] = sessionId
            it[KEY_VOICE_SESSION_AT] = atMillis
        }
    }

    fun clearVoiceSessionBlocking() = runBlocking {
        context.dataStore.edit {
            it.remove(KEY_VOICE_SESSION_ID)
            it.remove(KEY_VOICE_SESSION_AT)
        }
    }

    // --- Advisor consult session (separate from the rag voice session; 30-min idle window) ---
    fun loadAdvisorSessionBlocking(): Pair<String, Long>? = runBlocking {
        context.dataStore.data.map {
            val id = it[KEY_ADVISOR_SESSION_ID]
            val at = it[KEY_ADVISOR_SESSION_AT] ?: 0L
            if (id.isNullOrBlank()) null else id to at
        }.first()
    }

    fun saveAdvisorSessionBlocking(sessionId: String, atMillis: Long) = runBlocking {
        context.dataStore.edit {
            it[KEY_ADVISOR_SESSION_ID] = sessionId
            it[KEY_ADVISOR_SESSION_AT] = atMillis
        }
    }

    fun clearAdvisorSessionBlocking() = runBlocking {
        context.dataStore.edit {
            it.remove(KEY_ADVISOR_SESSION_ID)
            it.remove(KEY_ADVISOR_SESSION_AT)
        }
    }

    // --- Detective (Gotham) consult session (separate from rag/advisor; 30-min idle window) ---
    fun loadDetectiveSessionBlocking(): Pair<String, Long>? = runBlocking {
        context.dataStore.data.map {
            val id = it[KEY_DETECTIVE_SESSION_ID]
            val at = it[KEY_DETECTIVE_SESSION_AT] ?: 0L
            if (id.isNullOrBlank()) null else id to at
        }.first()
    }

    fun saveDetectiveSessionBlocking(sessionId: String, atMillis: Long) = runBlocking {
        context.dataStore.edit {
            it[KEY_DETECTIVE_SESSION_ID] = sessionId
            it[KEY_DETECTIVE_SESSION_AT] = atMillis
        }
    }

    fun clearDetectiveSessionBlocking() = runBlocking {
        context.dataStore.edit {
            it.remove(KEY_DETECTIVE_SESSION_ID)
            it.remove(KEY_DETECTIVE_SESSION_AT)
        }
    }

    private suspend fun edit(key: androidx.datastore.preferences.core.Preferences.Key<String>, value: String) {
        context.dataStore.edit { it[key] = value }
    }

    companion object {
        val KEY_URL = stringPreferencesKey("backend_url")
        val KEY_TOKEN = stringPreferencesKey("device_token")
        val KEY_DEVICE_ID = stringPreferencesKey("device_id")
        val KEY_AUDIO_ONLY = booleanPreferencesKey("audio_only")
        val KEY_UPRIGHT_BAKE = booleanPreferencesKey("upright_bake")
        val KEY_DISK_CAP_BYTES = longPreferencesKey("disk_cap_bytes")
        val KEY_WAKE_WORD = stringPreferencesKey("wake_word")
        val KEY_RAG_URL = stringPreferencesKey("rag_url")
        val KEY_RAG_TOKEN = stringPreferencesKey("rag_token")
        val KEY_ASSISTANT_ENABLED = booleanPreferencesKey("assistant_enabled")
        val KEY_OWNER_EMBEDDING = stringPreferencesKey("owner_embedding")
        val KEY_VOICE_SESSION_ID = stringPreferencesKey("voice_session_id")
        val KEY_VOICE_SESSION_AT = longPreferencesKey("voice_session_at_millis")
        val KEY_ADVISOR_URL = stringPreferencesKey("advisor_url")
        val KEY_ADVISOR_TOKEN = stringPreferencesKey("advisor_token")
        val KEY_ADVISOR_SESSION_ID = stringPreferencesKey("advisor_session_id")
        val KEY_ADVISOR_SESSION_AT = longPreferencesKey("advisor_session_at_millis")
        val KEY_DETECTIVE_SESSION_ID = stringPreferencesKey("detective_session_id")
        val KEY_DETECTIVE_SESSION_AT = longPreferencesKey("detective_session_at_millis")
        // Debug/dev defaults (cleartext over the USB `adb reverse` tunnel / emulator).
        // RELEASE builds forbid cleartext (see src/release/network_security_config.xml),
        // so a release deployment MUST set an `https://<lan-ip>:8080` URL (cert SAN) via
        // the Settings UI or the `url`/`rag_url` Intent extras; the bundled LAN CA is trusted.
        const val DEFAULT_URL = "http://10.0.2.2:8080"
        // No default device token on purpose: the operator mints one per camera
        // (`local_dev/run_stack.sh --add-camera <name>`) and supplies it via the Settings
        // UI or the `token` Intent extra. Shipping a guessable fallback would hand every
        // reader of this source a working bearer for any stack left unconfigured, and the
        // same token also reaches the destructive admin API (see SECURITY.md). Empty means
        // the backend answers 401 until a real token is set — matching DEFAULT_RAG_TOKEN
        // and DEFAULT_ADVISOR_TOKEN below.
        const val DEFAULT_TOKEN = ""
        const val DEFAULT_RAG_TOKEN = ""
        // 2 GB default offline buffer cap; generous for hours of audio + a long
        // video outage, well within typical free space. User-adjustable.
        const val DEFAULT_DISK_CAP_BYTES = 2L * 1024 * 1024 * 1024
        // A common word the small ASR model recognizes reliably; user-changeable.
        const val DEFAULT_WAKE_WORD = "computer"
        const val DEFAULT_RAG_URL = "http://localhost:8090"
        const val DEFAULT_ADVISOR_URL = "http://localhost:8095"
        const val DEFAULT_ADVISOR_TOKEN = ""
    }
}
