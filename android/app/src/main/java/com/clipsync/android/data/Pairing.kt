package com.clipsync.android.data

import android.content.Context

class DeviceStore(context: Context) {
    private val preferences = context.getSharedPreferences("clipsync", Context.MODE_PRIVATE)
    var deviceId: String?
        get() = preferences.getString("device_id", null)
        set(value) { preferences.edit().putString("device_id", value).apply() }
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
