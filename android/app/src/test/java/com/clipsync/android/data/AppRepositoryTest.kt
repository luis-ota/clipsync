package com.clipsync.android.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class AppRepositoryTest {
    private fun server(id: String, host: String) = DiscoveredServer("service-$id", id, id, host, 8765)

    @Test fun `endpoint selecionado acompanha snapshot autoritativo e ignora epoca antiga`() {
        AppRepository.setServers(DiscoverySnapshot(100, listOf(server("server", "192.168.1.2"))))
        AppRepository.select("server")
        assertEquals("192.168.1.2", AppRepository.targets.value?.host)

        AppRepository.setServers(DiscoverySnapshot(101, emptyList()))
        assertNull(AppRepository.targets.value)
        AppRepository.setServers(DiscoverySnapshot(100, listOf(server("server", "192.168.1.99"))))
        assertNull(AppRepository.targets.value)

        AppRepository.setServers(DiscoverySnapshot(102, listOf(server("server", "10.0.0.4"))))
        assertEquals("10.0.0.4", AppRepository.targets.value?.host)

        val first = DiscoveredServer("one", "server-a", "desktop", "10.0.0.1", 8765)
        val second = DiscoveredServer("two", "server-b", "desktop", "10.0.0.2", 8765)
        AppRepository.setServers(DiscoverySnapshot(103, listOf(first, second)))
        assertEquals(setOf("server-a", "server-b"), AppRepository.state.value.servers.map { it.id }.toSet())
    }
}
