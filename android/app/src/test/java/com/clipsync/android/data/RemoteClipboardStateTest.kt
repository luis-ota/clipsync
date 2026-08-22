package com.clipsync.android.data

import org.junit.Assert.assertEquals
import org.junit.Test
import com.clipsync.android.service.RemoteClipboardBuffer

class RemoteClipboardStateTest {
    @Test fun `estado expoe resumo sem perder mensagem atual`() {
        val message = Message.ClipboardText("text/plain", "conteudo remoto", "pc-1", "hash")
        AppRepository.recordRemote(message)
        assertEquals("conteudo remoto", AppRepository.state.value.lastRemoteItem?.preview)
        RemoteClipboardBuffer.set(message)
        assertEquals(message, RemoteClipboardBuffer.get())
    }
}
