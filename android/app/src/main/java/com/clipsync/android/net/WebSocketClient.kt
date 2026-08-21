package com.clipsync.android.net

import com.clipsync.android.data.DiscoveredServer
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener

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
    private val scope: CoroutineScope,
    private val callbacks: Callbacks,
    private val client: OkHttpClient = OkHttpClient.Builder()
        .pingInterval(30, TimeUnit.SECONDS)
        .readTimeout(0, TimeUnit.MILLISECONDS)
        .build(),
    private val policy: ReconnectPolicy = ReconnectPolicy(),
) {
    interface Callbacks {
        fun onConnecting(delayMillis: Long?)
        fun onOpen()
        fun onMessage(payload: String)
        fun onDisconnected(reason: String)
    }
    private var target: DiscoveredServer? = null
    private var socket: WebSocket? = null
    private var reconnectJob: Job? = null
    private var attempt = 0
    private var generation = 0

    fun connect(server: DiscoveredServer) {
        disconnect()
        target = server
        generation++
        attempt = 0
        open(generation)
    }
    fun send(payload: String): Boolean = socket?.send(payload) == true
    fun disconnect() {
        target = null
        generation++
        reconnectJob?.cancel()
        reconnectJob = null
        socket?.close(1000, "client disconnect")
        socket = null
    }
    private fun open(connectionGeneration: Int) {
        val server = target ?: return
        callbacks.onConnecting(null)
        val request = Request.Builder().url("ws://${server.host}:${server.port}/ws").build()
        socket = client.newWebSocket(request, object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                if (connectionGeneration != generation) {
                    webSocket.close(1000, "stale")
                    return
                }
                attempt = 0
                callbacks.onOpen()
            }
            override fun onMessage(webSocket: WebSocket, text: String) {
                if (connectionGeneration == generation) callbacks.onMessage(text)
            }
            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                failed(connectionGeneration, reason.ifBlank { "conexao encerrada" })
            }
            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                failed(connectionGeneration, t.message ?: "falha de rede")
            }
        })
    }
    private fun failed(connectionGeneration: Int, reason: String) {
        if (connectionGeneration != generation || target == null || reconnectJob?.isActive == true) return
        callbacks.onDisconnected(reason)
        val wait = policy.delayMillis(attempt++)
        callbacks.onConnecting(wait)
        reconnectJob = scope.launch {
            delay(wait)
            if (connectionGeneration == generation && target != null) open(connectionGeneration)
        }
    }
}
