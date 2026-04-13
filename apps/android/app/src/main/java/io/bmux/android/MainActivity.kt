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
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import androidx.lifecycle.viewmodel.compose.viewModel
import io.bmux.android.terminal.TerminalEndpoint
import io.bmux.android.terminal.TerminalSessionScreen
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.bmux_mobile_ffi.ConnectionStateFfi
import uniffi.bmux_mobile_ffi.ConnectionStatusFfi
import uniffi.bmux_mobile_ffi.HostKeyPinSuggestionFfi
import uniffi.bmux_mobile_ffi.MobileApiFfi
import uniffi.bmux_mobile_ffi.TerminalOpenRequestFfi
import uniffi.bmux_mobile_ffi.TerminalSizeFfi
import uniffi.bmux_mobile_ffi.TargetRecordFfi

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

data class TargetUi(
    val id: String,
    val name: String,
    val canonicalTarget: String,
    val transport: String,
)

data class DiscoveredTargetUi(
    val serviceName: String,
    val host: String,
    val port: Int,
) {
    val targetInput: String get() = "$host:$port"
}

data class MainUiState(
    val loading: Boolean = false,
    val targets: List<TargetUi> = emptyList(),
    val discoveredTargets: List<DiscoveredTargetUi> = emptyList(),
    val discoveryHistory: List<DiscoveredTargetUi> = emptyList(),
    val discoveryRunning: Boolean = false,
    val reconnectServiceEnabled: Boolean = false,
    val selectedSession: String = "main",
    val activeTerminalTarget: TargetUi? = null,
    val lastConnection: String = "",
    val lastHostKey: String = "",
    val info: String = "",
    val warning: String? = null,
)

class MainViewModel(application: Application) : AndroidViewModel(application) {
    private val gateway = MobileGateway(application)
    private val discovery = LanDiscoveryManager(application)

    var state by mutableStateOf(MainUiState(info = gateway.startupInfo))
        private set

    var newTarget by mutableStateOf("")
    var targetName by mutableStateOf("")

    init {
        refreshTargets()
    }

    fun refreshTargets() {
        viewModelScope.launch {
            state = state.copy(loading = true, warning = null)
            val loaded = withContext(Dispatchers.IO) { gateway.listTargets() }
            state = state.copy(
                loading = false,
                targets = loaded,
                discoveryHistory = gateway.discoveryHistory(),
                info = gateway.startupInfoWithSecurityState(),
            )
        }
    }

    fun importTarget() {
        val source = newTarget.trim()
        if (source.isEmpty()) {
            state = state.copy(warning = "Target cannot be empty")
            return
        }
        AlphaTelemetry.log(AlphaEventKind.ImportTarget, "source=$source")

        viewModelScope.launch {
            state = state.copy(loading = true, warning = null)
            val result = withContext(Dispatchers.IO) {
                gateway.importTarget(source, targetName.trim().ifBlank { null })
            }
            if (result.isFailure) {
                state = state.copy(
                    loading = false,
                    warning = result.exceptionOrNull()?.message ?: "Import failed",
                )
                return@launch
            }

            newTarget = ""
            targetName = ""
            val loaded = withContext(Dispatchers.IO) { gateway.listTargets() }
            state = state.copy(loading = false, targets = loaded)
        }
    }

    fun importDiscoveredTarget(target: DiscoveredTargetUi) {
        newTarget = target.targetInput
        if (targetName.isBlank()) {
            targetName = target.serviceName
        }
        importTarget()
    }

    fun connect(target: TargetUi) {
        viewModelScope.launch {
            state = state.copy(loading = true, warning = null)
            val session = state.selectedSession.ifBlank { null }
            AlphaTelemetry.log(AlphaEventKind.ConnectAttempt, "target=${target.id}")
            val result = withContext(Dispatchers.IO) { gateway.connect(target.id, session) }
            state = if (result.isSuccess) {
                val connection = result.getOrThrow()
                AlphaTelemetry.log(
                    AlphaEventKind.ConnectSuccess,
                    "target=${target.id},status=${connection.status}",
                )
                state.copy(
                    loading = false,
                    lastConnection = "${target.name}: ${connection.status}",
                )
            } else {
                AlphaTelemetry.log(
                    AlphaEventKind.ConnectFailure,
                    "target=${target.id},error=${result.exceptionOrNull()?.message ?: "unknown"}",
                )
                state.copy(
                    loading = false,
                    warning = result.exceptionOrNull()?.message ?: "Connection failed",
                )
            }
        }
    }

