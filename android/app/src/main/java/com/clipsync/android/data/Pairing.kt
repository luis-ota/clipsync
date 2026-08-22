package com.clipsync.android.data

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.nio.charset.StandardCharsets
import java.util.Base64
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

class DeviceStore(context: Context) {
    private val preferences = context.getSharedPreferences("clipsync", Context.MODE_PRIVATE)
    private val registry = DeviceIdentityRegistry(object : DeviceIdentityPersistence {
        override fun get(key: String): String? = preferences.getString(key, null)
        override fun put(key: String, value: String) { preferences.edit().putString(key, value).apply() }
        override fun claimLegacy(key: String): String? {
            val legacy = preferences.getString(LEGACY_KEY, null) ?: return null
            preferences.edit().putString(key, legacy).remove(LEGACY_KEY).commit()
            return legacy
        }
    })

    fun deviceIdFor(serverId: String): String? = registry.deviceIdFor(serverId)
    fun save(serverId: String, deviceId: String) = registry.save(serverId, deviceId)

    fun loadEndpoints(): List<DiscoveredServer> = SecureEndpointStore(preferences).load()
    fun saveEndpoints(endpoints: List<DiscoveredServer>) = SecureEndpointStore(preferences).save(endpoints)
    fun saveRelayToken(reference: String, token: String) = SecureEndpointStore(preferences).saveToken(reference, token)
    fun relayToken(reference: String): String? = SecureEndpointStore(preferences).loadToken(reference)

    private companion object { const val LEGACY_KEY = "device_id" }
}

/** Armazena apenas endpoint metadata; o token é referenciado por nome e não por valor. */
private class SecureEndpointStore(private val preferences: android.content.SharedPreferences) {
    fun load(): List<DiscoveredServer> = runCatching {
        val encoded = preferences.getString(ENDPOINTS_KEY, null) ?: return emptyList()
        decrypt(encoded).split('\n').filter { it.isNotBlank() }.mapNotNull { line ->
            val fields = line.split('|')
            if (fields.size != 7) null else DiscoveredServer(fields[0], fields[1].ifBlank { null }, fields[0], fields[2], fields[3].toInt(), fields[4] == "tls", fields[5].ifBlank { null }, fields[6].ifBlank { null }, true)
        }
    }.getOrDefault(emptyList())

    fun save(endpoints: List<DiscoveredServer>) {
        val value = endpoints.joinToString("\n") { listOf(it.serviceName, it.serverId.orEmpty(), it.host, it.port.toString(), if (it.tls) "tls" else "plain", it.tlsFingerprint.orEmpty(), it.credentialRef.orEmpty()).joinToString("|") }
        preferences.edit().putString(ENDPOINTS_KEY, encrypt(value)).apply()
    }

    fun saveToken(reference: String, token: String) {
        preferences.edit().putString("relay_token.$reference", encrypt(token)).apply()
    }

    fun loadToken(reference: String): String? = preferences.getString("relay_token.$reference", null)?.let {
        runCatching { decrypt(it) }.getOrNull()
    }

    private fun key(): SecretKey {
        val store = java.security.KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (store.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").apply {
            init(KeyGenParameterSpec.Builder(KEY_ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM).setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE).build())
        }.generateKey()
    }
    private fun encrypt(value: String): String {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.ENCRYPT_MODE, key()) }
        return android.util.Base64.encodeToString(cipher.iv + cipher.doFinal(value.toByteArray(StandardCharsets.UTF_8)), android.util.Base64.NO_WRAP)
    }
    private fun decrypt(value: String): String {
        val bytes = android.util.Base64.decode(value, android.util.Base64.NO_WRAP)
        val cipher = Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, bytes.copyOfRange(0, 12))) }
        return cipher.doFinal(bytes.copyOfRange(12, bytes.size)).toString(StandardCharsets.UTF_8)
    }
    private companion object { const val ENDPOINTS_KEY = "remote_endpoints"; const val KEY_ALIAS = "clipsync.endpoint.metadata" }
}

internal interface DeviceIdentityPersistence {
    fun get(key: String): String?
    fun put(key: String, value: String)
    fun claimLegacy(key: String): String?
}

internal class DeviceIdentityRegistry(private val persistence: DeviceIdentityPersistence) {
    @Synchronized fun deviceIdFor(serverId: String): String? {
        val key = key(serverId)
        return persistence.get(key) ?: persistence.claimLegacy(key)
    }

    @Synchronized fun save(serverId: String, deviceId: String) = persistence.put(key(serverId), deviceId)

    private fun key(serverId: String): String {
        val encoded = Base64.getUrlEncoder().withoutPadding().encodeToString(serverId.toByteArray())
        return "device_id.$encoded"
    }
}

sealed interface ProtocolAction {
    data class Send(val message: Message) : ProtocolAction
    data class RequestPin(val challenge: Message.PairChallenge) : ProtocolAction
    data class Paired(val result: Message.PairOk) : ProtocolAction
    data class PairingFailed(val reason: String) : ProtocolAction
    data class Clipboard(val message: Message) : ProtocolAction
    data class FatalError(val message: String) : ProtocolAction
}

class ProtocolEngine(private val device: DeviceInfo) {
    private var challenge: Message.PairChallenge? = null
    fun onOpen(): Message.Hello = Message.Hello(device = device)

    fun onMessage(message: Message): List<ProtocolAction> = when (message) {
        is Message.PairChallenge -> {
            challenge = message
            listOf(ProtocolAction.RequestPin(message))
        }
        is Message.PairOk -> {
            challenge = null
            listOf(ProtocolAction.Paired(message))
        }
        is Message.PairFail -> {
            challenge = null
            listOf(ProtocolAction.PairingFailed(message.message))
        }
        is Message.Ping -> listOf(ProtocolAction.Send(Message.Pong(message.ts)))
        is Message.ClipboardText, is Message.ClipboardImage, is Message.ClipboardHtml ->
            listOf(ProtocolAction.Clipboard(message))
        is Message.Error -> listOf(ProtocolAction.FatalError(message.message))
        is Message.Hello, is Message.PairSubmit, is Message.Pong -> emptyList()
    }

    fun submitPin(pin: String, nowEpochSeconds: Long = System.currentTimeMillis() / 1000): Message.PairSubmit? {
        val current = challenge ?: return null
        if (!pin.matches(Regex("[0-9]{6}")) || current.expires_at <= nowEpochSeconds) return null
        return Message.PairSubmit(current.challenge_id, pin, current.nonce)
    }
}
