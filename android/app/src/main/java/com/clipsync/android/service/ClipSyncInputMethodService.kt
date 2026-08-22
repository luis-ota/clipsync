package com.clipsync.android.service

import android.inputmethodservice.InputMethodService
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import com.clipsync.android.data.AppRepository
import com.clipsync.android.data.Message

/** A small IME action row. It only inserts text explicitly requested by the user. */
class ClipSyncInputMethodService : InputMethodService() {
    override fun onCreateInputView(): View {
        val layout = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(16, 8, 16, 8)
        }
        val item = AppRepository.state.value.lastRemoteItem
        layout.addView(TextView(this).apply {
            text = item?.preview ?: "Nenhum clipboard recebido do PC"
        })
        layout.addView(Button(this).apply {
            text = "Colar do PC"
            isEnabled = RemoteClipboardBuffer.get() is Message.ClipboardText
            setOnClickListener {
                val message = RemoteClipboardBuffer.get()
                if (message is Message.ClipboardText) {
                    currentInputConnection?.commitText(message.content, 1)
                }
            }
        })
        return layout
    }
}
