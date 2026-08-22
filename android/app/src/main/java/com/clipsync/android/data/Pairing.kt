package com.clipsync.android.data

import android.content.Context
import java.util.Base64

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

    private companion object { const val LEGACY_KEY = "device_id" }
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