    fun observeAndPin(target: TargetUi) {
        viewModelScope.launch {
            state = state.copy(loading = true, warning = null)
            AlphaTelemetry.log(AlphaEventKind.ObservePinAttempt, "target=${target.id}")
            val result = withContext(Dispatchers.IO) {
                gateway.observeAndApplyPin(target.canonicalTarget)
            }
            state = if (result.isSuccess) {
                AlphaTelemetry.log(AlphaEventKind.ObservePinSuccess, "target=${target.id}")
                state.copy(loading = false, lastHostKey = result.getOrThrow())
            } else {
                AlphaTelemetry.log(
                    AlphaEventKind.ObservePinFailure,
                    "target=${target.id},error=${result.exceptionOrNull()?.message ?: "unknown"}",
                )
                state.copy(
                    loading = false,
                    warning = result.exceptionOrNull()?.message ?: "Host key check failed",
                )
            }
        }
    }

    fun setReconnectService(target: TargetUi, enabled: Boolean) {
        if (enabled) {
            gateway.startReconnectService(target.id, state.selectedSession.ifBlank { null })
            AlphaTelemetry.log(AlphaEventKind.ReconnectEnabled, "target=${target.id}")
        } else {
            gateway.stopReconnectService()
            AlphaTelemetry.log(AlphaEventKind.ReconnectDisabled, "target=${target.id}")
        }
        state = state.copy(reconnectServiceEnabled = enabled)
    }

    fun openTerminal(target: TargetUi) {
        state = state.copy(activeTerminalTarget = target, warning = null)
    }

    fun closeTerminal() {
        state = state.copy(activeTerminalTarget = null)
    }

    fun openTerminal(targetId: String, session: String?, rows: Int, cols: Int): Result<String> {
        return gateway.openTerminal(targetId, session, rows, cols)
    }

    fun pollTerminalOutput(terminalId: String, maxChunks: Int): Result<List<ByteArray>> {
        return gateway.pollTerminalOutput(terminalId, maxChunks)
    }

    fun writeTerminalInput(terminalId: String, bytes: ByteArray): Result<Unit> {
        return gateway.writeTerminalInput(terminalId, bytes)
    }

    fun resizeTerminal(terminalId: String, rows: Int, cols: Int): Result<Unit> {
        return gateway.resizeTerminal(terminalId, rows, cols)
    }

    fun closeTerminalStream(terminalId: String): Result<Unit> {
        return gateway.closeTerminal(terminalId)
    }

    fun setDiscoveryEnabled(enabled: Boolean) {
        if (!enabled) {
            discovery.stop()
            AlphaTelemetry.log(AlphaEventKind.DiscoveryStop, "user_stopped=true")
            state = state.copy(discoveryRunning = false, discoveredTargets = emptyList())
            return
        }

        AlphaTelemetry.log(AlphaEventKind.DiscoveryStart, "user_started=true")

        discovery.start(
            onUpdate = { discovered ->
                gateway.recordDiscoverySnapshot(
                    discovered.map { DiscoveredTargetUi(it.serviceName, it.host, it.port) },
                )
                viewModelScope.launch {
                    AlphaTelemetry.log(AlphaEventKind.DiscoveryUpdate, "count=${discovered.size}")
                    state = state.copy(
                        discoveryRunning = true,
                        discoveredTargets = discovered.map {
                            DiscoveredTargetUi(it.serviceName, it.host, it.port)
                        },
                    )
                }
            },
            onError = { message ->
                viewModelScope.launch {
                    state = state.copy(discoveryRunning = false, warning = message)
                }
            },
        )
        state = state.copy(discoveryRunning = true)
    }

    fun updateSession(value: String) {
        state = state.copy(selectedSession = value)
    }

    override fun onCleared() {
        discovery.stop()
        super.onCleared()
    }
}

private class MobileGateway(application: Application) {
    private val context = application.applicationContext
    private val store = SecureStore(context)
    private val ffi = runCatching { MobileApiFfi() }.getOrNull()

    val startupInfo: String = if (ffi != null) {
        "FFI connected with encrypted local store"
    } else {
        "FFI unavailable in this runtime; offline encrypted store mode"
    }

    init {
        store.migrateLegacyIfNeeded()
    }

    fun startupInfoWithSecurityState(): String {
        val pinnedCount = store.pinnedTargets().size
        val lastTarget = store.lastConnectedTargetId()
        val migrated = if (store.migrateLegacyIfNeeded()) "legacy migrated" else "secure"
        val suffix = if (lastTarget != null) "last target tracked" else "no prior target"
        return "$startupInfo | pinned=$pinnedCount | $suffix | $migrated"
    }

