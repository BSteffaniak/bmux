package io.bmux.android

import android.Manifest
import android.app.Application
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import androidx.lifecycle.viewmodel.compose.viewModel
import io.bmux.android.terminal.TerminalChunkFrame
import io.bmux.android.terminal.TerminalChunkType
import io.bmux.android.terminal.TerminalDiagnosticFrame
import io.bmux.android.terminal.TerminalEndpoint
import io.bmux.android.terminal.TerminalMouseButton
import io.bmux.android.terminal.TerminalMouseEvent
import io.bmux.android.terminal.TerminalMouseEventKind
import io.bmux.android.terminal.TerminalPointerFrame
import io.bmux.android.terminal.TerminalSessionScreen
import io.bmux.android.terminal.TerminalStatusSeverity
import java.io.File
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.bmux_mobile_ffi.ConnectionStateFfi
import uniffi.bmux_mobile_ffi.MobileApiFfi
import uniffi.bmux_mobile_ffi.MobileFfiException
import uniffi.bmux_mobile_ffi.PairedHostFfi
import uniffi.bmux_mobile_ffi.PairedHostRoleFfi
import uniffi.bmux_mobile_ffi.PairingStateFfi
import uniffi.bmux_mobile_ffi.PairingStatusFfi
import uniffi.bmux_mobile_ffi.TerminalChunkKindFfi
import uniffi.bmux_mobile_ffi.TerminalDiagnosticFfi
import uniffi.bmux_mobile_ffi.TerminalMouseButtonFfi
import uniffi.bmux_mobile_ffi.TerminalMouseEventFfi
import uniffi.bmux_mobile_ffi.TerminalMouseEventKindFfi
import uniffi.bmux_mobile_ffi.TerminalOpenRequestFfi
import uniffi.bmux_mobile_ffi.TerminalPointerShapeFfi
import uniffi.bmux_mobile_ffi.TerminalSizeFfi
import uniffi.bmux_mobile_ffi.TerminalStatusSeverityFfi

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                val vm: MainViewModel = viewModel()
                MainScreen(vm)
            }
        }
    }
}

/**
 * The app-private directory holding this device's key and saved hosts. It is excluded from
 * backups: a restored copy of the device key on another phone would impersonate this one.
 */
fun mobileStorageDir(context: Context): String =
    File(context.applicationContext.noBackupFilesDir, "bmux").absolutePath

/** A saved paired host as the UI shows it. */
data class HostUi(
    val alias: String,
    val hostKey: String,
    val role: String,
    val lastPath: String?,
) {
    val shortKey: String get() = hostKey.take(12)
}

/** A pairing in progress, or the outcome of the last one. */
data class PairingUi(
    val id: String,
    val status: PairingStatusFfi,
    val phrase: String?,
    val error: String?,
) {
    val finished: Boolean
        get() = status == PairingStatusFfi.PAIRED ||
            status == PairingStatusFfi.FAILED ||
            status == PairingStatusFfi.CANCELLED
}

data class MainUiState(
    val loading: Boolean = false,
    val hosts: List<HostUi> = emptyList(),
    val pairing: PairingUi? = null,
    val reconnectServiceEnabled: Boolean = false,
    val selectedSession: String = "",
    val activeTerminalHost: HostUi? = null,
    val lastConnection: String = "",
    val info: String = "",
    val warning: String? = null,
)

class MainViewModel(application: Application) : AndroidViewModel(application) {
    private val gateway = MobileGateway(application)
    private var pairingPoll: Job? = null

    var state by mutableStateOf(MainUiState(info = gateway.startupInfo))
        private set

    var pairingLink by mutableStateOf("")
    var hostAlias by mutableStateOf("")

    init {
        refreshHosts()
    }

    fun refreshHosts() {
        viewModelScope.launch {
            state = state.copy(loading = true, warning = null)
            val loaded = withContext(Dispatchers.IO) { gateway.listHosts() }
            state = loaded.fold(
                onSuccess = { state.copy(loading = false, hosts = it) },
                onFailure = { state.copy(loading = false, warning = it.message) },
            )
        }
    }

