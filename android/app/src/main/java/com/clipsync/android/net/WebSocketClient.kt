package com.clipsync.android.net

import com.clipsync.android.data.DiscoveredServer
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import java.security.MessageDigest
import java.security.cert.X509Certificate
import javax.net.ssl.SSLContext
import javax.net.ssl.X509TrustManager

class ReconnectPolicy(private val initialMillis: Long = 1_000, private val maximumMillis: Long = 60_000) {
    fun delayMillis(attempt: Int): Long {
        if (attempt <= 0) return initialMillis
        var value = initialMillis
        repeat(attempt.coerceAtMost(62)) {
            if (value >= maximumMillis / 2) return maximumMillis
            value *= 2
        }
        return value.coerceAtMost(maximumMillis)
    }
}

class WebSocketClient(
    private val callbacks: Callbacks,
    private val client: OkHttpClient = OkHttpClient.Builder()
        .pingInterval(30, TimeUnit.SECONDS).readTimeout(0, TimeUnit.MILLISECONDS).build(),
    private val policy: ReconnectPolicy = ReconnectPolicy(),
    private val credentialProvider: (String) -> String? = { null },
) {
    interface Callbacks {
        fun onConnecting(generation: Long, delayMillis: Long?)
        fun onOpen(generation: Long)
        fun onMessage(generation: Long, payload: String)
        fun onBinaryMessage(generation: Long, payload: ByteArray) {}
        fun onDisconnected(generation: Long, reason: String)
        fun onSendFailed(generation: Long)
    }

    private sealed interface Event {
        data class Connect(val generation: Long, val server: DiscoveredServer) : Event
        data class Send(val generation: Long, val payload: String) : Event
        data class SendBinary(val generation: Long, val payload: ByteArray) : Event
        data object Disconnect : Event
        data object Shutdown : Event
        data class Opened(val generation: Long, val socket: WebSocket) : Event
        data class Message(val generation: Long, val socket: WebSocket, val payload: String) : Event
        data class BinaryMessage(val generation: Long, val socket: WebSocket, val payload: ByteArray) : Event
        data class Failed(val generation: Long, val socket: WebSocket, val reason: String) : Event
        data class Retry(val generation: Long) : Event
    }

    private val events = Channel<Event>(Channel.UNLIMITED)
    private val actorScope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var target: DiscoveredServer? = null
    private var socket: WebSocket? = null
    private var reconnectJob: Job? = null
    private var attempt = 0
    private var generation = -1L

    init {
        actorScope.launch {
            for (event in events) {
                if (event == Event.Shutdown) {
                    target = null
                    closeCurrent("client shutdown")
                    events.close()
                    break
                }
                reduce(event)
            }
            actorScope.cancel()
        }
    }

    fun connect(server: DiscoveredServer, generation: Long) {
        events.trySend(Event.Connect(generation, server))
    }

    fun send(payload: String, generation: Long) {
        events.trySend(Event.Send(generation, payload))
    }
    fun sendBinary(payload: ByteArray, generation: Long) {
        events.trySend(Event.SendBinary(generation, payload))
    }

    fun disconnect() { events.trySend(Event.Disconnect) }
    fun shutdown() { events.trySend(Event.Shutdown) }

    private fun reduce(event: Event) {
        when (event) {
            is Event.Connect -> {
                closeCurrent("new session")
                generation = event.generation
                target = event.server
                attempt = 0
                open()
            }
            is Event.Send -> if (event.generation == generation) {
                val current = socket
                if (current == null || !current.send(event.payload)) {
                    callbacks.onSendFailed(generation)
                    if (current != null) {
                        current.cancel()
                        fail(current, "fila de envio WebSocket cheia")
                    }
                }
            }
            is Event.SendBinary -> if (event.generation == generation) {
                val current = socket
                if (current == null || !current.send(ByteString.of(*event.payload))) callbacks.onSendFailed(generation)
            }
            Event.Disconnect -> {
                generation++
                target = null
                closeCurrent("client disconnect")
            }
            Event.Shutdown -> Unit
            is Event.Opened -> if (isCurrent(event.generation, event.socket)) {
                attempt = 0
                callbacks.onOpen(generation)
            } else {
                event.socket.close(1000, "stale")
            }
            is Event.Message -> if (isCurrent(event.generation, event.socket)) {
                callbacks.onMessage(generation, event.payload)
            }
            is Event.BinaryMessage -> if (isCurrent(event.generation, event.socket)) {
                callbacks.onBinaryMessage(generation, event.payload)
            }
            is Event.Failed -> if (isCurrent(event.generation, event.socket)) fail(event.socket, event.reason)
            is Event.Retry -> if (event.generation == generation && target != null) open()
        }
    }

    private fun open() {
        val server = target ?: return
        val connectionGeneration = generation
        callbacks.onConnecting(connectionGeneration, null)
        if (!server.tls || !isValidTlsFingerprint(server.tlsFingerprint)) {
            callbacks.onDisconnected(connectionGeneration, "servidor sem TLS/pinning; compatibilidade insegura desabilitada")
            return
        }
        val requestBuilder = Request.Builder().url("wss://${server.host}:${server.port}/ws")
        if (server.remote) {
            val token = server.credentialRef?.let(credentialProvider)
            if (token.isNullOrBlank()) {
                callbacks.onDisconnected(connectionGeneration, "credencial relay ausente")
                return
            }
            requestBuilder.header("Authorization", "Bearer $token")
        }
        val request = requestBuilder.build()
        val listener = object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                events.trySend(Event.Opened(connectionGeneration, webSocket))
            }
            override fun onMessage(webSocket: WebSocket, text: String) {
                events.trySend(Event.Message(connectionGeneration, webSocket, text))
            }
            override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
                events.trySend(Event.BinaryMessage(connectionGeneration, webSocket, bytes.toByteArray()))
            }
            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                events.trySend(Event.Failed(connectionGeneration, webSocket, reason.ifBlank { "conexao encerrada" }))
            }
            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                events.trySend(Event.Failed(connectionGeneration, webSocket, t.message ?: "falha de rede"))
            }
        }
        socket = pinnedClient(server.tlsFingerprint!!).newWebSocket(request, listener)
    }

    private fun pinnedClient(expected: String): OkHttpClient {
        val normalized = expected.lowercase()
        val trustManager = object : X509TrustManager {
            override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
            override fun checkClientTrusted(chain: Array<X509Certificate>, authType: String) = Unit
            override fun checkServerTrusted(chain: Array<X509Certificate>, authType: String) {
                if (chain.isEmpty() || sha256(chain[0].encoded) != normalized)
                    throw java.security.cert.CertificateException("fingerprint TLS não corresponde ao mDNS")
            }
        }
        val context = SSLContext.getInstance("TLS").apply { init(null, arrayOf(trustManager), null) }
        return client.newBuilder()
            .sslSocketFactory(context.socketFactory, trustManager)
            .hostnameVerifier { _, _ -> true } // autenticação é exclusivamente pelo pin acima
            .build()
    }

    private fun sha256(bytes: ByteArray): String = MessageDigest.getInstance("SHA-256").digest(bytes)
        .joinToString("") { "%02x".format(it) }

    private fun fail(failedSocket: WebSocket, reason: String) {
        if (failedSocket !== socket || target == null || reconnectJob?.isActive == true) return
        socket = null
        callbacks.onDisconnected(generation, reason)
        val wait = policy.delayMillis(attempt++)
        callbacks.onConnecting(generation, wait)
        val retryGeneration = generation
        reconnectJob = actorScope.launch {
            delay(wait)
            events.send(Event.Retry(retryGeneration))
        }
    }

    private fun isCurrent(eventGeneration: Long, eventSocket: WebSocket): Boolean =
        eventGeneration == generation && eventSocket === socket

    private fun closeCurrent(reason: String) {
        reconnectJob?.cancel()
        reconnectJob = null
        socket?.close(1000, reason)
        socket = null
    }
}

internal fun isValidTlsFingerprint(value: String?): Boolean =
    value?.matches(Regex("[0-9a-fA-F]{64}")) == true
