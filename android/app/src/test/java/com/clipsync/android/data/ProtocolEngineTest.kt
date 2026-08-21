package com.clipsync.android.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ProtocolEngineTest {
    @Test fun `hello inclui device id persistido`() {
        assertEquals("known", ProtocolEngine(DeviceInfo("Pixel", id = "known")).onOpen().device.id)
    }
    @Test fun `challenge guarda nonce e gera submit`() {
        val engine = ProtocolEngine(DeviceInfo("Pixel"))
        val challenge = Message.PairChallenge("ch", 2_000_000_000, "nonce")
        assertTrue(engine.onMessage(challenge).single() is ProtocolAction.RequestPin)
        assertEquals(Message.PairSubmit("ch", "123456", "nonce"), engine.submitPin("123456", 1_900_000_000))
    }
    @Test fun `pin invalido ou expirado nao e enviado`() {
        val engine = ProtocolEngine(DeviceInfo("Pixel"))
        engine.onMessage(Message.PairChallenge("ch", 100, "nonce"))
        assertNull(engine.submitPin("12345", 1))
        assertNull(engine.submitPin("123456", 100))
    }
    @Test fun `ping responde pong e pair ok conclui`() {
        val engine = ProtocolEngine(DeviceInfo("Pixel"))
        assertEquals(ProtocolAction.Send(Message.Pong(7)), engine.onMessage(Message.Ping(7)).single())
        assertTrue(engine.onMessage(Message.PairOk("id", "session", "pc")).single() is ProtocolAction.Paired)
    }
}
