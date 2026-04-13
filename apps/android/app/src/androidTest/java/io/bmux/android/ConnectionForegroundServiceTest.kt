package io.bmux.android

import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.rule.ServiceTestRule
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class ConnectionForegroundServiceTest {
    @get:Rule
    val serviceRule = ServiceTestRule()

    @Test
    fun startAndStopActionsCanBeIssued() {
        val context = ApplicationProvider.getApplicationContext<android.content.Context>()
        val component = serviceRule.startService(
            ConnectionForegroundService.createStartIntent(context, "target-1", "main"),
        )
        assertNotNull(component)

        val stopComponent = context.startService(ConnectionForegroundService.createStopIntent(context))
        assertNotNull(stopComponent)
    }

    @Test
    fun reconnectBackoffGrowsAndResets() {
        val backoff = ReconnectBackoff(initialMs = 1_000, maxMs = 8_000)
        assertEquals(1_000, backoff.nextDelayMs())
        assertEquals(2_000, backoff.nextDelayMs())
        assertEquals(4_000, backoff.nextDelayMs())
        assertEquals(8_000, backoff.nextDelayMs())
        assertEquals(8_000, backoff.nextDelayMs())
        backoff.reset()
        assertEquals(1_000, backoff.nextDelayMs())
    }
}
