package io.bmux.android.terminal

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.Card
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import java.nio.charset.StandardCharsets
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

private const val MAX_STATUS_LINES = 12

@Composable
fun TerminalSessionScreen(
    endpoint: TerminalEndpoint,
    session: String?,
    onBack: () -> Unit,
    openTerminal: suspend (targetId: String, session: String?, rows: Int, cols: Int) -> Result<String>,
    pollTerminalOutput: suspend (terminalId: String, maxChunks: Int) -> Result<List<TerminalChunkFrame>>,
    writeTerminalInput: suspend (terminalId: String, bytes: ByteArray) -> Result<Unit>,
    resizeTerminal: suspend (terminalId: String, rows: Int, cols: Int) -> Result<Unit>,
    closeTerminal: suspend (terminalId: String) -> Result<Unit>,
) {
    val renderer = remember(endpoint.id) { TermlibTerminalRenderer() }
    val transport = remember(endpoint.id) {
        CoreTerminalTransport(
            openTerminal = openTerminal,
            pollTerminalOutput = pollTerminalOutput,
            writeTerminalInput = writeTerminalInput,
            resizeTerminal = resizeTerminal,
            closeTerminal = closeTerminal,
        )
    }

    var connection by remember(endpoint.id) { mutableStateOf<TerminalTransportConnection?>(null) }
    var warning by remember(endpoint.id) { mutableStateOf<String?>(null) }
    var statusLines by remember(endpoint.id) { mutableStateOf(emptyList<TerminalStatusEvent>()) }
    var statusPanelVisible by remember(endpoint.id) { mutableStateOf(true) }
    var statusPanelExpanded by remember(endpoint.id) { mutableStateOf(false) }

    LaunchedEffect(endpoint.id, session) {
        warning = null
        connection = null
        statusLines = emptyList()
        statusPanelVisible = true
        statusPanelExpanded = false
        runCatching {
            transport.open(
                endpoint = endpoint,
                session = session,
                sink = { bytes -> renderer.appendOutput(bytes) },
                onStatus = { event ->
                    if (shouldKeepStatusEvent(event)) {
                        statusLines = (statusLines + event).takeLast(MAX_STATUS_LINES)
                        if (event.severity != TerminalStatusSeverity.INFO) {
                            statusPanelVisible = true
                            statusPanelExpanded = true
                        }
                    }
                },
            )
        }.onSuccess {
            connection = it
            renderer.setOnInput(it::send)
            renderer.setOnResize(it::resize)
        }.onFailure {
            warning = it.message ?: "Failed to open terminal"
        }
    }

    DisposableEffect(endpoint.id) {
        onDispose {
            connection?.close()
            renderer.dispose()
        }
    }

    Column(
        modifier = Modifier.fillMaxSize(),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
        ) {
            Text("Terminal", fontWeight = FontWeight.SemiBold)
            Button(onClick = onBack) {
                Text("Back")
            }
        }

        Text(
            text = "Renderer: termlib (Apache-2.0). Transport: mobile-core terminal stream.",
            style = MaterialTheme.typography.bodySmall,
        )

        warning?.let {
            Text(it, color = MaterialTheme.colorScheme.error)
        }
        if (statusLines.isNotEmpty()) {
            Card(modifier = Modifier.fillMaxWidth()) {
                Column(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(8.dp),
                    verticalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.SpaceBetween,
                    ) {
                        Text("Status (${statusLines.size})", style = MaterialTheme.typography.labelMedium)
                        Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                            TextButton(onClick = { statusPanelExpanded = !statusPanelExpanded }) {
                                Text(if (statusPanelExpanded) "Collapse" else "Expand")
                            }
                            TextButton(onClick = { statusPanelVisible = !statusPanelVisible }) {
                                Text(if (statusPanelVisible) "Dismiss" else "Show")
                            }
                            TextButton(
                                onClick = {
                                    statusLines = emptyList()
                                    statusPanelVisible = false
                                },
                            ) {
                                Text("Clear")
                            }
                        }
                    }

                    if (statusPanelVisible) {
                        val visibleLines = if (statusPanelExpanded) {
                            statusLines
                        } else {
                            listOf(statusLines.last())
                        }
                        LazyColumn(
                            modifier = Modifier
                                .fillMaxWidth()
                                .heightIn(max = if (statusPanelExpanded) 100.dp else 28.dp),
                            verticalArrangement = Arrangement.spacedBy(4.dp),
                        ) {
                            itemsIndexed(visibleLines, key = { index, line -> "$index-${line.message}" }) { _, line ->
                                val color = when (line.severity) {
                                    TerminalStatusSeverity.INFO -> MaterialTheme.colorScheme.onSurface
                                    TerminalStatusSeverity.WARN -> MaterialTheme.colorScheme.tertiary
                                    TerminalStatusSeverity.ERROR -> MaterialTheme.colorScheme.error
                                }
                                Text(
                                    text = line.message,
                                    style = MaterialTheme.typography.bodySmall,
                                    color = color,
                                )
                            }
                        }
                    } else {
                        Text(
                            text = statusLines.last().message,
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
            }
        }

        renderer.Render(
            modifier = Modifier
                .fillMaxSize()
                .padding(bottom = 8.dp),
        )
    }
}

private fun shouldKeepStatusEvent(event: TerminalStatusEvent): Boolean {
    if (event.severity != TerminalStatusSeverity.INFO) {
        return true
    }
    val message = event.message.lowercase()
    return !message.startsWith("resize ")
}

