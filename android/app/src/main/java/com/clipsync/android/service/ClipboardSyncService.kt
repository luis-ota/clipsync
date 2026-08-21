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
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.launch

class ClipboardSyncService : Service(), WebSocketClient.Callbacks {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private lateinit var deviceStore: DeviceStore
    private lateinit var discovery: NsdDiscovery
    private lateinit var webSocket: WebSocketClient
    private lateinit var clipboard: ClipboardWatcher
    private var engine: ProtocolEngine? = null
    private var selectedServer: DiscoveredServer? = null
    private var autoSelected = false
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) { scope.launch { discovery.restart() } }
    }

    override fun onCreate() {
        super.onCreate()
        createNotificationChannel()
        startForeground(NOTIFICATION_ID, notification("Iniciando descoberta"))
        deviceStore = DeviceStore(this)
        webSocket = WebSocketClient(scope, this)
        clipboard = ClipboardWatcher(this, scope, { deviceStore.deviceId }, ::sendClipboard).also { it.start() }
        discovery = NsdDiscovery(this) { servers ->
            scope.launch {
                AppRepository.setServers(servers)
                if (!autoSelected && selectedServer == null && servers.isNotEmpty()) {
                    autoSelected = true
                    AppRepository.select(servers.first())
                }
            }
        }.also { it.start() }
        getSystemService(ConnectivityManager::class.java).registerDefaultNetworkCallback(networkCallback)
        scope.launch { AppRepository.selections.collectLatest(::connect) }
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
        webSocket.disconnect()
        scope.cancel()
        super.onDestroy()
    }
    private fun connect(server: DiscoveredServer) {
        selectedServer = server
        engine = null
        webSocket.connect(server)
    }
    override fun onConnecting(delayMillis: Long?) {
        val detail = if (delayMillis == null) "Conectando a ${selectedServer?.name.orEmpty()}"
        else "Reconectando em ${delayMillis / 1000}s"
        setStatus(ConnectionStatus.CONNECTING, detail)
    }
    override fun onOpen() {
        engine = ProtocolEngine(DeviceInfo(
            name = "${Build.MANUFACTURER} ${Build.MODEL}".trim(),
            id = deviceStore.deviceId,
            os_version = "Android ${Build.VERSION.RELEASE}",
            app_version = "clipsync-android ${BuildConfig.VERSION_NAME}",
        ))
        send(engine!!.onOpen())
        setStatus(ConnectionStatus.AUTHENTICATING, "Autenticando dispositivo")
    }
    override fun onMessage(payload: String) {
        scope.launch {
            val message = try { ProtocolCodec.decode(payload) } catch (_: Exception) {
                setStatus(ConnectionStatus.ERROR, "Mensagem invalida recebida")
                return@launch
            }
            engine?.onMessage(message)?.forEach(::handleAction)
        }
    }
    override fun onDisconnected(reason: String) {
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
                deviceStore.deviceId = action.result.device_id
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
        if (AppRepository.state.value.status == ConnectionStatus.CONNECTED) send(message)
    }
    private fun send(message: Message) { webSocket.send(ProtocolCodec.encode(message)) }
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