    /** Start pairing with the host whose `bmux remote pair` link was pasted. */
    fun startPairing() {
        val link = pairingLink.trim()
        if (link.isEmpty()) {
            state = state.copy(warning = "Paste the pairing link from `bmux remote pair` on the host")
            return
        }
        AlphaTelemetry.log(AlphaEventKind.PairStart, "alias=${hostAlias.ifBlank { "auto" }}")
        viewModelScope.launch {
            state = state.copy(warning = null)
            val started = withContext(Dispatchers.IO) {
                gateway.startPairing(link, hostAlias.trim().ifBlank { null }, deviceName())
            }
            started.fold(
                onSuccess = { pairing ->
                    // The link is single-use and secret: never keep it after it is sent.
                    pairingLink = ""
                    state = state.copy(pairing = pairing)
                    followPairing(pairing.id)
                },
                onFailure = { state = state.copy(warning = it.message ?: "Pairing failed") },
            )
        }
    }

    fun cancelPairing() {
        val pairing = state.pairing ?: return
        pairingPoll?.cancel()
        viewModelScope.launch {
            val cancelled = withContext(Dispatchers.IO) { gateway.cancelPairing(pairing.id) }
            state = state.copy(pairing = cancelled.getOrNull() ?: pairing.copy(status = PairingStatusFfi.CANCELLED))
        }
    }

    fun dismissPairing() {
        if (state.pairing?.finished == true) {
            state = state.copy(pairing = null)
        }
    }

    /** Follow a pairing until it finishes. The state is local, so reading it often is cheap. */
    private fun followPairing(id: String) {
        pairingPoll?.cancel()
        pairingPoll = viewModelScope.launch {
            while (true) {
                delay(PAIRING_POLL_MS)
                val current = withContext(Dispatchers.IO) { gateway.pairingState(id) }.getOrElse {
                    state = state.copy(warning = it.message)
                    return@launch
                }
                state = state.copy(pairing = current)
                if (current.finished) {
                    when (current.status) {
                        PairingStatusFfi.PAIRED -> {
                            AlphaTelemetry.log(AlphaEventKind.PairSuccess, "pairing=$id")
                            hostAlias = ""
                            refreshHosts()
                        }
                        PairingStatusFfi.FAILED -> AlphaTelemetry.log(
                            AlphaEventKind.PairFailure,
                            "error=${current.error ?: "unknown"}",
                        )
                        else -> Unit
                    }
                    return@launch
                }
            }
        }
    }

    fun connect(host: HostUi) {
        viewModelScope.launch {
            state = state.copy(loading = true, warning = null)
            val session = state.selectedSession.ifBlank { null }
            AlphaTelemetry.log(AlphaEventKind.ConnectAttempt, "host=${host.alias}")
            val result = withContext(Dispatchers.IO) { gateway.connect(host.alias, session) }
            state = result.fold(
                onSuccess = { connection ->
                    AlphaTelemetry.log(
                        AlphaEventKind.ConnectSuccess,
                        "host=${host.alias},status=${connection.status}",
                    )
                    state.copy(loading = false, lastConnection = "${host.alias}: ${connection.status}")
                },
                onFailure = { error ->
                    AlphaTelemetry.log(
                        AlphaEventKind.ConnectFailure,
                        "host=${host.alias},error=${error.message ?: "unknown"}",
                    )
                    state.copy(loading = false, warning = error.message ?: "Connection failed")
                },
            )
        }
    }

    fun forget(host: HostUi) {
        viewModelScope.launch {
            val result = withContext(Dispatchers.IO) { gateway.forgetHost(host.alias) }
            if (result.isSuccess) {
                AlphaTelemetry.log(AlphaEventKind.ForgetHost, "host=${host.alias}")
                state = state.copy(
                    info = "Forgot ${host.alias}. Revoke this device on the host with `bmux remote revoke`.",
                )
                refreshHosts()
            } else {
                state = state.copy(warning = result.exceptionOrNull()?.message)
            }
        }
    }

