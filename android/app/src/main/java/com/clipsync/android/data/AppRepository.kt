package com.clipsync.android.data

import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update

data class DiscoveredServer(
    val serviceName: String,
    val serverId: String?,
    val name: String,
    val host: String,
    val port: Int,
) {
    val id: String get() = serverId ?: "legacy:$serviceName"
}
data class DiscoverySnapshot(val epoch: Long, val servers: List<DiscoveredServer>)
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
    private val mutableTargets = MutableStateFlow<DiscoveredServer?>(null)
    val targets = mutableTargets.asStateFlow()
    private val mutablePins = MutableSharedFlow<String>(extraBufferCapacity = 1)
    val pins = mutablePins.asSharedFlow()

    private var discoveryEpoch = -1L

    fun setServers(snapshot: DiscoverySnapshot) {
        if (snapshot.epoch < discoveryEpoch) return
        discoveryEpoch = snapshot.epoch
        val servers = snapshot.servers.sortedBy(DiscoveredServer::name)
        mutableState.update { it.copy(servers = servers) }
        mutableTargets.value = servers.firstOrNull { it.id == mutableState.value.selectedServerId }
    }
    fun select(serverId: String) {
        mutableState.update { it.copy(selectedServerId = serverId) }
        mutableTargets.value = mutableState.value.servers.firstOrNull { it.id == serverId }
    }
    fun submitPin(pin: String) { mutablePins.tryEmit(pin) }
    fun updateStatus(status: ConnectionStatus, detail: String, expiresAt: Long? = null) {
        mutableState.update { it.copy(status = status, statusDetail = detail, pinExpiresAt = expiresAt) }
    }
}
