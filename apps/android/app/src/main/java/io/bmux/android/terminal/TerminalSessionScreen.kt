package io.bmux.android.terminal

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
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
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import java.nio.charset.StandardCharsets
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.selects.onTimeout
import kotlinx.coroutines.selects.select
import kotlin.coroutines.coroutineContext

private const val MAX_STATUS_LINES = 12
private const val MAX_DIAGNOSTIC_LINES = 160

@Composable
fun TerminalSessionScreen(
    endpoint: TerminalEndpoint,
    session: String?,
    onBack: () -> Unit,
    openTerminal: suspend (targetId: String, session: String?, rows: Int, cols: Int) -> Result<String>,
    pollTerminalOutput: suspend (terminalId: String, maxChunks: Int) -> Result<List<TerminalChunkFrame>>,
    writeTerminalInput: suspend (terminalId: String, bytes: ByteArray) -> Result<Unit>,
    sendTerminalMouseEvent: suspend (terminalId: String, event: TerminalMouseEvent) -> Result<Unit>,
    resizeTerminal: suspend (terminalId: String, rows: Int, cols: Int) -> Result<Unit>,
    closeTerminal: suspend (terminalId: String) -> Result<Unit>,
    fetchTerminalDiagnostics: suspend (terminalId: String, sinceSequence: Long?, limit: Int) -> Result<List<TerminalDiagnosticFrame>>,
    latestTerminalFailure: suspend (terminalId: String) -> Result<String?>,
) {
    val clipboard = LocalClipboardManager.current
    val renderer = remember(endpoint.id) { TermlibTerminalRenderer() }
    val transport = remember(endpoint.id) {
        CoreTerminalTransport(
            openTerminal = openTerminal,
            pollTerminalOutput = pollTerminalOutput,
            writeTerminalInput = writeTerminalInput,
            sendTerminalMouseEvent = sendTerminalMouseEvent,
            resizeTerminal = resizeTerminal,
            closeTerminal = closeTerminal,
            fetchTerminalDiagnostics = fetchTerminalDiagnostics,
            latestTerminalFailure = latestTerminalFailure,
        )
    }

    var connection by remember(endpoint.id) { mutableStateOf<TerminalTransportConnection?>(null) }
    var warning by remember(endpoint.id) { mutableStateOf<String?>(null) }
    var statusLines by remember(endpoint.id) { mutableStateOf(emptyList<TerminalStatusEvent>()) }
    var diagnosticLines by remember(endpoint.id) { mutableStateOf(emptyList<TerminalDiagnosticFrame>()) }
    var statusPanelVisible by remember(endpoint.id) { mutableStateOf(false) }
    var statusPanelExpanded by remember(endpoint.id) { mutableStateOf(false) }

    LaunchedEffect(endpoint.id, session) {
        connection?.close()
        warning = null
        connection = null
        statusLines = emptyList()
        diagnosticLines = emptyList()
        statusPanelVisible = false
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
                onDiagnostics = { events ->
                    if (events.isNotEmpty()) {
                        diagnosticLines = (diagnosticLines + events).takeLast(MAX_DIAGNOSTIC_LINES)
                        if (events.any { it.severity != TerminalStatusSeverity.INFO }) {
                            statusPanelVisible = true
                            statusPanelExpanded = true
                        }
                    }
                },
                onTerminalFailure = { message ->
                    warning = message
                    statusPanelVisible = true
                    statusPanelExpanded = true
                },
            )
        }.onSuccess {
            connection = it
            renderer.setOnInput(it::send)
            renderer.setOnMouseEvent(it::mouse)
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

        Box(modifier = Modifier.fillMaxSize()) {
            renderer.Render(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(bottom = 8.dp),
            )

            if ((statusLines.isNotEmpty() || diagnosticLines.isNotEmpty()) && statusPanelVisible) {
                val displayLines = (statusLines.map {
                    DisplayLine(message = "[status] ${it.message}", severity = it.severity)
                } + diagnosticLines.map {
                    DisplayLine(
                        message = "[diag ${formatDiagnosticTimestamp(it.timestampMs)}][${it.stage}] ${it.message}",
                        severity = it.severity,
                    )
                }).takeLast(MAX_DIAGNOSTIC_LINES)

                Card(
                    modifier = Modifier
                        .align(Alignment.TopCenter)
                        .fillMaxWidth()
                        .padding(8.dp),
                ) {
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
                            Text(
                                "Status ${statusLines.size} | Diagnostics ${diagnosticLines.size}",
                                style = MaterialTheme.typography.labelMedium,
                            )
                            Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                                TextButton(
                                    onClick = {
                                        clipboard.setText(
                                            AnnotatedString(
                                                buildDiagnosticsReport(
                                                    endpoint = endpoint,
                                                    statusLines = statusLines,
                                                    diagnostics = diagnosticLines,
                                                ),
                                            ),
                                        )
                                    },
                                ) {
                                    Text("Copy")
                                }
                                TextButton(onClick = { statusPanelExpanded = !statusPanelExpanded }) {
                                    Text(if (statusPanelExpanded) "Collapse" else "Expand")
                                }
                                TextButton(onClick = { statusPanelVisible = false }) {
                                    Text("Dismiss")
                                }
                                TextButton(
                                    onClick = {
                                        statusLines = emptyList()
                                        diagnosticLines = emptyList()
                                        statusPanelVisible = false
                                    },
                                ) {
                                    Text("Clear")
                                }
                            }
                        }

                        val visibleLines = if (statusPanelExpanded) {
                            displayLines
                        } else {
                            displayLines.takeLast(1)
                        }
                        LazyColumn(
                            modifier = Modifier
                                .fillMaxWidth()
                                .heightIn(max = if (statusPanelExpanded) 160.dp else 28.dp),
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
                    }
                }
            } else if (statusLines.isNotEmpty() || diagnosticLines.isNotEmpty()) {
                TextButton(
                    modifier = Modifier
                        .align(Alignment.TopEnd)
                        .padding(8.dp),
                    onClick = { statusPanelVisible = true },
                ) {
                    Text("Show diagnostics")
                }
            }
        }
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
    private val sendTerminalMouseEvent: suspend (terminalId: String, event: TerminalMouseEvent) -> Result<Unit>,
    private val resizeTerminal: suspend (terminalId: String, rows: Int, cols: Int) -> Result<Unit>,
    private val closeTerminal: suspend (terminalId: String) -> Result<Unit>,
    private val fetchTerminalDiagnostics: suspend (terminalId: String, sinceSequence: Long?, limit: Int) -> Result<List<TerminalDiagnosticFrame>>,
    private val latestTerminalFailure: suspend (terminalId: String) -> Result<String?>,
) : TerminalTransport {
    override suspend fun open(
        endpoint: TerminalEndpoint,
        session: String?,
        sink: (ByteArray) -> Unit,
        onStatus: (TerminalStatusEvent) -> Unit,
        onDiagnostics: (List<TerminalDiagnosticFrame>) -> Unit,
        onTerminalFailure: (String) -> Unit,
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
            sendTerminalMouseEvent = sendTerminalMouseEvent,
            resizeTerminal = resizeTerminal,
            closeTerminal = closeTerminal,
            fetchTerminalDiagnostics = fetchTerminalDiagnostics,
            latestTerminalFailure = latestTerminalFailure,
            onDiagnostics = onDiagnostics,
            onTerminalFailure = onTerminalFailure,
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
    private val sendTerminalMouseEvent: suspend (terminalId: String, event: TerminalMouseEvent) -> Result<Unit>,
    private val resizeTerminal: suspend (terminalId: String, rows: Int, cols: Int) -> Result<Unit>,
    private val closeTerminal: suspend (terminalId: String) -> Result<Unit>,
    private val fetchTerminalDiagnostics: suspend (terminalId: String, sinceSequence: Long?, limit: Int) -> Result<List<TerminalDiagnosticFrame>>,
    private val latestTerminalFailure: suspend (terminalId: String) -> Result<String?>,
    private val onDiagnostics: (List<TerminalDiagnosticFrame>) -> Unit,
    private val onTerminalFailure: (String) -> Unit,
) : TerminalTransportConnection {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val outboundCommands = Channel<OutboundCommand>(capacity = Channel.BUFFERED)
    @Volatile
    private var terminalFailed = false
    private var closed = false

    init {
        scope.launch {
            runOutboundLoop()
        }
        scope.launch {
            var diagnosticsSequence: Long? = null
            var nextDiagnosticsFetchAtMs = 0L
            while (isActive && !closed) {
                val now = System.currentTimeMillis()
                if (now >= nextDiagnosticsFetchAtMs) {
                    nextDiagnosticsFetchAtMs = now + DIAGNOSTICS_POLL_INTERVAL_MS
                    fetchTerminalDiagnostics(terminalId, diagnosticsSequence, DIAGNOSTICS_FETCH_LIMIT)
                        .onSuccess { diagnostics ->
                            if (diagnostics.isNotEmpty()) {
                                diagnosticsSequence = diagnostics.last().sequence
                                scope.launch(Dispatchers.Main) {
                                    onDiagnostics(diagnostics)
                                }
                            }
                        }
                }

                val chunks = pollTerminalOutput(terminalId, 64)
                    .getOrElse { error ->
                        val flushedDiagnostics = fetchTerminalDiagnostics(
                            terminalId,
                            diagnosticsSequence,
                            DIAGNOSTICS_FETCH_LIMIT,
                        ).getOrDefault(emptyList())
                        if (flushedDiagnostics.isNotEmpty()) {
                            diagnosticsSequence = flushedDiagnostics.last().sequence
                            scope.launch(Dispatchers.Main) {
                                onDiagnostics(flushedDiagnostics)
                            }
                        }

                        val failureMessage = latestTerminalFailure(terminalId)
                            .getOrNull()
                            ?.takeIf { it.isNotBlank() }
                        val latestDiagnosticFailure = flushedDiagnostics
                            .asReversed()
                            .firstOrNull { it.severity == TerminalStatusSeverity.ERROR }
                            ?.let { "${it.stage}: ${it.message}" }
                        val detail = failureMessage
                            ?: latestDiagnosticFailure
                            ?: (error.message ?: "unknown")
                        sink("\r\n[terminal poll failed: $detail]\r\n".encodeToByteArray())
                        scope.launch(Dispatchers.Main) {
                            onStatus(
                                TerminalStatusEvent(
                                    message = "terminal poll failed: $detail",
                                    severity = TerminalStatusSeverity.ERROR,
                                ),
                            )
                            onTerminalFailure(detail)
                        }
                        markTerminalFailed()
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

        val queueResult = outboundCommands.trySend(OutboundCommand.Input(data.copyOf()))
        if (queueResult.isFailure && !closed) {
            val message = queueResult.exceptionOrNull()?.message ?: "unknown"
            sink("\r\n[terminal write queue failed: $message]\r\n".encodeToByteArray())
        }
    }

    override fun mouse(event: TerminalMouseEvent) {
        if (closed) {
            return
        }

        val queueResult = outboundCommands.trySend(OutboundCommand.Mouse(event))
        if (queueResult.isFailure && !closed) {
            val message = queueResult.exceptionOrNull()?.message ?: "unknown"
            sink("\r\n[terminal mouse queue failed: $message]\r\n".encodeToByteArray())
        }
    }

    override fun resize(rows: Int, cols: Int) {
        if (closed) {
            return
        }

        val queueResult = outboundCommands.trySend(OutboundCommand.Resize(rows = rows, cols = cols))
        if (queueResult.isFailure && !closed) {
            val message = queueResult.exceptionOrNull()?.message ?: "unknown"
            sink("\r\n[terminal resize queue failed: $message]\r\n".encodeToByteArray())
        }
    }

    override fun close() {
        if (!closed) {
            closed = true
            outboundCommands.trySend(OutboundCommand.Close)
            outboundCommands.close()
            scope.launch {
                delay(CLOSE_DRAIN_DELAY_MS)
                scope.coroutineContext.cancel()
            }
        }
    }

    @OptIn(ExperimentalCoroutinesApi::class)
    private suspend fun runOutboundLoop() {
        var pendingResize: PendingResize? = null
        var lastAppliedResize: Pair<Int, Int>? = null

        while (coroutineContext.isActive) {
            if (terminalFailed) {
                return
            }

            val timeoutMillis = pendingResize
                ?.let { pending -> (pending.deadlineMillis - System.currentTimeMillis()).coerceAtLeast(0L) }
            val command = if (timeoutMillis == null) {
                outboundCommands.receiveCatching().getOrNull() ?: break
            } else {
                select<OutboundCommand?> {
                    outboundCommands.onReceiveCatching { result -> result.getOrNull() }
                    onTimeout(timeoutMillis) { null }
                }
            }

            if (terminalFailed) {
                return
            }

            if (command == null) {
                val resize = pendingResize ?: continue
                pendingResize = null
                val requested = resize.rows to resize.cols
                if (requested == lastAppliedResize) {
                    continue
                }
                resizeTerminal(terminalId, resize.rows, resize.cols).onFailure { error ->
                    sink("\r\n[terminal resize failed: ${error.message ?: "unknown"}]\r\n".encodeToByteArray())
                }
                lastAppliedResize = requested
                continue
            }

            when (command) {
                is OutboundCommand.Input -> {
                    writeTerminalInput(terminalId, command.bytes).onFailure { error ->
                        sink("\r\n[terminal write failed: ${error.message ?: "unknown"}]\r\n".encodeToByteArray())
                    }
                }
                is OutboundCommand.Mouse -> {
                    sendTerminalMouseEvent(terminalId, command.event).onFailure { error ->
                        sink("\r\n[terminal mouse failed: ${error.message ?: "unknown"}]\r\n".encodeToByteArray())
                    }
                }
                is OutboundCommand.Resize -> {
                    val requested = command.rows to command.cols
                    val pending = pendingResize?.let { resize -> resize.rows to resize.cols }
                    if (requested == lastAppliedResize || requested == pending) {
                        continue
                    }
                    pendingResize = PendingResize(
                        rows = command.rows,
                        cols = command.cols,
                        deadlineMillis = System.currentTimeMillis() + RESIZE_DEBOUNCE_MS,
                    )
                }
                OutboundCommand.Close -> {
                    closeTerminal(terminalId).onFailure { error ->
                        sink("\r\n[terminal close failed: ${error.message ?: "unknown"}]\r\n".encodeToByteArray())
                    }
                    return
                }
            }
        }
    }

    private companion object {
        private const val POLL_IDLE_DELAY_MS = 25L
        private const val RESIZE_DEBOUNCE_MS = 80L
        private const val CLOSE_DRAIN_DELAY_MS = 250L
        private const val DIAGNOSTICS_POLL_INTERVAL_MS = 250L
        private const val DIAGNOSTICS_FETCH_LIMIT = 48
    }

    private sealed interface OutboundCommand {
        data class Input(val bytes: ByteArray) : OutboundCommand
        data class Mouse(val event: TerminalMouseEvent) : OutboundCommand
        data class Resize(val rows: Int, val cols: Int) : OutboundCommand
        object Close : OutboundCommand
    }

    private data class PendingResize(
        val rows: Int,
        val cols: Int,
        val deadlineMillis: Long,
    )

    private fun markTerminalFailed() {
        if (terminalFailed) {
            return
        }
        terminalFailed = true
        closed = true
        outboundCommands.close()
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

private data class DisplayLine(
    val message: String,
    val severity: TerminalStatusSeverity,
)

private fun formatDiagnosticTimestamp(timestampMs: Long): String {
    val formatter = SimpleDateFormat("HH:mm:ss.SSS", Locale.US)
    return formatter.format(Date(timestampMs))
}

private fun buildDiagnosticsReport(
    endpoint: TerminalEndpoint,
    statusLines: List<TerminalStatusEvent>,
    diagnostics: List<TerminalDiagnosticFrame>,
): String {
    val report = StringBuilder()
    report.append("bmux mobile terminal diagnostics\n")
    report.append("endpoint=").append(endpoint.name)
        .append(" target=").append(endpoint.canonicalTarget)
        .append('\n')

    val primaryCause = diagnostics.asReversed()
        .firstOrNull { it.severity == TerminalStatusSeverity.ERROR }
        ?.let { "${it.stage}: ${it.message}" }
        ?: statusLines.asReversed()
            .firstOrNull { it.severity == TerminalStatusSeverity.ERROR }
            ?.message
    primaryCause?.let { cause ->
        report.append("primary_cause=").append(cause).append('\n')
    }

    if (statusLines.isNotEmpty()) {
        report.append("\nstatus events\n")
        statusLines.forEach { line ->
            report.append('[')
                .append(line.severity.name)
                .append("] ")
                .append(line.message)
                .append('\n')
        }
    }

    if (diagnostics.isNotEmpty()) {
        report.append("\ndiagnostic timeline\n")
        diagnostics.forEach { line ->
            report.append('#')
                .append(line.sequence)
                .append(' ')
                .append(formatDiagnosticTimestamp(line.timestampMs))
                .append(' ')
                .append('[')
                .append(line.severity.name)
                .append(']')
                .append('[')
                .append(line.stage)
                .append(']')
            line.code?.let { code ->
                report.append('[').append(code).append(']')
            }
            report.append(' ')
                .append(line.message)
                .append('\n')
        }
    }

    return report.toString()
}
