package io.bmux.android

import android.content.Context
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey
import org.json.JSONArray
import org.json.JSONObject

data class StoredTarget(
    val id: String,
    val name: String,
    val canonicalTarget: String,
    val transport: String,
)

data class StoredDiscovery(
    val serviceName: String,
    val host: String,
    val port: Int,
    val lastSeenMs: Long,
)

data class TargetHealth(
    val targetId: String,
    val lastSuccessMs: Long?,
    val lastFailureMs: Long?,
)

class SecureStore(
    private val context: Context,
    private val storeName: String = "bmux_mobile_secure_store",
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

    fun migrateLegacyIfNeeded(legacyStoreName: String = "bmux_mobile_legacy_store"): Boolean {
        if (prefs.getBoolean(KEY_MIGRATED, false)) {
            return false
        }
        val legacy = context.getSharedPreferences(legacyStoreName, Context.MODE_PRIVATE)
        if (legacy.all.isEmpty()) {
            prefs.edit().putBoolean(KEY_MIGRATED, true).apply()
            return false
        }

        legacy.getString(KEY_TARGETS, null)?.let { prefs.edit().putString(KEY_TARGETS, it).apply() }
        legacy.getStringSet(KEY_PINNED, null)?.let { prefs.edit().putStringSet(KEY_PINNED, it).apply() }
        legacy.getString(KEY_LAST_TARGET_ID, null)?.let {
            prefs.edit().putString(KEY_LAST_TARGET_ID, it).apply()
        }
        legacy.getString(KEY_DISCOVERY_HISTORY, null)?.let {
            prefs.edit().putString(KEY_DISCOVERY_HISTORY, it).apply()
        }
        legacy.getString(KEY_HEALTH, null)?.let { prefs.edit().putString(KEY_HEALTH, it).apply() }

        prefs.edit().putBoolean(KEY_MIGRATED, true).apply()
        legacy.edit().clear().apply()
        return true
    }

    fun saveTargets(targets: List<StoredTarget>) {
        val array = JSONArray()
        targets.forEach {
            array.put(
                JSONObject()
                    .put("id", it.id)
                    .put("name", it.name)
                    .put("canonicalTarget", it.canonicalTarget)
                    .put("transport", it.transport),
            )
        }
        prefs.edit().putString(KEY_TARGETS, array.toString()).apply()
    }

    fun loadTargets(): List<StoredTarget> {
        val raw = prefs.getString(KEY_TARGETS, null) ?: return emptyList()
        val array = JSONArray(raw)
        return buildList {
            for (index in 0 until array.length()) {
                val item = array.optJSONObject(index) ?: continue
                add(
                    StoredTarget(
                        id = item.optString("id"),
                        name = item.optString("name"),
                        canonicalTarget = item.optString("canonicalTarget"),
                        transport = item.optString("transport"),
                    ),
                )
            }
        }
    }

    fun savePinnedTarget(canonicalTarget: String) {
        val updated = prefs.getStringSet(KEY_PINNED, emptySet())
            ?.toMutableSet()
            ?: mutableSetOf()
        updated.add(canonicalTarget)
        prefs.edit().putStringSet(KEY_PINNED, updated).apply()
    }

    fun pinnedTargets(): Set<String> = prefs.getStringSet(KEY_PINNED, emptySet()) ?: emptySet()

    fun saveLastConnectedTarget(targetId: String) {
        prefs.edit().putString(KEY_LAST_TARGET_ID, targetId).apply()
    }

    fun lastConnectedTargetId(): String? = prefs.getString(KEY_LAST_TARGET_ID, null)

    fun upsertDiscoveryHistory(entries: List<StoredDiscovery>) {
        val current = loadDiscoveryHistory().associateBy { "${it.host}:${it.port}" }.toMutableMap()
        entries.forEach { entry ->
            current["${entry.host}:${entry.port}"] = entry
        }
        val array = JSONArray()
        current.values.sortedByDescending { it.lastSeenMs }.take(20).forEach {
            array.put(
                JSONObject()
                    .put("serviceName", it.serviceName)
                    .put("host", it.host)
                    .put("port", it.port)
                    .put("lastSeenMs", it.lastSeenMs),
            )
        }
        prefs.edit().putString(KEY_DISCOVERY_HISTORY, array.toString()).apply()
    }

    fun loadDiscoveryHistory(): List<StoredDiscovery> {
        val raw = prefs.getString(KEY_DISCOVERY_HISTORY, null) ?: return emptyList()
        val array = JSONArray(raw)
        return buildList {
            for (index in 0 until array.length()) {
                val item = array.optJSONObject(index) ?: continue
                add(
                    StoredDiscovery(
                        serviceName = item.optString("serviceName"),
                        host = item.optString("host"),
                        port = item.optInt("port"),
                        lastSeenMs = item.optLong("lastSeenMs"),
                    ),
                )
            }
        }
    }

    fun recordTargetHealth(targetId: String, success: Boolean, timestampMs: Long) {
        val map = loadTargetHealth().associateBy { it.targetId }.toMutableMap()
        val existing = map[targetId]
        map[targetId] = TargetHealth(
            targetId = targetId,
            lastSuccessMs = if (success) timestampMs else existing?.lastSuccessMs,
            lastFailureMs = if (success) existing?.lastFailureMs else timestampMs,
        )

        val array = JSONArray()
        map.values.forEach {
            array.put(
                JSONObject()
                    .put("targetId", it.targetId)
                    .put("lastSuccessMs", it.lastSuccessMs ?: JSONObject.NULL)
                    .put("lastFailureMs", it.lastFailureMs ?: JSONObject.NULL),
            )
        }
        prefs.edit().putString(KEY_HEALTH, array.toString()).apply()
    }

    fun loadTargetHealth(): List<TargetHealth> {
        val raw = prefs.getString(KEY_HEALTH, null) ?: return emptyList()
        val array = JSONArray(raw)
        return buildList {
            for (index in 0 until array.length()) {
                val item = array.optJSONObject(index) ?: continue
                add(
                    TargetHealth(
                        targetId = item.optString("targetId"),
                        lastSuccessMs = if (item.isNull("lastSuccessMs")) {
                            null
                        } else {
                            item.optLong("lastSuccessMs")
                        },
                        lastFailureMs = if (item.isNull("lastFailureMs")) {
                            null
                        } else {
                            item.optLong("lastFailureMs")
                        },
                    ),
                )
            }
        }
    }

    private companion object {
        const val KEY_TARGETS = "targets"
        const val KEY_PINNED = "pinned_targets"
        const val KEY_LAST_TARGET_ID = "last_target_id"
        const val KEY_DISCOVERY_HISTORY = "discovery_history"
        const val KEY_HEALTH = "target_health"
        const val KEY_MIGRATED = "legacy_migrated"
    }
}
