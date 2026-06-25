package com.hushai.android.assistant

import android.content.Context
import com.hushai.android.util.HushaiLog
import org.vosk.Model
import org.vosk.SpeakerModel
import java.io.File

/**
 * Loads the Vosk acoustic + speaker models, unpacking them from APK assets to
 * internal storage on first run (Vosk needs a filesystem path, not an asset
 * stream). Blocking + idempotent + cached — call off the main thread.
 */
object VoiceModels {
    private const val EN_ASSET = "vosk/model-en"
    private const val SPK_ASSET = "vosk/model-spk"

    @Volatile private var model: Model? = null
    @Volatile private var spk: SpeakerModel? = null

    @Synchronized
    fun load(context: Context): Pair<Model, SpeakerModel> {
        model?.let { m -> spk?.let { s -> return m to s } }
        val enDir = unpackAsset(context, EN_ASSET)
        val spkDir = unpackAsset(context, SPK_ASSET)
        val m = Model(enDir.absolutePath)
        val s = SpeakerModel(spkDir.absolutePath)
        model = m
        spk = s
        return m to s
    }

    /** Copy assets/<assetPath> to filesDir/<assetPath> once (guarded by a marker). */
    private fun unpackAsset(context: Context, assetPath: String): File {
        val out = File(context.filesDir, assetPath)
        val marker = File(out, ".unpacked")
        if (marker.exists()) return out
        if (out.exists()) out.deleteRecursively()
        copyAsset(context, assetPath, out)
        marker.createNewFile()
        HushaiLog.info("unpacked vosk asset $assetPath -> ${out.absolutePath}")
        return out
    }

    private fun copyAsset(context: Context, assetPath: String, dest: File) {
        val children = context.assets.list(assetPath) ?: emptyArray()
        if (children.isEmpty()) {
            // Leaf = a file (an empty dir would also list empty, but the models have none).
            dest.parentFile?.mkdirs()
            context.assets.open(assetPath).use { input ->
                dest.outputStream().use { input.copyTo(it) }
            }
            return
        }
        dest.mkdirs()
        for (child in children) {
            copyAsset(context, "$assetPath/$child", File(dest, child))
        }
    }
}
