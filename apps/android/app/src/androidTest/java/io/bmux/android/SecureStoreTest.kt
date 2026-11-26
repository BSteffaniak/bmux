package io.bmux.android

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import uniffi.bmux_mobile_ffi.MobileApiFfi
import uniffi.bmux_mobile_ffi.MobileFfiException
import java.io.File

@RunWith(AndroidJUnit4::class)
class SecureStoreTest {
    private val context = ApplicationProvider.getApplicationContext<Context>()

    @Test
    fun hostHealthAndLastHostRoundTripAndAreForgotten() {
        val store = SecureStore(context, "bmux_mobile_secure_${System.nanoTime()}")
        store.saveLastConnectedHost("desktop")
        store.recordHostHealth("desktop", success = true, timestampMs = 10)
        store.recordHostHealth("desktop", success = false, timestampMs = 20)
        store.recordHostHealth("laptop", success = true, timestampMs = 30)

        val desktop = store.loadHostHealth().single { it.host == "desktop" }
        assertEquals(10L, desktop.lastSuccessMs)
        assertEquals(20L, desktop.lastFailureMs)
        assertEquals("desktop", store.lastConnectedHost())

        store.forgetHost("desktop")
        assertEquals(listOf("laptop"), store.loadHostHealth().map { it.host })
        assertNull(store.lastConnectedHost())
    }

    @Test
    fun coreStartsWithNoPairedHostsAndRejectsNonPairingLinks() {
        val dir = File(context.cacheDir, "bmux-test-${System.nanoTime()}")
        try {
            val api = MobileApiFfi(dir.absolutePath)
            assertTrue(api.listHosts().isEmpty())
            val refused = runCatching { api.startPairing("ssh://ops@prod:22", null, "phone") }
            assertTrue(refused.exceptionOrNull() is MobileFfiException.InvalidPairingLink)
        } finally {
            dir.deleteRecursively()
        }
    }
}