    fun listTargets(): List<TargetUi> {
        val healthMap = store.loadTargetHealth().associateBy { it.targetId }
        val fromFfi = runCatching { ffi?.listTargets() }
            .getOrNull()
            ?.map(::mapTarget)
            ?.sortedWith(compareByDescending<TargetUi> { healthMap[it.id]?.lastSuccessMs ?: 0L }
                .thenBy { it.name.lowercase() })
            .orEmpty()
        if (fromFfi.isNotEmpty()) {
            store.saveTargets(fromFfi.map { it.toStored() })
            return fromFfi
        }
        return store.loadTargets().map {
            TargetUi(
                id = it.id,
                name = it.name,
                canonicalTarget = it.canonicalTarget,
                transport = it.transport,
            )
        }.sortedWith(compareByDescending<TargetUi> { healthMap[it.id]?.lastSuccessMs ?: 0L }
            .thenBy { it.name.lowercase() })
    }

    fun importTarget(source: String, displayName: String?): Result<TargetUi> {
        val imported = if (ffi != null) {
            val client = ffi
            runCatching { mapTarget(client!!.importTarget(source, displayName)) }
        } else {
            val local = TargetUi(
                id = "local-${System.currentTimeMillis()}",
                name = displayName ?: source,
                canonicalTarget = source,
                transport = guessTransport(source),
            )
            Result.success(local)
        }

        if (imported.isSuccess) {
            val updated = listTargets().toMutableList()
            val candidate = imported.getOrThrow()
            if (updated.none { it.id == candidate.id }) {
                updated.add(candidate)
            }
            store.saveTargets(updated.map { it.toStored() })
        }
        return imported
    }

    fun connect(targetId: String, session: String?): Result<ConnectionStateFfi> {
        store.saveLastConnectedTarget(targetId)
        val connected = if (ffi != null) {
            val client = ffi
            runCatching { client!!.connect(targetId, session) }
        } else {
            Result.success(
                ConnectionStateFfi(
                    id = "fallback-${System.currentTimeMillis()}",
                    targetId = targetId,
                    status = ConnectionStatusFfi.CONNECTED,
                    session = session,
                    lastError = null,
                ),
            )
        }
        store.recordTargetHealth(
            targetId = targetId,
            success = connected.isSuccess,
            timestampMs = System.currentTimeMillis(),
        )
        return connected
    }

    fun observeAndApplyPin(target: String): Result<String> {
        val client = ffi
            ?: return Result.failure(IllegalStateException("FFI host-key observation unavailable"))

        return runCatching {
            val suggestion: HostKeyPinSuggestionFfi = client.observeSshHostKeyWithPinSuggestion(target)
            val pinned = client.applyPinSuggestionToTarget(target, suggestion)
            store.savePinnedTarget(pinned)
            "Pinned ${suggestion.observed.algorithm} ${suggestion.observed.fingerprintSha256.take(16)}..."
        }
    }

    fun openTerminal(targetId: String, session: String?, rows: Int, cols: Int): Result<String> {
        val client = ffi ?: return Result.failure(
            IllegalStateException("FFI terminal stream unavailable"),
        )
        return runCatching {
            val request = TerminalOpenRequestFfi(
                targetId = targetId,
                session = session,
                rows = rows.toTerminalUShort("rows"),
                cols = cols.toTerminalUShort("cols"),
            )
            client.openTerminal(request).id
        }
    }

    fun pollTerminalOutput(terminalId: String, maxChunks: Int): Result<List<ByteArray>> {
        val client = ffi ?: return Result.failure(
            IllegalStateException("FFI terminal stream unavailable"),
        )
        return runCatching {
            client.pollTerminalOutput(terminalId, maxChunks.toUInt()).map { it.bytes }
        }
    }

    fun writeTerminalInput(terminalId: String, bytes: ByteArray): Result<Unit> {
        val client = ffi ?: return Result.failure(
            IllegalStateException("FFI terminal stream unavailable"),
        )
        return runCatching {
            client.writeTerminalInput(terminalId, bytes)
        }
    }

