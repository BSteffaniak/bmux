package io.bmux.android

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo

data class DiscoveredTarget(
    val serviceName: String,
    val host: String,
    val port: Int,
)

class LanDiscoveryManager(context: Context) {
    private val nsdManager = context.getSystemService(NsdManager::class.java)
    private var listener: NsdManager.DiscoveryListener? = null

    fun start(onUpdate: (List<DiscoveredTarget>) -> Unit, onError: (String) -> Unit) {
        stop()
        val seen = linkedMapOf<String, DiscoveredTarget>()

        listener = object : NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(regType: String) = Unit

            override fun onServiceFound(serviceInfo: NsdServiceInfo) {
                nsdManager.resolveService(
                    serviceInfo,
                    object : NsdManager.ResolveListener {
                        override fun onResolveFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
                            onError("Resolve failed for ${serviceInfo.serviceName} ($errorCode)")
                        }

                        override fun onServiceResolved(resolved: NsdServiceInfo) {
                            val host = resolved.host?.hostAddress ?: return
                            val key = "${resolved.serviceName}|$host|${resolved.port}"
                            seen[key] = DiscoveredTarget(
                                serviceName = resolved.serviceName,
                                host = host,
                                port = resolved.port,
                            )
                            onUpdate(seen.values.toList())
                        }
                    },
                )
            }

            override fun onServiceLost(serviceInfo: NsdServiceInfo) {
                val prefix = "${serviceInfo.serviceName}|"
                val keys = seen.keys.filter { it.startsWith(prefix) }
                keys.forEach(seen::remove)
                onUpdate(seen.values.toList())
            }

            override fun onDiscoveryStopped(serviceType: String) = Unit

            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                onError("Discovery start failed ($errorCode)")
                stop()
            }

            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {
                onError("Discovery stop failed ($errorCode)")
                stop()
            }
        }

        nsdManager.discoverServices(
            "_bmux._tcp.",
            NsdManager.PROTOCOL_DNS_SD,
            listener,
        )
    }

    fun stop() {
        val active = listener ?: return
        runCatching { nsdManager.stopServiceDiscovery(active) }
        listener = null
    }
}
