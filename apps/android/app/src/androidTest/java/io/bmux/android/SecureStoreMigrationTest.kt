package io.bmux.android

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class SecureStoreMigrationTest {
    @Test
    fun migratesLegacyPrefsIntoEncryptedStore() {
        val context = ApplicationProvider.getApplicationContext<Context>()
        val suffix = System.currentTimeMillis().toString()
        val legacyName = "bmux_mobile_legacy_$suffix"
        val secureName = "bmux_mobile_secure_$suffix"

        val legacy = context.getSharedPreferences(legacyName, Context.MODE_PRIVATE)
        legacy.edit()
            .putString(
                "targets",
                "[{\"id\":\"one\",\"name\":\"prod\",\"canonicalTarget\":\"iroh://prod\",\"transport\":\"iroh\"}]",
            )
            .putStringSet("pinned_targets", setOf("ssh://ops@prod:22"))
            .putString("last_target_id", "one")
            .apply()

        val store = SecureStore(context, secureName)
        val migrated = store.migrateLegacyIfNeeded(legacyName)
        assertTrue(migrated)

        val targets = store.loadTargets()
        assertEquals(1, targets.size)
        assertEquals("one", targets.first().id)
        assertTrue(store.pinnedTargets().contains("ssh://ops@prod:22"))
        assertEquals("one", store.lastConnectedTargetId())

        val noSecondMigration = store.migrateLegacyIfNeeded(legacyName)
        assertFalse(noSecondMigration)

        val legacyAfter = context.getSharedPreferences(legacyName, Context.MODE_PRIVATE)
        assertNotNull(legacyAfter)
        assertTrue(legacyAfter.all.isEmpty())
    }
}
