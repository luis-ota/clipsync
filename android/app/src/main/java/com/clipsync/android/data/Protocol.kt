package com.clipsync.android.data

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonClassDiscriminator
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec
import java.util.Base64

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
        val server_id: String? = null,
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
    @Serializable @SerialName("transfer_offer")
    data class TransferOffer(
        val transfer_id: String, val mime: String, val name: String? = null,
        val size: Long, val chunks: Int, val sha256: String, val file: Boolean,
        val origin: String,
    ) : Message
    @Serializable @SerialName("transfer_accept") data class TransferAccept(val transfer_id: String) : Message
    @Serializable @SerialName("transfer_reject") data class TransferReject(val transfer_id: String, val reason: String) : Message
    @Serializable @SerialName("transfer_complete") data class TransferComplete(val transfer_id: String, val sha256: String) : Message
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
    fun encodeEnvelope(envelope: RelayEnvelope): String = json.encodeToString(RelayEnvelope.serializer(), envelope)
    fun decodeEnvelope(payload: String): RelayEnvelope = json.decodeFromString(RelayEnvelope.serializer(), payload)
}

@Serializable
data class RelayPayload(val key_id: String, val nonce: String, val ciphertext: String)
@Serializable
data class RelayEnvelope(
    @SerialName("type") val kind: String,
    val session_id: String,
    val source: String,
    val destination: String? = null,
    val group: String,
    val sequence: Long,
    val payload: RelayPayload,
)

object RelayCrypto {
    private val random = SecureRandom()
    fun encrypt(message: Message, keyMaterial: String, session: String, source: String, sequence: Long): RelayEnvelope {
        val fields = keyMaterial.trim().split(Regex("\\s+"))
        require(fields.size == 3 && fields[2].length == 64)
        val nonce = ByteArray(12).also(random::nextBytes)
        val group = fields[1]
        val aad = aad(session, source, null, group, sequence, fields[0])
        val cipher = Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.ENCRYPT_MODE, SecretKeySpec(hex(fields[2]), "AES"), GCMParameterSpec(128, nonce)) }
        cipher.updateAAD(aad.toByteArray(Charsets.UTF_8))
        val encrypted = cipher.doFinal(ProtocolCodec.encode(message).toByteArray(Charsets.UTF_8))
        return RelayEnvelope("relay_envelope", session, source, null, group, sequence, RelayPayload(fields[0], nonce.joinToString("") { "%02x".format(it) }, Base64.getEncoder().encodeToString(encrypted)))
    }
    fun decrypt(envelope: RelayEnvelope, keyMaterial: String): Message {
        require(envelope.kind == "relay_envelope")
        val fields = keyMaterial.trim().split(Regex("\\s+")); require(fields.size == 3 && envelope.payload.key_id == fields[0])
        val nonce = hex(envelope.payload.nonce); require(nonce.size == 12)
        val cipher = Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.DECRYPT_MODE, SecretKeySpec(hex(fields[2]), "AES"), GCMParameterSpec(128, nonce)) }
        cipher.updateAAD(aad(envelope.session_id, envelope.source, envelope.destination, envelope.group, envelope.sequence, envelope.payload.key_id).toByteArray(Charsets.UTF_8))
        val plain = cipher.doFinal(Base64.getDecoder().decode(envelope.payload.ciphertext))
        return ProtocolCodec.decode(plain.toString(Charsets.UTF_8))
    }
    private fun aad(session: String, source: String, destination: String?, group: String, sequence: Long, key: String) = "clipsync-relay-v1\u0000$session\u0000$source\u0000${destination.orEmpty()}\u0000$group\u0000$sequence\u0000$key"
    private fun hex(value: String): ByteArray = value.chunked(2).map { it.toInt(16).toByte() }.toByteArray()
}