    fun setReconnectService(host: HostUi, enabled: Boolean) {
        if (enabled) {
            gateway.startReconnectService(host.alias, state.selectedSession.ifBlank { null })
            AlphaTelemetry.log(AlphaEventKind.ReconnectEnabled, "host=${host.alias}")
        } else {
            gateway.stopReconnectService()
            AlphaTelemetry.log(AlphaEventKind.ReconnectDisabled, "host=${host.alias}")
        }
        state = state.copy(reconnectServiceEnabled = enabled)
    }

    fun openTerminal(host: HostUi) {
        state = state.copy(activeTerminalHost = host, warning = null)
    }

    fun closeTerminal() {
        state = state.copy(activeTerminalHost = null)
    }

    fun openTerminal(host: String, session: String?, rows: Int, cols: Int): Result<String> {
        return gateway.openTerminal(host, session, rows, cols)
    }

    fun pollTerminalOutput(terminalId: String, maxChunks: Int): Result<List<TerminalChunkFrame>> {
        return gateway.pollTerminalOutput(terminalId, maxChunks)
    }

    fun writeTerminalInput(terminalId: String, bytes: ByteArray): Result<Unit> {
        return gateway.writeTerminalInput(terminalId, bytes)
    }

    fun sendTerminalMouseEvent(terminalId: String, event: TerminalMouseEvent): Result<Unit> {
        return gateway.sendTerminalMouseEvent(terminalId, event)
    }

    fun resizeTerminal(terminalId: String, rows: Int, cols: Int): Result<Unit> {
        return gateway.resizeTerminal(terminalId, rows, cols)
    }

    fun closeTerminalStream(terminalId: String): Result<Unit> {
        return gateway.closeTerminal(terminalId)
    }

    fun terminalDiagnostics(
        terminalId: String,
        sinceSequence: Long?,
        limit: Int,
    ): Result<List<TerminalDiagnosticFrame>> {
        return gateway.terminalDiagnostics(terminalId, sinceSequence, limit)
    }

    fun latestTerminalFailure(terminalId: String): Result<String?> {
        return gateway.latestTerminalFailure(terminalId)
    }

    fun updateSession(value: String) {
        state = state.copy(selectedSession = value)
    }

    override fun onCleared() {
        pairingPoll?.cancel()
        super.onCleared()
    }

    private fun deviceName(): String = listOf(Build.MANUFACTURER, Build.MODEL)
        .filter { !it.isNullOrBlank() }
        .joinToString(" ")
        .ifBlank { "android" }

    private companion object {
        const val PAIRING_POLL_MS = 250L
    }
}

private class MobileGateway(application: Application) {
    private val context = application.applicationContext
    private val store = SecureStore(context)
    private val ffi = runCatching { MobileApiFfi(mobileStorageDir(context)) }.getOrNull()

    val startupInfo: String = if (ffi != null) {
        "Pair a host with the link from `bmux remote pair` on it"
    } else {
        "bmux native library unavailable in this build"
    }

    private fun client(): MobileApiFfi =
        ffi ?: throw IllegalStateException("bmux native library unavailable")

    fun listHosts(): Result<List<HostUi>> = runGatewayCall {
        val health = store.loadHostHealth().associateBy { it.host }
        client().listHosts()
            .map(::mapHost)
            .sortedWith(
                compareByDescending<HostUi> { health[it.alias]?.lastSuccessMs ?: 0L }
                    .thenBy { it.alias.lowercase() },
            )
    }

    fun startPairing(link: String, alias: String?, deviceName: String): Result<PairingUi> =
        runGatewayCall { mapPairing(client().startPairing(link, alias, deviceName)) }

    fun pairingState(id: String): Result<PairingUi> =
        runGatewayCall { mapPairing(client().pairingState(id)) }

    fun cancelPairing(id: String): Result<PairingUi> =
        runGatewayCall { mapPairing(client().cancelPairing(id)) }

    fun forgetHost(alias: String): Result<Unit> = runGatewayCall {
        client().forgetHost(alias)
        store.forgetHost(alias)
    }

    fun connect(host: String, session: String?): Result<ConnectionStateFfi> {
        store.saveLastConnectedHost(host)
        val connected = runGatewayCall { client().connect(host, session) }
        store.recordHostHealth(
            host = host,
            success = connected.isSuccess,
            timestampMs = System.currentTimeMillis(),
        )
        return connected
    }

