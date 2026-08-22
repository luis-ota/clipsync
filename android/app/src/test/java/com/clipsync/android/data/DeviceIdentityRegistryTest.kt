package com.clipsync.android.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class DeviceIdentityRegistryTest {
    @Test fun `migra identidade global uma vez e mantem ids por daemon`() {
        val values = mutableMapOf("legacy" to "old-device")
        val persistence = object : DeviceIdentityPersistence {
            override fun get(key: String) = values[key]
            override fun put(key: String, value: String) { values[key] = value }
            override fun claimLegacy(key: String): String? = values.remove("legacy")?.also { values[key] = it }
        }
        val registry = DeviceIdentityRegistry(persistence)

        assertEquals("old-device", registry.deviceIdFor("server-a"))
        assertNull(registry.deviceIdFor("server-b"))
        registry.save("server-b", "new-device")
        assertEquals("old-device", registry.deviceIdFor("server-a"))
        assertEquals("new-device", registry.deviceIdFor("server-b"))
    }
}
