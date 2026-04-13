package io.bmux.android

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
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import androidx.lifecycle.viewmodel.compose.viewModel
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.bmux_mobile_ffi.ConnectionStateFfi
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

data class MainUiState(
    val loading: Boolean = false,
    val targets: List<TargetUi> = emptyList(),
    val selectedSession: String = "main",
    val lastConnection: String = "",
    val lastHostKey: String = "",
    val info: String = "",
    val warning: String? = null,
)

class MainViewModel : ViewModel() {
    private val gateway = MobileGateway()

    var state by mutableStateOf(
        MainUiState(info = gateway.startupInfo),
    )
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
            state = state.copy(loading = false, targets = loaded)
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
                gateway.importTarget(
                    source = source,
                    displayName = targetName.trim().ifBlank { null },
                )
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

    fun connect(target: TargetUi) {
        viewModelScope.launch {
            state = state.copy(loading = true, warning = null)
            val result = withContext(Dispatchers.IO) {
                gateway.connect(target.id, state.selectedSession.ifBlank { null })
            }
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
                state.copy(
                    loading = false,
                    lastHostKey = result.getOrThrow(),
                )
            } else {
                state.copy(
                    loading = false,
                    warning = result.exceptionOrNull()?.message ?: "Host key check failed",
                )
            }
        }
    }

    fun updateSession(value: String) {
        state = state.copy(selectedSession = value)
    }
}

private class MobileGateway {
    val startupInfo: String
    private val ffi: MobileApiFfi?
    private val fallbackTargets = mutableListOf<TargetUi>()

    init {
        val client = runCatching { MobileApiFfi() }
        ffi = client.getOrNull()
        startupInfo = if (ffi != null) {
            "FFI connected"
        } else {
            "FFI unavailable in this runtime; using in-memory fallback"
        }
    }

    fun listTargets(): List<TargetUi> {
        val client = ffi ?: return fallbackTargets.toList()
        return runCatching { client.listTargets() }
            .getOrDefault(emptyList())
            .map(::mapTarget)
    }

    fun importTarget(source: String, displayName: String?): Result<TargetUi> {
        val client = ffi
        if (client == null) {
            val id = "local-${System.currentTimeMillis()}"
            val target = TargetUi(
                id = id,
                name = displayName ?: source,
                canonicalTarget = source,
                transport = guessTransport(source),
            )
            fallbackTargets.add(target)
            return Result.success(target)
        }

        return runCatching {
            mapTarget(client.importTarget(source, displayName))
        }
    }

    fun connect(targetId: String, session: String?): Result<ConnectionStateFfi> {
        val client = ffi
        if (client == null) {
            return Result.success(
                ConnectionStateFfi(
                    id = "fallback-${System.currentTimeMillis()}",
                    targetId = targetId,
                    status = uniffi.bmux_mobile_ffi.ConnectionStatusFfi.CONNECTED,
                    session = session,
                    lastError = null,
                ),
            )
        }

        return runCatching { client.connect(targetId, session) }
    }

    fun observeAndApplyPin(target: String): Result<String> {
        val client = ffi
            ?: return Result.failure(IllegalStateException("FFI host-key observation unavailable"))

        return runCatching {
            val suggestion: HostKeyPinSuggestionFfi = client.observeSshHostKeyWithPinSuggestion(target)
            val pinned = client.applyPinSuggestionToTarget(target, suggestion)
            "Pinned ${suggestion.observed.algorithm} ${suggestion.observed.fingerprintSha256.take(16)}... on $pinned"
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
}

@Composable
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
            verticalArrangement = Arrangement.spacedBy(12.dp),
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

            Text(
                text = if (state.loading) "Connecting..." else "Targets",
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.SemiBold,
            )

            LazyColumn(
                modifier = Modifier.fillMaxSize(),
                contentPadding = PaddingValues(bottom = 48.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                items(state.targets, key = { it.id }) { target ->
                    TargetCard(
                        target = target,
                        onConnect = { vm.connect(target) },
                        onPin = { vm.observeAndPin(target) },
                    )
                }
            }
        }
    }
}

@Composable
private fun TargetCard(target: TargetUi, onConnect: () -> Unit, onPin: () -> Unit) {
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
            }
        }
    }
}
