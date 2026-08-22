package com.clipsync.android.service

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.net.Uri
import android.util.Base64
import androidx.core.content.FileProvider
import com.clipsync.android.data.Message
import java.io.ByteArrayOutputStream
import java.io.File
import java.security.MessageDigest
import java.nio.charset.StandardCharsets
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

object ImageLimits {
    const val MAX_WEBSOCKET_MESSAGE_BYTES = 16 * 1024 * 1024
    private const val JSON_RESERVE_BYTES = 8 * 1024
    const val MAX_IMAGE_BYTES = 12 * 1024 * 1024 - 6 * 1024

    fun acceptsRawSize(size: Int): Boolean = size in 0..MAX_IMAGE_BYTES
    fun acceptsEncodedSize(size: Int): Boolean = size >= 0 &&
        size <= MAX_WEBSOCKET_MESSAGE_BYTES - JSON_RESERVE_BYTES
}

class PendingEchoes(
    private val ttlMillis: Long = 10_000,
    private val maximumEntries: Int = 32,
    private val clock: () -> Long = System::currentTimeMillis,
) {
    private data class Entry(val hash: String, val expiresAt: Long)
    private val entries = ArrayDeque<Entry>()

    @Synchronized fun add(hash: String) {
        expire()
        while (entries.size >= maximumEntries) entries.removeFirst()
        entries.addLast(Entry(hash, clock() + ttlMillis))
    }

    @Synchronized fun consume(hash: String): Boolean {
        expire()
        val index = entries.indexOfFirst { it.hash == hash }
        if (index < 0) return false
        entries.removeAt(index)
        return true
    }

    private fun expire() {
        val now = clock()
        entries.removeAll { it.expiresAt <= now }
    }
}

class ClipboardWatcher(
    private val context: Context,
    private val scope: CoroutineScope,
    private val deviceId: () -> String?,
    private val onMessage: (Message) -> Unit,
) : ClipboardManager.OnPrimaryClipChangedListener {
    private val clipboard = context.getSystemService(ClipboardManager::class.java)
    private val pendingEchoes = PendingEchoes()
    private var pendingNotification: Job? = null

    fun start() = clipboard.addPrimaryClipChangedListener(this)
    fun stop() = clipboard.removePrimaryClipChangedListener(this)

    fun sendCurrentClipboard() = emitCurrentClip()

    override fun onPrimaryClipChanged() {
        pendingNotification?.cancel()
        pendingNotification = scope.launch {
            delay(DEBOUNCE_MILLIS)
            emitCurrentClip()
        }
    }

    private fun emitCurrentClip() {
        val origin = deviceId() ?: return
        val clip = try { clipboard.primaryClip } catch (_: SecurityException) { null } ?: return
        val item = clip.getItemAt(0)
        item.text?.toString()?.let { text ->
            scope.launch {
                val hash = withContext(Dispatchers.Default) { sha256(text.toByteArray(StandardCharsets.UTF_8)) }
                if (!pendingEchoes.consume(hash)) {
                    onMessage(Message.ClipboardText("text/plain;charset=utf-8", text, origin, hash))
                }
            }
            return
        }
        val uri = item.uri ?: return
        val mime = clip.description.filterMimeTypes("image/*").firstOrNull()
            ?: context.contentResolver.getType(uri)?.takeIf { it.startsWith("image/") }
            ?: return
        scope.launch {
            val bytes = withContext(Dispatchers.IO) { readLimited(uri) } ?: return@launch
            val message = withContext(Dispatchers.Default) {
                val hash = sha256(bytes)
                if (pendingEchoes.consume(hash)) return@withContext null
                Message.ClipboardImage(
                    mime, Base64.encodeToString(bytes, Base64.NO_WRAP),
                    null, null, hash, origin,
                )
            }
            message?.let(onMessage)
        }
    }

    fun writeText(message: Message.ClipboardText) {
        scope.launch {
            val valid = withContext(Dispatchers.Default) {
                sha256(message.content.toByteArray(StandardCharsets.UTF_8)) == message.sha256
            }
            if (!valid) return@launch
            pendingEchoes.add(message.sha256)
            try {
                clipboard.setPrimaryClip(ClipData.newPlainText("ClipSync", message.content))
            } catch (_: RuntimeException) { pendingEchoes.consume(message.sha256) }
        }
    }

    fun writeImage(message: Message.ClipboardImage) {
        scope.launch {
            if (!ImageLimits.acceptsEncodedSize(message.data_b64.length)) return@launch
            val bytes = withContext(Dispatchers.Default) {
                try { Base64.decode(message.data_b64, Base64.DEFAULT) } catch (_: IllegalArgumentException) { null }
            } ?: return@launch
            val valid = withContext(Dispatchers.Default) {
                ImageLimits.acceptsRawSize(bytes.size) && sha256(bytes) == message.sha256
            }
            if (!valid) return@launch
            val extension = when (message.mime) {
                "image/jpeg" -> "jpg"; "image/gif" -> "gif"; "image/webp" -> "webp"; else -> "png"
            }
            val file = withContext(Dispatchers.IO) {
                val directory = File(context.cacheDir, "clipboard").apply { mkdirs() }
                File(directory, "remote-${message.sha256}.$extension").also { it.writeBytes(bytes) }
            }
            val uri = FileProvider.getUriForFile(context, "${context.packageName}.files", file)
            pendingEchoes.add(message.sha256)
            try {
                clipboard.setPrimaryClip(ClipData.newUri(context.contentResolver, "ClipSync image", uri))
            } catch (_: RuntimeException) { pendingEchoes.consume(message.sha256) }
        }
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
                if (!ImageLimits.acceptsRawSize(total)) return null
                output.write(buffer, 0, count)
            }
            output.toByteArray()
        }
    } catch (_: Exception) { null }

    companion object {
        private const val DEBOUNCE_MILLIS = 300L

        fun sha256(bytes: ByteArray): String = MessageDigest.getInstance("SHA-256")
            .digest(bytes).joinToString("") { "%02x".format(it.toInt() and 0xff) }
    }
}
