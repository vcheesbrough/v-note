package link.desync.vnote.ink

import androidx.compose.ui.geometry.Offset

private const val MIN_SCALE = 0.2f
private const val MAX_SCALE = 8f
private const val MOMENTUM_DECELERATION = 14_000f
private const val MOMENTUM_MAX_VELOCITY = 1_400f
internal const val MOMENTUM_STOP_VELOCITY = 18f

internal data class ViewportTransform(
    val scale: Float = 1f,
    val offset: Offset = Offset.Zero,
)

internal fun ViewportTransform.pan(delta: Offset): ViewportTransform = copy(offset = offset + delta)

internal fun ViewportTransform.zoomAround(
    zoom: Float,
    centroid: Offset,
): ViewportTransform {
    if (zoom == 1f) {
        return this
    }
    val nextScale = (scale * zoom).coerceIn(MIN_SCALE, MAX_SCALE)
    val effectiveZoom = nextScale / scale
    return ViewportTransform(
        scale = nextScale,
        offset = centroid - (centroid - offset) * effectiveZoom,
    )
}

internal fun ViewportTransform.applyGesture(
    panDelta: Offset,
    zoom: Float,
    centroid: Offset,
): ViewportTransform = pan(panDelta).zoomAround(zoom, centroid)

internal fun ViewportTransform.toWorld(position: Offset): Offset =
    Offset(
        x = (position.x - offset.x) / scale,
        y = (position.y - offset.y) / scale,
    )

internal fun dampVelocity(
    velocity: Offset,
    deltaSeconds: Float,
): Offset {
    if (deltaSeconds <= 0f) {
        return velocity
    }
    val speed = velocity.getDistance()
    if (speed <= MOMENTUM_STOP_VELOCITY) {
        return Offset.Zero
    }
    val nextSpeed = (speed - MOMENTUM_DECELERATION * deltaSeconds).coerceAtLeast(0f)
    if (nextSpeed <= MOMENTUM_STOP_VELOCITY) {
        return Offset.Zero
    }
    return velocity * (nextSpeed / speed)
}

internal fun shouldContinueMomentum(velocity: Offset): Boolean = velocity.getDistance() >= MOMENTUM_STOP_VELOCITY

internal fun capMomentumVelocity(velocity: Offset): Offset {
    val speed = velocity.getDistance()
    if (speed <= MOMENTUM_MAX_VELOCITY) {
        return velocity
    }
    return velocity * (MOMENTUM_MAX_VELOCITY / speed)
}

internal data class ViewportGestureStep(
    val panDelta: Offset,
    val zoom: Float,
    val centroid: Offset,
    val velocity: Offset,
)

internal class ViewportGestureTracker {
    private var previousCentroid: Offset? = null
    private var previousSpread: Float? = null
    private var previousPointerCount = 0
    private var previousEventTimeMillis: Long? = null
    private var latestVelocity = Offset.Zero
    private var movedWithSingleFinger = false

    fun update(
        pointerCount: Int,
        centroid: Offset,
        spread: Float,
        eventTimeMillis: Long,
    ): ViewportGestureStep {
        val pointerCountChanged = pointerCount != previousPointerCount
        val zoom =
            if (!pointerCountChanged && spread > 0f && previousSpread != null && previousSpread!! > 0f) {
                spread / previousSpread!!
            } else {
                1f
            }
        val panDelta =
            if (!pointerCountChanged) {
                previousCentroid?.let { centroid - it } ?: Offset.Zero
            } else {
                Offset.Zero
            }

        if (!pointerCountChanged && pointerCount == 1 && panDelta != Offset.Zero) {
            previousEventTimeMillis?.let { previous ->
                val deltaSeconds = ((eventTimeMillis - previous).coerceAtLeast(1L)) / 1000f
                latestVelocity = capMomentumVelocity(panDelta / deltaSeconds)
                movedWithSingleFinger = true
            }
        } else if (pointerCount != 1) {
            latestVelocity = Offset.Zero
            movedWithSingleFinger = false
        }

        previousCentroid = centroid
        previousSpread = spread
        previousPointerCount = pointerCount
        previousEventTimeMillis = eventTimeMillis

        return ViewportGestureStep(
            panDelta = panDelta,
            zoom = zoom,
            centroid = centroid,
            velocity = latestVelocity,
        )
    }

    fun velocity(): Offset = latestVelocity

    fun shouldLaunchMomentum(): Boolean = movedWithSingleFinger && shouldContinueMomentum(latestVelocity)
}
