package io.bmux.android.terminal

import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier

data class TerminalEndpoint(
    val id: String,
    val name: String,
    val canonicalTarget: String,
)

enum class TerminalChunkType {
    STDOUT,
    STDERR,
    STATUS,
}

data class TerminalChunkFrame(
    val kind: TerminalChunkType,
    val bytes: ByteArray,
    val statusSeverity: TerminalStatusSeverity? = null,
)

data class TerminalDiagnosticFrame(
    val sequence: Long,
    val timestampMs: Long,
    val severity: TerminalStatusSeverity,
    val stage: String,
    val code: String?,
    val message: String,
)

enum class TerminalStatusSeverity {
    INFO,
    WARN,
    ERROR,
}

data class TerminalStatusEvent(
    val message: String,
    val severity: TerminalStatusSeverity,
)

enum class TerminalMouseButton {
    LEFT,
    MIDDLE,
    RIGHT,
}

enum class TerminalMouseEventKind {
    DOWN,
    UP,
    DRAG,
    MOVE,
    SCROLL_UP,
    SCROLL_DOWN,
    SCROLL_LEFT,
    SCROLL_RIGHT,
}

data class TerminalMouseEvent(
    val kind: TerminalMouseEventKind,
    val button: TerminalMouseButton? = null,
    val row: Int,
    val col: Int,
    val shift: Boolean = false,
    val alt: Boolean = false,
    val control: Boolean = false,
)

interface TerminalTransportConnection {
    fun send(data: ByteArray)
    fun mouse(event: TerminalMouseEvent)
    fun resize(rows: Int, cols: Int)
    fun close()
}

interface TerminalTransport {
    suspend fun open(
        endpoint: TerminalEndpoint,
        session: String?,
        sink: (ByteArray) -> Unit,
        onStatus: (TerminalStatusEvent) -> Unit,
        onDiagnostics: (List<TerminalDiagnosticFrame>) -> Unit,
        onTerminalFailure: (String) -> Unit,
    ): TerminalTransportConnection
}

interface TerminalRenderer {
    fun appendOutput(data: ByteArray)
    fun setOnInput(handler: (ByteArray) -> Unit)
    fun setOnResize(handler: (rows: Int, cols: Int) -> Unit)
    fun setOnMouseEvent(handler: (TerminalMouseEvent) -> Unit)

    @Composable
    fun Render(modifier: Modifier)

    fun dispose()
}
