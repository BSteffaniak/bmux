package io.bmux.android

import android.app.Application
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
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
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import androidx.lifecycle.viewmodel.compose.viewModel
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.bmux_mobile_ffi.ConnectionStateFfi
import uniffi.bmux_mobile_ffi.ConnectionStatusFfi
import uniffi.bmux_mobile_ffi.HostKeyPinSuggestionFfi
import uniffi.bmux_mobile_ffi.MobileApiFfi
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
    val discoveryRunning: Boolean = false,
    val reconnectServiceEnabled: Boolean = false,
    val selectedSession: String = "main",
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
            val result = withContext(Dispatchers.IO) { gateway.connect(target.id, session) }
            state = if (result.isSuccess) {
                val connection = result.getOrThrow()
                state.copy(
                    loading = false,
                    lastConnection = "${target.name}: ${connection.status}",
                )
            } else {
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
            val result = withContext(Dispatchers.IO) {
                gateway.observeAndApplyPin(target.canonicalTarget)
            }
            state = if (result.isSuccess) {
                state.copy(loading = false, lastHostKey = result.getOrThrow())
            } else {
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
        } else {
            gateway.stopReconnectService()
        }
        state = state.copy(reconnectServiceEnabled = enabled)
    }

    fun setDiscoveryEnabled(enabled: Boolean) {
        if (!enabled) {
            discovery.stop()
            state = state.copy(discoveryRunning = false, discoveredTargets = emptyList())
            return
        }

        discovery.start(
            onUpdate = { discovered ->
                viewModelScope.launch {
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

    fun startupInfoWithSecurityState(): String {
        val pinnedCount = store.pinnedTargets().size
        val lastTarget = store.lastConnectedTargetId()
        val suffix = if (lastTarget != null) "last target tracked" else "no prior target"
        return "$startupInfo | pinned=$pinnedCount | $suffix"
    }

    fun listTargets(): List<TargetUi> {
        val fromFfi = runCatching { ffi?.listTargets() }
            .getOrNull()
            ?.map(::mapTarget)
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
        }
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

    fun startReconnectService(targetId: String, session: String?) {
        ConnectionForegroundService.start(context, targetId, session)
    }

    fun stopReconnectService() {
        ConnectionForegroundService.stop(context)
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
                Button(onClick = { vm.setDiscoveryEnabled(!state.discoveryRunning) }) {
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
                        onPin = { vm.observeAndPin(target) },
                        onToggleReconnect = { vm.setReconnectService(target, !state.reconnectServiceEnabled) },
                    )
                }
            }
        }
    }
}

@Composable
private fun TargetCard(
    target: TargetUi,
    reconnectEnabled: Boolean,
    onConnect: () -> Unit,
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
