package io.bmux.android.terminal

import android.view.PointerIcon
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class NativePointerAdapterTest {
    @Test
    fun hiddenAndDefaultAreDifferentNativeOutputs() {
        val hidden = TerminalPointerFrame(1, "attach", 1u, null)
        assertEquals(PointerIcon.TYPE_NULL, nativePointerType(hidden))
        assertEquals(PointerIcon.TYPE_DEFAULT, nativePointerType(hidden.copy(name = "default")))
        assertEquals(PointerIcon.TYPE_HAND, nativePointerType(hidden.copy(name = "pointer")))
        assertEquals(PointerIcon.TYPE_GRABBING, nativePointerType(hidden.copy(name = "grabbing")))
    }

    @Test
    fun resetInvalidatesQueuedPointerAdmissionWithoutRevivingOldLifetime() {
        val old = NativePointerRevisions()
        val hidden = TerminalPointerFrame(1, "old", 4u, null)
        assertTrue(old.accept(hidden))
        old.invalidate()
        // These model callbacks captured before reset but run afterward.
        assertFalse(old.accept(hidden.copy(revision = 5u)))
        assertFalse(old.accept(hidden.copy(generation = "replacement", revision = 1u)))
        old.reset()
        assertFalse(old.accept(hidden))

        val replacement = NativePointerRevisions()
        assertTrue(replacement.accept(hidden.copy(generation = "replacement", revision = 1u)))
    }

    @Test
    fun unsupportedMetadataDoesNotAdvanceRevisionAdmission() {
        val gate = NativePointerRevisions()
        val first = TerminalPointerFrame(1, "first", 1u, "default")
        assertFalse(gate.accept(first.copy(generation = "")))
        assertFalse(gate.accept(first.copy(version = 0, revision = 99u)))
        assertTrue(gate.accept(first))
    }

    @Test
    fun revisionAdmissionIsExplicitAndGenerationScoped() {
        val gate = NativePointerRevisions()
        val first = TerminalPointerFrame(1, "first", 8u, "text")
        assertFalse(gate.accept(first.copy(version = 2)))
        assertTrue(gate.accept(first))
        assertFalse(gate.accept(first))
        assertFalse(gate.accept(first.copy(revision = 7u)))
        assertTrue(gate.accept(first.copy(revision = 9u)))
        assertTrue(gate.accept(first.copy(generation = "replacement", revision = 1u)))
        gate.reset()
        assertTrue(gate.accept(first))
    }
}
