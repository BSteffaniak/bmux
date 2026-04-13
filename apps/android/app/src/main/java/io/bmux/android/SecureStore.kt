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

class SecureStore(context: Context) {
    private val prefs = EncryptedSharedPreferences.create(
        context,
        "bmux_mobile_secure_store",
        MasterKey.Builder(context)
            .setKeyScheme(MasterKey.KeyScheme.AES256_GCM)
            .build(),
        EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
        EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
    )

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

    private companion object {
        const val KEY_TARGETS = "targets"
        const val KEY_PINNED = "pinned_targets"
        const val KEY_LAST_TARGET_ID = "last_target_id"
    }
}