private class CoreTerminalTransport(
    private val openTerminal: suspend (targetId: String, session: String?, rows: Int, cols: Int) -> Result<String>,
    private val pollTerminalOutput: suspend (terminalId: String, maxChunks: Int) -> Result<List<TerminalChunkFrame>>,
    private val writeTerminalInput: suspend (terminalId: String, bytes: ByteArray) -> Result<Unit>,
    private val resizeTerminal: suspend (terminalId: String, rows: Int, cols: Int) -> Result<Unit>,
    private val closeTerminal: suspend (terminalId: String) -> Result<Unit>,
) : TerminalTransport {
    override suspend fun open(
        endpoint: TerminalEndpoint,
        session: String?,
        sink: (ByteArray) -> Unit,
        onStatus: (TerminalStatusEvent) -> Unit,
    ): TerminalTransportConnection {
        onStatus(
            TerminalStatusEvent(
                message = "opening terminal to ${endpoint.name}...",
                severity = TerminalStatusSeverity.INFO,
            ),
        )

        val terminalId = openTerminal(endpoint.id, session, DEFAULT_ROWS, DEFAULT_COLS).getOrElse { error ->
            onStatus(
                TerminalStatusEvent(
                    message = "terminal open failed: ${error.message ?: "unknown"}",
                    severity = TerminalStatusSeverity.ERROR,
                ),
            )
            throw error
        }

        onStatus(
            TerminalStatusEvent(
                message = "connected to ${endpoint.canonicalTarget}",
                severity = TerminalStatusSeverity.INFO,
            ),
        )

        return CoreTerminalTransportConnection(
            terminalId = terminalId,
            sink = sink,
            onStatus = onStatus,
            pollTerminalOutput = pollTerminalOutput,
            writeTerminalInput = writeTerminalInput,
            resizeTerminal = resizeTerminal,
            closeTerminal = closeTerminal,
        )
    }

    private companion object {
        private const val DEFAULT_ROWS = 24
        private const val DEFAULT_COLS = 80
    }
}

private class CoreTerminalTransportConnection(
    private val terminalId: String,
    private val sink: (ByteArray) -> Unit,
    private val onStatus: (TerminalStatusEvent) -> Unit,
    private val pollTerminalOutput: suspend (terminalId: String, maxChunks: Int) -> Result<List<TerminalChunkFrame>>,
    private val writeTerminalInput: suspend (terminalId: String, bytes: ByteArray) -> Result<Unit>,
    private val resizeTerminal: suspend (terminalId: String, rows: Int, cols: Int) -> Result<Unit>,
    private val closeTerminal: suspend (terminalId: String) -> Result<Unit>,
) : TerminalTransportConnection {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var closed = false

    init {
        scope.launch {
            while (isActive && !closed) {
                val chunks = pollTerminalOutput(terminalId, 64)
                    .getOrElse { error ->
                        sink("\r\n[terminal poll failed: ${error.message ?: "unknown"}]\r\n".encodeToByteArray())
                        break
                    }
                if (chunks.isEmpty()) {
                    delay(POLL_IDLE_DELAY_MS)
                    continue
                }
                for (chunk in chunks) {
                    when (chunk.kind) {
                        TerminalChunkType.STDOUT,
                        TerminalChunkType.STDERR,
                        -> sink(chunk.bytes)
                        TerminalChunkType.STATUS -> {
                            val message = chunk.bytes.toString(StandardCharsets.UTF_8).trim()
                            if (message.isNotEmpty()) {
                                scope.launch(Dispatchers.Main) {
                                    onStatus(
                                        TerminalStatusEvent(
                                            message = message,
                                            severity = chunk.statusSeverity ?: inferStatusSeverity(message),
                                        ),
                                    )
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    override fun send(data: ByteArray) {
        if (closed) {
            return
        }

        scope.launch {
            writeTerminalInput(terminalId, data).onFailure { error ->
                sink("\r\n[terminal write failed: ${error.message ?: "unknown"}]\r\n".encodeToByteArray())
            }
        }
    }

    override fun resize(rows: Int, cols: Int) {
        if (closed) {
            return
        }

        scope.launch {
            resizeTerminal(terminalId, rows, cols).onFailure { error ->
                sink("\r\n[terminal resize failed: ${error.message ?: "unknown"}]\r\n".encodeToByteArray())
            }
        }
    }

    override fun close() {
        if (!closed) {
            closed = true
            scope.launch {
                closeTerminal(terminalId).onFailure { error ->
                    sink("\r\n[terminal close failed: ${error.message ?: "unknown"}]\r\n".encodeToByteArray())
                }
            }
            scope.coroutineContext.cancel()
        }
    }

    private companion object {
        private const val POLL_IDLE_DELAY_MS = 25L
    }

    private fun inferStatusSeverity(message: String): TerminalStatusSeverity {
        val normalized = message.lowercase()
        return when {
            normalized.contains("error") ||
                normalized.contains("failed") ||
                normalized.contains("denied") ||
                normalized.contains("invalid") ||
                normalized.contains("unavailable") -> TerminalStatusSeverity.ERROR
            normalized.contains("warn") ||
                normalized.contains("retry") ||
                normalized.contains("reconnect") ||
                normalized.contains("timeout") -> TerminalStatusSeverity.WARN
            else -> TerminalStatusSeverity.INFO
        }
    }
}
