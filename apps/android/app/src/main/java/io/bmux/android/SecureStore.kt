package io.bmux.android

import android.content.Context
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey
import org.json.JSONArray
import org.json.JSONObject

/** When the app last reached, or failed to reach, a saved host. */
data class HostHealth(
    val host: String,
    val lastSuccessMs: Long?,
    val lastFailureMs: Long?,
)

/**
 * App preferences about saved hosts, in encrypted shared preferences. The saved hosts
 * themselves (alias, pinned host key, principal) and this device's key belong to the
 * Rust core, under [mobileStorageDir].
 */
class SecureStore(
    context: Context,
    storeName: String = "bmux_mobile_secure_store",
) {
    @Suppress("DEPRECATION")
    private val prefs = EncryptedSharedPreferences.create(
        context,
        storeName,
        MasterKey.Builder(context)
            .setKeyScheme(MasterKey.KeyScheme.AES256_GCM)
            .build(),
        EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
        EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
    )

    fun saveLastConnectedHost(host: String) {
        prefs.edit().putString(KEY_LAST_HOST, host).apply()
    }

    fun lastConnectedHost(): String? = prefs.getString(KEY_LAST_HOST, null)

    fun recordHostHealth(host: String, success: Boolean, timestampMs: Long) {
        val map = loadHostHealth().associateBy { it.host }.toMutableMap()
        val existing = map[host]
        map[host] = HostHealth(
            host = host,
            lastSuccessMs = if (success) timestampMs else existing?.lastSuccessMs,
            lastFailureMs = if (success) existing?.lastFailureMs else timestampMs,
        )
        saveHostHealth(map.values)
    }

    fun loadHostHealth(): List<HostHealth> {
        val raw = prefs.getString(KEY_HEALTH, null) ?: return emptyList()
        val array = JSONArray(raw)
        return buildList {
            for (index in 0 until array.length()) {
                val item = array.optJSONObject(index) ?: continue
                add(
                    HostHealth(
                        host = item.optString("host"),
                        lastSuccessMs = if (item.isNull("lastSuccessMs")) null else item.optLong("lastSuccessMs"),
                        lastFailureMs = if (item.isNull("lastFailureMs")) null else item.optLong("lastFailureMs"),
                    ),
                )
            }
        }
    }

    /** Drop what the app remembers about a host the core forgot. */
    fun forgetHost(host: String) {
        saveHostHealth(loadHostHealth().filter { it.host != host })
        if (lastConnectedHost() == host) {
            prefs.edit().remove(KEY_LAST_HOST).apply()
        }
    }

    private fun saveHostHealth(entries: Collection<HostHealth>) {
        val array = JSONArray()
        entries.forEach {
            array.put(
                JSONObject()
                    .put("host", it.host)
                    .put("lastSuccessMs", it.lastSuccessMs ?: JSONObject.NULL)
                    .put("lastFailureMs", it.lastFailureMs ?: JSONObject.NULL),
            )
        }
        prefs.edit().putString(KEY_HEALTH, array.toString()).apply()
    }

    private companion object {
        const val KEY_LAST_HOST = "last_host"
        const val KEY_HEALTH = "host_health"
    }
}
