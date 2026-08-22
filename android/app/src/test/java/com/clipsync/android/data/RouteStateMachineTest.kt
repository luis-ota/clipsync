package com.clipsync.android.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class RouteStateMachineTest {
    private fun route(name: String, remote: Boolean) = DiscoveredServer(name, null, name, "host", 8765, true, "a".repeat(64), null, remote)

    @Test fun `failover alterna LAN e relay sem criar server id`() {
        val machine = RouteStateMachine(listOf(route("lan", false), route("relay", true)))
        assertEquals(RouteKind.LAN, machine.kind())
        assertNull(machine.current()?.serverId)
        assertEquals("relay", machine.failover()?.serviceName)
        assertEquals(RouteKind.RELAY, machine.kind())
    }
}
