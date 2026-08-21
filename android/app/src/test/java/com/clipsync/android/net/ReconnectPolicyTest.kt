package com.clipsync.android.net

import org.junit.Assert.assertEquals
import org.junit.Test

class ReconnectPolicyTest {
    @Test fun `backoff dobra e limita em sessenta segundos`() {
        val policy = ReconnectPolicy()
        assertEquals(listOf(1_000L, 2_000L, 4_000L, 8_000L), (0..3).map(policy::delayMillis))
        assertEquals(60_000L, policy.delayMillis(20))
    }
}
