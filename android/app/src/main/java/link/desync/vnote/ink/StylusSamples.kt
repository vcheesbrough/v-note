package link.desync.vnote.ink

import android.view.MotionEvent
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import androidx.compose.ui.geometry.Offset
import link.desync.vnote.model.StrokePoint

// World-space stroke samples from stylus events, including coalesced history and
// normalised pressure. [pointerIndex] picks the stylus out of a multi-pointer
// event: a pen that lands while a finger or palm is already down is not index 0.

internal fun MotionEvent.toWorldPoint(
    viewport: ViewportTransform,
    startTime: Long,
    pressure: Double? = null,
    pointerIndex: Int = 0,
): StrokePoint {
    val world = viewport.toWorld(Offset(getX(pointerIndex), getY(pointerIndex)))
    return StrokePoint(world.x.toDouble(), world.y.toDouble(), eventTime - startTime, pressure)
}

// World-space sample for a historical (coalesced) pointer position in this
// event. Between frames Android batches several samples; capturing them keeps
// fast pressure changes and fine geometry from being lost.
internal fun MotionEvent.historicalWorldPoint(
    index: Int,
    viewport: ViewportTransform,
    startTime: Long,
    pressure: Double?,
    pointerIndex: Int = 0,
): StrokePoint {
    val world = viewport.toWorld(Offset(getHistoricalX(pointerIndex, index), getHistoricalY(pointerIndex, index)))
    return StrokePoint(
        world.x.toDouble(),
        world.y.toDouble(),
        getHistoricalEventTime(index) - startTime,
        pressure,
    )
}

// Normalise a raw stylus pressure sample for the wire: null when this gesture is
// not capturing pressure (no drawing style was captured at stylus-down),
// otherwise clamped to 0.0..1.0 (raw pressure can exceed 1.0 on some devices,
// and NaN must never reach the contract).
internal fun normalizePressure(
    raw: Float,
    capturesPressure: Boolean,
): Double? {
    if (!capturesPressure) return null
    if (raw.isNaN()) return 0.0
    return raw.toDouble().coerceIn(0.0, 1.0)
}

internal fun MotionEvent.capturedPressure(
    capturesPressure: Boolean,
    pointerIndex: Int = 0,
): Double? = normalizePressure(getPressure(pointerIndex), capturesPressure)

internal fun MotionEvent.capturedHistoricalPressure(
    index: Int,
    capturesPressure: Boolean,
    pointerIndex: Int = 0,
): Double? = normalizePressure(getHistoricalPressure(pointerIndex, index), capturesPressure)
