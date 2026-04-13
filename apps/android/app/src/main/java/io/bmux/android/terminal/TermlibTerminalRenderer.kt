package io.bmux.android.terminal

import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import org.connectbot.terminal.Terminal
import org.connectbot.terminal.TerminalEmulatorFactory

class TermlibTerminalRenderer : TerminalRenderer {
    private var onInput: ((ByteArray) -> Unit)? = null
    private var onResize: ((Int, Int) -> Unit)? = null

    private val emulator = TerminalEmulatorFactory.create(
        onKeyboardInput = { data -> onInput?.invoke(data) },
        onResize = { dimensions -> onResize?.invoke(dimensions.rows, dimensions.columns) },
    )

    override fun appendOutput(data: ByteArray) {
        emulator.writeInput(data)
    }

    override fun setOnInput(handler: (ByteArray) -> Unit) {
        onInput = handler
    }

    override fun setOnResize(handler: (rows: Int, cols: Int) -> Unit) {
        onResize = handler
    }

    @Composable
    override fun Render(modifier: Modifier) {
        Terminal(
            terminalEmulator = emulator,
            modifier = modifier,
            keyboardEnabled = true,
            showSoftKeyboard = true,
        )
    }

    override fun dispose() {
        onInput = null
        onResize = null
    }
}
