package io.bmux.android

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import uniffi.bmux_mobile_ffi.MobileApiFfi

class ConnectionForegroundService : Service() {
    private val scope = CoroutineScope(Dispatchers.IO)
    private var reconnectJob: Job? = null
    private val ffi by lazy { runCatching { MobileApiFfi() }.getOrNull() }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val channelId = "bmux_connection"
        val manager = getSystemService(NotificationManager::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            manager.createNotificationChannel(
                NotificationChannel(
                    channelId,
                    "bmux connections",
                    NotificationManager.IMPORTANCE_LOW,
                ),
            )
        }

        val action = intent?.action ?: ACTION_START
        if (action == ACTION_STOP) {
            reconnectJob?.cancel()
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            return START_NOT_STICKY
        }

        val targetId = intent?.getStringExtra(EXTRA_TARGET_ID)
        val session = intent?.getStringExtra(EXTRA_SESSION)

        val notification = buildNotification(channelId, "Starting reconnect loop")

        startForeground(1, notification)
        startReconnectLoop(targetId, session, channelId)
        return START_STICKY
    }

    private fun startReconnectLoop(targetId: String?, session: String?, channelId: String) {
        val id = targetId ?: return
        reconnectJob?.cancel()
        reconnectJob = scope.launch {
            var backoffMs = 1_000L
            while (isActive) {
                val result = runCatching { ffi?.connect(id, session) }
                if (result.isSuccess) {
                    notifyForeground(channelId, "Connected. Keeping session alive")
                    backoffMs = 1_000L
                    delay(30_000)
                } else {
                    val message = result.exceptionOrNull()?.message ?: "Reconnect failed"
                    notifyForeground(channelId, message)
                    delay(backoffMs)
                    backoffMs = (backoffMs * 2).coerceAtMost(60_000L)
                }
            }
        }
    }

    private fun notifyForeground(channelId: String, text: String) {
        val manager = getSystemService(NotificationManager::class.java)
        manager.notify(1, buildNotification(channelId, text))
    }

    private fun buildNotification(channelId: String, text: String): Notification {
        return NotificationCompat.Builder(this, channelId)
            .setContentTitle("bmux reconnect service")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
            .setOngoing(true)
            .build()
    }

    override fun onDestroy() {
        reconnectJob?.cancel()
        super.onDestroy()
    }

    companion object {
        private const val ACTION_START = "io.bmux.android.RECONNECT_START"
        private const val ACTION_STOP = "io.bmux.android.RECONNECT_STOP"
        private const val EXTRA_TARGET_ID = "target_id"
        private const val EXTRA_SESSION = "session"

        fun start(context: Context, targetId: String, session: String?) {
            val intent = Intent(context, ConnectionForegroundService::class.java)
                .setAction(ACTION_START)
                .putExtra(EXTRA_TARGET_ID, targetId)
                .putExtra(EXTRA_SESSION, session)
            context.startForegroundService(intent)
        }

        fun stop(context: Context) {
            val intent = Intent(context, ConnectionForegroundService::class.java)
                .setAction(ACTION_STOP)
            context.startService(intent)
        }
    }
}
