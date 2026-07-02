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

    fun diskCapBytesBlocking(): Long = runBlocking { diskCapBytes() }
    fun setDiskCapBytesBlocking(value: Long) = runBlocking { setDiskCapBytes(value) }

    fun wakeWordBlocking(): String = runBlocking { wakeWord() }
    fun ragUrlBlocking(): String = runBlocking { ragUrl() }
    fun ragTokenBlocking(): String = runBlocking { ragToken() }
    fun setRagTokenBlocking(value: String) = runBlocking { edit(KEY_RAG_TOKEN, value.trim()) }
    fun assistantEnabledBlocking(): Boolean = runBlocking { assistantEnabled() }
    fun ownerEmbeddingBlocking(): String = runBlocking { ownerEmbedding() }
    fun setWakeWordBlocking(value: String) = runBlocking { edit(KEY_WAKE_WORD, value.trim()) }
    fun setRagUrlBlocking(value: String) = runBlocking { edit(KEY_RAG_URL, value.trim()) }
    fun setAssistantEnabledBlocking(value: Boolean) =
        runBlocking { context.dataStore.edit { it[KEY_ASSISTANT_ENABLED] = value } }
    fun setOwnerEmbeddingBlocking(value: String) = runBlocking { edit(KEY_OWNER_EMBEDDING, value) }

    private suspend fun edit(key: androidx.datastore.preferences.core.Preferences.Key<String>, value: String) {
        context.dataStore.edit { it[key] = value }
    }

    companion object {
        val KEY_URL = stringPreferencesKey("backend_url")
        val KEY_TOKEN = stringPreferencesKey("device_token")
        val KEY_DEVICE_ID = stringPreferencesKey("device_id")
        val KEY_AUDIO_ONLY = booleanPreferencesKey("audio_only")
        val KEY_DISK_CAP_BYTES = longPreferencesKey("disk_cap_bytes")
        val KEY_WAKE_WORD = stringPreferencesKey("wake_word")
        val KEY_RAG_URL = stringPreferencesKey("rag_url")
        val KEY_RAG_TOKEN = stringPreferencesKey("rag_token")
        val KEY_ASSISTANT_ENABLED = booleanPreferencesKey("assistant_enabled")
        val KEY_OWNER_EMBEDDING = stringPreferencesKey("owner_embedding")
        // Debug/dev defaults (cleartext over the USB `adb reverse` tunnel / emulator).
        // RELEASE builds forbid cleartext (see src/release/network_security_config.xml),
        // so a release deployment MUST set an `https://<lan-ip>:8080` URL (cert SAN) via
        // the Settings UI or the `url`/`rag_url` Intent extras; the bundled LAN CA is trusted.
        const val DEFAULT_URL = "http://10.0.2.2:8080"
        const val DEFAULT_TOKEN = "dev-secret-token"
        const val DEFAULT_RAG_TOKEN = ""
        // 2 GB default offline buffer cap; generous for hours of audio + a long
        // video outage, well within typical free space. User-adjustable.
        const val DEFAULT_DISK_CAP_BYTES = 2L * 1024 * 1024 * 1024
        // A common word the small ASR model recognizes reliably; user-changeable.
        const val DEFAULT_WAKE_WORD = "computer"
        const val DEFAULT_RAG_URL = "http://localhost:8090"
    }
}