    fun openTerminal(host: String, session: String?, rows: Int, cols: Int): Result<String> {
        val client = ffi ?: return Result.failure(
            IllegalStateException("FFI terminal stream unavailable"),
        )
        return runGatewayCall {
            val request = TerminalOpenRequestFfi(
                host = host,
                session = session,
                rows = rows.toTerminalUShort("rows"),
                cols = cols.toTerminalUShort("cols"),
            )
            client.openTerminal(request).id
        }
    }

    fun pollTerminalOutput(terminalId: String, maxChunks: Int): Result<List<TerminalChunkFrame>> {
        val client = ffi ?: return Result.failure(
            IllegalStateException("FFI terminal stream unavailable"),
        )
        return runGatewayCall {
            val presentation = client.pollTerminalPresentation(terminalId, maxChunks.toUInt(), 1u)
            val chunks = presentation.chunks.map { chunk ->
                TerminalChunkFrame(
                    kind = when (chunk.kind) {
                        TerminalChunkKindFfi.STDOUT -> TerminalChunkType.STDOUT
                        TerminalChunkKindFfi.STDERR -> TerminalChunkType.STDERR
                        TerminalChunkKindFfi.STATUS -> TerminalChunkType.STATUS
                    },
                    bytes = chunk.bytes,
                    statusSeverity = when (chunk.statusSeverity) {
                        TerminalStatusSeverityFfi.INFO -> TerminalStatusSeverity.INFO
                        TerminalStatusSeverityFfi.WARN -> TerminalStatusSeverity.WARN
                        TerminalStatusSeverityFfi.ERROR -> TerminalStatusSeverity.ERROR
                        null -> null
                    },
                )
            }
            val pointer = presentation.pointer
            if (pointer == null) chunks else chunks + TerminalChunkFrame(
                kind = TerminalChunkType.POINTER,
                bytes = byteArrayOf(),
                pointer = TerminalPointerFrame(
                    version = pointer.version.toInt(),
                    generation = pointer.generation,
                    revision = pointer.revision,
                    name = when (val shape = pointer.shape) {
                        is TerminalPointerShapeFfi.Hidden -> null
                        is TerminalPointerShapeFfi.Shape -> shape.name
                    },
                ),
            )
        }
    }

    fun writeTerminalInput(terminalId: String, bytes: ByteArray): Result<Unit> {
        val client = ffi ?: return Result.failure(
            IllegalStateException("FFI terminal stream unavailable"),
        )
        return runGatewayCall {
            client.writeTerminalInput(terminalId, bytes)
        }
    }

    fun sendTerminalMouseEvent(terminalId: String, event: TerminalMouseEvent): Result<Unit> {
        val client = ffi ?: return Result.failure(
            IllegalStateException("FFI terminal stream unavailable"),
        )
        return runGatewayCall {
            client.sendTerminalMouseEvent(
                terminalId,
                TerminalMouseEventFfi(
                    kind = when (event.kind) {
                        TerminalMouseEventKind.DOWN -> TerminalMouseEventKindFfi.DOWN
                        TerminalMouseEventKind.UP -> TerminalMouseEventKindFfi.UP
                        TerminalMouseEventKind.DRAG -> TerminalMouseEventKindFfi.DRAG
                        TerminalMouseEventKind.MOVE -> TerminalMouseEventKindFfi.MOVE
                        TerminalMouseEventKind.LEAVE -> TerminalMouseEventKindFfi.LEAVE
                        TerminalMouseEventKind.SCROLL_UP -> TerminalMouseEventKindFfi.SCROLL_UP
                        TerminalMouseEventKind.SCROLL_DOWN -> TerminalMouseEventKindFfi.SCROLL_DOWN
                        TerminalMouseEventKind.SCROLL_LEFT -> TerminalMouseEventKindFfi.SCROLL_LEFT
                        TerminalMouseEventKind.SCROLL_RIGHT -> TerminalMouseEventKindFfi.SCROLL_RIGHT
                    },
                    button = when (event.button) {
                        TerminalMouseButton.LEFT -> TerminalMouseButtonFfi.LEFT
                        TerminalMouseButton.MIDDLE -> TerminalMouseButtonFfi.MIDDLE
                        TerminalMouseButton.RIGHT -> TerminalMouseButtonFfi.RIGHT
                        null -> null
                    },
                    row = event.row.toTerminalCellUShort("row"),
                    col = event.col.toTerminalCellUShort("col"),
                    shift = event.shift,
                    alt = event.alt,
                    control = event.control,
                ),
            )
        }
    }

