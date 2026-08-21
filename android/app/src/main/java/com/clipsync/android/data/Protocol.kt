package com.clipsync.android.data

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonClassDiscriminator

@Serializable
data class DeviceInfo(
    val name: String,
    val kind: String = "android",
    val id: String? = null,
    val os_version: String? = null,
    val app_version: String? = null,
    val capabilities: Capabilities = Capabilities(text = true, images = true),
)

@Serializable
data class Capabilities(
    val text: Boolean = false,
    val html: Boolean = false,
    val images: Boolean = false,
    val files: Boolean = false,
)

@OptIn(kotlinx.serialization.ExperimentalSerializationApi::class)
@Serializable
@JsonClassDiscriminator("type")
sealed interface Message {
    @Serializable @SerialName("hello")
    data class Hello(val v: Int = 1, val device: DeviceInfo) : Message
    @Serializable @SerialName("pair_challenge")
    data class PairChallenge(val challenge_id: String, val expires_at: Long, val nonce: String) : Message
    @Serializable @SerialName("pair_submit")
    data class PairSubmit(val challenge_id: String, val code: String, val nonce: String) : Message
    @Serializable @SerialName("pair_ok")
    data class PairOk(
        val device_id: String,
        val session_id: String,
        val server_name: String,
        val capabilities: Capabilities = Capabilities(),
    ) : Message
    @Serializable @SerialName("pair_fail")
    data class PairFail(val reason: String, val message: String) : Message
    @Serializable @SerialName("clipboard_text")
    data class ClipboardText(val mime: String, val content: String, val origin: String, val sha256: String) : Message
    @Serializable @SerialName("clipboard_image")
    data class ClipboardImage(
        val mime: String,
        val data_b64: String,
        val width: Int? = null,
        val height: Int? = null,
        val sha256: String,
        val origin: String,
    ) : Message
    @Serializable @SerialName("clipboard_html")
    data class ClipboardHtml(val html: String, val alt: String? = null, val sha256: String, val origin: String) : Message
    @Serializable @SerialName("ping") data class Ping(val ts: Long) : Message
    @Serializable @SerialName("pong") data class Pong(val ts: Long) : Message
    @Serializable @SerialName("error") data class Error(val code: String, val message: String) : Message
}

object ProtocolCodec {
    private val json = Json {
        ignoreUnknownKeys = true
        encodeDefaults = true
        explicitNulls = false
    }
    fun encode(message: Message): String = json.encodeToString(Message.serializer(), message)
    fun decode(payload: String): Message = json.decodeFromString(Message.serializer(), payload)
}
