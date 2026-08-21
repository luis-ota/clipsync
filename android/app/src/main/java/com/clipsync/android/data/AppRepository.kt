package com.clipsync.android.data

import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update

data class DiscoveredServer(val id: String, val name: String, val host: String, val port: Int)
enum class ConnectionStatus { DISCOVERING, CONNECTING, AUTHENTICATING, WAITING_FOR_PIN, CONNECTED, DISCONNECTED, ERROR }
data class AppUiState(
    val servers: List<DiscoveredServer> = emptyList(),
    val selectedServerId: String? = null,
    val status: ConnectionStatus = ConnectionStatus.DISCOVERING,
    val statusDetail: String = "Procurando servidores na rede local",
    val pinExpiresAt: Long? = null,
)

object AppRepository {
    private val mutableState = MutableStateFlow(AppUiState())
    val state = mutableState.asStateFlow()
    private val mutableSelections = MutableSharedFlow<DiscoveredServer>(extraBufferCapacity = 1)
    val selections = mutableSelections.asSharedFlow()
    private val mutablePins = MutableSharedFlow<String>(extraBufferCapacity = 1)
    val pins = mutablePins.asSharedFlow()

    fun setServers(servers: Collection<DiscoveredServer>) {
        mutableState.update { it.copy(servers = servers.sortedBy(DiscoveredServer::name)) }
    }
    fun select(server: DiscoveredServer) {
        mutableState.update { it.copy(selectedServerId = server.id) }
        mutableSelections.tryEmit(server)
    }
    fun submitPin(pin: String) { mutablePins.tryEmit(pin) }
    fun updateStatus(status: ConnectionStatus, detail: String, expiresAt: Long? = null) {
        mutableState.update { it.copy(status = status, statusDetail = detail, pinExpiresAt = expiresAt) }
    }
}
