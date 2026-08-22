package com.clipsync.android.service

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ClipboardPolicyTest {
    @Test fun `limite de imagem aceita imediatamente abaixo e rejeita acima`() {
        assertTrue(ImageLimits.acceptsRawSize(ImageLimits.MAX_IMAGE_BYTES))
        assertFalse(ImageLimits.acceptsRawSize(ImageLimits.MAX_IMAGE_BYTES + 1))
        val encodedUpperBound = ((ImageLimits.MAX_IMAGE_BYTES + 2) / 3) * 4 + 8 * 1024
        assertTrue(encodedUpperBound <= ImageLimits.MAX_WEBSOCKET_MESSAGE_BYTES)
    }

    @Test fun `anti eco suporta escritas consecutivas e expira callback ausente`() {
        var now = 1_000L
        val echoes = PendingEchoes(ttlMillis = 100, clock = { now })
        echoes.add("text")
        echoes.add("image")
        assertTrue(echoes.consume("text"))
        assertTrue(echoes.consume("image"))
        assertFalse(echoes.consume("text"))

        echoes.add("legitimate-later")
        now += 101
        assertFalse(echoes.consume("legitimate-later"))
    }
}
