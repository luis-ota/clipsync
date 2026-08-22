package com.clipsync.android.net

import org.junit.Assert.assertEquals
import org.junit.Test

class ReconnectPolicyTest {
    @Test fun `fingerprint aceita exatamente 64 caracteres hexadecimais`() {
        assertEquals(true, isValidTlsFingerprint("a".repeat(64)))
        assertEquals(true, isValidTlsFingerprint("A1".repeat(32)))
        assertEquals(false, isValidTlsFingerprint("g".repeat(64)))
        assertEquals(false, isValidTlsFingerprint("a".repeat(63)))
        assertEquals(false, isValidTlsFingerprint("a".repeat(65)))
    }

    @Test fun `backoff dobra e limita em sessenta segundos`() {
        val policy = ReconnectPolicy()
        assertEquals(listOf(1_000L, 2_000L, 4_000L, 8_000L), (0..3).map(policy::delayMillis))
        assertEquals(60_000L, policy.delayMillis(20))
    }
}
