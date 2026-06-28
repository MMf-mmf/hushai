package com.hushai.android.capture.imports

import android.content.Context
import android.media.MediaExtractor
import android.media.MediaFormat
import android.media.MediaMetadataRetriever
import android.net.Uri
import android.provider.DocumentsContract
import com.hushai.android.util.HushaiLog

/**
 * Pre-flight inspection of a picked file: which tracks exist, their codecs/geometry,
 * whether it's DRM-protected (un-importable), and a best-effort ORIGINAL capture time.
 * Fails fast with a user-facing reason rather than crashing the import pipeline.
 */
object MediaProbe {

    data class Probe(
        val hasAudio: Boolean,
        val hasVideo: Boolean,
        val audioMime: String?,
        val videoMime: String?,
        val width: Int,
        val height: Int,
        val frameRate: Int,
        val durationUs: Long,
        val sampleRate: Int,
        val channelCount: Int,
        val captureUnixNanos: Long,
    )

    fun probe(context: Context, uri: Uri): Result<Probe> = runCatching {
        val extractor = MediaExtractor()
        try {
            extractor.setDataSource(context, uri, null)
            var hasAudio = false
            var hasVideo = false
            var audioMime: String? = null
            var videoMime: String? = null
            var width = 0
            var height = 0
            var frameRate = 30
            var durationUs = 0L
            var sampleRate = 0
            var channelCount = 0
            var drm = false

            for (i in 0 until extractor.trackCount) {
                val fmt = extractor.getTrackFormat(i)
                val mime = fmt.getString(MediaFormat.KEY_MIME) ?: continue
                if (fmt.containsKey(MediaFormat.KEY_DURATION)) {
                    durationUs = maxOf(durationUs, fmt.getLong(MediaFormat.KEY_DURATION))
                }
                // A crypto/DRM track can't be decoded by an ordinary MediaCodec.
                if (mime.startsWith("application/") || fmt.containsKey("crypto-mode-bytes")) drm = true
                when {
                    mime.startsWith("audio/") -> {
                        hasAudio = true
                        audioMime = mime
                        sampleRate = fmt.optInt(MediaFormat.KEY_SAMPLE_RATE, 0)
                        channelCount = fmt.optInt(MediaFormat.KEY_CHANNEL_COUNT, 0)
                    }
                    mime.startsWith("video/") -> {
                        hasVideo = true
                        videoMime = mime
                        width = fmt.optInt(MediaFormat.KEY_WIDTH, 0)
                        height = fmt.optInt(MediaFormat.KEY_HEIGHT, 0)
                        frameRate = fmt.optInt(MediaFormat.KEY_FRAME_RATE, 30).coerceAtLeast(1)
                    }
                }
            }
            check(!drm) { "DRM-protected media can't be imported" }
            check(hasAudio || hasVideo) { "no decodable audio or video track" }

            Probe(
                hasAudio = hasAudio,
                hasVideo = hasVideo,
                audioMime = audioMime,
                videoMime = videoMime,
                width = width,
                height = height,
                frameRate = frameRate,
                durationUs = durationUs,
                sampleRate = sampleRate,
                channelCount = channelCount,
                captureUnixNanos = captureTimeNanos(context, uri),
            )
        } finally {
            runCatching { extractor.release() }
        }
    }

    /** Original capture time: media creation date -> document lastModified -> now. */
    private fun captureTimeNanos(context: Context, uri: Uri): Long {
        val fromMeta = runCatching {
            MediaMetadataRetriever().use { r ->
                r.setDataSource(context, uri)
                r.extractMetadata(MediaMetadataRetriever.METADATA_KEY_DATE)?.let { parseMediaDate(it) }
            }
        }.getOrNull()
        if (fromMeta != null) return fromMeta * 1_000_000L

        val lastModifiedMs = runCatching {
            context.contentResolver.query(
                uri,
                arrayOf(DocumentsContract.Document.COLUMN_LAST_MODIFIED),
                null, null, null,
            )?.use { c -> if (c.moveToFirst() && !c.isNull(0)) c.getLong(0) else null }
        }.getOrNull()
        if (lastModifiedMs != null && lastModifiedMs > 0) return lastModifiedMs * 1_000_000L

        return System.currentTimeMillis() * 1_000_000L
    }

    /** Parse common MediaMetadataRetriever date forms to epoch millis. */
    private fun parseMediaDate(raw: String): Long? {
        val patterns = listOf("yyyyMMdd'T'HHmmss.SSS'Z'", "yyyyMMdd'T'HHmmss'Z'", "yyyy-MM-dd HH:mm:ss")
        for (p in patterns) {
            val parsed = runCatching {
                val sdf = java.text.SimpleDateFormat(p, java.util.Locale.US)
                sdf.timeZone = java.util.TimeZone.getTimeZone("UTC")
                sdf.parse(raw)?.time
            }.getOrNull()
            if (parsed != null) return parsed
        }
        HushaiLog.info("import: unparsed media date '$raw' — falling back")
        return null
    }

    private fun MediaFormat.optInt(key: String, default: Int): Int =
        if (containsKey(key)) getInteger(key) else default
}
