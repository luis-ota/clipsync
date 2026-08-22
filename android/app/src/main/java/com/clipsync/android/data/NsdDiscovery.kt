package com.clipsync.android.data

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.net.wifi.WifiManager

@Suppress("DEPRECATION")
class NsdDiscovery(context: Context, private val onChanged: (DiscoverySnapshot) -> Unit) {
    private val nsdManager = context.getSystemService(NsdManager::class.java)
    private val multicastLock = context.getSystemService(WifiManager::class.java)
        .createMulticastLock("clipsync-mdns").apply { setReferenceCounted(false) }
    private val servers = linkedMapOf<String, DiscoveredServer>()
    private val lostServices = mutableSetOf<String>()
    private val pendingResolutions = ArrayDeque<NsdServiceInfo>()
    private var running = false
    private var resolving = false
    private var epoch = 0L
    private var listener: NsdManager.DiscoveryListener? = null

    fun start() {
        if (running) return
        running = true
        epoch++
        val currentEpoch = epoch
        servers.clear()
        lostServices.clear()
        pendingResolutions.clear()
        resolving = false
        onChanged(DiscoverySnapshot(currentEpoch, emptyList()))
        val currentListener = listener(currentEpoch).also { listener = it }
        if (!multicastLock.isHeld) multicastLock.acquire()
        try {
            nsdManager.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, currentListener)
        } catch (_: RuntimeException) {
            running = false
            if (multicastLock.isHeld) multicastLock.release()
        }
    }
    fun restart() { stop(); start() }
    fun stop() {
        val currentListener = listener
        if (running && currentListener != null) try {
            nsdManager.stopServiceDiscovery(currentListener)
        } catch (_: RuntimeException) { }
        running = false
        epoch++
        listener = null
        pendingResolutions.clear()
        resolving = false
        if (multicastLock.isHeld) multicastLock.release()
    }
    private fun listener(listenerEpoch: Long) = object : NsdManager.DiscoveryListener {
        override fun onDiscoveryStarted(serviceType: String) = Unit
        override fun onDiscoveryStopped(serviceType: String) = Unit
        override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
            if (listenerEpoch == epoch) stop()
        }
        override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) = Unit
        override fun onServiceFound(info: NsdServiceInfo) {
            lostServices.remove(info.serviceName)
            if (running && listenerEpoch == epoch && info.serviceType.startsWith(SERVICE_TYPE) &&
                pendingResolutions.none { it.serviceName == info.serviceName }
            ) {
                pendingResolutions.addLast(info)
                resolveNext(listenerEpoch)
            }
        }
        override fun onServiceLost(info: NsdServiceInfo) {
            if (listenerEpoch != epoch) return
            lostServices.add(info.serviceName)
            pendingResolutions.removeAll { it.serviceName == info.serviceName }
            servers.remove(info.serviceName)
            publish(listenerEpoch)
        }
    }
    private fun resolveNext(resolveEpoch: Long) {
        if (resolveEpoch != epoch) return
        if (!running || resolving) return
        val info = pendingResolutions.removeFirstOrNull() ?: return
        resolving = true
        try {
            nsdManager.resolveService(info, object : NsdManager.ResolveListener {
            override fun onResolveFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
                if (resolveEpoch != epoch) return
                resolving = false
                resolveNext(resolveEpoch)
            }
            override fun onServiceResolved(serviceInfo: NsdServiceInfo) {
                if (resolveEpoch != epoch) return
                resolving = false
                if (!running) return
                if (lostServices.contains(serviceInfo.serviceName)) {
                    resolveNext(resolveEpoch)
                    return
                }
                val host = serviceInfo.host?.hostAddress ?: run {
                    resolveNext(resolveEpoch)
                    return
                }
                val server = DiscoveredServer(
                    serviceName = serviceInfo.serviceName,
                    serverId = serviceInfo.attributes["server_id"]?.toString(Charsets.UTF_8),
                    name = serviceInfo.attributes["name"]?.toString(Charsets.UTF_8) ?: serviceInfo.serviceName,
                    host = host.substringBefore('%'),
                    port = serviceInfo.port,
                    tls = serviceInfo.attributes["tls"]?.toString(Charsets.UTF_8) == "1",
                    tlsFingerprint = serviceInfo.attributes["tls_fingerprint"]?.toString(Charsets.UTF_8),
                )
                servers[serviceInfo.serviceName] = server
                publish(resolveEpoch)
                resolveNext(resolveEpoch)
            }
            })
        } catch (_: RuntimeException) {
            resolving = false
            resolveNext(resolveEpoch)
        }
    }
    private fun publish(publishEpoch: Long) {
        if (publishEpoch == epoch) onChanged(DiscoverySnapshot(epoch, servers.values.toList()))
    }
    private companion object { const val SERVICE_TYPE = "_clipsync._tcp." }
}
