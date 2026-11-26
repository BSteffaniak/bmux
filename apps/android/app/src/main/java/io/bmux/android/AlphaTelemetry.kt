package io.bmux.android

import android.util.Log

enum class AlphaEventKind {
    PairStart,
    PairSuccess,
    PairFailure,
    ForgetHost,
    ConnectAttempt,
    ConnectSuccess,
    ConnectFailure,
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
