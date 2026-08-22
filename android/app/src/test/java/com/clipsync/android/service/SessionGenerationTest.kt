package com.clipsync.android.service

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class SessionGenerationTest {
    @Test fun `eventos de socket antigo sao rejeitados apos troca concorrente`() {
        val sessions = SessionGeneration()
        val first = sessions.advance()
        assertTrue(sessions.accepts(first))
        val second = sessions.advance()
        assertFalse(sessions.accepts(first))
        assertTrue(sessions.accepts(second))
    }
}
