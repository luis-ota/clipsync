package com.clipsync.android.data

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.net.wifi.WifiManager

@Suppress("DEPRECATION")
class NsdDiscovery(context: Context, private val onChanged: (List<DiscoveredServer>) -> Unit) {
    private val nsdManager = context.getSystemService(NsdManager::class.java)
    private val multicastLock = context.getSystemService(WifiManager::class.java)
        .createMulticastLock("clipsync-mdns").apply { setReferenceCounted(false) }
    private val servers = linkedMapOf<String, DiscoveredServer>()
    private val pendingResolutions = ArrayDeque<NsdServiceInfo>()
    private var running = false
    private var resolving = false
    private val listener = object : NsdManager.DiscoveryListener {
        override fun onDiscoveryStarted(serviceType: String) = Unit
        override fun onDiscoveryStopped(serviceType: String) = Unit
        override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) { stop() }
        override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) { running = false }
        override fun onServiceFound(info: NsdServiceInfo) {
            if (info.serviceType.startsWith(SERVICE_TYPE) &&
                pendingResolutions.none { it.serviceName == info.serviceName }
            ) {
                pendingResolutions.addLast(info)
                resolveNext()
            }
        }
        override fun onServiceLost(info: NsdServiceInfo) {
            servers.remove(info.serviceName)
            onChanged(servers.values.toList())
        }
    }

    fun start() {
        if (running) return
        running = true
        if (!multicastLock.isHeld) multicastLock.acquire()
        try {
            nsdManager.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, listener)
        } catch (_: RuntimeException) {
            running = false
            if (multicastLock.isHeld) multicastLock.release()
        }
    }
    fun restart() { stop(); start() }
    fun stop() {
        if (running) try { nsdManager.stopServiceDiscovery(listener) } catch (_: RuntimeException) { }
        running = false
        pendingResolutions.clear()
        if (multicastLock.isHeld) multicastLock.release()
    }
    private fun resolveNext() {
        if (!running || resolving) return
        val info = pendingResolutions.removeFirstOrNull() ?: return
        resolving = true
        try {
            nsdManager.resolveService(info, object : NsdManager.ResolveListener {
            override fun onResolveFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
                resolving = false
                resolveNext()
            }
            override fun onServiceResolved(serviceInfo: NsdServiceInfo) {
                resolving = false
                if (!running) return
                val host = serviceInfo.host?.hostAddress ?: run {
                    resolveNext()
                    return
                }
                val server = DiscoveredServer(
                    id = serviceInfo.serviceName,
                    name = serviceInfo.attributes["name"]?.toString(Charsets.UTF_8) ?: serviceInfo.serviceName,
                    host = host.substringBefore('%'),
                    port = serviceInfo.port,
                )
                servers[server.id] = server
                onChanged(servers.values.toList())
                resolveNext()
            }
            })
        } catch (_: RuntimeException) {
            resolving = false
            resolveNext()
        }
    }
    private companion object { const val SERVICE_TYPE = "_clipsync._tcp." }
}
