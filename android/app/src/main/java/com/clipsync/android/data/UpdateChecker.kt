package com.clipsync.android.data

import android.os.Build
import com.clipsync.android.BuildConfig
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONObject
import java.util.concurrent.TimeUnit

data class AppUpdate(
    val version: String,
    val downloadUrl: String,
)

object UpdateChecker {
    private const val RELEASES_URL =
        "https://api.github.com/repos/luis-ota/clipsync/releases/latest"
    private const val APK_PREFIX = "clipsync-android-"

    private val client = OkHttpClient.Builder()
        .callTimeout(8, TimeUnit.SECONDS)
        .build()

    suspend fun latest(): AppUpdate? = withContext(Dispatchers.IO) {
        runCatching {
            val request = Request.Builder()
                .url(RELEASES_URL)
                .header("Accept", "application/vnd.github+json")
                .header("User-Agent", "clipsync-android/${BuildConfig.VERSION_NAME}")
                .build()
            client.newCall(request).execute().use { response ->
                if (!response.isSuccessful) return@use null
                val body = response.body?.string() ?: return@use null
                val release = JSONObject(body)
                val tag = release.optString("tag_name").removePrefix("v")
                if (!isNewer(tag, BuildConfig.VERSION_NAME)) return@use null
                val assets = release.optJSONArray("assets") ?: return@use null
                for (index in 0 until assets.length()) {
                    val asset = assets.optJSONObject(index) ?: continue
                    val name = asset.optString("name")
                    if (name == "$APK_PREFIX$tag-debug.apk") {
                        val url = asset.optString("browser_download_url")
                        if (url.isNotBlank()) return@use AppUpdate(tag, url)
                    }
                }
                null
            }
        }.getOrNull()
    }

    internal fun isNewer(candidate: String, current: String): Boolean {
        val candidateParts = versionParts(candidate)
        val currentParts = versionParts(current)
        if (candidateParts == null || currentParts == null) return false
        return candidateParts.zip(currentParts)
            .firstOrNull { (candidatePart, currentPart) -> candidatePart != currentPart }
            ?.let { (candidatePart, currentPart) -> candidatePart > currentPart }
            ?: false
    }

    private fun versionParts(value: String): List<Int>? {
        val normalized = value.removePrefix("v")
        val parts = normalized.split('.')
        if (parts.size != 3 || parts.any { it.toIntOrNull() == null }) return null
        return parts.map(String::toInt)
    }
}
