package com.clipsync.android.service

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.graphics.BitmapFactory
import android.net.Uri
import android.util.Base64
import androidx.core.content.FileProvider
import com.clipsync.android.data.Message
import java.io.ByteArrayOutputStream
import java.io.File
import java.security.MessageDigest
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

class ClipboardWatcher(
    private val context: Context,
    private val scope: CoroutineScope,
    private val deviceId: () -> String?,
    private val onMessage: (Message) -> Unit,
) : ClipboardManager.OnPrimaryClipChangedListener {
    private val clipboard = context.getSystemService(ClipboardManager::class.java)
    @Volatile private var selfWriteHash: String? = null

    fun start() = clipboard.addPrimaryClipChangedListener(this)
    fun stop() = clipboard.removePrimaryClipChangedListener(this)
    override fun onPrimaryClipChanged() {
        val origin = deviceId() ?: return
        val clip = try { clipboard.primaryClip } catch (_: SecurityException) { null } ?: return
        val item = clip.getItemAt(0)
        item.text?.toString()?.let { text ->
            val hash = sha256(text.toByteArray())
            if (consumeSelfWrite(hash)) return
            onMessage(Message.ClipboardText("text/plain;charset=utf-8", text, origin, hash))
            return
        }
        val uri = item.uri ?: return
        val mime = clip.description.filterMimeTypes("image/*").firstOrNull()
            ?: context.contentResolver.getType(uri)?.takeIf { it.startsWith("image/") }
            ?: return
        scope.launch {
            val bytes = withContext(Dispatchers.IO) { readLimited(uri) } ?: return@launch
            val hash = sha256(bytes)
            if (consumeSelfWrite(hash)) return@launch
            val bounds = BitmapFactory.Options().also { it.inJustDecodeBounds = true }
            BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)
            onMessage(Message.ClipboardImage(
                mime, Base64.encodeToString(bytes, Base64.NO_WRAP),
                bounds.outWidth.takeIf { it > 0 }, bounds.outHeight.takeIf { it > 0 }, hash, origin,
            ))
        }
    }
    fun writeText(message: Message.ClipboardText) {
        if (sha256(message.content.toByteArray()) != message.sha256) return
        selfWriteHash = message.sha256
        clipboard.setPrimaryClip(ClipData.newPlainText("ClipSync", message.content))
    }
    fun writeImage(message: Message.ClipboardImage) {
        val bytes = try { Base64.decode(message.data_b64, Base64.DEFAULT) } catch (_: IllegalArgumentException) { return }
        if (bytes.size > MAX_IMAGE_BYTES || sha256(bytes) != message.sha256) return
        val extension = when (message.mime) {
            "image/jpeg" -> "jpg"; "image/gif" -> "gif"; "image/webp" -> "webp"; else -> "png"
        }
        val directory = File(context.cacheDir, "clipboard").apply { mkdirs() }
        val file = File(directory, "remote-${message.sha256}.$extension")
        file.writeBytes(bytes)
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.files", file)
        selfWriteHash = message.sha256
        clipboard.setPrimaryClip(ClipData.newUri(context.contentResolver, "ClipSync image", uri))
    }
    @Synchronized private fun consumeSelfWrite(hash: String): Boolean {
        if (selfWriteHash != hash) return false
        selfWriteHash = null
        return true
    }
    private fun readLimited(uri: Uri): ByteArray? = try {
        context.contentResolver.openInputStream(uri)?.use { input ->
            val output = ByteArrayOutputStream()
            val buffer = ByteArray(8 * 1024)
            var total = 0
            while (true) {
                val count = input.read(buffer)
                if (count < 0) break
                total += count
                if (total > MAX_IMAGE_BYTES) return null
                output.write(buffer, 0, count)
            }
            output.toByteArray()
        }
    } catch (_: Exception) { null }
    companion object {
        const val MAX_IMAGE_BYTES = 25 * 1024 * 1024
        fun sha256(bytes: ByteArray): String = MessageDigest.getInstance("SHA-256")
            .digest(bytes).joinToString("") { "%02x".format(it.toInt() and 0xff) }
    }
}
