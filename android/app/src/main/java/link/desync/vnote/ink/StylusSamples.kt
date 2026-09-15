package link.desync.vnote.ink

import android.view.MotionEvent
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import androidx.compose.ui.geometry.Offset
import link.desync.vnote.model.StrokePoint

// World-space stroke samples from stylus events, including coalesced history and
// normalised pressure.

internal fun MotionEvent.toWorldPoint(
    viewport: ViewportTransform,
    startTime: Long,
    pressure: Double? = null,
): StrokePoint {
    val world = viewport.toWorld(Offset(x, y))
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
): StrokePoint {
    val world = viewport.toWorld(Offset(getHistoricalX(index), getHistoricalY(index)))
    return StrokePoint(
        world.x.toDouble(),
        world.y.toDouble(),
        getHistoricalEventTime(index) - startTime,
        pressure,
    )
}

// Normalise a raw stylus pressure sample for the wire: null when the active
// style ignores pressure (v1), otherwise clamped to 0.0..1.0 (raw pressure can
// exceed 1.0 on some devices, and NaN must never reach the contract).
internal fun normalizePressure(
    raw: Float,
    sensitive: Boolean,
): Double? {
    if (!sensitive) return null
    if (raw.isNaN()) return 0.0
    return raw.toDouble().coerceIn(0.0, 1.0)
}

internal fun MotionEvent.capturedPressure(sensitive: Boolean): Double? = normalizePressure(pressure, sensitive)

internal fun MotionEvent.capturedHistoricalPressure(
    index: Int,
    sensitive: Boolean,
): Double? = normalizePressure(getHistoricalPressure(index), sensitive)
