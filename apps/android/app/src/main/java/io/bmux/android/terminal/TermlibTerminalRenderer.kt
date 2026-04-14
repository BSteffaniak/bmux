package io.bmux.android.terminal

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
        if (Looper.myLooper() == Looper.getMainLooper()) {
            emulator.writeInput(data)
        } else {
            mainHandler.post { emulator.writeInput(data) }
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
        Terminal(
            terminalEmulator = emulator,
            modifier = modifier
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
        onInput = null
        onResize = null
        onMouseEvent = null
    }
}
