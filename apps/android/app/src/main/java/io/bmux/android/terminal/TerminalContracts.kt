package io.bmux.android.terminal

import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier

data class TerminalEndpoint(
    val id: String,
    val name: String,
    val canonicalTarget: String,
)

interface TerminalTransportConnection {
    fun send(data: ByteArray)
    fun resize(rows: Int, cols: Int)
    fun close()
}

interface TerminalTransport {
    suspend fun open(
        endpoint: TerminalEndpoint,
        session: String?,
        sink: (ByteArray) -> Unit,
    ): TerminalTransportConnection
}

interface TerminalRenderer {
    fun appendOutput(data: ByteArray)
    fun setOnInput(handler: (ByteArray) -> Unit)
    fun setOnResize(handler: (rows: Int, cols: Int) -> Unit)

    @Composable
    fun Render(modifier: Modifier)

    fun dispose()
}
