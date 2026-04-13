package io.bmux.android.terminal

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
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

@Composable
fun TerminalSessionScreen(
    endpoint: TerminalEndpoint,
    session: String?,
    onBack: () -> Unit,
    connectAttempt: suspend (targetId: String, session: String?) -> Result<String>,
) {
    val renderer = remember(endpoint.id) { TermlibTerminalRenderer() }
    val transport = remember(endpoint.id) { PreviewTerminalTransport(connectAttempt) }

    var connection by remember(endpoint.id) { mutableStateOf<TerminalTransportConnection?>(null) }
    var warning by remember(endpoint.id) { mutableStateOf<String?>(null) }

    LaunchedEffect(endpoint.id, session) {
        warning = null
        connection = null
        runCatching {
            transport.open(endpoint, session) { bytes -> renderer.appendOutput(bytes) }
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
            text = "Renderer: termlib (Apache-2.0). Transport: M7 preview adapter.",
            style = MaterialTheme.typography.bodySmall,
        )

        warning?.let {
            Text(it, color = MaterialTheme.colorScheme.error)
        }

        renderer.Render(
            modifier = Modifier
                .fillMaxSize()
                .padding(bottom = 8.dp),
        )
    }
}

private class PreviewTerminalTransport(
    private val connectAttempt: suspend (targetId: String, session: String?) -> Result<String>,
) : TerminalTransport {
    override suspend fun open(
        endpoint: TerminalEndpoint,
        session: String?,
        sink: (ByteArray) -> Unit,
    ): TerminalTransportConnection {
        val status = connectAttempt(endpoint.id, session)
            .getOrElse { error ->
                val line = "Connection failed for ${endpoint.name}: ${error.message ?: "unknown"}\r\n"
                sink(line.toByteArray(StandardCharsets.UTF_8))
                throw error
            }

        val banner = buildString {
            append("Connected to ${endpoint.name} (${endpoint.canonicalTarget})\r\n")
            append("$status\r\n")
            append("Preview terminal transport active. Type 'help' for commands.\r\n")
            append("$ ")
        }
        sink(banner.toByteArray(StandardCharsets.UTF_8))

        return PreviewTerminalTransportConnection(sink)
    }
}

private class PreviewTerminalTransportConnection(
    private val sink: (ByteArray) -> Unit,
) : TerminalTransportConnection {
    private val commandBuffer = StringBuilder()
    private var closed = false

    override fun send(data: ByteArray) {
        if (closed) {
            return
        }

        for (byte in data) {
            when (byte.toInt()) {
                3 -> {
                    commandBuffer.clear()
                    emit("^C\r\n$ ")
                }
                8, 127 -> {
                    if (commandBuffer.isNotEmpty()) {
                        commandBuffer.deleteAt(commandBuffer.lastIndex)
                        emit("\b \b")
                    }
                }
                10, 13 -> {
                    emit("\r\n")
                    runCommand(commandBuffer.toString().trim())
                    commandBuffer.clear()
                    emit("$ ")
                }
                else -> {
                    val asChar = byte.toInt().toChar()
                    commandBuffer.append(asChar)
                    sink(byteArrayOf(byte))
                }
            }
        }
    }

    override fun resize(rows: Int, cols: Int) {
        if (closed) {
            return
        }
        emit("\r\n[terminal resized to ${rows}x${cols}]\r\n$ ")
    }

    override fun close() {
        if (!closed) {
            closed = true
            emit("\r\n[terminal closed]\r\n")
        }
    }

    private fun runCommand(command: String) {
        when (command) {
            "" -> Unit
            "help" -> emit("Commands: help, clear, status, exit\r\n")
            "clear" -> emit("\u001b[2J\u001b[H")
            "status" -> emit("Transport is running in preview mode.\r\n")
            "exit" -> {
                emit("Session closed.\r\n")
                close()
            }
            else -> emit("Command '${command}' is not wired yet in preview transport.\r\n")
        }
    }

    private fun emit(text: String) {
        sink(text.toByteArray(StandardCharsets.UTF_8))
    }
}
