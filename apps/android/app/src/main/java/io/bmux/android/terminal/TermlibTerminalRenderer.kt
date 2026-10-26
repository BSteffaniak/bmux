package io.bmux.android.terminal

import android.view.PointerIcon as AndroidPointerIcon
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.input.pointer.PointerEventType
import androidx.compose.ui.input.pointer.PointerIcon
import androidx.compose.ui.input.pointer.pointerHoverIcon
import androidx.compose.ui.platform.LocalContext
import android.os.Handler
import android.os.Looper
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalViewConfiguration
import org.connectbot.terminal.Terminal
import org.connectbot.terminal.TerminalEmulatorFactory

class TermlibTerminalRenderer : TerminalRenderer {
    private var onInput: ((ByteArray) -> Unit)? = null
    private var onResize: ((Int, Int) -> Unit)? = null
    private var onMouseEvent: ((TerminalMouseEvent) -> Unit)? = null
    private var latestRows: Int? = null
    private var latestCols: Int? = null
    private var layoutWidthPx: Int = 0
    private var layoutHeightPx: Int = 0
    private val mainHandler = Handler(Looper.getMainLooper())
    private var pointer by mutableStateOf<TerminalPointerFrame?>(null)
    private val pointerRevisions = NativePointerRevisions()
    @Volatile private var disposed = false
    private var outputAccepted = true

    private companion object {
        const val TAP_MAX_DURATION_MS = 450L
    }

    private val emulator = TerminalEmulatorFactory.create(
        onKeyboardInput = { data -> onInput?.invoke(data) },
        onResize = { dimensions ->
            latestRows = dimensions.rows
            latestCols = dimensions.columns
            onResize?.invoke(dimensions.rows, dimensions.columns)
        },
    )

    override fun appendOutput(data: ByteArray) {
        val accept = {
            if (!disposed) {
                outputAccepted = false
                emulator.writeInput(data)
                outputAccepted = true
            }
        }
        if (Looper.myLooper() == Looper.getMainLooper()) {
            accept()
        } else {
            mainHandler.post { accept() }
        }
    }

    override fun applyPointer(pointer: TerminalPointerFrame) {
        // FIFO main-thread tasks: prior emulator.writeInput calls complete first.
        mainHandler.post {
            if (disposed || !outputAccepted || !pointerRevisions.accept(pointer)) return@post
            this.pointer = pointer
        }
    }

    override fun resetPointer() {
        mainHandler.post {
            pointer = null
            pointerRevisions.reset()
        }
    }

    override fun setOnInput(handler: (ByteArray) -> Unit) {
        onInput = handler
    }

    override fun setOnResize(handler: (rows: Int, cols: Int) -> Unit) {
        onResize = handler
        val rows = latestRows
        val cols = latestCols
        if (rows != null && cols != null) {
            onResize?.invoke(rows, cols)
        }
    }

    override fun setOnMouseEvent(handler: (TerminalMouseEvent) -> Unit) {
        onMouseEvent = handler
    }

    @Composable
    override fun Render(modifier: Modifier) {
        val touchSlop = LocalViewConfiguration.current.touchSlop
        val context = LocalContext.current
        val pointerIcon = PointerIcon(AndroidPointerIcon.getSystemIcon(context, nativePointerType(pointer)))
        Terminal(
            terminalEmulator = emulator,
            modifier = modifier
                .pointerHoverIcon(pointerIcon)
                .pointerInput(Unit) {
                    awaitPointerEventScope {
                        while (true) {
                            val event = awaitPointerEvent(PointerEventPass.Initial)
                            if (event.type == PointerEventType.Exit) {
                                onMouseEvent?.invoke(TerminalMouseEvent(TerminalMouseEventKind.LEAVE, row = 0, col = 0))
                            } else if (event.type == PointerEventType.Move || event.type == PointerEventType.Enter) {
                                val change = event.changes.firstOrNull() ?: continue
                                if (!change.pressed) {
                                    val cell = pointerOffsetToCell(change.position) ?: continue
                                    onMouseEvent?.invoke(TerminalMouseEvent(TerminalMouseEventKind.MOVE, row = cell.first, col = cell.second))
                                }
                            }
                        }
                    }
                }
                .onSizeChanged {
                    layoutWidthPx = it.width
                    layoutHeightPx = it.height
                }
                .pointerInput(touchSlop) {
                    awaitEachGesture {
                        val down = awaitFirstDown(
                            requireUnconsumed = false,
                            pass = PointerEventPass.Initial,
                        )
                        var upOffset: Offset? = null
                        var upUptimeMillis: Long? = null
                        var maxTravelSq = 0f
                        while (true) {
                            val event = awaitPointerEvent(PointerEventPass.Initial)
                            if (event.changes.any { it.id != down.id && it.pressed }) {
                                return@awaitEachGesture
                            }

                            val change = event.changes.firstOrNull { it.id == down.id } ?: continue
                            val deltaX = change.position.x - down.position.x
                            val deltaY = change.position.y - down.position.y
                            val travelSq = (deltaX * deltaX) + (deltaY * deltaY)
                            if (travelSq > maxTravelSq) {
                                maxTravelSq = travelSq
                            }

                            if (!change.pressed) {
                                upOffset = change.position
                                upUptimeMillis = change.uptimeMillis
                                break
                            }
                        }

                        if (maxTravelSq > touchSlop * touchSlop) {
                            return@awaitEachGesture
                        }
                        val releaseUptimeMillis = upUptimeMillis ?: return@awaitEachGesture
                        val pressDurationMillis =
                            releaseUptimeMillis.toLong() - down.uptimeMillis.toLong()
                        if (pressDurationMillis > TAP_MAX_DURATION_MS) {
                            return@awaitEachGesture
                        }

                        emitTapMouseEvents(down.position, upOffset ?: return@awaitEachGesture)
                    }
                },
            keyboardEnabled = true,
            showSoftKeyboard = true,
        )
    }

    private fun emitTapMouseEvents(downOffset: Offset, upOffset: Offset) {
        val downCell = pointerOffsetToCell(downOffset) ?: return
        val upCell = pointerOffsetToCell(upOffset) ?: return

        onMouseEvent?.invoke(
            TerminalMouseEvent(
                kind = TerminalMouseEventKind.DOWN,
                button = TerminalMouseButton.LEFT,
                row = downCell.first,
                col = downCell.second,
            ),
        )
        onMouseEvent?.invoke(
            TerminalMouseEvent(
                kind = TerminalMouseEventKind.UP,
                button = TerminalMouseButton.LEFT,
                row = upCell.first,
                col = upCell.second,
            ),
        )
    }

    private fun pointerOffsetToCell(offset: Offset): Pair<Int, Int>? {
        val rows = latestRows ?: return null
        val cols = latestCols ?: return null
        if (rows <= 0 || cols <= 0 || layoutWidthPx <= 0 || layoutHeightPx <= 0) {
            return null
        }

        val clampedX = offset.x.coerceIn(0f, (layoutWidthPx - 1).toFloat())
        val clampedY = offset.y.coerceIn(0f, (layoutHeightPx - 1).toFloat())
        val col = ((clampedX / layoutWidthPx.toFloat()) * cols)
            .toInt()
            .coerceIn(0, cols - 1)
        val row = ((clampedY / layoutHeightPx.toFloat()) * rows)
            .toInt()
            .coerceIn(0, rows - 1)

        return row to col
    }

    override fun dispose() {
        disposed = true
        pointer = null
        pointerRevisions.reset()
        onInput = null
        onResize = null
        onMouseEvent = null
    }
}
