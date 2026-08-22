package com.clipsync.android.service

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.net.ConnectivityManager
import android.net.Network
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import com.clipsync.android.BuildConfig
import com.clipsync.android.MainActivity
import com.clipsync.android.R
import com.clipsync.android.data.AppRepository
import com.clipsync.android.data.ConnectionStatus
import com.clipsync.android.data.DeviceInfo
import com.clipsync.android.data.DeviceStore
import com.clipsync.android.data.DiscoveredServer
import com.clipsync.android.data.Message
import com.clipsync.android.data.NsdDiscovery
import com.clipsync.android.data.ProtocolAction
import com.clipsync.android.data.ProtocolCodec
import com.clipsync.android.data.ProtocolEngine
import com.clipsync.android.net.WebSocketClient
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

internal class SessionGeneration {
    var current: Long = 0
        private set
    fun advance(): Long = ++current
    fun accepts(candidate: Long): Boolean = candidate == current
}

class ClipboardSyncService : Service(), WebSocketClient.Callbacks {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private lateinit var deviceStore: DeviceStore
    private lateinit var discovery: NsdDiscovery
    private lateinit var webSocket: WebSocketClient
    private lateinit var clipboard: ClipboardWatcher
    private var engine: ProtocolEngine? = null
    private var selectedServer: DiscoveredServer? = null
    private var currentDeviceId: String? = null
    private val sessions = SessionGeneration()
    private val incomingMessages = Channel<Pair<Long, String>>(Channel.UNLIMITED)
    private var autoSelected = false
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) { scope.launch { discovery.restart() } }
    }

    override fun onCreate() {
        super.onCreate()
        createNotificationChannel()
        startForeground(NOTIFICATION_ID, notification("Iniciando descoberta"))
        deviceStore = DeviceStore(this)
        webSocket = WebSocketClient(this)
        clipboard = ClipboardWatcher(this, scope, { currentDeviceId }, ::sendClipboard).also { it.start() }
        discovery = NsdDiscovery(this) { snapshot ->
            scope.launch {
                AppRepository.setServers(snapshot)
                if (!autoSelected && AppRepository.state.value.selectedServerId == null && snapshot.servers.isNotEmpty()) {
                    autoSelected = true
                    AppRepository.select(snapshot.servers.first().id)
                }
            }
        }.also { it.start() }
        getSystemService(ConnectivityManager::class.java).registerDefaultNetworkCallback(networkCallback)
        scope.launch { AppRepository.targets.collectLatest(::connect) }
        scope.launch {
            for ((generation, payload) in incomingMessages) handlePayload(generation, payload)
        }
        scope.launch {
            AppRepository.pins.collectLatest { pin ->
                val submit = engine?.submitPin(pin)
                if (submit == null) setStatus(ConnectionStatus.ERROR, "PIN invalido ou expirado")
                else {
                    send(submit)
                    setStatus(ConnectionStatus.AUTHENTICATING, "Validando PIN")
                }
            }
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int = START_STICKY
    override fun onBind(intent: Intent?): IBinder? = null
    override fun onTimeout(startId: Int, fgsType: Int) { stopSelf(startId) }
    override fun onDestroy() {
        getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(networkCallback)
        clipboard.stop()
        discovery.stop()
        incomingMessages.close()
        webSocket.shutdown()
        scope.cancel()
        super.onDestroy()
    }
    private fun connect(server: DiscoveredServer?) {
        val generation = sessions.advance()
        selectedServer = server
        engine = null
        currentDeviceId = server?.serverId?.let(deviceStore::deviceIdFor)
        if (server == null) {
            webSocket.disconnect()
            setStatus(ConnectionStatus.DISCOVERING, "Servidor selecionado indisponivel nesta rede")
        } else {
            webSocket.connect(server, generation)
        }
    }
    override fun onConnecting(generation: Long, delayMillis: Long?) {
        if (!sessions.accepts(generation)) return
        val detail = if (delayMillis == null) "Conectando a ${selectedServer?.name.orEmpty()}"
        else "Reconectando em ${delayMillis / 1000}s"
        setStatus(ConnectionStatus.CONNECTING, detail)
    }
    override fun onOpen(generation: Long) {
        if (!sessions.accepts(generation)) return
        engine = ProtocolEngine(DeviceInfo(
            name = "${Build.MANUFACTURER} ${Build.MODEL}".trim(),
            id = currentDeviceId,
            os_version = "Android ${Build.VERSION.RELEASE}",
            app_version = "clipsync-android ${BuildConfig.VERSION_NAME}",
        ))
        send(engine!!.onOpen())
        setStatus(ConnectionStatus.AUTHENTICATING, "Autenticando dispositivo")
    }
    override fun onMessage(generation: Long, payload: String) {
        incomingMessages.trySend(generation to payload)
    }
    private suspend fun handlePayload(generation: Long, payload: String) {
        if (!sessions.accepts(generation)) return
        val message = try { withContext(Dispatchers.Default) { ProtocolCodec.decode(payload) } } catch (_: Exception) {
            setStatus(ConnectionStatus.ERROR, "Mensagem invalida recebida")
            return
        }
        if (!sessions.accepts(generation)) return
        engine?.onMessage(message)?.forEach(::handleAction)
    }
    override fun onDisconnected(generation: Long, reason: String) {
        if (!sessions.accepts(generation)) return
        engine = null
        setStatus(ConnectionStatus.DISCONNECTED, reason)
    }
    private fun handleAction(action: ProtocolAction) {
        when (action) {
            is ProtocolAction.Send -> send(action.message)
            is ProtocolAction.RequestPin -> setStatus(
                ConnectionStatus.WAITING_FOR_PIN,
                "Digite o PIN exibido no computador",
                action.challenge.expires_at,
            )
            is ProtocolAction.Paired -> {
                val discoveredId = selectedServer?.serverId
                val protocolId = action.result.server_id
                if (discoveredId != null && protocolId != null && discoveredId != protocolId) {
                    setStatus(ConnectionStatus.ERROR, "Identidade do servidor divergiu do discovery")
                    webSocket.disconnect()
                    return
                }
                // A instância mDNS é a chave de compatibilidade para servidores
                // antigos que ainda não anunciam nem retornam server_id.
                val serverId = protocolId ?: discoveredId ?: selectedServer?.id
                if (serverId == null) return
                deviceStore.save(serverId, action.result.device_id)
                currentDeviceId = action.result.device_id
                setStatus(ConnectionStatus.CONNECTED, "Conectado a ${action.result.server_name}")
            }
            is ProtocolAction.PairingFailed -> setStatus(ConnectionStatus.ERROR, action.reason)
            is ProtocolAction.Clipboard -> when (val message = action.message) {
                is Message.ClipboardText -> clipboard.writeText(message)
                is Message.ClipboardImage -> clipboard.writeImage(message)
                is Message.ClipboardHtml -> {
                    val text = message.alt ?: message.html
                    clipboard.writeText(Message.ClipboardText(
                        "text/plain;charset=utf-8", text, message.origin, ClipboardWatcher.sha256(text.toByteArray()),
                    ))
                }
                else -> Unit
            }
            is ProtocolAction.FatalError -> {
                setStatus(ConnectionStatus.ERROR, action.message)
                webSocket.disconnect()
            }
        }
    }
    private fun sendClipboard(message: Message) {
        if (AppRepository.state.value.status != ConnectionStatus.CONNECTED) return
        val sendGeneration = sessions.current
        scope.launch {
            val payload = withContext(Dispatchers.Default) { ProtocolCodec.encode(message) }
            if (payload.toByteArray().size > ImageLimits.MAX_WEBSOCKET_MESSAGE_BYTES) {
                setStatus(ConnectionStatus.ERROR, "Conteudo excede o limite de envio")
            } else if (sessions.accepts(sendGeneration)) {
                webSocket.send(payload, sendGeneration)
            }
        }
    }
    private fun send(message: Message) { webSocket.send(ProtocolCodec.encode(message), sessions.current) }
    override fun onSendFailed(generation: Long) {
        if (sessions.accepts(generation)) {
            setStatus(ConnectionStatus.ERROR, "Falha ao enfileirar mensagem para envio")
        }
    }
    private fun setStatus(status: ConnectionStatus, detail: String, expiresAt: Long? = null) {
        AppRepository.updateStatus(status, detail, expiresAt)
        getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification(detail))
    }
    private fun notification(text: String) = NotificationCompat.Builder(this, CHANNEL_ID)
        .setSmallIcon(android.R.drawable.stat_notify_sync)
        .setContentTitle("ClipSync ativo")
        .setContentText(text)
        .setOngoing(true)
        .setOnlyAlertOnce(true)
        .setContentIntent(PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )).build()
    private fun createNotificationChannel() {
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel(CHANNEL_ID, getString(R.string.notification_channel_name), NotificationManager.IMPORTANCE_LOW)
        )
    }
    private companion object {
        const val CHANNEL_ID = "clipboard_sync"
        const val NOTIFICATION_ID = 1
    }
}