    fun resizeTerminal(terminalId: String, rows: Int, cols: Int): Result<Unit> {
        val client = ffi ?: return Result.failure(
            IllegalStateException("FFI terminal stream unavailable"),
        )
        return runGatewayCall {
            client.resizeTerminal(
                terminalId,
                TerminalSizeFfi(
                    rows = rows.toTerminalUShort("rows"),
                    cols = cols.toTerminalUShort("cols"),
                ),
            )
        }
    }

    fun closeTerminal(terminalId: String): Result<Unit> {
        val client = ffi ?: return Result.failure(
            IllegalStateException("FFI terminal stream unavailable"),
        )
        return runGatewayCall {
            client.closeTerminal(terminalId)
            Unit
        }
    }

    fun terminalDiagnostics(
        terminalId: String,
        sinceSequence: Long?,
        limit: Int,
    ): Result<List<TerminalDiagnosticFrame>> {
        val client = ffi ?: return Result.failure(
            IllegalStateException("FFI terminal stream unavailable"),
        )
        val safeLimit = limit.coerceIn(1, 512)
        return runGatewayCall {
            client.terminalDiagnostics(
                terminalId,
                sinceSequence?.toULong(),
                safeLimit.toUInt(),
            ).map(::mapTerminalDiagnostic)
        }
    }

    fun latestTerminalFailure(terminalId: String): Result<String?> {
        val client = ffi ?: return Result.failure(
            IllegalStateException("FFI terminal stream unavailable"),
        )
        return runGatewayCall {
            client.latestTerminalFailure(terminalId)
        }
    }

    fun startReconnectService(host: String, session: String?) {
        ConnectionForegroundService.start(context, host, session)
    }

    fun stopReconnectService() {
        ConnectionForegroundService.stop(context)
    }

    private fun mapHost(host: PairedHostFfi): HostUi = HostUi(
        alias = host.alias,
        hostKey = host.hostKey,
        role = when (host.role) {
            PairedHostRoleFfi.FULL -> "full"
            PairedHostRoleFfi.VIEW_ONLY -> "view-only"
            null -> "unknown"
        },
        lastPath = host.lastPath,
    )

    private fun mapPairing(state: PairingStateFfi): PairingUi = PairingUi(
        id = state.id,
        status = state.status,
        phrase = state.phrase,
        error = state.error,
    )

    private fun <T> runGatewayCall(block: () -> T): Result<T> {
        return try {
            Result.success(block())
        } catch (error: Throwable) {
            Result.failure(IllegalStateException(userFacingGatewayMessage(error), error))
        }
    }

    private fun userFacingGatewayMessage(error: Throwable): String {
        val resolved = when (error) {
            is MobileFfiException.InvalidPairingLink -> "not a bmux pairing link: ${error.v1}"
            is MobileFfiException.PairingNotFound -> "pairing is no longer in progress"
            is MobileFfiException.HostNotFound -> "no saved host named ${error.v1}"
            is MobileFfiException.HostStore -> error.v1
            is MobileFfiException.ConnectionFailed -> error.v1
            is MobileFfiException.InvalidArgument -> "invalid argument: ${error.v1}"
            is MobileFfiException.ConnectionNotActive -> "connection not active: ${error.v1}"
            is MobileFfiException.TerminalSessionNotFound -> "terminal session ended"
            is MobileFfiException.TerminalSessionClosed -> "terminal session is closed"
            is MobileFfiException.InvalidTerminalSize -> "invalid terminal size"
            is MobileFfiException.TerminalBackendFailure -> error.v1
            else -> error.message
        }
        return resolved?.takeIf { it.isNotBlank() } ?: "unknown error"
    }

