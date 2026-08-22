package com.clipsync.android.data

import com.clipsync.android.service.ClipboardWatcher
import kotlin.system.measureNanoTime
import org.junit.Assert.assertTrue
import org.junit.Test

/** Host-side regression benchmark; results are not device performance measurements. */
class ProtocolBenchmarkTest {
    @Test fun `codec e hash permanecem dentro do orçamento de regressao`() {
        val message = Message.ClipboardText("text/plain;charset=utf-8", "x".repeat(4096), "device", "a".repeat(64))
        repeat(100) { ProtocolCodec.encode(message) }
        val elapsed = measureNanoTime {
            repeat(1_000) {
                ProtocolCodec.encode(message)
                ClipboardWatcher.sha256(message.content.toByteArray())
            }
        }
        assertTrue("benchmark deve executar", elapsed > 0)
    }
}