    fun resizeTerminal(terminalId: String, rows: Int, cols: Int): Result<Unit> {
        val client = ffi ?: return Result.failure(
            IllegalStateException("FFI terminal stream unavailable"),
        )
        return runCatching {
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
        return runCatching {
            client.closeTerminal(terminalId)
            Unit
        }
    }

    fun startReconnectService(targetId: String, session: String?) {
        ConnectionForegroundService.start(context, targetId, session)
    }

    fun stopReconnectService() {
        ConnectionForegroundService.stop(context)
    }

    fun recordDiscoverySnapshot(targets: List<DiscoveredTargetUi>) {
        store.upsertDiscoveryHistory(
            targets.map {
                StoredDiscovery(
                    serviceName = it.serviceName,
                    host = it.host,
                    port = it.port,
                    lastSeenMs = System.currentTimeMillis(),
                )
            },
        )
    }

    fun discoveryHistory(): List<DiscoveredTargetUi> {
        return store.loadDiscoveryHistory().map {
            DiscoveredTargetUi(
                serviceName = it.serviceName,
                host = it.host,
                port = it.port,
            )
        }
    }

    private fun mapTarget(record: TargetRecordFfi): TargetUi = TargetUi(
        id = record.id,
        name = record.name,
        canonicalTarget = record.canonicalTarget,
        transport = record.transport.name.lowercase(),
    )

    private fun guessTransport(source: String): String = when {
        source.startsWith("iroh://") -> "iroh"
        source.startsWith("ssh://") || source.contains('@') -> "ssh"
        source.startsWith("tls://") || source.contains(':') -> "tls"
        else -> "bmux"
    }

    private fun Int.toTerminalUShort(name: String): UShort {
        require(this in 1..65_535) { "$name out of range: $this" }
        return toUShort()
    }
}

private fun TargetUi.toStored(): StoredTarget = StoredTarget(
    id = id,
    name = name,
    canonicalTarget = canonicalTarget,
    transport = transport,
)

@Composable
@OptIn(ExperimentalMaterial3Api::class)
private fun MainScreen(vm: MainViewModel) {
    val state = vm.state
    val context = androidx.compose.ui.platform.LocalContext.current
    val discoveryPermissions = remember { discoveryRuntimePermissions() }
    val notificationPermissions = remember { notificationRuntimePermissions() }

    val discoveryPermissionLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { granted ->
        if (granted.values.all { it }) {
            vm.setDiscoveryEnabled(!state.discoveryRunning)
        }
    }
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
            state.activeTerminalTarget?.let { activeTarget ->
                TerminalSessionScreen(
                    endpoint = TerminalEndpoint(
                        id = activeTarget.id,
                        name = activeTarget.name,
                        canonicalTarget = activeTarget.canonicalTarget,
                    ),
                    session = state.selectedSession.ifBlank { null },
                    onBack = vm::closeTerminal,
                    openTerminal = { targetId, session, rows, cols ->
                        withContext(Dispatchers.IO) { vm.openTerminal(targetId, session, rows, cols) }
                    },
                    pollTerminalOutput = { terminalId, maxChunks ->
                        withContext(Dispatchers.IO) { vm.pollTerminalOutput(terminalId, maxChunks) }
                    },
                    writeTerminalInput = { terminalId, bytes ->
                        withContext(Dispatchers.IO) { vm.writeTerminalInput(terminalId, bytes) }
                    },
                    resizeTerminal = { terminalId, rows, cols ->
                        withContext(Dispatchers.IO) { vm.resizeTerminal(terminalId, rows, cols) }
                    },
                    closeTerminal = { terminalId ->
                        withContext(Dispatchers.IO) { vm.closeTerminalStream(terminalId) }
                    },
                )
                return@Column
            }

            Text(text = state.info, style = MaterialTheme.typography.bodyMedium)

            OutlinedTextField(
                value = vm.newTarget,
                onValueChange = { vm.newTarget = it },
                modifier = Modifier.fillMaxWidth(),
                label = { Text("Target URI or host") },
                placeholder = { Text("iroh://..., ssh://user@host:22, host:7443") },
                singleLine = true,
            )

            OutlinedTextField(
                value = vm.targetName,
                onValueChange = { vm.targetName = it },
                modifier = Modifier.fillMaxWidth(),
                label = { Text("Display name (optional)") },
                singleLine = true,
            )

            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                Button(onClick = { vm.importTarget() }) {
                    Text("Add target")
                }
                Button(onClick = { vm.refreshTargets() }) {
                    Text("Refresh")
                }
                Button(onClick = {
                    if (hasAllPermissions(context, discoveryPermissions)) {
                        vm.setDiscoveryEnabled(!state.discoveryRunning)
                    } else {
                        discoveryPermissionLauncher.launch(discoveryPermissions)
                    }
                }) {
                    Text(if (state.discoveryRunning) "Stop discovery" else "Start discovery")
                }
            }