    private fun mapTerminalDiagnostic(record: TerminalDiagnosticFfi): TerminalDiagnosticFrame {
        return TerminalDiagnosticFrame(
            sequence = record.sequence.toLong(),
            timestampMs = record.timestampMs.toLong(),
            severity = when (record.severity) {
                TerminalStatusSeverityFfi.INFO -> TerminalStatusSeverity.INFO
                TerminalStatusSeverityFfi.WARN -> TerminalStatusSeverity.WARN
                TerminalStatusSeverityFfi.ERROR -> TerminalStatusSeverity.ERROR
            },
            stage = record.stage,
            code = record.code,
            message = record.message,
        )
    }

    private fun Int.toTerminalUShort(name: String): UShort {
        require(this in 1..65_535) { "$name out of range: $this" }
        return toUShort()
    }

    private fun Int.toTerminalCellUShort(name: String): UShort {
        require(this in 0..65_535) { "$name out of range: $this" }
        return toUShort()
    }
}


@Composable
@OptIn(ExperimentalMaterial3Api::class)
private fun MainScreen(vm: MainViewModel) {
    val state = vm.state
    val context = androidx.compose.ui.platform.LocalContext.current
    val notificationPermissions = remember { notificationRuntimePermissions() }
    val notificationPermissionLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { _ -> }

    Scaffold(
        topBar = {
            TopAppBar(title = { Text("bmux mobile alpha") })
        },
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            state.activeTerminalHost?.let { activeHost ->
                TerminalSessionScreen(
                    endpoint = TerminalEndpoint(
                        host = activeHost.alias,
                        description = "${activeHost.role}, host key ${activeHost.shortKey}",
                    ),
                    session = state.selectedSession.ifBlank { null },
                    onBack = vm::closeTerminal,
                    openTerminal = { host, session, rows, cols ->
                        withContext(Dispatchers.IO) { vm.openTerminal(host, session, rows, cols) }
                    },
                    pollTerminalOutput = { terminalId, maxChunks ->
                        withContext(Dispatchers.IO) { vm.pollTerminalOutput(terminalId, maxChunks) }
                    },
                    writeTerminalInput = { terminalId, bytes ->
                        withContext(Dispatchers.IO) { vm.writeTerminalInput(terminalId, bytes) }
                    },
                    sendTerminalMouseEvent = { terminalId, event ->
                        withContext(Dispatchers.IO) { vm.sendTerminalMouseEvent(terminalId, event) }
                    },
                    resizeTerminal = { terminalId, rows, cols ->
                        withContext(Dispatchers.IO) { vm.resizeTerminal(terminalId, rows, cols) }
                    },
                    closeTerminal = { terminalId ->
                        withContext(Dispatchers.IO) { vm.closeTerminalStream(terminalId) }
                    },
                    fetchTerminalDiagnostics = { terminalId, sinceSequence, limit ->
                        withContext(Dispatchers.IO) {
                            vm.terminalDiagnostics(terminalId, sinceSequence, limit)
                        }
                    },
                    latestTerminalFailure = { terminalId ->
                        withContext(Dispatchers.IO) { vm.latestTerminalFailure(terminalId) }
                    },
                )
                return@Column
            }

            Text(text = state.info, style = MaterialTheme.typography.bodyMedium)

            val pairing = state.pairing
            if (pairing == null || pairing.finished) {
                OutlinedTextField(
                    value = vm.pairingLink,
                    onValueChange = { vm.pairingLink = it },
                    modifier = Modifier.fillMaxWidth(),
                    label = { Text("Pairing link") },
                    placeholder = { Text("bmux-pair:...") },
                    visualTransformation = PasswordVisualTransformation(),
                    singleLine = true,
                )
                OutlinedTextField(
                    value = vm.hostAlias,
                    onValueChange = { vm.hostAlias = it },
                    modifier = Modifier.fillMaxWidth(),
                    label = { Text("Save host as (optional)") },
                    singleLine = true,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    Button(onClick = { vm.startPairing() }) {
                        Text("Pair")
                    }
                    Button(onClick = { vm.refreshHosts() }) {
                        Text("Refresh")
                    }
                }
            }
            pairing?.let {
                PairingCard(
                    pairing = it,
                    onCancel = vm::cancelPairing,
                    onDismiss = vm::dismissPairing,
                )
            }

            OutlinedTextField(
                value = state.selectedSession,
                onValueChange = { vm.updateSession(it) },
                modifier = Modifier.fillMaxWidth(),
                label = { Text("Session (optional)") },
                singleLine = true,
            )

            state.warning?.let {
                Text(text = it, color = MaterialTheme.colorScheme.error)
            }
            if (state.lastConnection.isNotEmpty()) {
                Text(text = state.lastConnection)
            }

            Text(
                text = if (state.loading) "Working..." else "Paired hosts",
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.SemiBold,
            )
            if (!state.loading && state.hosts.isEmpty()) {
                Text(
                    text = "No paired hosts. On the host, run `bmux remote pair` and paste its link above.",
                    style = MaterialTheme.typography.bodySmall,
                )
            }

            LazyColumn(
                modifier = Modifier.fillMaxSize(),
                contentPadding = PaddingValues(bottom = 40.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                items(state.hosts, key = { it.alias }) { host ->
                    HostCard(
                        host = host,
                        reconnectEnabled = state.reconnectServiceEnabled,
                        onConnect = { vm.connect(host) },
                        onTerminal = { vm.openTerminal(host) },
                        onForget = { vm.forget(host) },
                        onToggleReconnect = {
                            if (!state.reconnectServiceEnabled && !hasAllPermissions(context, notificationPermissions)) {
                                notificationPermissionLauncher.launch(notificationPermissions)
                            } else {
                                vm.setReconnectService(host, !state.reconnectServiceEnabled)
                            }
                        },
                    )
                }
            }
        }
    }
}

