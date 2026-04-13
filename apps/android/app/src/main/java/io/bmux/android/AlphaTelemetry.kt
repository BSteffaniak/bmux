package io.bmux.android

import android.util.Log

enum class AlphaEventKind {
    ImportTarget,
    ConnectAttempt,
    ConnectSuccess,
    ConnectFailure,
    ObservePinAttempt,
    ObservePinSuccess,
    ObservePinFailure,
    DiscoveryStart,
    DiscoveryUpdate,
    DiscoveryStop,
    ReconnectEnabled,
    ReconnectDisabled,
}

object AlphaTelemetry {
    private const val TAG = "BmuxAlpha"

    fun log(kind: AlphaEventKind, message: String) {
        if (!BuildConfig.ALPHA_TELEMETRY_ENABLED) {
            return
        }
        Log.i(TAG, "${kind.name}|$message")
    }
}
