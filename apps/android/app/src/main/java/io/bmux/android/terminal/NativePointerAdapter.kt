package io.bmux.android.terminal

import android.view.PointerIcon

internal class NativePointerRevisions {
    private var generation: String? = null
    private var revision: ULong = 0u
    @Volatile private var active = true

    fun invalidate() {
        active = false
    }

    fun accept(pointer: TerminalPointerFrame): Boolean {
        if (!active || pointer.version != 1 || pointer.generation.isEmpty()) return false
        // Updates arrive in FIFO order from one backend actor. A fresh generation
        // is explicit backend replacement (including pre-output recovery).
        if (generation == pointer.generation && pointer.revision <= revision) return false
        generation = pointer.generation
        revision = pointer.revision
        return true
    }

    fun reset() {
        generation = null
        revision = 0u
    }
}

/** Native output adapter only; all shape selection remains in the shared retained resolver. */
internal fun nativePointerType(pointer: TerminalPointerFrame?): Int {
    if (pointer == null) return PointerIcon.TYPE_DEFAULT
    if (pointer.name == null) return PointerIcon.TYPE_NULL
    return when (pointer.name) {
        "pointer", "alias" -> PointerIcon.TYPE_HAND
        "text" -> PointerIcon.TYPE_TEXT
        "vertical-text" -> PointerIcon.TYPE_VERTICAL_TEXT
        "crosshair", "cell" -> PointerIcon.TYPE_CROSSHAIR
        "grab" -> PointerIcon.TYPE_GRAB
        "grabbing" -> PointerIcon.TYPE_GRABBING
        "move", "all-scroll" -> PointerIcon.TYPE_ALL_SCROLL
        "e-resize", "w-resize", "ew-resize", "col-resize" -> PointerIcon.TYPE_HORIZONTAL_DOUBLE_ARROW
        "n-resize", "s-resize", "ns-resize", "row-resize" -> PointerIcon.TYPE_VERTICAL_DOUBLE_ARROW
        "ne-resize", "sw-resize", "nesw-resize" -> PointerIcon.TYPE_TOP_RIGHT_DIAGONAL_DOUBLE_ARROW
        "nw-resize", "se-resize", "nwse-resize" -> PointerIcon.TYPE_TOP_LEFT_DIAGONAL_DOUBLE_ARROW
        "not-allowed", "no-drop" -> PointerIcon.TYPE_NO_DROP
        "copy" -> PointerIcon.TYPE_COPY
        "help" -> PointerIcon.TYPE_HELP
        "wait", "progress" -> PointerIcon.TYPE_WAIT
        "zoom-in" -> PointerIcon.TYPE_ZOOM_IN
        "zoom-out" -> PointerIcon.TYPE_ZOOM_OUT
        // Android has no exact native icon for every CSS shape. Do not invent new policy.
        else -> PointerIcon.TYPE_DEFAULT
    }
}
