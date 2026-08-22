package com.clipsync.android.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PairingDeepLinksTest {
    @Test fun `aceita pairing com servidor e pin`() {
        val link = PairingDeepLinks.parse("clipsync://pair?server_id=desktop-1&pin=123456")
        assertEquals(PairingDeepLink("desktop-1", "123456"), link)
    }

    @Test fun `rejeita esquema host e pin invalidos`() {
        assertNull(PairingDeepLinks.parse("https://pair?server_id=desktop-1"))
        assertNull(PairingDeepLinks.parse("clipsync://pair?pin=12345"))
        assertNull(PairingDeepLinks.parse("clipsync://pair"))
    }
}