            OutlinedTextField(
                value = state.selectedSession,
                onValueChange = { vm.updateSession(it) },
                modifier = Modifier.fillMaxWidth(),
                label = { Text("Session") },
                singleLine = true,
            )

            state.warning?.let {
                Text(text = it, color = MaterialTheme.colorScheme.error)
            }
            if (state.lastConnection.isNotEmpty()) {
                Text(text = state.lastConnection)
            }
            if (state.lastHostKey.isNotEmpty()) {
                Text(text = state.lastHostKey)
            }

            if (state.discoveredTargets.isNotEmpty()) {
                Text(text = "Discovered on LAN", fontWeight = FontWeight.SemiBold)
                LazyColumn(
                    modifier = Modifier.fillMaxWidth(),
                    contentPadding = PaddingValues(bottom = 8.dp),
                    verticalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    items(state.discoveredTargets, key = { "${it.host}:${it.port}" }) { target ->
                        Card(modifier = Modifier.fillMaxWidth()) {
                            Row(
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .padding(10.dp),
                                horizontalArrangement = Arrangement.SpaceBetween,
                            ) {
                                Column {
                                    Text(target.serviceName, fontWeight = FontWeight.SemiBold)
                                    Text("${target.host}:${target.port}")
                                }
                                Button(onClick = { vm.importDiscoveredTarget(target) }) {
                                    Text("Import")
                                }
                            }
                        }
                    }
                }
            }

            if (!state.discoveryRunning && state.discoveredTargets.isEmpty() && state.discoveryHistory.isNotEmpty()) {
                Text(text = "Recent LAN targets", fontWeight = FontWeight.SemiBold)
                LazyColumn(
                    modifier = Modifier.fillMaxWidth(),
                    contentPadding = PaddingValues(bottom = 8.dp),
                    verticalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    items(state.discoveryHistory, key = { "recent-${it.host}:${it.port}" }) { target ->
                        Card(modifier = Modifier.fillMaxWidth()) {
                            Row(
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .padding(10.dp),
                                horizontalArrangement = Arrangement.SpaceBetween,
                            ) {
                                Column {
                                    Text(target.serviceName, fontWeight = FontWeight.SemiBold)
                                    Text("${target.host}:${target.port}")
                                }
                                Button(onClick = { vm.importDiscoveredTarget(target) }) {
                                    Text("Import")
                                }
                            }
                        }
                    }
                }
            }

            Text(
                text = if (state.loading) "Working..." else "Targets",
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.SemiBold,
            )

            LazyColumn(
                modifier = Modifier.fillMaxSize(),
                contentPadding = PaddingValues(bottom = 40.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                items(state.targets, key = { it.id }) { target ->
                    TargetCard(
                        target = target,
                        reconnectEnabled = state.reconnectServiceEnabled,
                        onConnect = { vm.connect(target) },
                        onTerminal = { vm.openTerminal(target) },
                        onPin = { vm.observeAndPin(target) },
                        onToggleReconnect = {
                            if (!state.reconnectServiceEnabled && !hasAllPermissions(context, notificationPermissions)) {
                                notificationPermissionLauncher.launch(notificationPermissions)
                            } else {
                                vm.setReconnectService(target, !state.reconnectServiceEnabled)
                            }
                        },
                    )
                }
            }
        }
    }
}

private fun discoveryRuntimePermissions(): Array<String> {
    return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
        arrayOf(Manifest.permission.NEARBY_WIFI_DEVICES)
    } else {
        arrayOf(Manifest.permission.ACCESS_FINE_LOCATION)
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
private fun TargetCard(
    target: TargetUi,
    reconnectEnabled: Boolean,
    onConnect: () -> Unit,
    onTerminal: () -> Unit,
    onPin: () -> Unit,
    onToggleReconnect: () -> Unit,
) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Text(text = target.name, fontWeight = FontWeight.SemiBold)
            Text(text = target.canonicalTarget, style = MaterialTheme.typography.bodySmall)
            Text(text = "transport: ${target.transport}", style = MaterialTheme.typography.bodySmall)
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = onConnect) {
                    Text("Connect")
                }
                Button(onClick = onTerminal) {
                    Text("Terminal")
                }
                Button(onClick = onPin) {
                    Text("Observe + Pin")
                }
                Button(onClick = onToggleReconnect) {
                    Text(if (reconnectEnabled) "Stop Reconnect" else "Reconnect")
                }
            }
        }
    }
}
