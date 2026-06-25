package com.hushai.android.config

import android.content.Context
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.core.edit
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

    /** Stable per-install id, minted once and persisted (contract §4 device_id). */
    suspend fun deviceId(): String {
        context.dataStore.data.map { it[KEY_DEVICE_ID] }.first()?.let { return it }
        val minted = "android-" + Uuid7.bytes().hex()
        edit(KEY_DEVICE_ID, minted)
        return minted
    }

    // --- Voice assistant settings ---
    suspend fun wakeWord(): String = context.dataStore.data.map { it[KEY_WAKE_WORD] ?: DEFAULT_WAKE_WORD }.first()
    suspend fun ragUrl(): String = context.dataStore.data.map { it[KEY_RAG_URL] ?: DEFAULT_RAG_URL }.first()
    suspend fun assistantEnabled(): Boolean = context.dataStore.data.map { it[KEY_ASSISTANT_ENABLED] ?: false }.first()
    suspend fun ownerEmbedding(): String = context.dataStore.data.map { it[KEY_OWNER_EMBEDDING] ?: "" }.first()

    fun urlBlocking(): String = runBlocking { url() }
    fun tokenBlocking(): String = runBlocking { token() }
    fun deviceIdBlocking(): String = runBlocking { deviceId() }
    fun setUrlBlocking(value: String) = runBlocking { setUrl(value) }
    fun setTokenBlocking(value: String) = runBlocking { setToken(value) }

    fun wakeWordBlocking(): String = runBlocking { wakeWord() }
    fun ragUrlBlocking(): String = runBlocking { ragUrl() }
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
        val KEY_WAKE_WORD = stringPreferencesKey("wake_word")
        val KEY_RAG_URL = stringPreferencesKey("rag_url")
        val KEY_ASSISTANT_ENABLED = booleanPreferencesKey("assistant_enabled")
        val KEY_OWNER_EMBEDDING = stringPreferencesKey("owner_embedding")
        const val DEFAULT_URL = "http://10.0.2.2:8080"
        const val DEFAULT_TOKEN = "dev-secret-token"
        // A common word the small ASR model recognizes reliably; user-changeable.
        const val DEFAULT_WAKE_WORD = "computer"
        const val DEFAULT_RAG_URL = "http://localhost:8090"
    }
}
