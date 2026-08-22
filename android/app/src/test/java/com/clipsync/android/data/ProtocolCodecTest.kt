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
    @Test fun `relay envelope cifra com AAD e rejeita chave ou sequencia errada`() {
        val key = "v1 group-1 " + "11".repeat(32)
        val message = Message.ClipboardText("text/plain", "segredo", "device", "hash")
        val envelope = RelayCrypto.encrypt(message, key, "session", "device", 1)
        assertEquals(message, RelayCrypto.decrypt(envelope, key))
        assertThrows { RelayCrypto.decrypt(envelope.copy(sequence = 2), key) }
        assertThrows { RelayCrypto.decrypt(envelope, "v1 group-1 " + "22".repeat(32)) }
        val tampered = envelope.copy(payload = envelope.payload.copy(ciphertext = "A" + envelope.payload.ciphertext.drop(1)))
        assertThrows { RelayCrypto.decrypt(tampered, key) }
    }
    @Test fun `relay rejeita grupo ou nonce hexadecimal invalido`() {
        val key = "v1 group-1 " + "11".repeat(32)
        val envelope = RelayCrypto.encrypt(Message.Ping(1), key, "session", "device", 1)
        assertThrows { RelayCrypto.decrypt(envelope.copy(group = "other"), key) }
        assertThrows { RelayCrypto.decrypt(envelope.copy(payload = envelope.payload.copy(nonce = "0")), key) }
    }
    @Test fun `frame binario CSB1 faz round trip e respeita bounds`() {
        val chunk = BinaryTransferChunk(ByteArray(16) { it.toByte() }, 0, 1, 3, byteArrayOf(1, 2, 3))
        assertEquals(chunk.data.toList(), BinaryTransferCodec.decode(BinaryTransferCodec.encode(chunk)).data.toList())
        assertThrows {
            BinaryTransferCodec.encode(chunk.copy(data = ByteArray(BinaryTransferCodec.MAX_CHUNK_BYTES + 1)))
        }
        assertThrows {
            BinaryTransferCodec.decode(BinaryTransferCodec.encode(chunk).copyOf().also { it[0] = 'X'.code.toByte() })
        }
    }
    private fun assertThrows(block: () -> Unit) { runCatching(block).onSuccess { error("esperava falha") } }
}
