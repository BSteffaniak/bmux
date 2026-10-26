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