private fun notificationRuntimePermissions(): Array<String> {
    return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
        arrayOf(Manifest.permission.POST_NOTIFICATIONS)
    } else {
        emptyArray()
    }
}

private fun hasAllPermissions(context: Context, permissions: Array<String>): Boolean {
    if (permissions.isEmpty()) {
        return true
    }
    return permissions.all { permission ->
        ContextCompat.checkSelfPermission(context, permission) == PackageManager.PERMISSION_GRANTED
    }
}

@Composable
private fun PairingCard(pairing: PairingUi, onCancel: () -> Unit, onDismiss: () -> Unit) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            when (pairing.status) {
                PairingStatusFfi.CONNECTING -> Text("Contacting the host...")
                PairingStatusFfi.AWAITING_APPROVAL -> {
                    Text("Check that the host shows the same phrase, then approve it there:")
                    Text(
                        text = pairing.phrase.orEmpty(),
                        style = MaterialTheme.typography.headlineSmall,
                        fontWeight = FontWeight.SemiBold,
                    )
                }
                PairingStatusFfi.PAIRED -> Text("Paired.")
                PairingStatusFfi.FAILED -> Text(
                    text = "Pairing failed: ${pairing.error ?: "unknown error"}",
                    color = MaterialTheme.colorScheme.error,
                )
                PairingStatusFfi.CANCELLED -> Text("Pairing cancelled.")
            }
            if (pairing.finished) {
                OutlinedButton(onClick = onDismiss) { Text("Dismiss") }
            } else {
                OutlinedButton(onClick = onCancel) { Text("Cancel") }
            }
        }
    }
}

@Composable
private fun HostCard(
    host: HostUi,
    reconnectEnabled: Boolean,
    onConnect: () -> Unit,
    onTerminal: () -> Unit,
    onForget: () -> Unit,
    onToggleReconnect: () -> Unit,
) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Text(text = host.alias, fontWeight = FontWeight.SemiBold)
            Text(
                text = "host key ${host.shortKey} | role ${host.role}" +
                    (host.lastPath?.let { " | last path $it" } ?: ""),
                style = MaterialTheme.typography.bodySmall,
            )
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = onConnect) {
                    Text("Connect")
                }
                Button(onClick = onTerminal) {
                    Text("Terminal")
                }
                Button(onClick = onToggleReconnect) {
                    Text(if (reconnectEnabled) "Stop Reconnect" else "Reconnect")
                }
                OutlinedButton(onClick = onForget) {
                    Text("Forget")
                }
            }
        }
    }
}
