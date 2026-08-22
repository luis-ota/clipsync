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
    val tls: Boolean = true,
    val tlsFingerprint: String? = null,
    val credentialRef: String? = null,
    val remote: Boolean = false,
    val deviceId: String? = null,
) {
    val id: String get() = serverId ?: "legacy:$serviceName"
}
enum class RouteKind { LAN, RELAY }

class RouteStateMachine(private val routes: List<DiscoveredServer>) {
    private var index = 0
    fun current(): DiscoveredServer? = routes.getOrNull(index)
    fun failover(): DiscoveredServer? {
        if (routes.isEmpty()) return null
        val currentKind = kind()
        val next = (1..routes.size).firstOrNull { offset ->
            val candidate = routes[(index + offset) % routes.size]
            candidate.remote != (currentKind == RouteKind.RELAY)
        } ?: 1
        index = (index + next) % routes.size
        return current()
    }
    fun kind(): RouteKind? = current()?.let { if (it.remote) RouteKind.RELAY else RouteKind.LAN }
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
    private val remoteServers = linkedMapOf<String, DiscoveredServer>()
    private var routeMachine = RouteStateMachine(emptyList())

    fun setRemoteEndpoints(endpoints: List<DiscoveredServer>) {
        remoteServers.clear()
        endpoints.forEach { remoteServers[it.id] = it.copy(remote = true) }
        publishServers()
    }

    fun addRemoteEndpoint(endpoint: DiscoveredServer) {
        remoteServers[endpoint.id] = endpoint.copy(remote = true)
        publishServers()
    }

    fun setServers(snapshot: DiscoverySnapshot) {
        if (snapshot.epoch < discoveryEpoch) return
        discoveryEpoch = snapshot.epoch
        val servers = (snapshot.servers + remoteServers.values)
            .distinctBy(DiscoveredServer::id).sortedBy(DiscoveredServer::name)
        mutableState.update { it.copy(servers = servers) }
        routeMachine = RouteStateMachine(servers)
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
    fun alternateTarget(current: DiscoveredServer): DiscoveredServer? {
        val routes = mutableState.value.servers
        if (routeMachine.current()?.id != current.id) {
            routeMachine = RouteStateMachine(listOf(current) + routes.filter { it.id != current.id })
        }
        return routeMachine.failover()
    }

    private fun publishServers() {
        val servers = (mutableState.value.servers.filterNot(DiscoveredServer::remote) + remoteServers.values)
            .distinctBy(DiscoveredServer::id).sortedBy(DiscoveredServer::name)
        mutableState.update { it.copy(servers = servers) }
        routeMachine = RouteStateMachine(servers)
        mutableTargets.value = servers.firstOrNull { it.id == mutableState.value.selectedServerId }
    }
}
