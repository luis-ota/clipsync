package com.clipsync.android.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ProtocolCodecTest {
    @Test fun `hello serializa com discriminador e versao`() {
        val encoded = ProtocolCodec.encode(Message.Hello(device = DeviceInfo("Pixel", id = null)))
        assertTrue(encoded.contains("\"type\":\"hello\""))
        assertTrue(encoded.contains("\"v\":1"))
        assertFalse(encoded.contains("\"id\":null"))
        assertEquals("Pixel", (ProtocolCodec.decode(encoded) as Message.Hello).device.name)
    }
    @Test fun `mensagens do protocolo fazem round trip`() {
        val messages = listOf<Message>(
            Message.PairChallenge("challenge", 2_000_000_000, "nonce"),
            Message.PairSubmit("challenge", "123456", "nonce"),
            Message.PairOk("device", "session", "desktop", Capabilities(true, images = true), "server"),
            Message.PairFail("invalid_code", "PIN invalido"),
            Message.ClipboardText("text/plain", "ola", "device", "abc"),
            Message.ClipboardImage("image/png", "AA==", 1, 1, "abc", "device"),
            Message.ClipboardHtml("<b>x</b>", "x", "abc", "device"),
            Message.Ping(42), Message.Pong(42), Message.Error("bad", "erro"),
        )
        messages.forEach { assertEquals(it, ProtocolCodec.decode(ProtocolCodec.encode(it))) }
    }
    @Test fun `pair ok legado sem server id continua decodificavel`() {
        val decoded = ProtocolCodec.decode(
            """{"type":"pair_ok","device_id":"device","session_id":"session","server_name":"desktop"}"""
        ) as Message.PairOk
        assertEquals(null, decoded.server_id)
    }
}
